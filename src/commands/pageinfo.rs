//! Page info: url, title, html, text, attr, pdf.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::commands::print_value;
use crate::session::PageSession;

/// Print the current page URL.
pub fn url(sess: &mut PageSession) -> Result<()> {
    print_value(&sess.eval("location.href")?);
    Ok(())
}

/// Print the current page title.
pub fn title(sess: &mut PageSession) -> Result<()> {
    print_value(&sess.eval("document.title")?);
    Ok(())
}

/// Print page HTML, or the outerHTML of the first selector match.
pub fn html(sess: &mut PageSession, selector: Option<&str>) -> Result<()> {
    let value = match selector {
        Some(selector) => {
            let object_id = sess.element(selector)?;
            sess.call_on(&object_id, "function() { return this.outerHTML; }", &[])?
        }
        None => sess.eval("document.documentElement.outerHTML")?,
    };
    print_value(&value);
    Ok(())
}

/// Print the text content of the first selector match.
pub fn text(sess: &mut PageSession, selector: &str) -> Result<()> {
    let object_id = sess.element(selector)?;
    let value = sess.call_on(&object_id, "function() { return this.textContent; }", &[])?;
    print_value(&value);
    Ok(())
}

/// Print an attribute of the first selector match.
pub fn attr(sess: &mut PageSession, selector: &str, name: &str) -> Result<()> {
    let object_id = sess.element(selector)?;
    let value = sess.call_on(
        &object_id,
        "function(n) { return this.getAttribute(n); }",
        &[json!(name)],
    )?;
    if value == Value::Null {
        bail!("no attribute `{name}` on first match of `{selector}`");
    }
    print_value(&value);
    Ok(())
}

/// Save the page as PDF (default file: page.pdf).
pub fn pdf(sess: &mut PageSession, file: Option<&Path>) -> Result<()> {
    let path = file
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("page.pdf"));
    let result = sess.call("Page.printToPDF", json!({}))?;
    let data = result["data"]
        .as_str()
        .context("Page.printToPDF response missing data")?;
    let bytes = crate::commands::decode_base64(data)?;
    std::fs::write(&path, bytes).with_context(|| format!("writing PDF to {}", path.display()))?;
    println!("saved {}", path.display());
    Ok(())
}
