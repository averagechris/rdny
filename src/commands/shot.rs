//! Screenshots: screenshot, screenshot-el.

use std::path::Path;

use anyhow::{Result, bail};

use crate::session::PageSession;

/// Capture a page screenshot (default file: screenshot.png).
pub fn screenshot(
    _sess: &mut PageSession,
    _width: Option<u32>,
    _height: Option<u32>,
    _file: Option<&Path>,
) -> Result<()> {
    bail!("screenshot: not implemented yet")
}

/// Capture a screenshot clipped to the first selector match.
pub fn screenshot_el(_sess: &mut PageSession, _selector: &str, _file: Option<&Path>) -> Result<()> {
    bail!("screenshot-el: not implemented yet")
}
