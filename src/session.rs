//! Shared page-session layer: connect to the recorded browser session,
//! attach to the current page target, and provide the CDP helpers the
//! command implementations build on.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::cdp::client::{CdpClient, Event};
use crate::cdp::http;
use crate::hint::hint_error;
use crate::selector::{ElementProbe, ElementSelector};
use crate::state;

pub(crate) mod recording;

pub const RDNY_INSTRUMENTATION_VERSION: u32 = 2;
pub const RDNY_INSTRUMENTATION_SCRIPT: &str = r#"(() => {
    const key = Symbol.for('rdny.wait.instrumentation.v1');
    const state = window[key] || { active: 0, seq: 0, installed: false, version: 0 };
    if (state.version !== 2) { state.version = 2; state.seq += 1; }
    const inc = () => { state.active += 1; state.seq += 1; };
    const dec = () => { if (state.active > 0) state.active -= 1; state.seq += 1; };
    Object.defineProperty(window, key, { value: state, configurable: true });
    if (typeof window.fetch === 'function' && window.fetch.__rdnyVersion !== 2) {
        const baseFetch = window.fetch.__rdnyOriginal || window.fetch;
        const wrappedFetch = (...args) => {
            inc();
            try {
                return Promise.resolve(baseFetch.apply(window, args)).finally(dec);
            } catch (err) {
                dec();
                throw err;
            }
        };
        Object.defineProperty(wrappedFetch, '__rdnyVersion', { value: 2 });
        Object.defineProperty(wrappedFetch, '__rdnyOriginal', { value: baseFetch });
        window.fetch = wrappedFetch;
    }
    if (window.XMLHttpRequest && window.XMLHttpRequest.prototype.__rdnyVersion !== 2) {
        const proto = window.XMLHttpRequest.prototype;
        const originalOpen = proto.__rdnyOriginalOpen || proto.open;
        const originalSend = proto.__rdnyOriginalSend || proto.send;
        proto.open = function(...args) {
            const prior = this.__rdnyRequest;
            if (prior && prior.counted && !prior.done) prior.finish();
            this.__rdnyRequest = null;
            return originalOpen.apply(this, args);
        };
        proto.send = function(...args) {
            if (this.__rdnyRequest && this.__rdnyRequest.counted && !this.__rdnyRequest.done) {
                return originalSend.apply(this, args);
            }
            const request = { counted: false, done: false, finish: null };
            request.finish = () => {
                if (!request.done) {
                    request.done = true;
                    if (request.counted) dec();
                    request.counted = false;
                }
            };
            const onDone = () => request.finish();
            this.addEventListener('loadend', onDone, { once: true });
            this.addEventListener('error', onDone, { once: true });
            this.addEventListener('abort', onDone, { once: true });
            this.addEventListener('timeout', onDone, { once: true });
            try {
                request.counted = true;
                this.__rdnyRequest = request;
                inc();
                const result = originalSend.apply(this, args);
                if (this.readyState === 4) request.finish();
                return result;
            } catch (err) {
                if (!request.done) request.finish();
                throw err;
            }
        };
        Object.defineProperty(proto, '__rdnyVersion', { value: 2 });
        Object.defineProperty(proto, '__rdnyOriginalOpen', { value: originalOpen });
        Object.defineProperty(proto, '__rdnyOriginalSend', { value: originalSend });
    }
    state.installed = true;
    return true;
})()"#;

pub const RDNY_INSTRUMENTATION_STATE: &str = r#"(() => {
    const state = window[Symbol.for('rdny.wait.instrumentation.v1')];
    return { active: state ? state.active : 0, seq: state ? state.seq : 0, version: state ? state.version : 0, readyState: document.readyState };
})()"#;

/// Absolute timeout budget for commands that mix CDP calls, event polling, and sleeps.
#[derive(Debug, Clone, Copy)]
pub struct Deadline {
    at: Instant,
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::selector::{ElementProbe, ElementSelector};
    use std::net::TcpListener;
    use std::thread;
    use tungstenite::{Message, accept};

    #[test]
    fn recording_page_command_post_dispatch_drain_writes_and_acks_frame() {
        let _env = crate::state::ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let prior = std::env::var_os("RDNY_STATE_DIR");
        let prior_xdg = std::env::var_os("XDG_STATE_HOME");
        let temp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("RDNY_STATE_DIR", temp.path().join("state"));
            std::env::set_var("XDG_STATE_HOME", temp.path().join("xdg"));
        }
        let frames = crate::state::create_recording_frames_dir("drain-regression")
            .unwrap()
            .unwrap();
        crate::state::replace(&crate::state::SessionState {
            instance_id: None,
            endpoint: None,
            ws_url: "ws://unused".into(),
            host: "127.0.0.1".into(),
            port: 1,
            pid: None,
            process_identity: None,
            user_data_dir: None,
            browser_path: None,
            target_id: Some("target".into()),
            label: None,
            viewport: None,
            recording: true,
            recording_id: Some("drain-regression".into()),
            recording_frames_dir: Some(frames.path().to_path_buf()),
            recoverable_recording: None,
            recoverable_recordings: Vec::new(),
            instrumentation: None,
        })
        .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = accept(stream).unwrap();
            let command: Value = match socket.read().unwrap() {
                Message::Text(text) => serde_json::from_str(&text).unwrap(),
                message => panic!("unexpected command message {message:?}"),
            };
            assert_eq!(command["method"], "Runtime.evaluate");
            socket
                .send(Message::Text(
                    r#"{"method":"Page.screencastFrame","sessionId":"page-session","params":{"data":"aGVsbG8=","sessionId":42,"metadata":{"timestamp":1.25}}}"#
                        .into(),
                ))
                .unwrap();
            socket
                .send(Message::Text(
                    format!(
                        r#"{{"id":{},"result":{{"result":{{"value":"ok"}}}}}}"#,
                        command["id"]
                    )
                    .into(),
                ))
                .unwrap();
            let ack: Value = match socket.read().unwrap() {
                Message::Text(text) => serde_json::from_str(&text).unwrap(),
                message => panic!("unexpected ack message {message:?}"),
            };
            assert_eq!(ack["method"], "Page.screencastFrameAck");
            assert_eq!(ack["params"]["sessionId"], 42);
            socket
                .send(Message::Text(
                    format!(r#"{{"id":{},"result":{{}}}}"#, ack["id"]).into(),
                ))
                .unwrap();
            thread::sleep(Duration::from_millis(150));
            let _ = socket.close(None);
        });

        let client = CdpClient::connect(&format!("ws://127.0.0.1:{port}")).unwrap();
        let mut session = PageSession {
            client,
            session_id: "page-session".into(),
            instance_id: Some("instance".into()),
            target_id: "target".into(),
            frames_dir: Some(frames),
            instrumentation_registered: true,
            timeout: Duration::from_secs(1),
            deadline: Deadline::after(Duration::from_secs(1)),
        };
        assert!(session.is_recording());
        assert_eq!(session.instance_id(), Some("instance"));
        assert_eq!(session.target_id(), "target");
        let context = session.page_identity().unwrap();
        assert_eq!(context.instance.as_deref(), Some("instance"));
        assert_eq!(context.target.as_deref(), Some("target"));
        assert_eq!(context.url.as_deref(), Some("ok"));
        session.drain_events(Duration::from_millis(100)).unwrap();
        server.join().unwrap();
        assert!(
            std::fs::read_dir(temp.path().join("state/recordings/drain-regression/frames"))
                .unwrap()
                .flatten()
                .any(|entry| entry.path().extension().is_some_and(|ext| ext == "jpg"))
        );
        if let Some(prior) = prior {
            unsafe { std::env::set_var("RDNY_STATE_DIR", prior) };
        } else {
            unsafe { std::env::remove_var("RDNY_STATE_DIR") };
        }
        if let Some(prior) = prior_xdg {
            unsafe { std::env::set_var("XDG_STATE_HOME", prior) };
        } else {
            unsafe { std::env::remove_var("XDG_STATE_HOME") };
        }
    }

    #[test]
    fn element_and_wait_probe_share_nested_shadow_traversal() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = accept(stream).unwrap();

            let element_command = read_json_message(&mut socket);
            assert_eq!(element_command["method"], "Runtime.evaluate");
            let element_expression = element_command["params"]["expression"].as_str().unwrap();
            assert!(
                element_expression
                    .contains(r#"const segments = ["outer-host","inner-host","button.save"];"#)
            );
            assert!(element_expression.contains("root = element.shadowRoot"));
            assert!(element_expression.contains("const probe = false"));
            socket
                .send(Message::Text(
                    format!(
                        r#"{{"id":{},"result":{{"result":{{"type":"object","subtype":"node","objectId":"node-1"}}}}}}"#,
                        element_command["id"]
                    )
                    .into(),
                ))
                .unwrap();

            let probe_command = read_json_message(&mut socket);
            assert_eq!(probe_command["method"], "Runtime.evaluate");
            let probe_expression = probe_command["params"]["expression"].as_str().unwrap();
            assert!(
                probe_expression
                    .contains(r#"const segments = ["outer-host","inner-host","button.save"];"#)
            );
            assert!(probe_expression.contains("root = element.shadowRoot"));
            assert!(probe_expression.contains("const probe = true"));
            socket
                .send(Message::Text(
                    format!(
                        r#"{{"id":{},"result":{{"result":{{"type":"object","value":{{"found":false,"kind":"missing","index":1,"css":"inner-host"}}}}}}}}"#,
                        probe_command["id"]
                    )
                    .into(),
                ))
                .unwrap();
        });

        let mut session = test_page_session(port);
        let selector =
            ElementSelector::parse("outer-host >>> inner-host >>> button.save", true).unwrap();
        assert_eq!(session.element(&selector).unwrap(), "node-1");
        assert_eq!(
            session.probe_element(&selector).unwrap(),
            ElementProbe::Missing {
                index: 1,
                css: "inner-host".into()
            }
        );
        server.join().unwrap();
    }

    #[test]
    fn element_surfaces_segment_specific_invalid_css_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = accept(stream).unwrap();
            let command = read_json_message(&mut socket);
            socket
                .send(Message::Text(
                    format!(
                        r#"{{"id":{},"result":{{"result":{{"type":"object","subtype":"error"}},"exceptionDetails":{{"text":"Uncaught","exception":{{"description":"Error: invalid CSS in shadow host segment 1 `[`: SyntaxError"}}}}}}}}"#,
                        command["id"]
                    )
                    .into(),
                ))
                .unwrap();
        });

        let mut session = test_page_session(port);
        let selector = ElementSelector::parse("[ >>> button", true).unwrap();
        let error = session.element(&selector).unwrap_err().to_string();
        assert!(error.contains("selector resolution failed"), "{error}");
        assert!(
            error.contains("invalid CSS in shadow host segment 1 `[`"),
            "{error}"
        );
        server.join().unwrap();
    }

    #[test]
    fn action_point_and_revalidation_reuse_the_resolved_remote_node() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = accept(stream).unwrap();

            let resolve = read_json_message(&mut socket);
            assert_eq!(resolve["method"], "Runtime.evaluate");
            socket
                .send(Message::Text(
                    format!(
                        r#"{{"id":{},"result":{{"result":{{"type":"object","subtype":"node","objectId":"selected-node"}}}}}}"#,
                        resolve["id"]
                    )
                    .into(),
                ))
                .unwrap();

            let geometry = read_json_message(&mut socket);
            assert_eq!(geometry["method"], "Runtime.callFunctionOn");
            assert_eq!(geometry["params"]["objectId"], "selected-node");
            assert_eq!(geometry["params"]["returnByValue"], true);
            assert_eq!(geometry["params"]["arguments"][0]["value"], "resolve");
            assert_eq!(geometry["params"]["arguments"][1]["value"], Value::Null);
            assert!(
                geometry["params"]["functionDeclaration"]
                    .as_str()
                    .unwrap()
                    .contains("const isComposedDescendant")
            );
            socket
                .send(Message::Text(
                    format!(
                        r#"{{"id":{},"result":{{"result":{{"type":"object","value":{{"status":"ready","x":10.5,"y":20.25,"selected":{{"tag":"button","id":"save","classes":[]}}}}}}}}}}"#,
                        geometry["id"]
                    )
                    .into(),
                ))
                .unwrap();

            let revalidate = read_json_message(&mut socket);
            assert_eq!(revalidate["method"], "Runtime.callFunctionOn");
            assert_eq!(revalidate["params"]["objectId"], "selected-node");
            assert_eq!(revalidate["params"]["arguments"][0]["value"], "validate");
            assert_eq!(revalidate["params"]["arguments"][1]["value"], 10.5);
            assert_eq!(revalidate["params"]["arguments"][2]["value"], 20.25);
            socket
                .send(Message::Text(
                    format!(
                        r#"{{"id":{},"result":{{"result":{{"type":"object","value":{{"status":"ready","x":10.5,"y":20.25,"selected":{{"tag":"button","id":"save","classes":[]}}}}}}}}}}"#,
                        revalidate["id"]
                    )
                    .into(),
                ))
                .unwrap();
        });

        let mut session = test_page_session(port);
        let selector = ElementSelector::parse("#save", false).unwrap();
        let id = session.element(&selector).unwrap();
        let point = session.element_action_point(&id).unwrap();
        assert_eq!(
            point,
            crate::interaction_target::ActionPoint { x: 10.5, y: 20.25 }
        );
        session.revalidate_element_action_point(&id, point).unwrap();
        server.join().unwrap();
    }

    fn read_json_message(socket: &mut tungstenite::WebSocket<std::net::TcpStream>) -> Value {
        match socket.read().unwrap() {
            Message::Text(text) => serde_json::from_str(&text).unwrap(),
            message => panic!("unexpected command message {message:?}"),
        }
    }

    fn test_page_session(port: u16) -> PageSession {
        PageSession {
            client: CdpClient::connect(&format!("ws://127.0.0.1:{port}")).unwrap(),
            session_id: "page-session".into(),
            instance_id: None,
            target_id: "target".into(),
            frames_dir: None,
            instrumentation_registered: true,
            timeout: Duration::from_secs(1),
            deadline: Deadline::after(Duration::from_secs(1)),
        }
    }
}

impl Deadline {
    pub fn after(timeout: Duration) -> Self {
        let start = Instant::now();
        Self {
            at: start + timeout,
        }
    }

    pub fn at(at: Instant) -> Self {
        Self { at }
    }

    pub fn remaining(self) -> Option<Duration> {
        self.at.checked_duration_since(Instant::now())
    }

    pub fn expired(self) -> bool {
        self.remaining().is_none_or(|r| r.is_zero())
    }

    pub fn instant(self) -> Instant {
        self.at
    }

    pub fn sleep(self, maximum: Duration) {
        if let Some(remaining) = self.remaining() {
            std::thread::sleep(remaining.min(maximum));
        }
    }
}

/// A live connection to the session's current page target.
pub struct PageSession {
    client: CdpClient,
    session_id: String,
    instance_id: Option<String>,
    target_id: String,
    frames_dir: Option<state::SecureDir>,
    instrumentation_registered: bool,
    /// Overall budget for waiting-style commands (from --timeout).
    pub timeout: Duration,
    deadline: Deadline,
}

/// Identity owned by a live page session, independent of command output DTOs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PageIdentity {
    pub instance: Option<String>,
    pub target: Option<String>,
    pub url: Option<String>,
}

/// Load the session state, connect, and attach to the current page.
/// If the recorded target is gone, falls back to the first open page
/// (and persists the switch).
pub fn connect(deadline: Deadline, timeout: Duration) -> Result<PageSession> {
    let state = state::require()?;
    let targets = targets_until(&state, deadline).map_err(|_| {
        hint_error(
            format!(
                "cannot reach the browser for this session at {}:{}",
                state.host, state.port
            ),
            "run `rdny status`; if it reports stale, run `rdny stop` then `rdny start`",
            None,
        )
    })?;
    let pages: Vec<_> = targets.iter().filter(|t| t.target_type == "page").collect();
    let target = state
        .target_id
        .as_ref()
        .and_then(|id| pages.iter().find(|t| &t.id == id).copied())
        .or_else(|| pages.first().copied())
        .ok_or_else(|| {
            hint_error(
                "the browser has no open pages",
                "open one with `rdny newpage <url>`",
                None,
            )
        })?;
    let target_id = target.id.clone();
    if state.target_id.as_deref() != Some(target_id.as_str()) {
        state::update(|state| {
            state.target_id = Some(target_id.clone());
            Ok(())
        })?;
    }
    let mut client = CdpClient::connect_state_until(&state, deadline)
        .context("connecting to browser CDP transport")?;
    client.set_timeout(timeout);
    let session_id = client.attach_to_target_until(&target_id, deadline)?;
    // Enable Network once per attached page session so waitidle can observe
    // requests that began before the wait command but after rdny attached.
    let _ = client.call_until(Some(&session_id), "Network.enable", json!({}), deadline);
    if let Some(viewport) = &state.viewport {
        client.call_until(
            Some(&session_id),
            "Emulation.setDeviceMetricsOverride",
            viewport.cdp_params(),
            deadline,
        )?;
    }
    let frames_dir = if state.recording {
        let frames_dir = state::recording_frames_dir(&state)?;
        frames_dir.validate_external_path()?;
        client.call_until(
            Some(&session_id),
            "Page.startScreencast",
            json!({"format": "jpeg", "quality": 70, "everyNthFrame": 1}),
            deadline,
        )?;
        Some(frames_dir)
    } else {
        None
    };
    let mut session = PageSession {
        client,
        session_id,
        instance_id: state.instance_id.clone(),
        target_id,
        frames_dir,
        instrumentation_registered: false,
        timeout,
        deadline,
    };
    session.ensure_page_instrumentation_until(deadline)?;
    Ok(session)
}

pub(crate) fn targets_until(
    state: &state::SessionState,
    deadline: Deadline,
) -> Result<Vec<http::TargetInfo>> {
    if state.endpoint.is_none() {
        return http::list_targets_until(&state.host, state.port, deadline.instant());
    }
    let mut client = CdpClient::connect_state_until(state, deadline)?;
    let result = client.call_until(None, "Target.getTargets", json!({}), deadline)?;
    Ok(result["targetInfos"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| {
            Some(http::TargetInfo {
                id: value["targetId"].as_str()?.to_string(),
                target_type: value["type"].as_str()?.to_string(),
                title: value["title"].as_str().unwrap_or_default().to_string(),
                url: value["url"].as_str().unwrap_or_default().to_string(),
            })
        })
        .collect())
}

pub(crate) fn create_target_until(
    state: &state::SessionState,
    deadline: Deadline,
) -> Result<http::TargetInfo> {
    if state.endpoint.is_none() {
        return http::new_tab_until(&state.host, state.port, None, deadline.instant());
    }
    let mut client = CdpClient::connect_state_until(state, deadline)?;
    let result = client.call_until(
        None,
        "Target.createTarget",
        json!({"url":"about:blank"}),
        deadline,
    )?;
    let id = result["targetId"]
        .as_str()
        .context("Target.createTarget response missing targetId")?
        .to_string();
    Ok(http::TargetInfo {
        id,
        target_type: "page".into(),
        title: String::new(),
        url: "about:blank".into(),
    })
}

fn check_exception(result: &Value, what: &str) -> Result<()> {
    let Some(details) = result.get("exceptionDetails") else {
        return Ok(());
    };
    let msg = details["exception"]["description"]
        .as_str()
        .or_else(|| details["exception"]["value"].as_str())
        .or_else(|| details["text"].as_str())
        .unwrap_or("unknown JavaScript exception");
    bail!("{what}: {msg}");
}

impl PageSession {
    #[cfg(test)]
    pub(crate) fn new_for_test(client: CdpClient, session_id: &str, target_id: &str) -> Self {
        Self {
            client,
            session_id: session_id.into(),
            instance_id: Some("test-instance".into()),
            target_id: target_id.into(),
            frames_dir: None,
            instrumentation_registered: true,
            timeout: Duration::from_secs(1),
            deadline: Deadline::after(Duration::from_secs(1)),
        }
    }

    /// Minimal page session for focused fake-CDP input protocol tests.
    #[cfg(test)]
    pub(crate) fn connect_for_input_test(ws_url: &str) -> Result<Self> {
        let timeout = Duration::from_secs(2);
        Ok(Self {
            client: CdpClient::connect(ws_url)?,
            session_id: "page-session".into(),
            instance_id: None,
            target_id: "target".into(),
            frames_dir: None,
            instrumentation_registered: true,
            timeout,
            deadline: Deadline::after(timeout),
        })
    }

    pub(crate) fn is_recording(&self) -> bool {
        self.frames_dir.is_some()
    }

    /// Send a CDP command to the page session.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.call_until(method, params, self.deadline)
    }

    /// Send a CDP command to the page session within an absolute deadline.
    pub fn call_until(&mut self, method: &str, params: Value, deadline: Deadline) -> Result<Value> {
        if deadline.expired() {
            bail!("timed out before {method}");
        }
        self.client
            .call_until(Some(&self.session_id), method, params, deadline)
    }

    pub fn ensure_page_instrumentation(&mut self) -> Result<()> {
        if !self.instrumentation_registered {
            self.remove_persisted_instrumentation()?;
            let result = self.call(
                "Page.addScriptToEvaluateOnNewDocument",
                json!({"source": RDNY_INSTRUMENTATION_SCRIPT}),
            )?;
            self.persist_instrumentation_id(result["identifier"].as_str())?;
            self.instrumentation_registered = true;
        }
        let _ = self.eval(RDNY_INSTRUMENTATION_SCRIPT)?;
        Ok(())
    }

    pub fn ensure_page_instrumentation_until(&mut self, deadline: Deadline) -> Result<()> {
        if !self.instrumentation_registered {
            self.remove_persisted_instrumentation_until(deadline)?;
            let result = self.call_until(
                "Page.addScriptToEvaluateOnNewDocument",
                json!({"source": RDNY_INSTRUMENTATION_SCRIPT}),
                deadline,
            )?;
            self.persist_instrumentation_id(result["identifier"].as_str())?;
            self.instrumentation_registered = true;
        }
        let _ = self.eval_until(RDNY_INSTRUMENTATION_SCRIPT, deadline)?;
        Ok(())
    }

    fn remove_persisted_instrumentation(&mut self) -> Result<()> {
        let Some(old) = state::require()?.instrumentation else {
            return Ok(());
        };
        if old.target_id == self.target_id && old.version <= RDNY_INSTRUMENTATION_VERSION {
            let _ = self.call(
                "Page.removeScriptToEvaluateOnNewDocument",
                json!({"identifier": old.script_id}),
            );
        }
        Ok(())
    }

    fn remove_persisted_instrumentation_until(&mut self, deadline: Deadline) -> Result<()> {
        let Some(old) = state::require()?.instrumentation else {
            return Ok(());
        };
        if old.target_id == self.target_id && old.version <= RDNY_INSTRUMENTATION_VERSION {
            let _ = self.call_until(
                "Page.removeScriptToEvaluateOnNewDocument",
                json!({"identifier": old.script_id}),
                deadline,
            );
        }
        Ok(())
    }

    fn persist_instrumentation_id(&self, script_id: Option<&str>) -> Result<()> {
        let Some(script_id) = script_id else {
            return Ok(());
        };
        let target_id = self.target_id.clone();
        let script_id = script_id.to_string();
        state::update(|state| {
            state.instrumentation = Some(Box::new(state::InstrumentationState {
                target_id,
                version: RDNY_INSTRUMENTATION_VERSION,
                script_id,
            }));
            Ok(())
        })
    }

    /// Poll the explicitly unbounded `logs --follow` stream after bounded setup.
    pub fn next_event_follow(&mut self, timeout: Duration) -> Result<Option<Event>> {
        self.next_event_until(Deadline::after(timeout))
    }

    /// Pull the next event before an absolute deadline while still processing
    /// recording acknowledgements. Non-recording events are returned to callers.
    pub fn next_event_until(&mut self, deadline: Deadline) -> Result<Option<Event>> {
        loop {
            if deadline.expired() {
                return Ok(None);
            }
            let Some(event) = self.client.next_event_until(deadline)? else {
                return Ok(None);
            };
            if event.session_id.as_deref() != Some(self.session_id.as_str()) {
                continue;
            }
            if !self.process_recording_event_until(&event, deadline)? {
                return Ok(Some(event));
            }
        }
    }

    /// Pull one event already buffered by earlier CDP calls without reading the socket.
    pub fn next_buffered_event(&mut self, deadline: Deadline) -> Result<Option<Event>> {
        if deadline.expired() {
            return Ok(None);
        }
        while let Some(event) = self.client.next_buffered_event() {
            if event.session_id.as_deref() != Some(self.session_id.as_str()) {
                continue;
            }
            if !self.process_recording_event_until(&event, deadline)? {
                return Ok(Some(event));
            }
        }
        Ok(None)
    }

    /// Drain buffered and briefly-arriving events, capturing screencast frames.
    pub fn drain_events(&mut self, max_wait: Duration) -> Result<()> {
        let deadline = (Instant::now() + max_wait).min(self.deadline.instant());
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Ok(());
            }
            let Some(event) = self
                .client
                .next_event((deadline - now).min(Duration::from_millis(50)))?
            else {
                return Ok(());
            };
            let _ = self.process_recording_event_until(&event, Deadline::at(deadline))?;
        }
    }

    fn process_recording_event_until(&mut self, event: &Event, deadline: Deadline) -> Result<bool> {
        if event.method != "Page.screencastFrame"
            || event.session_id.as_deref() != Some(self.session_id.as_str())
        {
            return Ok(false);
        }
        let Some(frames_dir) = self.frames_dir.as_ref() else {
            return Ok(false);
        };
        if let Some(ack_id) = recording::ingest_screencast_frame(&event.params, frames_dir)? {
            self.call_until(
                "Page.screencastFrameAck",
                json!({"sessionId": ack_id.parse::<i64>().unwrap_or_default()}),
                deadline,
            )?;
        }
        Ok(true)
    }

    /// Flat-protocol session id for this attached page target.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Browser instance selected when this page session was attached.
    pub fn instance_id(&self) -> Option<&str> {
        self.instance_id.as_deref()
    }

    /// CDP page target id (not the flat-protocol session id).
    pub fn target_id(&self) -> &str {
        &self.target_id
    }

    /// Capture page identity before a live page produces an artifact.
    pub fn page_identity(&mut self) -> Result<PageIdentity> {
        let url = self
            .eval("location.href")?
            .as_str()
            .filter(|value| !value.is_empty())
            .context("location.href did not return the current page URL")?
            .to_string();
        Ok(PageIdentity {
            instance: self.instance_id().map(str::to_string),
            target: Some(self.target_id().to_string()),
            url: Some(url),
        })
    }

    /// Capture optional context for recovery paths that must succeed without a
    /// responsive browser.
    pub fn page_identity_best_effort(&mut self) -> PageIdentity {
        let url = self
            .eval("location.href")
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .filter(|value| !value.is_empty());
        PageIdentity {
            instance: self.instance_id().map(str::to_string),
            target: Some(self.target_id().to_string()),
            url,
        }
    }

    /// Evaluate a JavaScript expression, returning its value by value.
    /// Promises are awaited; page exceptions become errors.
    pub fn eval(&mut self, expression: &str) -> Result<Value> {
        let result = self.call(
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true,
            }),
        )?;
        check_exception(&result, "js exception")?;
        Ok(result["result"]["value"].clone())
    }

    pub fn deadline(&self) -> Deadline {
        self.deadline
    }

    /// Evaluate JavaScript within an absolute deadline.
    pub fn eval_until(&mut self, expression: &str, deadline: Deadline) -> Result<Value> {
        let result = self.call_until(
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true,
            }),
            deadline,
        )?;
        check_exception(&result, "js exception")?;
        Ok(result["result"]["value"].clone())
    }

    /// Resolve an ordinary or open-shadow-piercing selector to a remote object
    /// id. The traversal and segment validation are shared with `wait` probes.
    pub fn element(&mut self, selector: &ElementSelector) -> Result<String> {
        let result = self.call(
            "Runtime.evaluate",
            json!({
                "expression": selector.resolution_expression(false),
                "returnByValue": false,
            }),
        )?;
        check_exception(&result, "selector resolution failed")?;
        let object = &result["result"];
        match object["objectId"].as_str() {
            Some(id) if object["subtype"] != "null" => Ok(id.to_string()),
            _ => Err(hint_error(
                format!("no element matches CSS selector `{}`", selector.raw()),
                "inspect the page with `rdny html` to find the right selector",
                None,
            )),
        }
    }

    /// Probe the same selector traversal used by `element`, returning why it is
    /// not yet resolvable without treating a missing host/target/root as fatal.
    pub(crate) fn probe_element(&mut self, selector: &ElementSelector) -> Result<ElementProbe> {
        let result = self.call(
            "Runtime.evaluate",
            json!({
                "expression": selector.resolution_expression(true),
                "returnByValue": true,
            }),
        )?;
        check_exception(&result, "selector resolution failed")?;
        ElementProbe::from_value(&result["result"]["value"])
    }

    /// Call a JS function with the resolved element bound to `this`.
    /// `args` must be a JSON array of plain values.
    pub fn call_on(&mut self, object_id: &str, function: &str, args: &[Value]) -> Result<Value> {
        let arguments: Vec<Value> = args.iter().map(|v| json!({ "value": v })).collect();
        let result = self.call(
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": function,
                "arguments": arguments,
                "returnByValue": true,
                "awaitPromise": true,
            }),
        )?;
        check_exception(&result, "js exception")?;
        Ok(result["result"]["value"].clone())
    }

    /// Call a JS function with the resolved element bound to `this`, returning
    /// the resulting remote object id instead of serializing it by value.
    pub fn call_on_object(
        &mut self,
        object_id: &str,
        function: &str,
        args: &[Value],
    ) -> Result<String> {
        let arguments: Vec<Value> = args.iter().map(|v| json!({ "value": v })).collect();
        let result = self.call(
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": function,
                "arguments": arguments,
                "returnByValue": false,
                "awaitPromise": true,
            }),
        )?;
        check_exception(&result, "js exception")?;
        result["result"]["objectId"]
            .as_str()
            .map(str::to_string)
            .context("remote object result missing objectId")
    }

    /// Call a JS function with an arbitrary remote object bound to `this`.
    pub fn call_remote_object(
        &mut self,
        object_id: &str,
        function: &str,
        args: &[Value],
    ) -> Result<Value> {
        self.call_on(object_id, function, args)
    }

    /// Call a JS function with an arbitrary remote object within an absolute deadline.
    pub fn call_remote_object_until(
        &mut self,
        object_id: &str,
        function: &str,
        args: &[Value],
        deadline: Deadline,
    ) -> Result<Value> {
        let arguments: Vec<Value> = args.iter().map(|v| json!({ "value": v })).collect();
        let result = self.call_until(
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": function,
                "arguments": arguments,
                "returnByValue": true,
                "awaitPromise": true,
            }),
            deadline,
        )?;
        check_exception(&result, "js exception")?;
        Ok(result["result"]["value"].clone())
    }

    /// Best-effort release of a Runtime remote object within an absolute deadline.
    pub fn release_remote_object_until(
        &mut self,
        object_id: &str,
        deadline: Deadline,
    ) -> Result<()> {
        self.call_until(
            "Runtime.releaseObject",
            json!({ "objectId": object_id }),
            deadline,
        )?;
        Ok(())
    }
}
