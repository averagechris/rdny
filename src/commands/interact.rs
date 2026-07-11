//! Interaction: js, click, input, clear, file, download, select,
//! submit, hover, focus.

use std::io;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::commands::artifacts::{HumanArtifactOutput, ProducedArtifact};
use crate::commands::{artifacts, decode_base64, print_value};
use crate::input::{MouseButton, PointerTarget, pointer_click, pointer_move};
use crate::selector::ElementSelector;
use crate::session::PageSession;

const DOWNLOAD_DECODED_CHUNK_BYTES: usize = 512 * 1024;
const DOWNLOAD_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);

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
    let stream = _sess.call_on_object(
        &id,
        DOWNLOAD_STREAM_FACTORY,
        &[json!(max_bytes), json!(DOWNLOAD_DECODED_CHUNK_BYTES)],
    )?;
    match _file {
        None => {
            stream_download_to_writer(_sess, &stream, io::stdout().lock(), max_bytes)?;
            Ok(None)
        }
        Some(path) if path == Path::new("-") => {
            stream_download_to_writer(_sess, &stream, io::stdout().lock(), max_bytes)?;
            Ok(None)
        }
        Some(path) => Ok(Some(save_download_file(
            _sess, &stream, path, force, max_bytes, context,
        )?)),
    }
}

const DOWNLOAD_STREAM_FACTORY: &str = r#"async function(max, chunkSize) {
    const url = this.href || this.currentSrc || this.src;
    const resp = await fetch(url, {credentials: 'include'});
    if (!resp.ok) throw new Error('fetch failed: HTTP ' + resp.status);
    const contentType = resp.headers.get('content-type');
    const len = Number(resp.headers.get('content-length'));
    if (Number.isFinite(len) && len > max) throw new Error('download content-length ' + len + ' exceeds max ' + max);
    const reader = resp.body && resp.body.getReader ? resp.body.getReader() : null;
    if (!reader) throw new Error('download streaming is unavailable in this page');
    let done = false, total = 0, pending = null, pendingOffset = 0;
    const encode = (value) => { let s = ''; for (let i = 0; i < value.length; i += 0x8000) s += String.fromCharCode.apply(null, value.subarray(i, i + 0x8000)); return btoa(s); };
    return {
      contentType,
      async next() {
        if (done) return {done: true, contentType, total};
        let value;
        while (!pending || pendingOffset >= pending.byteLength) {
          const read = await reader.read();
          if (read.done) { done = true; return {done: true, contentType, total}; }
          pending = read.value;
          pendingOffset = 0;
        }
        const end = Math.min(pending.byteLength, pendingOffset + chunkSize);
        value = pending.subarray(pendingOffset, end);
        pendingOffset = end;
        total += value.byteLength;
        if (total > max) { try { await reader.cancel(); } finally { done = true; } throw new Error('download exceeds max ' + max); }
        return {done: false, chunk: encode(value), contentType, total};
      },
      async cancel() { done = true; await reader.cancel(); pending = null; return true; }
    };
}"#;

fn save_download_file(
    sess: &mut PageSession,
    stream_object_id: &str,
    path: &Path,
    force: bool,
    max_bytes: u64,
    context: artifacts::ArtifactContext,
) -> Result<ProducedArtifact> {
    let mut reservation = artifacts::ReservedArtifact::reserve(path, force)?;
    let media_type =
        stream_download_to_writer(sess, stream_object_id, reservation.as_file_mut(), max_bytes)?;
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

fn stream_download_to_writer(
    sess: &mut PageSession,
    stream_object_id: &str,
    mut out: impl std::io::Write,
    max: u64,
) -> Result<String> {
    let mut total = 0_u64;
    let mut media_type = None;
    let mut primary_error = None;
    loop {
        let step = sess.call_remote_object(
            stream_object_id,
            "async function() { return await this.next(); }",
            &[],
        );
        let step = match step {
            Ok(step) => step,
            Err(error) => {
                primary_error = Some(error);
                break;
            }
        };
        media_type = media_type.or_else(|| step["contentType"].as_str().map(str::to_string));
        if step["done"].as_bool().unwrap_or(false) {
            break;
        }
        let chunk = match step["chunk"].as_str() {
            Some(c) => c,
            None => {
                primary_error = Some(anyhow::anyhow!("download stream returned no chunk"));
                break;
            }
        };
        let bytes = match decode_base64(chunk) {
            Ok(b) => b,
            Err(e) => {
                primary_error = Some(e);
                break;
            }
        };
        total = total.saturating_add(bytes.len() as u64);
        if total > max {
            primary_error = Some(anyhow::anyhow!("download is larger than {max} bytes"));
            break;
        }
        if let Err(error) = out.write_all(&bytes).context("writing download chunk") {
            primary_error = Some(error);
            break;
        }
    }
    let cleanup_deadline = crate::session::Deadline::after(DOWNLOAD_CLEANUP_TIMEOUT);
    let cancel = sess.call_remote_object_until(
        stream_object_id,
        "async function() { if (this.cancel) return await this.cancel(); return true; }",
        &[],
        cleanup_deadline,
    );
    let release = sess.release_remote_object_until(stream_object_id, cleanup_deadline);
    let cleanup_error = cancel
        .map(|_| ())
        .and(release)
        .err()
        .map(|error| anyhow::anyhow!("download cleanup failed: {error:#}"));
    if let Some(error) = primary_error {
        return match cleanup_error {
            Some(cleanup) => Err(error.context(cleanup.to_string())),
            None => Err(error),
        };
    }
    if let Some(error) = cleanup_error {
        return Err(error);
    }
    Ok(artifacts::normalize_media_type(media_type.as_deref()))
}

#[cfg(test)]
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
    use super::{
        DOWNLOAD_DECODED_CHUNK_BYTES, DOWNLOAD_STREAM_FACTORY, download, write_download_chunks,
    };
    use crate::selector::ElementSelector;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde_json::{Value, json};
    use std::net::TcpListener;
    use std::thread;
    use tungstenite::{Message, accept};

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
    fn download_browser_stream_has_fixed_chunking_and_no_full_body_fallback() {
        let encoded = STANDARD.encode(vec![0_u8; DOWNLOAD_DECODED_CHUNK_BYTES]);
        assert!(encoded.len() < crate::cdp::client::MAX_WEBSOCKET_FRAME_BYTES);
        assert!(!DOWNLOAD_STREAM_FACTORY.contains("arrayBuffer"));
        assert!(DOWNLOAD_STREAM_FACTORY.contains("pendingOffset"));
    }

    #[test]
    fn download_pulls_bounded_chunks_before_next_and_writes_file_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("payload.bin");
        let (url, server) = fake_download_cdp(vec![
            json!({"done":false,"chunk":"aGVs","contentType":"application/octet-stream","total":3}),
            json!({"done":false,"chunk":"bG8=","contentType":"application/octet-stream","total":5}),
            json!({"done":true,"contentType":"application/octet-stream","total":5}),
        ]);
        let mut sess = crate::session::PageSession::connect_for_input_test(&url).unwrap();
        let artifact = download(
            &mut sess,
            &ElementSelector::parse("a", false).unwrap(),
            Some(&out),
            false,
            Some(5),
        )
        .unwrap()
        .unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"hello");
        assert_eq!(artifact.bytes, 5);
        assert_eq!(artifact.media_type, "application/octet-stream");
        drop(sess);
        server.join().unwrap();
    }

    #[test]
    fn download_midstream_failure_removes_temp_and_preserves_primary_error() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("payload.bin");
        let (url, server) = fake_download_cdp(vec![
            json!({"done":false,"chunk":"aGVs","contentType":"text/plain","total":3}),
            json!({"error":"network exploded"}),
        ]);
        let mut sess = crate::session::PageSession::connect_for_input_test(&url).unwrap();
        let err = download(
            &mut sess,
            &ElementSelector::parse("a", false).unwrap(),
            Some(&out),
            false,
            Some(10),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("network exploded"));
        assert!(!out.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        drop(sess);
        server.join().unwrap();
    }

    #[test]
    fn download_enforces_max_after_chunk_without_publishing_file() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("payload.bin");
        let (url, server) = fake_download_cdp(vec![
            json!({"done":false,"chunk":"aGVs","contentType":"text/plain","total":3}),
            json!({"done":false,"chunk":"bG8=","contentType":"text/plain","total":5}),
        ]);
        let mut sess = crate::session::PageSession::connect_for_input_test(&url).unwrap();
        let err = download(
            &mut sess,
            &ElementSelector::parse("a", false).unwrap(),
            Some(&out),
            false,
            Some(4),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("larger than 4 bytes"));
        assert!(!out.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        drop(sess);
        server.join().unwrap();
    }

    #[test]
    fn download_streams_total_above_websocket_message_limit_in_bounded_responses() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("large.bin");
        let chunk = STANDARD.encode(vec![7_u8; DOWNLOAD_DECODED_CHUNK_BYTES]);
        let chunks =
            (crate::cdp::client::MAX_WEBSOCKET_MESSAGE_BYTES / DOWNLOAD_DECODED_CHUNK_BYTES) + 2;
        let mut steps = (0..chunks)
            .map(|index| {
                json!({"done":false,"chunk":chunk,"contentType":"application/octet-stream","total":(index + 1) * DOWNLOAD_DECODED_CHUNK_BYTES})
            })
            .collect::<Vec<_>>();
        steps.push(json!({"done":true,"contentType":"application/octet-stream","total":chunks * DOWNLOAD_DECODED_CHUNK_BYTES}));
        let (url, server) = fake_download_cdp(steps);
        let mut sess = crate::session::PageSession::connect_for_input_test(&url).unwrap();
        let artifact = download(
            &mut sess,
            &ElementSelector::parse("a", false).unwrap(),
            Some(&out),
            false,
            Some((chunks * DOWNLOAD_DECODED_CHUNK_BYTES) as u64),
        )
        .unwrap()
        .unwrap();
        assert!(artifact.bytes > crate::cdp::client::MAX_WEBSOCKET_MESSAGE_BYTES as u64);
        drop(sess);
        server.join().unwrap();
    }

    #[test]
    fn download_exact_max_succeeds_and_over_max_fails() {
        let exact_dir = tempfile::tempdir().unwrap();
        let exact_out = exact_dir.path().join("exact.bin");
        let (url, server) = fake_download_cdp(vec![
            json!({"done":false,"chunk":"aGVsbG8=","contentType":"text/plain","total":5}),
            json!({"done":true,"contentType":"text/plain","total":5}),
        ]);
        let mut sess = crate::session::PageSession::connect_for_input_test(&url).unwrap();
        download(
            &mut sess,
            &ElementSelector::parse("a", false).unwrap(),
            Some(&exact_out),
            false,
            Some(5),
        )
        .unwrap();
        drop(sess);
        server.join().unwrap();

        let over_dir = tempfile::tempdir().unwrap();
        let over_out = over_dir.path().join("over.bin");
        let (url, server) = fake_download_cdp(vec![
            json!({"done":false,"chunk":"aGVsbG8=","contentType":"text/plain","total":5}),
            json!({"done":false,"chunk":"IQ==","contentType":"text/plain","total":6}),
        ]);
        let mut sess = crate::session::PageSession::connect_for_input_test(&url).unwrap();
        let err = download(
            &mut sess,
            &ElementSelector::parse("a", false).unwrap(),
            Some(&over_out),
            false,
            Some(5),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("larger than 5 bytes"));
        drop(sess);
        server.join().unwrap();
    }

    #[test]
    fn download_midstream_cleanup_error_is_context_not_primary() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("payload.bin");
        let (url, server) = fake_download_cdp_with_release_error(vec![
            json!({"done":false,"chunk":"aGVs","contentType":"text/plain","total":3}),
            json!({"error":"network exploded"}),
        ]);
        let mut sess = crate::session::PageSession::connect_for_input_test(&url).unwrap();
        let err = download(
            &mut sess,
            &ElementSelector::parse("a", false).unwrap(),
            Some(&out),
            false,
            Some(10),
        )
        .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("network exploded"));
        assert!(text.contains("download cleanup failed"));
        assert!(text.contains("release failed"));
        assert!(!out.exists());
        drop(sess);
        server.join().unwrap();
    }

    fn fake_download_cdp(steps: Vec<Value>) -> (String, thread::JoinHandle<()>) {
        fake_download_cdp_inner(steps, false)
    }

    fn fake_download_cdp_with_release_error(steps: Vec<Value>) -> (String, thread::JoinHandle<()>) {
        fake_download_cdp_inner(steps, true)
    }

    fn fake_download_cdp_inner(
        steps: Vec<Value>,
        fail_release: bool,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = accept(stream).unwrap();
            let mut next_index = 0usize;
            let mut prior_next_written = true;
            loop {
                let command: Value = match socket.read().unwrap() {
                    Message::Text(text) => serde_json::from_str(&text).unwrap(),
                    Message::Close(_) => break,
                    message => panic!("unexpected message {message:?}"),
                };
                let id = command["id"].as_u64().unwrap();
                let method = command["method"].as_str().unwrap();
                let params = &command["params"];
                let response = if method == "Runtime.evaluate" {
                    if params["returnByValue"].as_bool() == Some(true) {
                        json!({"id":id,"result":{"result":{"value":"https://page.test/"}}})
                    } else {
                        json!({"id":id,"result":{"result":{"objectId":"element-1"}}})
                    }
                } else if method == "Runtime.callFunctionOn" {
                    let function = params["functionDeclaration"].as_str().unwrap();
                    if function.contains("this.href")
                        && params["returnByValue"].as_bool() == Some(true)
                    {
                        json!({"id":id,"result":{"result":{"value":"https://page.test/file"}}})
                    } else if function.contains("fetch(url") {
                        json!({"id":id,"result":{"result":{"objectId":"stream-1"}}})
                    } else if function.contains("this.next") {
                        assert!(
                            prior_next_written,
                            "next chunk requested before previous response could be processed"
                        );
                        prior_next_written = false;
                        let step = steps[next_index].clone();
                        next_index += 1;
                        if let Some(error) = step["error"].as_str() {
                            json!({"id":id,"result":{"exceptionDetails":{"text":error,"exception":{"description":error}}}})
                        } else {
                            prior_next_written = true;
                            json!({"id":id,"result":{"result":{"value":step}}})
                        }
                    } else if function.contains("this.cancel") {
                        json!({"id":id,"result":{"result":{"value":true}}})
                    } else {
                        panic!("unexpected function {function}");
                    }
                } else if method == "Runtime.releaseObject" {
                    if fail_release {
                        json!({"id":id,"error":{"message":"release failed"}})
                    } else {
                        json!({"id":id,"result":{}})
                    }
                } else {
                    panic!("unexpected method {method}");
                };
                let response_text = response.to_string();
                assert!(
                    response_text.len() < crate::cdp::client::MAX_WEBSOCKET_FRAME_BYTES,
                    "fake CDP response exceeded frame bound: {}",
                    response_text.len()
                );
                socket.send(Message::Text(response_text.into())).unwrap();
            }
        });
        (url, server)
    }
}
