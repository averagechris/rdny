//! Page info: url, title, html, text, attr, prop, pdf.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::commands::artifacts::{HumanArtifactOutput, ProducedArtifact};
use crate::commands::{OutputFormat, print_value};
use crate::selector::ElementSelector;
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
pub fn html(sess: &mut PageSession, selector: Option<&ElementSelector>) -> Result<()> {
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
pub fn text(sess: &mut PageSession, selector: &ElementSelector) -> Result<()> {
    let object_id = sess.element(selector)?;
    let value = sess.call_on(&object_id, "function() { return this.textContent; }", &[])?;
    print_value(&value);
    Ok(())
}

/// Print an attribute of the first selector match.
pub fn attr(sess: &mut PageSession, selector: &ElementSelector, name: &str) -> Result<()> {
    let object_id = sess.element(selector)?;
    let value = sess.call_on(
        &object_id,
        "function(n) { return this.getAttribute(n); }",
        &[json!(name)],
    )?;
    if value == Value::Null {
        bail!(
            "no attribute `{name}` on first match of selector `{}`",
            selector.raw()
        );
    }
    print_value(&value);
    Ok(())
}

const PROP_OUTPUT_LIMIT_BYTES: usize = 64 * 1024;
const PROP_METADATA_LIMIT_BYTES: usize = 4096;

/// Read a live DOM property as a typed, JSON-compatible value.
pub fn prop(
    sess: &mut PageSession,
    selector: &ElementSelector,
    property: &str,
    format: OutputFormat,
) -> Result<()> {
    let object_id = sess.element(selector)?;
    let value = sess.call_on(
        &object_id,
        PROP_READER,
        &[
            json!(property),
            json!(selector.raw()),
            json!(selector.segments()),
            json!(PROP_OUTPUT_LIMIT_BYTES),
        ],
    )?;
    let status = value
        .get("status")
        .and_then(Value::as_str)
        .context("property reader returned malformed envelope: missing status")?;
    if status == "error" {
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .context("property reader returned malformed error envelope: missing message")?;
        bail!("{}", sanitize_diagnostic(message));
    } else if status != "ok" {
        bail!(
            "property reader returned malformed envelope: unknown status `{}`",
            sanitize_diagnostic(status)
        );
    }
    let property_value = value
        .get("value")
        .context("property reader returned malformed ok envelope: missing value")?
        .clone();
    if serde_json::to_vec(&property_value)?.len() > PROP_OUTPUT_LIMIT_BYTES {
        bail!(
            "property `{property}` on selector `{}` exceeds {} byte output limit",
            selector.raw(),
            PROP_OUTPUT_LIMIT_BYTES
        );
    }
    match format {
        OutputFormat::Human => print_value(&property_value),
        OutputFormat::Json | OutputFormat::Jsonl => {
            let record = json!({
                "schemaVersion": 1,
                "kind": "prop",
                "selector": selector.raw(),
                "property": property,
                "value": property_value,
            });
            if serde_json::to_vec(&record)?.len()
                > PROP_OUTPUT_LIMIT_BYTES + PROP_METADATA_LIMIT_BYTES
            {
                bail!("prop output record exceeds structured output size limit");
            }
            format.emit_json(&record)?
        }
    }
    Ok(())
}

pub fn validate_property_name(property: &str) -> Result<String> {
    if property.is_empty() {
        bail!("property name must not be empty");
    }
    if property.len() > 256 {
        bail!("property name exceeds 256 byte limit");
    }
    if property.chars().any(char::is_whitespace)
        || property.contains([
            '.', '[', ']', '(', ')', '/', '\\', ';', '=', '<', '>', '`', '\'', '"',
        ])
    {
        bail!(
            "property `{property}` looks like traversal or JavaScript; pass one literal property name such as `value`, or use `rdny js` for expressions"
        );
    }
    Ok(property.to_owned())
}

fn sanitize_diagnostic(message: &str) -> String {
    message
        .chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .take(2048)
        .collect()
}

const PROP_READER: &str = r#"function(property, selector, segments, limit) {
    const Array_isArray = Array.isArray.bind(Array);
    const Object_getPrototypeOf = Object.getPrototypeOf.bind(Object);
    const Object_keys = Object.keys.bind(Object);
    const Object_prototype_hasOwn = Object.prototype.hasOwnProperty;
    const Number_isFinite = Number.isFinite.bind(Number);
    const fail = (code, message) => ({status:'error', code, message});
    const MAX_STRING_BYTES = limit;
    const MAX_KEY_BYTES = 1024;
    const MAX_DEPTH = 32;
    const MAX_NODES = 4096;
    const MAX_ARRAY_LENGTH = 4096;
    const MAX_KEYS = 4096;
    const MAX_PATH = 256;
    let bytes = 0;
    let nodes = 0;
    const add = (n) => { bytes += n; if (bytes > limit) throw new Error('value exceeds ' + limit + ' byte UTF-8 JSON output limit'); };
    const pathJoin = (base, part) => (base.length + part.length > MAX_PATH ? base + '...' : base + part);
    const utf8LenBounded = (s, max, path) => {
        let n = 0;
        for (let i = 0; i < s.length; i++) {
            const c = s.charCodeAt(i);
            if (c < 0x80) n += 1;
            else if (c < 0x800) n += 2;
            else if (c >= 0xD800 && c <= 0xDBFF && i + 1 < s.length) {
                const d = s.charCodeAt(i + 1);
                if (d >= 0xDC00 && d <= 0xDFFF) { n += 4; i += 1; } else n += 3;
            } else n += 3;
            if (n > max) throw new Error('string/key at ' + path + ' exceeds UTF-8 byte limit');
        }
        return n;
    };
    const addJsonStringBytes = (s, path) => {
        add(2);
        for (let i = 0; i < s.length; i++) {
            const c = s.charCodeAt(i);
            if (c === 0x22 || c === 0x5c) add(2);
            else if (c <= 0x1f) add(6);
            else if (c < 0x80) add(1);
            else if (c < 0x800) add(2);
            else if (c >= 0xD800 && c <= 0xDBFF && i + 1 < s.length) {
                const d = s.charCodeAt(i + 1);
                if (d >= 0xDC00 && d <= 0xDFFF) { add(4); i += 1; } else add(6);
            } else if (c >= 0xD800 && c <= 0xDFFF) add(6);
            else add(3);
        }
    };
    if (!this || !this.isConnected) return fail('detached', 'selector `' + selector + '` resolved to a detached element');
    let root = document;
    for (let i = 0; i < segments.length; i++) {
        const css = segments[i];
        let all;
        try { all = root.querySelectorAll(css); }
        catch (e) { return fail('invalid-selector', 'invalid CSS while revalidating selector `' + selector + '` segment `' + css + '`: ' + (e && e.message ? e.message : String(e))); }
        if (all.length !== 1) return fail(all.length === 0 ? 'missing' : 'multiple', 'selector `' + selector + '` must match exactly one live element; segment `' + css + '` matched ' + all.length);
        const el = all.item(0);
        if (i === segments.length - 1) {
            if (el !== this) return fail('detached', 'selector `' + selector + '` changed while reading property `' + property + '`');
        } else {
            if (!el.shadowRoot) return fail('shadow-root', 'pierced selector segment `' + css + '` does not expose an open shadow root; closed roots are unsupported');
            root = el.shadowRoot;
        }
    }
    let raw;
    try { raw = this[property]; } catch (e) { return fail('getter-threw', 'getter for property `' + property + '` threw: ' + (e && e.message ? e.message : String(e))); }
    if (!this || !this.isConnected) return fail('detached', 'selector `' + selector + '` detached while reading property `' + property + '`');
    let checkRoot = document;
    for (let i = 0; i < segments.length; i++) {
        let all;
        try { all = checkRoot.querySelectorAll(segments[i]); } catch (e) { return fail('invalid-selector', 'invalid CSS while revalidating selector `' + selector + '`'); }
        if (all.length !== 1) return fail(all.length === 0 ? 'missing' : 'multiple', 'selector `' + selector + '` changed while reading property `' + property + '`');
        const el = all.item(0);
        if (i === segments.length - 1 && el !== this) return fail('detached', 'selector `' + selector + '` changed while reading property `' + property + '`');
        if (i < segments.length - 1) checkRoot = el.shadowRoot;
    }
    const seen = new Set();
    function enc(v, path, depth) {
        nodes += 1;
        if (nodes > MAX_NODES) throw new Error('value exceeds node count limit ' + MAX_NODES);
        if (depth > MAX_DEPTH) throw new Error('value exceeds nesting depth limit ' + MAX_DEPTH + ' at ' + path);
        const t = typeof v;
        if (v === null) { add(4); return null; }
        if (t === 'boolean') { add(v ? 4 : 5); return v; }
        if (t === 'string') { utf8LenBounded(v, MAX_STRING_BYTES, path); addJsonStringBytes(v, path); return v; }
        if (t === 'number') { if (!Number_isFinite(v)) throw new Error('non-finite number at ' + path); add(String(v).length); return v; }
        if (t === 'undefined') throw new Error('undefined value');
        if (t === 'symbol') throw new Error('symbol value at ' + path);
        if (t === 'function') throw new Error('function/callable value at ' + path);
        if (t !== 'object') throw new Error('unsupported value at ' + path);
        if (typeof Node !== 'undefined' && v instanceof Node) throw new Error('DOM/host object at ' + path + ' is unsupported');
        if (v === window || v === document) throw new Error('DOM/host object at ' + path + ' is unsupported');
        if (seen.has(v)) throw new Error('cyclic value at ' + path);
        let proto;
        try { proto = Object_getPrototypeOf(v); } catch (e) { throw new Error('cannot inspect object at ' + path + ': ' + (e && e.message ? e.message : String(e))); }
        const isArray = Array_isArray(v);
        if (!isArray && proto !== Object.prototype && proto !== null) throw new Error('non-plain object at ' + path + ' is unsupported');
        seen.add(v);
        let out;
        if (isArray) {
            out = [];
            if (v.length > MAX_ARRAY_LENGTH) throw new Error('array at ' + path + ' exceeds length limit ' + MAX_ARRAY_LENGTH);
            add(2);
            for (let i = 0; i < v.length; i++) {
                if (i > 0) add(1);
                if (!Object_prototype_hasOwn.call(v, i)) throw new Error('array hole/undefined value at ' + path + '[' + i + ']');
                let item;
                try { item = v[i]; } catch (e) { throw new Error('getter for ' + path + '[' + i + '] threw: ' + (e && e.message ? e.message : String(e))); }
                out.push(enc(item, pathJoin(path, '[' + i + ']'), depth + 1));
            }
        } else {
            out = {__proto__: null};
            let keys;
            try { keys = Object_keys(v); } catch (e) { throw new Error('cannot enumerate object at ' + path + ': ' + (e && e.message ? e.message : String(e))); }
            if (keys.length > MAX_KEYS) throw new Error('object at ' + path + ' exceeds key count limit ' + MAX_KEYS);
            add(2);
            for (let keyIndex = 0; keyIndex < keys.length; keyIndex++) {
                const k = keys[keyIndex];
                utf8LenBounded(k, MAX_KEY_BYTES, path);
                if (keyIndex > 0) add(1);
                addJsonStringBytes(k, path); add(1);
                let item;
                try { item = v[k]; } catch (e) { throw new Error('getter for ' + pathJoin(path, '.' + k) + ' threw: ' + (e && e.message ? e.message : String(e))); }
                out[k] = enc(item, pathJoin(path, '.' + k), depth + 1);
            }
        }
        seen.delete(v);
        return out;
    }
    try {
        const value = enc(raw, property, 0);
        if (!this || !this.isConnected) return fail('detached', 'selector `' + selector + '` detached while encoding property `' + property + '`');
        let finalRoot = document;
        for (let i = 0; i < segments.length; i++) {
            let all;
            try { all = finalRoot.querySelectorAll(segments[i]); } catch (e) { return fail('invalid-selector', 'invalid CSS while finally revalidating selector `' + selector + '`'); }
            if (all.length !== 1) return fail(all.length === 0 ? 'missing' : 'multiple', 'selector `' + selector + '` changed while encoding property `' + property + '`');
            const el = all.item(0);
            if (i === segments.length - 1 && el !== this) return fail('detached', 'selector `' + selector + '` changed while encoding property `' + property + '`');
            if (i < segments.length - 1) {
                if (!el.shadowRoot) return fail('shadow-root', 'pierced selector changed while encoding property `' + property + '`');
                finalRoot = el.shadowRoot;
            }
        }
        return {status:'ok', value};
    } catch (e) { return fail('unsupported', 'property `' + property + '` is not JSON-compatible: ' + (e && e.message ? e.message : String(e))); }
}"#;

/// Save the page as PDF (default file: page.pdf).
pub fn pdf(sess: &mut PageSession, file: Option<&Path>, force: bool) -> Result<ProducedArtifact> {
    let path = file
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("page.pdf"));
    let context = sess.page_identity()?.into();
    let result = sess.call("Page.printToPDF", json!({}))?;
    let data = result["data"]
        .as_str()
        .context("Page.printToPDF response missing data")?;
    let bytes = crate::commands::decode_base64(data)?;
    save_pdf(path, &bytes, force, context)
}

fn save_pdf(
    path: PathBuf,
    bytes: &[u8],
    force: bool,
    context: crate::commands::artifacts::ArtifactContext,
) -> Result<ProducedArtifact> {
    let published = crate::commands::artifacts::write_artifact(&path, bytes, force)
        .with_context(|| format!("writing PDF to {}", path.display()))?;
    Ok(ProducedArtifact::new(
        published,
        path,
        HumanArtifactOutput::Saved,
        "application/pdf",
        None,
        context,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdp::client::CdpClient;
    use std::net::TcpListener;
    use std::thread;
    use tungstenite::{Message, accept};

    #[test]
    fn pdf_file_result_has_final_metadata_and_mime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("page.pdf");
        let artifact = save_pdf(
            path.clone(),
            b"%PDF-test",
            false,
            crate::commands::artifacts::ArtifactContext::default(),
        )
        .unwrap();
        assert_eq!(
            artifact.path,
            path.canonicalize().unwrap().to_string_lossy()
        );
        assert_eq!(artifact.media_type, "application/pdf");
        assert_eq!(artifact.bytes, 9);
        assert_eq!((artifact.width, artifact.height), (None, None));
    }

    #[test]
    fn property_name_validation_rejects_expression_like_traversal() {
        for name in ["", "a.b", "a[0]", "onclick()", "a/b", "a b", "x=y"] {
            let error = validate_property_name(name).unwrap_err().to_string();
            assert!(
                error.contains("literal property name") || error.contains("must not be empty"),
                "{error}"
            );
        }
        for name in [
            "value",
            "checked",
            "selectedIndex",
            "naturalWidth",
            "data-custom",
            "aria_current",
        ] {
            validate_property_name(name).unwrap();
        }
    }

    #[test]
    fn prop_reader_documents_safety_classes_and_limit() {
        assert!(PROP_READER.contains("this[property]"));
        assert!(!PROP_READER.contains("eval("));
        assert!(!PROP_READER.contains("TextEncoder"));
        assert!(!PROP_READER.contains("JSON.stringify"));
        assert!(PROP_READER.contains("out = {__proto__: null}"));
        assert!(PROP_READER.contains("querySelectorAll(css)"));
        assert!(!PROP_READER.contains("Array.from"));
        assert!(PROP_READER.contains("undefined value"));
        assert!(PROP_READER.contains("non-finite number"));
        assert!(PROP_READER.contains("function/callable"));
        assert!(PROP_READER.contains("cyclic value"));
        assert!(PROP_READER.contains("DOM/host object"));
        assert!(PROP_READER.contains("array hole/undefined"));
        assert!(PROP_READER.contains("MAX_DEPTH"));
        assert!(PROP_READER.contains("MAX_NODES"));
        assert_eq!(PROP_OUTPUT_LIMIT_BYTES, 64 * 1024);
    }

    #[test]
    fn prop_envelope_validation_rejects_malformed_and_sanitizes_controls() {
        for envelope in [
            json!({}),
            json!({"status":"ok"}),
            json!({"status":"error"}),
            json!({"status":"wat","message":"nope"}),
        ] {
            let err = run_prop_protocol(envelope, "value")
                .unwrap_err()
                .to_string();
            assert!(err.contains("malformed"), "{err}");
        }

        let err = run_prop_protocol(
            json!({"status":"error","message":"getter \u{1b}[31mboom"}),
            "value",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("getter �[31mboom"), "{err}");
        assert!(!err.contains('\u{1b}'), "{err:?}");
    }

    #[test]
    fn prop_protocol_sends_property_as_argument_and_accepts_typed_envelope() {
        let seen = run_prop_protocol(
            json!({"status":"ok","value":{"s":"ok","b":true,"n":7,"a":[1,null]}}),
            "data-custom",
        )
        .unwrap();
        assert_eq!(seen["method"], "Runtime.callFunctionOn");
        assert_eq!(seen["params"]["objectId"], "node-1");
        assert_eq!(seen["params"]["functionDeclaration"], PROP_READER);
        assert_eq!(seen["params"]["returnByValue"], true);
        assert_eq!(seen["params"]["arguments"][0]["value"], "data-custom");
        assert_eq!(seen["params"]["arguments"][1]["value"], "#target");
        assert_eq!(seen["params"]["arguments"][2]["value"], json!(["#target"]));
        assert_eq!(
            seen["params"]["arguments"][3]["value"],
            PROP_OUTPUT_LIMIT_BYTES
        );
        let serialized = serde_json::to_string(&seen).unwrap();
        assert_eq!(serialized.matches("data-custom").count(), 1);
    }

    #[test]
    fn prop_protocol_error_envelopes_are_actionable() {
        for (code, detail) in [
            ("getter-threw", "getter for property `value` threw: boom"),
            (
                "unsupported",
                "property `value` is not JSON-compatible: undefined value",
            ),
            (
                "unsupported",
                "property `value` is not JSON-compatible: non-finite number at value",
            ),
            (
                "unsupported",
                "property `value` is not JSON-compatible: function/callable value at value",
            ),
            (
                "unsupported",
                "property `value` is not JSON-compatible: cyclic value at value.self",
            ),
            (
                "unsupported",
                "property `value` is not JSON-compatible: value exceeds 65536 byte UTF-8 JSON output limit",
            ),
            (
                "detached",
                "selector `#target` changed while reading property `value`",
            ),
            (
                "invalid-selector",
                "invalid CSS while revalidating selector `#target`",
            ),
        ] {
            let error = run_prop_protocol(
                json!({"status":"error","code":code,"message":detail}),
                "value",
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains(detail), "{error}");
        }
    }

    fn run_prop_protocol(envelope: Value, property: &str) -> Result<Value> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = accept(stream).unwrap();
            let resolve = read_json_message(&mut socket);
            assert_eq!(resolve["method"], "Runtime.evaluate");
            assert!(
                resolve["params"]["expression"]
                    .as_str()
                    .unwrap()
                    .contains("querySelector")
            );
            socket
                .send(Message::Text(
                    format!(
                        r#"{{"id":{},"result":{{"result":{{"type":"object","subtype":"node","objectId":"node-1"}}}}}}"#,
                        resolve["id"]
                    )
                    .into(),
                ))
                .unwrap();
            let call = read_json_message(&mut socket);
            socket
                .send(Message::Text(
                    json!({"id": call["id"], "result": {"result": {"value": envelope}}})
                        .to_string()
                        .into(),
                ))
                .unwrap();
            call
        });
        let client = CdpClient::connect(&format!("ws://127.0.0.1:{port}"))?;
        let mut sess = PageSession::new_for_test(client, "page-session", "target");
        let selector = ElementSelector::parse("#target", false)?;
        let result = prop(&mut sess, &selector, property, OutputFormat::Human);
        let call = server.join().unwrap();
        result.map(|_| call)
    }

    fn read_json_message(socket: &mut tungstenite::WebSocket<std::net::TcpStream>) -> Value {
        match socket.read().unwrap() {
            Message::Text(text) => serde_json::from_str(&text).unwrap(),
            message => panic!("unexpected message {message:?}"),
        }
    }
}
