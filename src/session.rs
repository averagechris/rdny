//! Shared page-session layer: connect to the recorded browser session,
//! attach to the current page target, and provide the CDP helpers the
//! command implementations build on.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::cdp::client::{CdpClient, Event};
use crate::cdp::http;
use crate::hint::hint_error;
use crate::state;

/// A live connection to the session's current page target.
pub struct PageSession {
    client: CdpClient,
    session_id: String,
    frames_dir: Option<state::SecureDir>,
    /// Overall budget for waiting-style commands (from --timeout).
    pub timeout: Duration,
}

/// Load the session state, connect, and attach to the current page.
/// If the recorded target is gone, falls back to the first open page
/// (and persists the switch).
pub fn connect(timeout_secs: f64) -> Result<PageSession> {
    let state = state::require()?;
    let targets = http::list_targets(&state.host, state.port).map_err(|_| {
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
    let timeout = Duration::from_secs_f64(timeout_secs.max(0.001));
    let mut client = CdpClient::connect(&state.ws_url)
        .with_context(|| format!("connecting to browser websocket {}", state.ws_url))?;
    client.set_timeout(timeout.max(Duration::from_secs(5)));
    let session_id = client.attach_to_target(&target_id)?;
    if let Some(viewport) = &state.viewport {
        client.call(
            Some(&session_id),
            "Emulation.setDeviceMetricsOverride",
            viewport.cdp_params(),
        )?;
    }
    let frames_dir = if state.recording {
        let frames_dir = state::frames_dir()?;
        client.call(
            Some(&session_id),
            "Page.startScreencast",
            json!({"format": "jpeg", "quality": 70, "everyNthFrame": 1}),
        )?;
        Some(frames_dir)
    } else {
        None
    };
    Ok(PageSession {
        client,
        session_id,
        frames_dir,
        timeout,
    })
}

/// Quote a string as a JavaScript string literal.
pub fn js_string(s: &str) -> String {
    serde_json::to_string(s).expect("strings always serialize")
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
    /// Send a CDP command to the page session.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.client.call(Some(&self.session_id), method, params)
    }

    /// Pull the next buffered/incoming CDP event.
    pub fn next_event(&mut self, timeout: Duration) -> Result<Option<Event>> {
        loop {
            let Some(event) = self.client.next_event(timeout)? else {
                return Ok(None);
            };
            if !self.process_recording_event(&event)? {
                return Ok(Some(event));
            }
        }
    }

    /// Drain buffered and briefly-arriving events, capturing screencast frames.
    pub fn drain_events(&mut self, max_wait: Duration) -> Result<()> {
        let deadline = Instant::now() + max_wait;
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
            let _ = self.process_recording_event(&event)?;
        }
    }

    fn process_recording_event(&mut self, event: &Event) -> Result<bool> {
        if event.method != "Page.screencastFrame"
            || event.session_id.as_deref() != Some(self.session_id.as_str())
        {
            return Ok(false);
        }
        let Some(frames_dir) = self.frames_dir.as_ref() else {
            return Ok(false);
        };
        if let Some(ack_id) =
            crate::commands::video::handle_screencast_frame(&event.params, frames_dir)?
        {
            self.call(
                "Page.screencastFrameAck",
                json!({"sessionId": ack_id.parse::<i64>().unwrap_or_default()}),
            )?;
        }
        Ok(true)
    }

    /// Flat-protocol session id for this attached page target.
    pub fn session_id(&self) -> &str {
        &self.session_id
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

    /// Resolve a CSS selector to a remote object id, or a hint error
    /// when nothing matches.
    pub fn element(&mut self, selector: &str) -> Result<String> {
        let result = self.call(
            "Runtime.evaluate",
            json!({
                "expression": format!("document.querySelector({})", js_string(selector)),
                "returnByValue": false,
            }),
        )?;
        check_exception(&result, "invalid selector")?;
        let object = &result["result"];
        match object["objectId"].as_str() {
            Some(id) if object["subtype"] != "null" => Ok(id.to_string()),
            _ => Err(hint_error(
                format!("no element matches selector `{selector}`"),
                "inspect the page with `rdny html` to find the right selector",
                None,
            )),
        }
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

    /// Viewport center of the element's content box, after scrolling it
    /// into view. Used for real mouse-event dispatch.
    pub fn element_center(&mut self, object_id: &str) -> Result<(f64, f64)> {
        // Best effort: not all targets support it, and getBoxModel will
        // fail loudly enough if the element is unrenderable.
        let _ = self.call(
            "DOM.scrollIntoViewIfNeeded",
            json!({ "objectId": object_id }),
        );
        let result = self.call("DOM.getBoxModel", json!({ "objectId": object_id }))?;
        let quad = result["model"]["content"]
            .as_array()
            .context("element has no box model (is it rendered?)")?;
        let coord = |i: usize| quad.get(i).and_then(Value::as_f64).unwrap_or(0.0);
        let x = (coord(0) + coord(2) + coord(4) + coord(6)) / 4.0;
        let y = (coord(1) + coord(3) + coord(5) + coord(7)) / 4.0;
        Ok((x, y))
    }
}
