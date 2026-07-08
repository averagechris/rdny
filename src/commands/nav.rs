//! Navigation: open, back, forward, reload, clear-cache.

use anyhow::{Result, bail};
use serde_json::json;
use std::time::{Duration, Instant};

use crate::session::PageSession;

/// Prefix bare hostnames with https:// (about: and scheme'd URLs pass
/// through untouched).
pub fn normalize_url(url: &str) -> String {
    if url.contains("://") || url.starts_with("about:") || url.starts_with("data:") {
        url.to_string()
    } else {
        format!("https://{url}")
    }
}

/// Navigate the current page and wait for the load event.
pub fn open(sess: &mut PageSession, url: &str) -> Result<()> {
    let url = normalize_url(url);
    sess.call("Page.enable", json!({}))?;
    let result = sess.call("Page.navigate", json!({ "url": url }))?;
    if let Some(err) = result["errorText"].as_str()
        && !err.is_empty()
    {
        bail!("open {url}: {err}");
    }
    wait_for_load_event(sess)
}

/// Wait until Page.loadEventFired (Page domain must be enabled), within
/// the session timeout. Tolerates the event having already fired by
/// also polling document.readyState.
pub fn wait_for_load_event(sess: &mut PageSession) -> Result<()> {
    let deadline = Instant::now() + sess.timeout;
    loop {
        while let Some(event) = sess.next_event(Duration::from_millis(200))? {
            if event.method == "Page.loadEventFired" {
                return Ok(());
            }
        }
        let ready = sess.eval("document.readyState")?;
        if ready == json!("complete") {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out after {:.0?} waiting for the page to load",
                sess.timeout
            );
        }
    }
}

/// Go back in history.
pub fn back(_sess: &mut PageSession) -> Result<()> {
    bail!("back: not implemented yet")
}

/// Go forward in history.
pub fn forward(_sess: &mut PageSession) -> Result<()> {
    bail!("forward: not implemented yet")
}

/// Reload the page; `hard` bypasses the cache.
pub fn reload(_sess: &mut PageSession, _hard: bool) -> Result<()> {
    bail!("reload: not implemented yet")
}

/// Clear the browser cache.
pub fn clear_cache(_sess: &mut PageSession) -> Result<()> {
    bail!("clear-cache: not implemented yet")
}
