//! Interaction: js, click, input, clear, file, download, select,
//! submit, hover, focus.

use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::commands::artifacts::{HumanArtifactOutput, ProducedArtifact};
use crate::commands::{artifacts, decode_base64, print_value};
use crate::input::{MouseButton, PointerTarget, pointer_click, pointer_move};
use crate::selector::ElementSelector;
use crate::session::PageSession;

/// Evaluate a JavaScript expression and print its result.
pub fn js(sess: &mut PageSession, expression: &str) -> Result<()> {
    let value = sess.eval(expression)?;
    print_value(&value);
    Ok(())
}

/// Click the first selector match (real mouse events).
pub fn click(_sess: &mut PageSession, _selector: &ElementSelector) -> Result<()> {
    pointer_click(
        _sess,
        &PointerTarget::selector(_selector.clone()),
        MouseButton::Left,
    )
}

/// Type text into the first selector match.
pub fn input(_sess: &mut PageSession, _selector: &ElementSelector, _text: &str) -> Result<()> {
    let id = _sess.element(_selector)?;
    _sess.call_on(&id, "function() { this.focus(); }", &[])?;
    _sess.call("Input.insertText", json!({ "text": _text }))?;
    Ok(())
}

/// Clear the value of the first selector match.
pub fn clear(_sess: &mut PageSession, _selector: &ElementSelector) -> Result<()> {
    let id = _sess.element(_selector)?;
    _sess.call_on(
        &id,
        "function() { this.value = ''; this.dispatchEvent(new Event('input', {bubbles: true})); this.dispatchEvent(new Event('change', {bubbles: true})); }",
        &[],
    )?;
    Ok(())
}

/// Set a file on a file input; path "-" reads the payload from stdin.
pub fn file(_sess: &mut PageSession, _selector: &ElementSelector, _path: &Path) -> Result<()> {
    let upload;
    let path = if _path == Path::new("-") {
        upload = artifacts::stdin_upload(io::stdin())?;
        upload.path().to_path_buf()
    } else {
        _path
            .canonicalize()
            .with_context(|| format!("canonicalizing {}", _path.display()))?
    };
    let id = _sess.element(_selector)?;
    _sess.call(
        "DOM.setFileInputFiles",
        json!({ "files": [path], "objectId": id }),
    )?;
    Ok(())
}

/// Download the href/src target of the first selector match. An omitted file or
/// file "-" streams raw bytes to stdout.
pub fn download(
    _sess: &mut PageSession,
    _selector: &ElementSelector,
    _file: Option<&Path>,
    force: bool,
    max_bytes: Option<u64>,
) -> Result<Option<ProducedArtifact>> {
    let context = _sess.artifact_context()?;
    let id = _sess.element(_selector)?;
    let url_value = _sess.call_on(
        &id,
        "function() { return this.href || this.currentSrc || this.src || null; }",
        &[],
    )?;
    let _url = url_value
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("element has no href or src"))?
        .to_string();
    let max_bytes = artifacts::configured_max_download_bytes(max_bytes)?;
    let len = _sess.call_on(&id,
        "async function(max) { const url = this.href || this.currentSrc || this.src; const r = await fetch(url, {method:'HEAD', credentials:'include'}).catch(() => null); const n = r && r.headers ? Number(r.headers.get('content-length')) : NaN; return Number.isFinite(n) ? n : null; }",
        &[json!(max_bytes)],
    )?;
    if let Some(len) = len.as_u64()
        && len > max_bytes
    {
        bail!(
            "download preflight content-length {len} exceeds max {max_bytes} bytes (--max-bytes or RDNY_MAX_DOWNLOAD_BYTES)"
        );
    }
    let data = _sess.call_on(&id,
        "async function(max) { const url = this.href || this.currentSrc || this.src; const resp = await fetch(url, {credentials: 'include'}); if (!resp.ok) { throw new Error('fetch failed: HTTP ' + resp.status); } const contentType = resp.headers.get('content-type'); const len = Number(resp.headers.get('content-length')); if (Number.isFinite(len) && len > max) { throw new Error('download content-length ' + len + ' exceeds max ' + max); } const reader = resp.body && resp.body.getReader ? resp.body.getReader() : null; if (!reader) { const buf = await resp.arrayBuffer(); if (buf.byteLength > max) throw new Error('download exceeds max ' + max); const bytes = new Uint8Array(buf); let s = ''; for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000)); return {chunks: btoa(s), contentType}; } let chunks = []; let total = 0; for (;;) { const {done, value} = await reader.read(); if (done) break; total += value.byteLength; if (total > max) throw new Error('download exceeds max ' + max); let s = ''; for (let i = 0; i < value.length; i += 0x8000) s += String.fromCharCode.apply(null, value.subarray(i, i + 0x8000)); chunks.push(btoa(s)); } return {chunks: chunks.join('\\n'), contentType}; }",
        &[json!(max_bytes)],
    )?;
    let chunks = data["chunks"]
        .as_str()
        .context("download returned no data")?;
    let media_type = artifacts::normalize_media_type(data["contentType"].as_str());
    match _file {
        None => {
            write_download_chunks(chunks, io::stdout().lock(), max_bytes)?;
            Ok(None)
        }
        Some(path) if path == Path::new("-") => {
            write_download_chunks(chunks, io::stdout().lock(), max_bytes)?;
            Ok(None)
        }
        Some(path) => Ok(Some(save_download_file(
            chunks, path, force, max_bytes, media_type, context,
        )?)),
    }
}

fn save_download_file(
    chunks: &str,
    path: &Path,
    force: bool,
    max_bytes: u64,
    media_type: String,
    context: artifacts::ArtifactContext,
) -> Result<ProducedArtifact> {
    let mut reservation = artifacts::ReservedArtifact::reserve(path, force)?;
    write_download_chunks(chunks, reservation.as_file_mut(), max_bytes)?;
    let published = reservation.finalize(force)?;
    Ok(ProducedArtifact::new(
        published,
        path.to_path_buf(),
        HumanArtifactOutput::Saved,
        media_type,
        None,
        context,
    ))
}

fn write_download_chunks(chunks: &str, mut out: impl std::io::Write, max: u64) -> Result<u64> {
    let mut total = 0_u64;
    for chunk in chunks.split('\n').filter(|s| !s.is_empty()) {
        let bytes = decode_base64(chunk)?;
        total = total.saturating_add(bytes.len() as u64);
        if total > max {
            bail!("download is larger than {max} bytes");
        }
        out.write_all(&bytes).context("writing download chunk")?;
    }
    Ok(total)
}

/// Select a dropdown option by value.
pub fn select(_sess: &mut PageSession, _selector: &ElementSelector, _value: &str) -> Result<()> {
    let id = _sess.element(_selector)?;
    _sess.call_on(
        &id,
        "function(v) { this.value = v; if (this.value !== v) { throw new Error('no option with value ' + v); } this.dispatchEvent(new Event('input', {bubbles: true})); this.dispatchEvent(new Event('change', {bubbles: true})); }",
        &[json!(_value)],
    )?;
    Ok(())
}

/// Submit the form containing (or matching) the selector.
pub fn submit(_sess: &mut PageSession, _selector: &ElementSelector) -> Result<()> {
    let id = _sess.element(_selector)?;
    _sess.call_on(
        &id,
        "function() { const f = this.tagName === 'FORM' ? this : (this.form || this.closest('form')); if (!f) { throw new Error('no form found for selector'); } if (f.requestSubmit) { f.requestSubmit(); } else { f.submit(); } }",
        &[],
    )?;
    Ok(())
}

/// Hover over the first selector match (real mouse events).
pub fn hover(_sess: &mut PageSession, _selector: &ElementSelector) -> Result<()> {
    pointer_move(_sess, &PointerTarget::selector(_selector.clone()))
}

/// Focus the first selector match.
pub fn focus(_sess: &mut PageSession, _selector: &ElementSelector) -> Result<()> {
    let id = _sess.element(_selector)?;
    _sess.call_on(&id, "function() { this.focus(); }", &[])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{save_download_file, write_download_chunks};
    use crate::commands::artifacts::ArtifactContext;

    #[test]
    fn download_chunks_write_incrementally_and_bound_total() {
        let mut out = Vec::new();
        assert_eq!(write_download_chunks("aGVs\nbG8=", &mut out, 5).unwrap(), 5);
        assert_eq!(out, b"hello");
        let mut out = Vec::new();
        let err = write_download_chunks("aGVs\nbG8=", &mut out, 4).unwrap_err();
        assert!(format!("{err}").contains("larger than 4 bytes"));
    }

    #[test]
    fn download_file_result_uses_response_mime_and_final_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload.txt");
        let artifact = save_download_file(
            "aGVsbG8=",
            &path,
            false,
            5,
            "text/plain".into(),
            ArtifactContext {
                instance: Some("i".into()),
                target: Some("t".into()),
                url: Some("https://example.test/".into()),
            },
        )
        .unwrap();
        assert_eq!(
            artifact.path,
            path.canonicalize().unwrap().to_string_lossy()
        );
        assert_eq!(artifact.media_type, "text/plain");
        assert_eq!(artifact.bytes, 5);
        assert_eq!(std::fs::read(path).unwrap(), b"hello");
        assert_eq!(artifact.instance.as_deref(), Some("i"));
    }
}
