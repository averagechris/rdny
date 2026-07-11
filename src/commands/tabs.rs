//! Tabs: pages, page, newpage. These act at the browser level through
//! the /json endpoints and the state file; no page session needed.

use anyhow::Result;

use crate::cdp::http::{self, TargetInfo};
use crate::commands::OutputFormat;
use crate::session::Deadline;

/// List open pages with indices; the current page is marked with `*`.
#[allow(dead_code)]
pub fn pages() -> Result<()> {
    pages_format(OutputFormat::Human)
}

pub fn pages_format(format: OutputFormat) -> Result<()> {
    pages_format_until(format, Deadline::after(http::HTTP_TIMEOUT))
}

pub fn pages_format_until(format: OutputFormat, deadline: Deadline) -> Result<()> {
    let state = crate::state::require()?;
    let pages = page_targets(&state, deadline)?;
    if format.is_structured() {
        let rows: Vec<_> = pages.iter().enumerate().map(|(index, target)| serde_json::json!({
            "index": index, "current": state.target_id.as_deref() == Some(target.id.as_str()),
            "id": target.id, "url": target.url, "title": target.title
        })).collect();
        format.emit_json(&serde_json::json!({"schemaVersion":1,"kind":"pages","pages":rows}))?;
        return Ok(());
    }
    for (index, target) in pages.iter().enumerate() {
        let marker = if state.target_id.as_deref() == Some(target.id.as_str()) {
            "* "
        } else {
            "  "
        };
        println!("{marker}{index}: {} ({})", target.url, target.title);
    }
    Ok(())
}

/// Switch the session's current page by index (as printed by `pages`).
#[allow(dead_code)]
pub fn page(index: usize) -> Result<()> {
    page_until(index, Deadline::after(http::HTTP_TIMEOUT))
}

pub fn page_until(index: usize, deadline: Deadline) -> Result<()> {
    let state = crate::state::require()?;
    let pages = page_targets(&state, deadline)?;
    let target = pages
        .get(index)
        .ok_or_else(|| anyhow::anyhow!("no page at index {index} (see `rdny pages`)"))?;
    crate::state::update(|state| {
        state.target_id = Some(target.id.clone());
        Ok(())
    })?;
    println!("switched to {index}: {}", target.url);
    Ok(())
}

/// Open a new page/tab, optionally at a URL, and make it current.
pub fn newpage_with_policy(
    url: Option<&str>,
    timeout: std::time::Duration,
    deadline: Deadline,
    policy: &crate::commands::nav::UrlPolicy,
) -> Result<()> {
    let normalized = url
        .map(|url| crate::commands::nav::normalize_url(url, policy))
        .transpose()?;
    let state = crate::state::require()?;
    let target = crate::session::create_target_until(&state, deadline)?;
    crate::state::update(|state| {
        state.target_id = Some(target.id.clone());
        Ok(())
    })?;
    if let Some(url) = normalized.as_deref() {
        let mut sess = crate::session::connect(deadline, timeout)?;
        crate::commands::nav::open_with_policy(&mut sess, url, policy)?;
    }
    let opened = if target.url.is_empty() {
        normalized.as_deref().unwrap_or(target.id.as_str())
    } else {
        target.url.as_str()
    };
    println!("opened {opened}");
    Ok(())
}

fn page_targets(state: &crate::state::SessionState, deadline: Deadline) -> Result<Vec<TargetInfo>> {
    let mut pages: Vec<_> = crate::session::targets_until(state, deadline)?
        .into_iter()
        .filter(|target| target.target_type == "page")
        .collect();
    // Chrome's /json/list returns newest tabs first; present stable user-facing
    // indices in oldest-to-newest order so opening a tab does not renumber older pages.
    pages.reverse();
    Ok(pages)
}
