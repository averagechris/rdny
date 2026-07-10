//! Navigation: open, back, forward, reload, clear-cache.

use anyhow::{Context, Result, bail};
use serde_json::json;
use std::net::{IpAddr, Ipv6Addr};
use std::time::{Duration, Instant};

use crate::session::PageSession;

#[derive(Debug, Clone, Default)]
pub struct UrlPolicy {
    pub allow_file: bool,
    pub allow_data: bool,
    pub allow_private: bool,
    pub allow_link_local: bool,
    pub allow_local: bool,
}

/// Parse a navigation URL using WHATWG URL rules and enforce the local-target
/// policy. Loopback and `localhost` remain enabled for trusted development and
/// bare localhost names use HTTP; other bare names use HTTPS.
pub fn normalize_url(url: &str, policy: &UrlPolicy) -> Result<String> {
    if url.bytes().any(|byte| byte.is_ascii_control()) {
        bail!("URL contains a control character");
    }
    validate_percent_escapes(url)?;
    let candidate = if url.contains("://")
        || url.starts_with("about:")
        || url.starts_with("data:")
        || url.starts_with("file:")
    {
        url.to_string()
    } else if url.split(['/', '?', '#']).next().is_some_and(|authority| {
        let host = authority
            .rsplit_once(':')
            .filter(|(_, port)| port.bytes().all(|byte| byte.is_ascii_digit()))
            .map_or(authority, |(host, _)| host);
        host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost")
    }) {
        format!("http://{url}")
    } else {
        format!("https://{url}")
    };
    let parsed = url::Url::parse(&candidate).context("malformed URL")?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("URLs containing embedded credentials are not supported");
    }
    match parsed.scheme() {
        "http" | "https" | "about" => {}
        "file" if policy.allow_file => {}
        "data" if policy.allow_data => {}
        "file" => bail!("file: URLs require --allow-file-url"),
        "data" => bail!("data: URLs require --allow-data-url"),
        scheme => bail!("unsupported URL scheme `{scheme}`"),
    }
    if let Some(host) = parsed.host_str() {
        if host.ends_with(".local") && !policy.allow_local {
            bail!("local hostname targets require --allow-local-url");
        }
        let ip = match parsed.host() {
            Some(url::Host::Ipv4(ip)) => Some(IpAddr::V4(ip)),
            Some(url::Host::Ipv6(ip)) => Some(IpAddr::V6(ip)),
            _ => None,
        };
        if let Some(ip) = ip {
            let loopback = ip.is_loopback();
            let link_local = match ip {
                IpAddr::V4(ip) => ip.is_link_local(),
                IpAddr::V6(ip) => ip.is_unicast_link_local(),
            };
            let private = match ip {
                IpAddr::V4(ip) => ip.is_private(),
                IpAddr::V6(ip) => ipv6_is_unique_local(ip),
            };
            if link_local && !policy.allow_link_local {
                bail!("link-local URL targets require --allow-link-local-url");
            }
            if private && !loopback && !policy.allow_private {
                bail!("private-network URL targets require --allow-private-url");
            }
        }
    }
    Ok(parsed.into())
}

fn ipv6_is_unique_local(ip: Ipv6Addr) -> bool {
    ip.octets()[0] & 0xfe == 0xfc
}

fn validate_percent_escapes(value: &str) -> Result<()> {
    let bytes = value.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'%'
            && (index.checked_add(2).is_none_or(|end| end >= bytes.len())
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit())
        {
            bail!("URL contains a malformed percent escape");
        }
    }
    Ok(())
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
    open_with_policy(sess, url, &UrlPolicy::default())
}

pub fn open_with_policy(sess: &mut PageSession, url: &str, policy: &UrlPolicy) -> Result<()> {
    let url = normalize_url(url, policy)?;
    sess.ensure_page_instrumentation()?;
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
    sess.ensure_page_instrumentation()?;
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
    use super::{UrlPolicy, history_target, normalize_url};

    #[test]
    fn normalize_url_prefixes_bare_hosts() {
        assert_eq!(
            normalize_url("example.com", &UrlPolicy::default()).unwrap(),
            "https://example.com/"
        );
        assert_eq!(
            normalize_url("localhost:8080", &UrlPolicy::default()).unwrap(),
            "http://localhost:8080/"
        );
    }

    #[test]
    fn normalize_url_leaves_special_and_schemed_urls_alone() {
        assert_eq!(
            normalize_url("about:blank", &UrlPolicy::default()).unwrap(),
            "about:blank"
        );
        assert_eq!(
            normalize_url("http://example.com", &UrlPolicy::default()).unwrap(),
            "http://example.com/"
        );
        assert_eq!(
            normalize_url("https://example.com", &UrlPolicy::default()).unwrap(),
            "https://example.com/"
        );
    }

    #[test]
    fn url_policy_rejects_unsafe_targets_and_controls() {
        let policy = UrlPolicy::default();
        assert!(normalize_url("file:///etc/passwd", &policy).is_err());
        assert!(normalize_url("data:text/plain,hi", &policy).is_err());
        assert!(normalize_url("http://10.0.0.1", &policy).is_err());
        assert!(normalize_url("http://169.254.1.1", &policy).is_err());
        assert!(normalize_url("http://printer.local", &policy).is_err());
        assert!(normalize_url("https://example.com/\r\nX: y", &policy).is_err());
        assert!(normalize_url("http://[broken", &policy).is_err());
        assert!(normalize_url("https://example.com/%zz", &policy).is_err());
    }

    #[test]
    fn explicit_url_policy_flags_allow_each_target_class() {
        assert!(
            normalize_url(
                "file:///tmp/a b",
                &UrlPolicy {
                    allow_file: true,
                    ..Default::default()
                }
            )
            .unwrap()
            .contains("a%20b")
        );
        assert!(
            normalize_url(
                "data:text/plain,hi",
                &UrlPolicy {
                    allow_data: true,
                    ..Default::default()
                }
            )
            .is_ok()
        );
        assert!(
            normalize_url(
                "http://10.0.0.1",
                &UrlPolicy {
                    allow_private: true,
                    ..Default::default()
                }
            )
            .is_ok()
        );
        assert!(
            normalize_url(
                "http://169.254.1.1",
                &UrlPolicy {
                    allow_link_local: true,
                    ..Default::default()
                }
            )
            .is_ok()
        );
        assert!(
            normalize_url(
                "http://printer.local",
                &UrlPolicy {
                    allow_local: true,
                    ..Default::default()
                }
            )
            .is_ok()
        );
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
