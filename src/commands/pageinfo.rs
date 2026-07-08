//! Page info: url, title, html, text, attr, pdf.

use std::path::Path;

use anyhow::{Result, bail};

use crate::session::PageSession;

/// Print the current page URL.
pub fn url(_sess: &mut PageSession) -> Result<()> {
    bail!("url: not implemented yet")
}

/// Print the current page title.
pub fn title(_sess: &mut PageSession) -> Result<()> {
    bail!("title: not implemented yet")
}

/// Print page HTML, or the outerHTML of the first selector match.
pub fn html(_sess: &mut PageSession, _selector: Option<&str>) -> Result<()> {
    bail!("html: not implemented yet")
}

/// Print the text content of the first selector match.
pub fn text(_sess: &mut PageSession, _selector: &str) -> Result<()> {
    bail!("text: not implemented yet")
}

/// Print an attribute of the first selector match.
pub fn attr(_sess: &mut PageSession, _selector: &str, _name: &str) -> Result<()> {
    bail!("attr: not implemented yet")
}

/// Save the page as PDF (default file: page.pdf).
pub fn pdf(_sess: &mut PageSession, _file: Option<&Path>) -> Result<()> {
    bail!("pdf: not implemented yet")
}
