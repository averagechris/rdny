//! Chrome's built-in HTTP endpoints: /json/version, /json/list,
//! /json/new. Hand-rolled HTTP/1.1 over std::net (Connection: close),
//! parsing Content-Length or chunked bodies.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

fn request(host: &str, port: u16, method: &str, path: &str) -> Result<Value> {
    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&addr).with_context(|| format!("connecting to {addr}"))?;
    stream.set_read_timeout(Some(HTTP_TIMEOUT))?;
    stream.set_write_timeout(Some(HTTP_TIMEOUT))?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    )?;
    // Chrome's DevTools HTTP server ignores `Connection: close` and keeps
    // the socket open, so we must frame the body ourselves instead of
    // reading to EOF.
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    let header_end = loop {
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let n = stream
            .read(&mut buf)
            .with_context(|| format!("reading response headers from {addr}"))?;
        if n == 0 {
            bail!("connection to {addr} closed before response headers finished");
        }
        raw.extend_from_slice(&buf[..n]);
    };
    let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let mut body = raw[header_end + 4..].to_vec();
    let status_line = head.lines().next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .context("malformed HTTP status line")?;
    let header_value = |name: &str| -> Option<String> {
        head.lines().skip(1).find_map(|l| {
            let (key, value) = l.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
    };
    let chunked = header_value("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let content_length: Option<usize> = header_value("content-length").and_then(|v| v.parse().ok());
    let body = if chunked {
        loop {
            if let Some(out) = dechunk(&body)? {
                break out;
            }
            let n = stream
                .read(&mut buf)
                .with_context(|| format!("reading chunked body from {addr}"))?;
            if n == 0 {
                bail!("connection to {addr} closed mid chunked body");
            }
            body.extend_from_slice(&buf[..n]);
        }
    } else if let Some(len) = content_length {
        while body.len() < len {
            let n = stream
                .read(&mut buf)
                .with_context(|| format!("reading body from {addr}"))?;
            if n == 0 {
                bail!("connection to {addr} closed mid body");
            }
            body.extend_from_slice(&buf[..n]);
        }
        body.truncate(len);
        body
    } else {
        // No framing headers: fall back to read-until-close.
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break body,
                Ok(n) => body.extend_from_slice(&buf[..n]),
                Err(e) => return Err(e).with_context(|| format!("reading body from {addr}")),
            }
        }
    };
    if !(200..300).contains(&status) {
        bail!(
            "HTTP {status} from {method} {path}: {}",
            String::from_utf8_lossy(&body).trim()
        );
    }
    serde_json::from_slice(&body).with_context(|| format!("parsing JSON from {method} {path}"))
}

/// Decode a chunked body. Ok(None) means the data is incomplete and the
/// caller should read more; Err means definitively malformed.
fn dechunk(mut body: &[u8]) -> Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    loop {
        let Some(line_end) = body.windows(2).position(|w| w == b"\r\n") else {
            return Ok(None);
        };
        let size_str = String::from_utf8_lossy(&body[..line_end]);
        let size = usize::from_str_radix(size_str.trim().split(';').next().unwrap_or("0"), 16)
            .context("malformed chunk size")?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Ok(Some(out));
        }
        if body.len() < size + 2 {
            return Ok(None);
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

/// GET a /json/* endpoint.
pub fn get_json(host: &str, port: u16, path: &str) -> Result<Value> {
    request(host, port, "GET", path)
}

/// PUT a /json/* endpoint (required by /json/new since Chrome 111).
pub fn put_json(host: &str, port: u16, path: &str) -> Result<Value> {
    request(host, port, "PUT", path)
}

/// Subset of /json/version we care about.
#[derive(Debug, Clone, Deserialize)]
pub struct VersionInfo {
    #[serde(rename = "Browser")]
    pub browser: String,
    #[serde(rename = "webSocketDebuggerUrl")]
    pub ws_url: String,
}

/// Probe /json/version; the definitive "is the debug port live" check.
pub fn version(host: &str, port: u16) -> Result<VersionInfo> {
    let value = get_json(host, port, "/json/version")?;
    serde_json::from_value(value).context("parsing /json/version")
}

/// A target entry from /json/list.
#[derive(Debug, Clone, Deserialize)]
pub struct TargetInfo {
    pub id: String,
    #[serde(rename = "type")]
    pub target_type: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(rename = "webSocketDebuggerUrl", default)]
    pub ws_url: Option<String>,
}

/// List all targets; callers typically filter `target_type == "page"`.
pub fn list_targets(host: &str, port: u16) -> Result<Vec<TargetInfo>> {
    let value = get_json(host, port, "/json/list")?;
    serde_json::from_value(value).context("parsing /json/list")
}

/// Open a new tab, optionally at a URL.
pub fn new_tab(host: &str, port: u16, url: Option<&str>) -> Result<TargetInfo> {
    let path = match url {
        Some(u) => format!("/json/new?{u}"),
        None => "/json/new".to_string(),
    };
    let value = put_json(host, port, &path)?;
    serde_json::from_value(value).context("parsing /json/new")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    fn serve_once(response: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            sock.write_all(response.as_bytes()).unwrap();
            let _ = sock.shutdown(std::net::Shutdown::Write);
            // Drain until the client closes so dropping the socket
            // sends FIN rather than RST with unread data queued.
            while matches!(sock.read(&mut buf), Ok(n) if n > 0) {}
        });
        port
    }

    #[test]
    fn parses_content_length_body() {
        let port = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n{\"ok\": true}\n",
        );
        let v = get_json("127.0.0.1", port, "/json/version").unwrap();
        assert_eq!(v["ok"], Value::Bool(true));
    }

    #[test]
    fn parses_chunked_body() {
        let port = serve_once(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n7\r\n{\"a\": 1\r\n1\r\n}\r\n0\r\n\r\n",
        );
        let v = get_json("127.0.0.1", port, "/json/list").unwrap();
        assert_eq!(v["a"], Value::from(1));
    }

    #[test]
    fn non_2xx_is_error() {
        let port = serve_once("HTTP/1.1 500 Oops\r\nContent-Length: 4\r\n\r\nnope");
        let err = get_json("127.0.0.1", port, "/json/version").unwrap_err();
        assert!(format!("{err}").contains("HTTP 500"));
    }

    #[test]
    fn version_parses_fields() {
        let port = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Length: 91\r\n\r\n{\"Browser\": \"Chrome/140.0.0.0\", \"webSocketDebuggerUrl\": \"ws://127.0.0.1:9222/devtools/b/1\"}",
        );
        let v = version("127.0.0.1", port).unwrap();
        assert_eq!(v.browser, "Chrome/140.0.0.0");
        assert!(v.ws_url.starts_with("ws://"));
    }
}
