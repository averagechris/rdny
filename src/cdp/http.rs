//! Chrome's built-in HTTP endpoints with bounded HTTP/1.1 framing.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

pub const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_HEADER_BYTES: usize = 32 * 1024;
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_CHUNK_OVERHEAD: usize = 256 * 1024;
const MAX_CHUNK_LINE_BYTES: usize = 8 * 1024;
const MAX_TRAILER_BYTES: usize = 16 * 1024;

fn remaining(deadline: Instant, operation: &str) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .with_context(|| format!("HTTP request deadline elapsed while {operation}"))
}

fn connect_until(host: &str, port: u16, deadline: Instant) -> Result<TcpStream> {
    let addr = authority(host, port);
    let addresses = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("resolving {addr}"))?
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        bail!("{addr} resolved to no addresses");
    }
    let mut last_error = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, remaining(deadline, "connecting")?) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.expect("at least one address")).with_context(|| format!("connecting to {addr}"))
}

fn authority(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn request_until(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    deadline: Instant,
) -> Result<Value> {
    if path
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte == b' ')
    {
        bail!("HTTP request target contains a space or control character");
    }
    if host
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        bail!("HTTP host contains whitespace or a control character");
    }
    let addr = authority(host, port);
    let mut stream = connect_until(host, port, deadline)?;
    stream.set_write_timeout(Some(remaining(deadline, "writing request")?))?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    )?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    let header_end = loop {
        if let Some(position) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            if position > MAX_HEADER_BYTES {
                bail!("HTTP response headers exceed {MAX_HEADER_BYTES} bytes");
            }
            break position;
        }
        if raw.len() >= MAX_HEADER_BYTES {
            bail!("HTTP response headers exceed {MAX_HEADER_BYTES} bytes");
        }
        stream.set_read_timeout(Some(remaining(deadline, "reading response headers")?))?;
        let capacity = buf.len().min(MAX_HEADER_BYTES + 4 - raw.len());
        let read = stream
            .read(&mut buf[..capacity])
            .with_context(|| format!("reading response headers from {addr}"))?;
        if read == 0 {
            bail!("connection to {addr} closed before response headers finished");
        }
        raw.extend_from_slice(&buf[..read]);
    };

    let head = std::str::from_utf8(&raw[..header_end]).context("HTTP headers are not UTF-8")?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().context("missing HTTP status line")?;
    let mut status_parts = status_line.split(' ');
    let version = status_parts.next().unwrap_or_default();
    let status: u16 = status_parts
        .next()
        .context("malformed HTTP status line")?
        .parse()
        .context("malformed HTTP status code")?;
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1") || !(100..=999).contains(&status) {
        bail!("malformed HTTP status line");
    }

    let mut content_length = None;
    let mut transfer_encoding = None;
    for line in lines {
        let (name, value) = line.split_once(':').context("malformed HTTP header")?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
            || value
                .bytes()
                .any(|byte| byte.is_ascii_control() && byte != b'\t')
        {
            bail!("malformed HTTP header");
        }
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            let parsed = value.parse::<usize>().context("malformed Content-Length")?;
            if content_length
                .replace(parsed)
                .is_some_and(|old| old != parsed)
            {
                bail!("conflicting Content-Length headers");
            }
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            && transfer_encoding
                .replace(value.to_ascii_lowercase())
                .is_some()
        {
            bail!("multiple Transfer-Encoding headers are unsupported");
        }
    }
    if content_length.is_some() && transfer_encoding.is_some() {
        bail!("response contains both Content-Length and Transfer-Encoding");
    }

    let mut body = raw[header_end + 4..].to_vec();
    let body = if let Some(encoding) = transfer_encoding {
        if encoding != "chunked" {
            bail!("unsupported Transfer-Encoding `{encoding}`");
        }
        loop {
            if let Some(output) = dechunk(&body)? {
                break output;
            }
            let wire_limit = MAX_BODY_BYTES
                .checked_add(MAX_CHUNK_OVERHEAD)
                .context("chunk wire limit overflow")?;
            if body.len() >= wire_limit {
                bail!("chunked HTTP body framing exceeds limit");
            }
            stream.set_read_timeout(Some(remaining(deadline, "reading chunked body")?))?;
            let capacity = buf.len().min(wire_limit - body.len());
            let read = stream.read(&mut buf[..capacity])?;
            if read == 0 {
                bail!("connection to {addr} closed mid chunked body");
            }
            body.extend_from_slice(&buf[..read]);
        }
    } else if let Some(length) = content_length {
        if length > MAX_BODY_BYTES {
            bail!("HTTP body length {length} exceeds {MAX_BODY_BYTES} bytes");
        }
        while body.len() < length {
            stream.set_read_timeout(Some(remaining(deadline, "reading body")?))?;
            let capacity = buf.len().min(length - body.len());
            let read = stream.read(&mut buf[..capacity])?;
            if read == 0 {
                bail!("connection to {addr} closed mid body");
            }
            body.extend_from_slice(&buf[..read]);
        }
        body.truncate(length);
        body
    } else {
        loop {
            if body.len() >= MAX_BODY_BYTES {
                bail!("HTTP close-delimited body exceeds {MAX_BODY_BYTES} bytes");
            }
            stream.set_read_timeout(Some(remaining(deadline, "reading close-delimited body")?))?;
            let capacity = buf.len().min(MAX_BODY_BYTES - body.len());
            let read = stream.read(&mut buf[..capacity])?;
            if read == 0 {
                break body;
            }
            body.extend_from_slice(&buf[..read]);
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

/// Decode a complete chunked body, validating chunk CRLF and trailers.
fn dechunk(mut body: &[u8]) -> Result<Option<Vec<u8>>> {
    let mut output = Vec::new();
    loop {
        let Some(line_end) = body.windows(2).position(|window| window == b"\r\n") else {
            if body.len() > MAX_CHUNK_LINE_BYTES {
                bail!("chunk size line exceeds limit");
            }
            return Ok(None);
        };
        if line_end > MAX_CHUNK_LINE_BYTES {
            bail!("chunk size line exceeds limit");
        }
        let line = std::str::from_utf8(&body[..line_end]).context("chunk size is not ASCII")?;
        let (size_text, extension) = line
            .split_once(';')
            .map_or((line, None), |(size, extension)| (size, Some(extension)));
        if extension.is_some_and(|extension| {
            extension
                .bytes()
                .any(|byte| byte.is_ascii_control() || !byte.is_ascii())
        }) {
            bail!("malformed chunk extension");
        }
        if size_text.is_empty() || !size_text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("malformed chunk size");
        }
        let size = usize::from_str_radix(size_text, 16).context("chunk size overflow")?;
        body = body.get(line_end + 2..).context("chunk offset overflow")?;
        if size == 0 {
            let Some(end) = body.windows(4).position(|window| window == b"\r\n\r\n") else {
                if body == b"\r\n" {
                    return Ok(Some(output));
                }
                if body.len() > MAX_TRAILER_BYTES {
                    bail!("HTTP trailers exceed limit");
                }
                return Ok(None);
            };
            if end > MAX_TRAILER_BYTES {
                bail!("HTTP trailers exceed limit");
            }
            for trailer in body[..end].split(|byte| *byte == b'\n') {
                let trailer = trailer.strip_suffix(b"\r").unwrap_or(trailer);
                let Some(separator) = trailer.iter().position(|byte| *byte == b':') else {
                    bail!("malformed HTTP trailer");
                };
                let (name, value_with_colon) = trailer.split_at(separator);
                let value = &value_with_colon[1..];
                if name.is_empty()
                    || !name.iter().all(|byte| {
                        byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(byte)
                    })
                    || value
                        .iter()
                        .any(|byte| byte.is_ascii_control() && *byte != b'\t')
                {
                    bail!("malformed HTTP trailer");
                }
                if name.eq_ignore_ascii_case(b"content-length")
                    || name.eq_ignore_ascii_case(b"transfer-encoding")
                {
                    bail!("framing headers are forbidden in HTTP trailers");
                }
            }
            return Ok(Some(output));
        }
        let framed = size.checked_add(2).context("chunk framing overflow")?;
        if body.len() < framed {
            return Ok(None);
        }
        if &body[size..framed] != b"\r\n" {
            bail!("chunk data is not followed by CRLF");
        }
        let new_len = output
            .len()
            .checked_add(size)
            .context("chunk body size overflow")?;
        if new_len > MAX_BODY_BYTES {
            bail!("chunked HTTP body exceeds {MAX_BODY_BYTES} bytes");
        }
        output.extend_from_slice(&body[..size]);
        body = &body[framed..];
    }
}

pub fn get_json_until(host: &str, port: u16, path: &str, deadline: Instant) -> Result<Value> {
    request_until(host, port, "GET", path, deadline)
}

pub fn put_json_until(host: &str, port: u16, path: &str, deadline: Instant) -> Result<Value> {
    request_until(host, port, "PUT", path, deadline)
}

pub fn get_json(host: &str, port: u16, path: &str) -> Result<Value> {
    get_json_until(host, port, path, Instant::now() + HTTP_TIMEOUT)
}

pub fn put_json(host: &str, port: u16, path: &str) -> Result<Value> {
    put_json_until(host, port, path, Instant::now() + HTTP_TIMEOUT)
}

#[derive(Debug, Clone, Deserialize)]
pub struct VersionInfo {
    #[serde(rename = "Browser")]
    pub browser: String,
    #[serde(rename = "webSocketDebuggerUrl")]
    pub ws_url: String,
}

pub fn version(host: &str, port: u16) -> Result<VersionInfo> {
    version_until(host, port, Instant::now() + HTTP_TIMEOUT)
}

pub fn version_until(host: &str, port: u16, deadline: Instant) -> Result<VersionInfo> {
    serde_json::from_value(get_json_until(host, port, "/json/version", deadline)?)
        .context("parsing /json/version")
}

#[derive(Debug, Clone, Deserialize)]
pub struct TargetInfo {
    pub id: String,
    #[serde(rename = "type")]
    pub target_type: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
}

pub fn list_targets(host: &str, port: u16) -> Result<Vec<TargetInfo>> {
    serde_json::from_value(get_json(host, port, "/json/list")?).context("parsing /json/list")
}

pub fn new_tab(host: &str, port: u16, url: Option<&str>) -> Result<TargetInfo> {
    let path = match url {
        Some(url) => {
            if url.bytes().any(|byte| byte.is_ascii_control()) {
                bail!("new tab URL contains a control character");
            }
            validate_percent_escapes(url)?;
            let parsed = url::Url::parse(url).context("malformed new tab URL")?;
            let encoded: String =
                url::form_urlencoded::byte_serialize(parsed.as_str().as_bytes()).collect();
            format!("/json/new?{encoded}")
        }
        None => "/json/new".to_string(),
    };
    serde_json::from_value(put_json(host, port, &path)?).context("parsing /json/new")
}

fn validate_percent_escapes(value: &str) -> Result<()> {
    let bytes = value.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'%'
            && (index.checked_add(2).is_none_or(|end| end >= bytes.len())
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit())
        {
            bail!("URL contains a malformed percent escape");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    fn serve_once(response: Vec<u8>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request);
            let _ = socket.write_all(&response);
        });
        port
    }

    #[test]
    fn parses_content_length_and_chunked_bodies() {
        let port =
            serve_once(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}".to_vec());
        assert_eq!(get_json("127.0.0.1", port, "/").unwrap()["ok"], true);
        let port = serve_once(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n7\r\n{\"a\":1}\r\n0\r\nX-Ok: yes\r\n\r\n".to_vec());
        assert_eq!(get_json("127.0.0.1", port, "/").unwrap()["a"], 1);
    }

    #[test]
    fn rejects_oversized_headers_and_bodies() {
        let mut response = b"HTTP/1.1 200 OK\r\nX-Big: ".to_vec();
        response.extend(vec![b'a'; MAX_HEADER_BYTES]);
        let port = serve_once(response);
        assert!(
            format!("{}", get_json("127.0.0.1", port, "/").unwrap_err()).contains("headers exceed")
        );
        let port = serve_once(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                MAX_BODY_BYTES + 1
            )
            .into_bytes(),
        );
        assert!(
            format!("{}", get_json("127.0.0.1", port, "/").unwrap_err()).contains("body length")
        );
    }

    #[test]
    fn rejects_ambiguous_and_malformed_chunk_framing() {
        let port = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nTransfer-Encoding: chunked\r\n\r\n{}"
                .to_vec(),
        );
        assert!(format!("{}", get_json("127.0.0.1", port, "/").unwrap_err()).contains("both"));
        let port = serve_once(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}XX0\r\n\r\n".to_vec(),
        );
        assert!(format!("{}", get_json("127.0.0.1", port, "/").unwrap_err()).contains("CRLF"));
        let port = serve_once(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\nnot-a-trailer\r\n\r\n"
                .to_vec(),
        );
        assert!(format!("{}", get_json("127.0.0.1", port, "/").unwrap_err()).contains("trailer"));
    }

    #[test]
    fn overall_deadline_stops_slow_drip_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            for byte in b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}" {
                if socket.write_all(&[*byte]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(15));
            }
        });
        let deadline = Instant::now() + Duration::from_millis(80);
        assert!(get_json_until("127.0.0.1", port, "/", deadline).is_err());
    }

    #[test]
    fn new_tab_percent_encodes_url_and_rejects_controls() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut chunk).unwrap();
                assert!(read > 0);
                request.extend_from_slice(&chunk[..read]);
            }
            let text = String::from_utf8_lossy(&request);
            assert!(text.starts_with("PUT /json/new?https%3A%2F%2Fexample.com%2Fa%2520b%3Fx%3D1%25202%26y%3D%25E2%2598%2583%23frag HTTP/1.1\r\n"));
            let body = br#"{"id":"1","type":"page","title":"","url":""}"#;
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
            .unwrap();
            socket.write_all(body).unwrap();
        });
        new_tab(
            "127.0.0.1",
            port,
            Some("https://example.com/a%20b?x=1%202&y=☃#frag"),
        )
        .unwrap();
        handle.join().unwrap();
        assert!(new_tab("127.0.0.1", 9, Some("https://x/\r\nInjected: yes")).is_err());
        assert!(new_tab("127.0.0.1", 9, Some("https://x/%zz")).is_err());
    }
}
