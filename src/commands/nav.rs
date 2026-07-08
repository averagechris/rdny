//! Navigation: open, back, forward, reload, clear-cache.

use anyhow::{Context, Result, bail};
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

fn history_target(current: usize, len: usize, delta: i64) -> Option<usize> {
    let target = current as i64 + delta;
    if target < 0 || target >= len as i64 {
        None
    } else {
        Some(target as usize)
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
pub fn back(sess: &mut PageSession) -> Result<()> {
    navigate_history(sess, -1)
}

/// Go forward in history.
pub fn forward(sess: &mut PageSession) -> Result<()> {
    navigate_history(sess, 1)
}

/// Reload the page; `hard` bypasses the cache.
pub fn reload(sess: &mut PageSession, hard: bool) -> Result<()> {
    sess.call("Page.enable", json!({}))?;
    sess.call("Page.reload", json!({ "ignoreCache": hard }))?;
    wait_for_load_event(sess)
}

/// Clear the browser cache.
pub fn clear_cache(sess: &mut PageSession) -> Result<()> {
    sess.call("Network.enable", json!({}))?;
    sess.call("Network.clearBrowserCache", json!({}))?;
    let _ = sess.call("Network.disable", json!({}));
    Ok(())
}

fn navigate_history(sess: &mut PageSession, delta: i64) -> Result<()> {
    sess.call("Page.enable", json!({}))?;
    let history = sess.call("Page.getNavigationHistory", json!({}))?;
    let current = history["currentIndex"]
        .as_u64()
        .context("navigation history missing current index")? as usize;
    let entries = history["entries"]
        .as_array()
        .context("navigation history missing entries")?;

    let Some(target) = history_target(current, entries.len(), delta) else {
        return Ok(());
    };
    let entry_id = entries[target]["id"]
        .as_i64()
        .context("navigation history entry missing id")?;
    sess.call(
        "Page.navigateToHistoryEntry",
        json!({ "entryId": entry_id }),
    )?;
    wait_for_load_event(sess)
}

#[cfg(test)]
mod tests {
    use super::{history_target, normalize_url};

    #[test]
    fn normalize_url_prefixes_bare_hosts() {
        assert_eq!(normalize_url("example.com"), "https://example.com");
        assert_eq!(normalize_url("localhost:8080"), "https://localhost:8080");
    }

    #[test]
    fn normalize_url_leaves_special_and_schemed_urls_alone() {
        assert_eq!(normalize_url("about:blank"), "about:blank");
        assert_eq!(normalize_url("data:text/plain,hi"), "data:text/plain,hi");
        assert_eq!(normalize_url("http://example.com"), "http://example.com");
        assert_eq!(normalize_url("https://example.com"), "https://example.com");
    }

    #[test]
    fn history_target_moves_within_bounds() {
        assert_eq!(history_target(1, 3, -1), Some(0));
        assert_eq!(history_target(1, 3, 1), Some(2));
    }

    #[test]
    fn history_target_returns_none_at_edges() {
        assert_eq!(history_target(0, 3, -1), None);
        assert_eq!(history_target(2, 3, 1), None);
        assert_eq!(history_target(0, 0, 1), None);
    }
}
