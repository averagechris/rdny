//! Tabs: pages, page, newpage. These act at the browser level through
//! the /json endpoints and the state file; no page session needed.

use anyhow::{Result, bail};

/// List open pages with indices; the current page is marked with `*`.
pub fn pages() -> Result<()> {
    bail!("pages: not implemented yet")
}

/// Switch the session's current page by index (as printed by `pages`).
pub fn page(_index: usize) -> Result<()> {
    bail!("page: not implemented yet")
}

/// Open a new page/tab, optionally at a URL, and make it current.
pub fn newpage(_url: Option<&str>) -> Result<()> {
    bail!("newpage: not implemented yet")
}
