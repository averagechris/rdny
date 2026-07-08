//! Waiting: wait, waitload, waitstable, waitidle, sleep.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::json;

use crate::session::PageSession;

/// Wait for a selector to match an element.
pub fn wait(sess: &mut PageSession, selector: &str) -> Result<()> {
    let deadline = Instant::now() + sess.timeout;
    let expression = format!(
        "document.querySelector({}) !== null",
        crate::session::js_string(selector)
    );
    loop {
        if sess.eval(&expression)? == json!(true) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out after {:.3}s waiting for selector {selector}",
                sess.timeout.as_secs_f64()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
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
    let deadline = Instant::now() + sess.timeout;
    let mut previous = None;
    loop {
        let sample = sess.eval("document.documentElement.outerHTML.length + ':' + document.querySelectorAll('*').length")?;
        // One matching repeat means the cheap DOM fingerprint was unchanged for at least 250ms.
        if previous.as_ref() == Some(&sample) {
            return Ok(());
        }
        previous = Some(sample);
        if Instant::now() >= deadline {
            bail!(
                "timed out after {:.3}s waiting for the dom to stabilize",
                sess.timeout.as_secs_f64()
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Wait for the network to go idle.
pub fn waitidle(sess: &mut PageSession) -> Result<()> {
    sess.call("Network.enable", json!({}))?;
    let deadline = Instant::now() + sess.timeout;
    let mut inflight = HashSet::new();
    let mut last_network_event = Instant::now();
    loop {
        if let Some(event) = sess.next_event(Duration::from_millis(100))?
            && let Some(request_id) = event.params["requestId"].as_str()
            && apply_network_event(&mut inflight, &event.method, request_id)
        {
            last_network_event = Instant::now();
        }
        if inflight.is_empty() && last_network_event.elapsed() >= Duration::from_millis(500) {
            let _ = sess.call("Network.disable", json!({}));
            return Ok(());
        }
        if Instant::now() >= deadline {
            let _ = sess.call("Network.disable", json!({}));
            bail!(
                "timed out after {:.3}s waiting for network idle",
                sess.timeout.as_secs_f64()
            );
        }
    }
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
pub fn sleep(seconds: f64) -> Result<()> {
    if !seconds.is_finite() || seconds < 0.0 {
        bail!("sleep: seconds must be a non-negative number");
    }
    std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
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
    }
}
