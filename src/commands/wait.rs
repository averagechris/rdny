//! Waiting: wait, waitload, waitstable, waitidle, sleep.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::session::{Deadline, PageSession};

const DEFAULT_QUIET_WINDOW: Duration = Duration::from_millis(500);

/// Wait for a selector to match an element.
pub fn wait(sess: &mut PageSession, selector: &str) -> Result<()> {
    let deadline = sess.deadline();
    let expression = format!(
        "document.querySelector({}) !== null",
        crate::session::js_string(selector)
    );
    loop {
        if sess.eval(&expression)? == json!(true) {
            return Ok(());
        }
        if deadline.expired() {
            bail!(
                "timed out after {:.3}s waiting for selector {selector}",
                sess.timeout.as_secs_f64()
            );
        }
        deadline.sleep(Duration::from_millis(100));
    }
}

/// Wait for the page load event.
pub fn waitload(sess: &mut PageSession) -> Result<()> {
    sess.call("Page.enable", json!({}))?;
    if sess.eval("document.readyState")? == json!("complete") {
        return Ok(());
    }
    crate::commands::nav::wait_for_load_event(sess)
}

/// Wait for the DOM to stop mutating.
pub fn waitstable(sess: &mut PageSession) -> Result<()> {
    waitstable_quiet(sess, DEFAULT_QUIET_WINDOW)
}

pub fn waitstable_quiet(sess: &mut PageSession, quiet_window: Duration) -> Result<()> {
    let deadline = sess.deadline();
    install_mutation_clock(sess, deadline)?;
    loop {
        if deadline.expired() {
            bail!(
                "timed out after {:.3}s waiting for the dom to stabilize",
                sess.timeout.as_secs_f64()
            );
        }
        install_mutation_clock(sess, deadline)?;
        if mutation_quiet_for(sess, deadline)? >= quiet_window {
            if deadline.expired() {
                bail!(
                    "timed out after {:.3}s waiting for the dom to stabilize",
                    sess.timeout.as_secs_f64()
                );
            }
            return Ok(());
        }
        if deadline.expired() {
            bail!(
                "timed out after {:.3}s waiting for the dom to stabilize",
                sess.timeout.as_secs_f64()
            );
        }
        deadline.sleep(Duration::from_millis(50));
    }
}

fn install_mutation_clock(sess: &mut PageSession, deadline: Deadline) -> Result<()> {
    sess.eval_until(r#"(() => {
        const key = Symbol.for('rdny.waitstable.mutationClock.v1');
        if (window[key] && window[key].observer) return true;
        const state = { last: performance.now(), observer: null };
        state.observer = new MutationObserver(() => { state.last = performance.now(); });
        state.observer.observe(document, { subtree: true, childList: true, attributes: true, characterData: true });
        Object.defineProperty(window, key, { value: state, configurable: true });
        return true;
    })()"#, deadline)?;
    Ok(())
}

fn mutation_quiet_for(sess: &mut PageSession, deadline: Deadline) -> Result<Duration> {
    let ms = sess
        .eval_until(
            "(() => { const s = window[Symbol.for('rdny.waitstable.mutationClock.v1')]; return s ? performance.now() - s.last : 0; })()",
            deadline,
        )?
        .as_f64()
        .unwrap_or(0.0);
    Ok(Duration::from_secs_f64(ms.max(0.0) / 1000.0))
}

/// Wait for the network to go idle.
pub fn waitidle(sess: &mut PageSession) -> Result<()> {
    waitidle_quiet(sess, DEFAULT_QUIET_WINDOW)
}

pub fn waitidle_quiet(sess: &mut PageSession, quiet_window: Duration) -> Result<()> {
    let deadline = sess.deadline();
    sess.ensure_page_instrumentation_until(deadline)?;
    sess.call_until("Network.enable", json!({}), deadline)?;
    // Network is also enabled when rdny attaches to the page, so this wait can
    // observe requests that started earlier in the same rdny page session and
    // remain buffered. CDP still cannot reconstruct requests that started before
    // rdny attached/enabled Network. WebSocket lifecycle events are intentionally
    // ignored by waitidle; HTTP(S) long-polls are tracked as regular requests.
    let mut inflight = HashSet::new();
    let mut last_network_event = Instant::now();
    let mut last_instrumentation_seq = 0;
    loop {
        let page_state = page_wait_state(sess, deadline)?;
        if page_state.instrumentation_seq != last_instrumentation_seq {
            last_instrumentation_seq = page_state.instrumentation_seq;
            last_network_event = Instant::now();
        }
        if deadline.expired() {
            bail!(
                "timed out after {:.3}s waiting for network idle",
                sess.timeout.as_secs_f64()
            );
        }
        if inflight.is_empty()
            && page_state.instrumented_active == 0
            && page_state.document_ready
            && last_network_event.elapsed() >= quiet_window
            && quiet_candidate_still_idle(
                sess,
                deadline,
                &mut inflight,
                &mut last_network_event,
                &mut last_instrumentation_seq,
                quiet_window,
            )?
        {
            return Ok(());
        }
        let quiet_deadline = last_network_event + quiet_window;
        let next_slice = if inflight.is_empty() && page_state.instrumented_active == 0 {
            quiet_deadline
        } else {
            Instant::now() + Duration::from_millis(50)
        };
        let poll_deadline = deadline
            .remaining()
            .map(|remaining| Instant::now() + remaining)
            .unwrap_or_else(Instant::now)
            .min(next_slice);
        if let Some(event) = sess.next_event_until(Deadline::at(poll_deadline))?
            && let Some(request_id) = event.params["requestId"].as_str()
            && apply_network_event(&mut inflight, &event.method, request_id)
        {
            last_network_event = Instant::now();
        }
    }
}

fn quiet_candidate_still_idle(
    sess: &mut PageSession,
    deadline: Deadline,
    inflight: &mut HashSet<String>,
    last_network_event: &mut Instant,
    last_instrumentation_seq: &mut u64,
    quiet_window: Duration,
) -> Result<bool> {
    // Phase 1: drain all events already buffered by Runtime.evaluate/previous calls.
    let mut saw_activity = drain_buffered_network_activity(sess, deadline, inflight)?;
    if saw_activity {
        *last_network_event = Instant::now();
    }

    // Phase 2: re-read page state. Runtime.evaluate can itself buffer Network
    // events that arrived while Chrome was evaluating, so drain again below.
    let page_state = page_wait_state(sess, deadline)?;
    if page_state.instrumentation_seq != *last_instrumentation_seq {
        *last_instrumentation_seq = page_state.instrumentation_seq;
        *last_network_event = Instant::now();
        saw_activity = true;
    }
    if drain_buffered_network_activity(sess, deadline, inflight)? {
        *last_network_event = Instant::now();
        saw_activity = true;
    }

    if saw_activity || deadline.expired() {
        return Ok(false);
    }
    Ok(inflight.is_empty()
        && page_state.instrumented_active == 0
        && page_state.document_ready
        && last_network_event.elapsed() >= quiet_window)
}

fn drain_buffered_network_activity(
    sess: &mut PageSession,
    deadline: Deadline,
    inflight: &mut HashSet<String>,
) -> Result<bool> {
    let mut saw_activity = false;
    while let Some(event) = sess.next_buffered_event(deadline)? {
        if let Some(request_id) = event.params["requestId"].as_str()
            && apply_network_event(inflight, &event.method, request_id)
        {
            saw_activity = true;
        }
    }
    Ok(saw_activity)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PageWaitState {
    instrumented_active: u64,
    instrumentation_seq: u64,
    document_ready: bool,
}

fn page_wait_state(sess: &mut PageSession, deadline: Deadline) -> Result<PageWaitState> {
    page_wait_state_from_value(
        sess.eval_until(crate::session::RDNY_INSTRUMENTATION_STATE, deadline)?,
    )
}

fn page_wait_state_from_value(value: Value) -> Result<PageWaitState> {
    let active = value["active"].as_u64().unwrap_or(0);
    let ready = matches!(
        value["readyState"].as_str(),
        Some("interactive" | "complete")
    );
    Ok(PageWaitState {
        instrumented_active: active,
        instrumentation_seq: value["seq"].as_u64().unwrap_or(0),
        document_ready: ready,
    })
}

fn apply_network_event(inflight: &mut HashSet<String>, method: &str, request_id: &str) -> bool {
    match method {
        "Network.requestWillBeSent" => {
            inflight.insert(request_id.to_string());
            true
        }
        "Network.loadingFinished" | "Network.loadingFailed" => {
            inflight.remove(request_id);
            true
        }
        _ => false,
    }
}

/// Sleep for a number of seconds (fractions allowed).
pub fn sleep(deadline: Deadline) -> Result<()> {
    deadline.sleep(deadline.remaining().unwrap_or_default());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_events_update_inflight_requests() {
        let mut inflight = HashSet::new();
        assert!(apply_network_event(
            &mut inflight,
            "Network.requestWillBeSent",
            "a"
        ));
        assert!(inflight.contains("a"));
        assert!(apply_network_event(
            &mut inflight,
            "Network.loadingFinished",
            "a"
        ));
        assert!(inflight.is_empty());
        assert!(apply_network_event(
            &mut inflight,
            "Network.requestWillBeSent",
            "b"
        ));
        assert!(apply_network_event(
            &mut inflight,
            "Network.loadingFailed",
            "b"
        ));
        assert!(inflight.is_empty());
        assert!(!apply_network_event(
            &mut inflight,
            "Page.loadEventFired",
            "b"
        ));
        assert!(!apply_network_event(
            &mut inflight,
            "Network.webSocketCreated",
            "ws"
        ));
    }

    #[test]
    fn redirects_keep_request_inflight_until_terminal_event() {
        let mut inflight = HashSet::new();
        assert!(apply_network_event(
            &mut inflight,
            "Network.requestWillBeSent",
            "redirected"
        ));
        assert!(apply_network_event(
            &mut inflight,
            "Network.requestWillBeSent",
            "redirected"
        ));
        assert!(inflight.contains("redirected"));
        assert!(apply_network_event(
            &mut inflight,
            "Network.loadingFinished",
            "redirected"
        ));
        assert!(inflight.is_empty());
    }

    #[test]
    fn failures_clear_request_from_inflight() {
        let mut inflight = HashSet::new();
        assert!(apply_network_event(
            &mut inflight,
            "Network.requestWillBeSent",
            "failed"
        ));
        assert!(apply_network_event(
            &mut inflight,
            "Network.loadingFailed",
            "failed"
        ));
        assert!(inflight.is_empty());
    }

    #[test]
    fn page_wait_state_tracks_active_seq_and_readiness() {
        let state = page_wait_state_from_value(json!({
            "active": 2,
            "seq": 7,
            "readyState": "interactive"
        }))
        .unwrap();
        assert_eq!(state.instrumented_active, 2);
        assert_eq!(state.instrumentation_seq, 7);
        assert!(state.document_ready);

        let loading = page_wait_state_from_value(json!({
            "active": 0,
            "seq": 8,
            "readyState": "loading"
        }))
        .unwrap();
        assert!(!loading.document_ready);
    }

    #[test]
    #[ignore = "browser-backed acceptance: run with RDNY_BROWSER_TESTS=1 RDNY_BIN=target/debug/rdny"]
    fn browser_cross_process_wait_semantics() {
        let rdny = browser_test_bin();
        let temp = tempfile::tempdir().unwrap();
        let server = TestServer::new();
        let mut cmd = browser_cmd(&rdny, temp.path());
        assert!(cmd.arg("start").status().unwrap().success());

        assert!(
            browser_cmd(&rdny, temp.path())
                .arg("open")
                .arg(server.url("/mutate"))
                .status()
                .unwrap()
                .success()
        );
        let start = Instant::now();
        let stable = browser_cmd(&rdny, temp.path())
            .args(["--timeout", "0.2", "waitstable", "--quiet-ms", "300"])
            .output()
            .unwrap();
        assert!(!stable.status.success());
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(
            String::from_utf8_lossy(&stable.stderr)
                .contains("timed out after 0.200s waiting for the dom to stabilize")
        );

        assert!(
            browser_cmd(&rdny, temp.path())
                .arg("open")
                .arg(server.url("/long"))
                .status()
                .unwrap()
                .success()
        );
        let start = Instant::now();
        let idle = browser_cmd(&rdny, temp.path())
            .args(["--timeout", "0.3", "waitidle", "--quiet-ms", "100"])
            .output()
            .unwrap();
        assert!(!idle.status.success());
        assert!(start.elapsed() >= Duration::from_millis(250));
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(String::from_utf8_lossy(&idle.stderr).contains("timed out"));

        assert!(
            browser_cmd(&rdny, temp.path())
                .arg("open")
                .arg(server.url("/xhr-abort-reuse"))
                .status()
                .unwrap()
                .success()
        );
        assert!(
            browser_cmd(&rdny, temp.path())
                .args(["--timeout", "4", "waitidle", "--quiet-ms", "100"])
                .status()
                .unwrap()
                .success()
        );
        let xhr_state = browser_cmd(&rdny, temp.path())
            .args([
                "js",
                "window[Symbol.for('rdny.wait.instrumentation.v1')].active",
            ])
            .output()
            .unwrap();
        assert!(xhr_state.status.success());
        assert_eq!(String::from_utf8_lossy(&xhr_state.stdout).trim(), "0");

        assert!(
            browser_cmd(&rdny, temp.path())
                .arg("open")
                .arg(server.url("/xhr-invalid-send"))
                .status()
                .unwrap()
                .success()
        );
        let invalid = browser_cmd(&rdny, temp.path())
            .args(["js", "window.__rdnyInvalidSendOk && window[Symbol.for('rdny.wait.instrumentation.v1')].active === 0"])
            .output()
            .unwrap();
        assert!(invalid.status.success());
        assert_eq!(String::from_utf8_lossy(&invalid.stdout).trim(), "true");

        assert!(
            browser_cmd(&rdny, temp.path())
                .arg("open")
                .arg(server.url("/sync-xhr"))
                .status()
                .unwrap()
                .success()
        );
        let sync = browser_cmd(&rdny, temp.path())
            .args(["js", "window.__rdnySyncXhrOk && window[Symbol.for('rdny.wait.instrumentation.v1')].active === 0"])
            .output()
            .unwrap();
        assert!(sync.status.success());
        assert_eq!(String::from_utf8_lossy(&sync.stdout).trim(), "true");

        assert!(
            browser_cmd(&rdny, temp.path())
                .arg("open")
                .arg(server.url("/short-fetch"))
                .status()
                .unwrap()
                .success()
        );
        let start = Instant::now();
        assert!(
            browser_cmd(&rdny, temp.path())
                .args(["--timeout", "3", "waitidle", "--quiet-ms", "100"])
                .status()
                .unwrap()
                .success()
        );
        assert!(start.elapsed() >= Duration::from_millis(650));

        assert!(
            browser_cmd(&rdny, temp.path())
                .arg("open")
                .arg(server.url("/late-img"))
                .status()
                .unwrap()
                .success()
        );
        let start = Instant::now();
        assert!(
            browser_cmd(&rdny, temp.path())
                .args(["--timeout", "3", "waitidle", "--quiet-ms", "100"])
                .status()
                .unwrap()
                .success()
        );
        assert!(start.elapsed() >= Duration::from_millis(100));

        for _ in 0..3 {
            assert!(
                browser_cmd(&rdny, temp.path())
                    .arg("open")
                    .arg(server.url("/ok"))
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let versions = browser_cmd(&rdny, temp.path())
            .args(["js", "window.fetch.__rdnyVersion === 2 && XMLHttpRequest.prototype.__rdnyVersion === 2 && window[Symbol.for('rdny.wait.instrumentation.v1')].version === 2"])
            .output()
            .unwrap();
        assert!(versions.status.success());
        assert_eq!(String::from_utf8_lossy(&versions.stdout).trim(), "true");

        let normal = browser_cmd(&rdny, temp.path())
            .args(["js", "fetch('/ok').then(r => r.text())"])
            .output()
            .unwrap();
        assert!(normal.status.success());
        assert_eq!(String::from_utf8_lossy(&normal.stdout).trim(), "ok");

        assert!(
            browser_cmd(&rdny, temp.path())
                .arg("start-video")
                .status()
                .unwrap()
                .success()
        );
        assert!(
            browser_cmd(&rdny, temp.path())
                .arg("open")
                .arg(server.url("/mixed"))
                .status()
                .unwrap()
                .success()
        );
        assert!(
            browser_cmd(&rdny, temp.path())
                .args(["--timeout", "2", "waitidle", "--quiet-ms", "100"])
                .status()
                .unwrap()
                .success()
        );
        let video = temp.path().join("recording.mp4");
        assert!(
            browser_cmd(&rdny, temp.path())
                .arg("stop-video")
                .arg(video)
                .status()
                .unwrap()
                .success()
        );
        let _ = browser_cmd(&rdny, temp.path()).arg("stop").status();
    }

    fn browser_test_bin() -> std::ffi::OsString {
        std::env::var_os("RDNY_BROWSER_TESTS")
            .expect("RDNY_BROWSER_TESTS=1 is required when running ignored browser tests");
        std::env::var_os("RDNY_BIN").expect("RDNY_BIN=target/debug/rdny is required")
    }

    fn browser_cmd(rdny: &std::ffi::OsStr, state: &std::path::Path) -> std::process::Command {
        let mut cmd = std::process::Command::new(rdny);
        cmd.env("RDNY_STATE_DIR", state);
        cmd
    }

    struct TestServer {
        base: String,
    }
    impl TestServer {
        fn new() -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            std::thread::spawn(move || {
                for stream in listener.incoming().take(40) {
                    let mut stream = stream.unwrap();
                    let mut buf = [0; 1024];
                    let n = std::io::Read::read(&mut stream, &mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req.split_whitespace().nth(1).unwrap_or("/");
                    if path == "/hold" {
                        std::thread::sleep(Duration::from_secs(2));
                    }
                    if path.starts_with("/hold-short") {
                        std::thread::sleep(Duration::from_millis(750));
                    }
                    if path == "/redir" {
                        let _ = std::io::Write::write_all(
                            &mut stream,
                            b"HTTP/1.1 302 Found\r\nLocation: /ok\r\nContent-Length: 0\r\n\r\n",
                        );
                        continue;
                    }
                    if path == "/fail" {
                        let _ = std::io::Write::write_all(
                            &mut stream,
                            b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
                        );
                        continue;
                    }
                    let body = match path {
                        "/mutate" => {
                            "<script>setInterval(()=>document.body.textContent=document.body.textContent==='A'?'B':'A',50)</script>A"
                        }
                        "/long" => "<script>fetch('/hold')</script>long",
                        "/mixed" => {
                            "<script>fetch('/redir');fetch('/fail').catch(()=>{});setInterval(()=>console.log('tick'),10)</script>mixed"
                        }
                        "/short-fetch" => "<script>fetch('/hold-short')</script>short",
                        "/late-img" => {
                            "<script>let n=0;const t=setInterval(()=>{if(n++<2){const i=new Image();i.src='/hold-short?img='+n;document.body.appendChild(i)}else clearInterval(t)},200)</script>late"
                        }
                        "/sync-xhr" => {
                            "<script>const x=new XMLHttpRequest();x.open('GET','/hold-short',false);x.send();window.__rdnySyncXhrOk=x.responseText==='ok';</script>sync"
                        }
                        "/xhr-abort-reuse" => {
                            "<script>const x=new XMLHttpRequest();x.open('GET','/hold');x.send();x.abort();x.open('GET','/ok');x.onloadend=()=>{window.__rdnyXhrReuseDone=true};x.send();</script>xhr"
                        }
                        "/xhr-invalid-send" => {
                            "<script>const x=new XMLHttpRequest();try{x.send()}catch(e){window.__rdnyInvalidSendOk=true}</script>invalid"
                        }
                        _ => "ok",
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
                }
            });
            Self { base }
        }
        fn url(&self, path: &str) -> String {
            format!("{}{}", self.base, path)
        }
    }
}
