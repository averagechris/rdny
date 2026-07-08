//! Tabs: pages, page, newpage. These act at the browser level through
//! the /json endpoints and the state file; no page session needed.

use anyhow::Result;

use crate::cdp::http::{self, TargetInfo};

/// List open pages with indices; the current page is marked with `*`.
pub fn pages() -> Result<()> {
    let state = crate::state::require()?;
    let pages = page_targets(&state.host, state.port)?;
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
pub fn page(index: usize) -> Result<()> {
    let mut state = crate::state::require()?;
    let pages = page_targets(&state.host, state.port)?;
    let target = pages
        .get(index)
        .ok_or_else(|| anyhow::anyhow!("no page at index {index} (see `rdny pages`)"))?;
    state.target_id = Some(target.id.clone());
    crate::state::save(&state)?;
    println!("switched to {index}: {}", target.url);
    Ok(())
}

/// Open a new page/tab, optionally at a URL, and make it current.
pub fn newpage(url: Option<&str>) -> Result<()> {
    let mut state = crate::state::require()?;
    let normalized = url.map(crate::commands::nav::normalize_url);
    let target = http::new_tab(&state.host, state.port, normalized.as_deref())?;
    state.target_id = Some(target.id.clone());
    crate::state::save(&state)?;
    let opened = if target.url.is_empty() {
        target.id.as_str()
    } else {
        target.url.as_str()
    };
    println!("opened {opened}");
    Ok(())
}

fn page_targets(host: &str, port: u16) -> Result<Vec<TargetInfo>> {
    let mut pages: Vec<_> = http::list_targets(host, port)?
        .into_iter()
        .filter(|target| target.target_type == "page")
        .collect();
    // Chrome's /json/list returns newest tabs first; present stable user-facing
    // indices in oldest-to-newest order so opening a tab does not renumber older pages.
    pages.reverse();
    Ok(pages)
}
