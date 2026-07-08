//! Interaction: js, click, input, clear, file, download, select,
//! submit, hover, focus.

use std::fs;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::json;

use crate::commands::{decode_base64, print_value};
use crate::session::PageSession;

/// Evaluate a JavaScript expression and print its result.
pub fn js(sess: &mut PageSession, expression: &str) -> Result<()> {
    let value = sess.eval(expression)?;
    print_value(&value);
    Ok(())
}

/// Click the first selector match (real mouse events).
pub fn click(_sess: &mut PageSession, _selector: &str) -> Result<()> {
    let id = _sess.element(_selector)?;
    let (x, y) = _sess.element_center(&id)?;
    dispatch_mouse(_sess, "mouseMoved", x, y, None)?;
    dispatch_mouse(_sess, "mousePressed", x, y, Some("left"))?;
    dispatch_mouse(_sess, "mouseReleased", x, y, Some("left"))?;
    Ok(())
}

/// Type text into the first selector match.
pub fn input(_sess: &mut PageSession, _selector: &str, _text: &str) -> Result<()> {
    let id = _sess.element(_selector)?;
    _sess.call_on(&id, "function() { this.focus(); }", &[])?;
    _sess.call("Input.insertText", json!({ "text": _text }))?;
    Ok(())
}

/// Clear the value of the first selector match.
pub fn clear(_sess: &mut PageSession, _selector: &str) -> Result<()> {
    let id = _sess.element(_selector)?;
    _sess.call_on(
        &id,
        "function() { this.value = ''; this.dispatchEvent(new Event('input', {bubbles: true})); this.dispatchEvent(new Event('change', {bubbles: true})); }",
        &[],
    )?;
    Ok(())
}

/// Set a file on a file input; path "-" reads the payload from stdin.
pub fn file(_sess: &mut PageSession, _selector: &str, _path: &Path) -> Result<()> {
    let path = upload_path(_path)?;
    let id = _sess.element(_selector)?;
    _sess.call(
        "DOM.setFileInputFiles",
        json!({ "files": [path], "objectId": id }),
    )?;
    Ok(())
}

/// Download the href/src target of the first selector match; file "-"
/// (or no file) streams to stdout.
pub fn download(_sess: &mut PageSession, _selector: &str, _file: Option<&Path>) -> Result<()> {
    let id = _sess.element(_selector)?;
    let url_value = _sess.call_on(
        &id,
        "function() { return this.href || this.currentSrc || this.src || null; }",
        &[],
    )?;
    let url = url_value
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("element has no href or src"))?
        .to_string();
    let data = _sess.call_on(
        &id,
        "async function() { const url = this.href || this.currentSrc || this.src; const resp = await fetch(url, {credentials: 'include'}); if (!resp.ok) { throw new Error('fetch failed: HTTP ' + resp.status); } const buf = await resp.arrayBuffer(); const bytes = new Uint8Array(buf); let s = ''; for (let i = 0; i < bytes.length; i += 0x8000) { s += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000)); } return btoa(s); }",
        &[],
    )?;
    let bytes = decode_base64(data.as_str().context("download returned no data")?)?;
    match _file {
        Some(path) if path == Path::new("-") => io::stdout().lock().write_all(&bytes)?,
        Some(path) => {
            fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
            println!("saved {}", path.display());
        }
        None => {
            let name = filename_from_url(&url);
            fs::write(&name, bytes).with_context(|| format!("writing {name}"))?;
            println!("saved {name}");
        }
    }
    Ok(())
}

/// Select a dropdown option by value.
pub fn select(_sess: &mut PageSession, _selector: &str, _value: &str) -> Result<()> {
    let id = _sess.element(_selector)?;
    _sess.call_on(
        &id,
        "function(v) { this.value = v; if (this.value !== v) { throw new Error('no option with value ' + v); } this.dispatchEvent(new Event('input', {bubbles: true})); this.dispatchEvent(new Event('change', {bubbles: true})); }",
        &[json!(_value)],
    )?;
    Ok(())
}

/// Submit the form containing (or matching) the selector.
pub fn submit(_sess: &mut PageSession, _selector: &str) -> Result<()> {
    let id = _sess.element(_selector)?;
    _sess.call_on(
        &id,
        "function() { const f = this.tagName === 'FORM' ? this : (this.form || this.closest('form')); if (!f) { throw new Error('no form found for selector'); } if (f.requestSubmit) { f.requestSubmit(); } else { f.submit(); } }",
        &[],
    )?;
    Ok(())
}

/// Hover over the first selector match (real mouse events).
pub fn hover(_sess: &mut PageSession, _selector: &str) -> Result<()> {
    let id = _sess.element(_selector)?;
    let (x, y) = _sess.element_center(&id)?;
    dispatch_mouse(_sess, "mouseMoved", x, y, None)?;
    Ok(())
}

/// Focus the first selector match.
pub fn focus(_sess: &mut PageSession, _selector: &str) -> Result<()> {
    let id = _sess.element(_selector)?;
    _sess.call_on(&id, "function() { this.focus(); }", &[])?;
    Ok(())
}

fn dispatch_mouse(
    sess: &mut PageSession,
    event_type: &str,
    x: f64,
    y: f64,
    button: Option<&str>,
) -> Result<()> {
    let mut params = json!({ "type": event_type, "x": x, "y": y });
    if let Some(button) = button {
        params["button"] = json!(button);
        params["clickCount"] = json!(1);
    }
    sess.call("Input.dispatchMouseEvent", params)?;
    Ok(())
}

fn upload_path(path: &Path) -> Result<PathBuf> {
    if path == Path::new("-") {
        let mut bytes = Vec::new();
        io::stdin().read_to_end(&mut bytes)?;
        let tmp = std::env::temp_dir().join("rdny-stdin-upload");
        fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
        Ok(tmp)
    } else {
        path.canonicalize()
            .with_context(|| format!("canonicalizing {}", path.display()))
    }
}

fn filename_from_url(url: &str) -> String {
    let without_fragment = url.split('#').next().unwrap_or(url);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    let path = without_query
        .split_once("://")
        .map(|(_, rest)| rest.find('/').map(|i| &rest[i..]).unwrap_or(""))
        .unwrap_or(without_query);
    let name = path.rsplit('/').next().unwrap_or_default();
    if name.is_empty() {
        "download.bin".to_string()
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::filename_from_url;

    #[test]
    fn filename_from_url_strips_query() {
        assert_eq!(
            filename_from_url("https://example.com/files/report.pdf?x=1"),
            "report.pdf"
        );
    }

    #[test]
    fn filename_from_url_handles_trailing_slash() {
        assert_eq!(
            filename_from_url("https://example.com/files/"),
            "download.bin"
        );
    }

    #[test]
    fn filename_from_url_handles_bare_domain() {
        assert_eq!(filename_from_url("https://example.com"), "download.bin");
    }

    #[test]
    fn filename_from_url_strips_fragment() {
        assert_eq!(
            filename_from_url("https://example.com/a/b.txt#part"),
            "b.txt"
        );
    }
}
