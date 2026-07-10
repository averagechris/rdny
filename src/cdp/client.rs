//! Blocking CDP WebSocket client (flat session protocol).

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::session::Deadline;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tungstenite::client::IntoClientRequest;
use tungstenite::handshake::HandshakeError;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

/// Default per-call response timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_WEBSOCKET_FRAME_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_WEBSOCKET_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_PIPE_MESSAGE_BYTES: usize = 100 * 1024 * 1024;
pub const MAX_QUEUED_EVENTS: usize = 1024;

trait CdpTransport: Send {
    fn send_json(&mut self, value: &Value, deadline: Deadline) -> Result<()>;
    fn recv_json(&mut self, deadline: Deadline) -> Result<Value>;
    fn close(&mut self) -> Result<()>;
}

fn url_host_is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(host)) => {
            host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost")
        }
        None => false,
    }
}

/// Ensure discovery cannot redirect the WebSocket to a different authority.
/// Plain WS is intentionally supported only for loopback endpoints; remote CDP
/// must be carried through a verified SSH tunnel terminating on loopback.
pub fn validate_debugger_url(ws_url: &str, approved_host: &str, approved_port: u16) -> Result<()> {
    use std::net::IpAddr;

    let parsed = url::Url::parse(ws_url).context("malformed webSocketDebuggerUrl")?;
    if parsed.scheme() != "ws" {
        bail!(
            "unsupported debugger URL scheme `{}`; rdny supports ws:// only through a loopback endpoint",
            parsed.scheme()
        );
    }
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.fragment().is_some() {
        bail!("debugger URL must not contain credentials or a fragment");
    }
    let actual_host = parsed
        .host_str()
        .context("debugger URL is missing a host")?;
    let actual_port = parsed
        .port_or_known_default()
        .context("debugger URL is missing a port")?;
    if actual_port != approved_port {
        bail!(
            "debugger URL port {actual_port} does not match approved endpoint port {approved_port}"
        );
    }
    let approved_is_loopback = approved_host.eq_ignore_ascii_case("localhost")
        || approved_host.ends_with(".localhost")
        || approved_host
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if !(actual_host.eq_ignore_ascii_case(approved_host)
        || url_host_is_loopback(&parsed) && approved_is_loopback)
    {
        bail!(
            "debugger URL host `{actual_host}` does not match approved endpoint `{approved_host}`"
        );
    }
    Ok(())
}

/// A CDP event received out-of-band.
#[derive(Debug, Clone)]
pub struct Event {
    pub method: String,
    pub params: Value,
    /// Session the event belongs to (flat protocol). Only read by tests
    /// today, but inherent to the protocol and cheap to carry.
    #[allow(dead_code)]
    pub session_id: Option<String>,
}

/// Blocking connection to a browser WebSocket debugger URL.
pub struct CdpClient {
    transport: Box<dyn CdpTransport>,
    next_id: u64,
    events: VecDeque<Event>,
    timeout: Duration,
}

impl CdpClient {
    /// Connect to a bounded ws:// debugger URL (browser-level endpoint).
    #[cfg(test)]
    pub fn connect(ws_url: &str) -> Result<Self> {
        Self::connect_until(ws_url, Deadline::after(DEFAULT_TIMEOUT))
    }

    /// Connect TCP and complete the WebSocket handshake before `deadline`.
    pub fn connect_until(ws_url: &str, deadline: Deadline) -> Result<Self> {
        let parsed = url::Url::parse(ws_url).context("malformed browser WebSocket URL")?;
        if parsed.scheme() != "ws" || !url_host_is_loopback(&parsed) {
            bail!(
                "browser WebSocket must be a loopback ws:// URL; use a verified SSH tunnel for remote CDP"
            );
        }
        let config = WebSocketConfig::default()
            .max_frame_size(Some(MAX_WEBSOCKET_FRAME_BYTES))
            .max_message_size(Some(MAX_WEBSOCKET_MESSAGE_BYTES));
        let host = parsed
            .host_str()
            .context("browser WebSocket URL is missing a host")?;
        let port = parsed
            .port_or_known_default()
            .context("browser WebSocket URL is missing a port")?;
        let addresses = (host, port)
            .to_socket_addrs()
            .with_context(|| format!("resolving browser WebSocket host {host}"))?
            .collect::<Vec<_>>();
        if addresses.is_empty() {
            bail!("browser WebSocket host resolved to no addresses");
        }
        let mut last_error = None;
        let mut stream = None;
        for address in addresses {
            let remaining = remaining(deadline, "connecting browser WebSocket")?;
            match TcpStream::connect_timeout(&address, remaining) {
                Ok(connected) => {
                    stream = Some(connected);
                    break;
                }
                Err(error) => last_error = Some(error),
            }
        }
        let stream = stream.ok_or_else(|| {
            anyhow!(last_error.unwrap_or_else(|| io::Error::new(
                io::ErrorKind::TimedOut,
                "WebSocket connect deadline elapsed"
            )))
        })?;
        remaining(deadline, "performing browser WebSocket handshake")?;
        stream.set_nonblocking(true)?;
        let request = ws_url
            .into_client_request()
            .context("building WebSocket request")?;
        let mut handshake = tungstenite::client::client_with_config(
            request,
            MaybeTlsStream::Plain(stream),
            Some(config),
        );
        let (mut socket, _) = loop {
            match handshake {
                Ok(connected) => break connected,
                Err(HandshakeError::Interrupted(mid)) => {
                    let wait = remaining(deadline, "performing browser WebSocket handshake")?
                        .min(Duration::from_millis(1));
                    std::thread::sleep(wait);
                    handshake = mid.handshake();
                }
                Err(HandshakeError::Failure(error)) => {
                    return Err(error)
                        .with_context(|| format!("connecting to browser WebSocket at {ws_url}"));
                }
            }
        };
        if let MaybeTlsStream::Plain(stream) = socket.get_mut() {
            stream.set_nonblocking(false)?;
        }
        Ok(Self {
            transport: Box::new(WebSocketTransport { socket }),
            next_id: 1,
            events: VecDeque::new(),
            timeout: DEFAULT_TIMEOUT,
        })
    }

    /// Select the authenticated local broker for managed state, retaining the
    /// hardened loopback WebSocket transport only for legacy/external state.
    pub fn connect_state_until(
        state: &crate::state::SessionState,
        deadline: Deadline,
    ) -> Result<Self> {
        if state.endpoint.is_some() {
            let stream = crate::broker::connect(state, deadline)?;
            return Ok(Self {
                transport: Box::new(BrokerTransport { stream }),
                next_id: 1,
                events: VecDeque::new(),
                timeout: DEFAULT_TIMEOUT,
            });
        }
        Self::connect_until(&state.ws_url, deadline)
    }

    #[cfg(unix)]
    pub fn from_pipe_fds(read_fd: std::os::fd::OwnedFd, write_fd: std::os::fd::OwnedFd) -> Self {
        Self {
            transport: Box::new(PipeTransport::from_owned_fds(read_fd, write_fd)),
            next_id: 1,
            events: VecDeque::new(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    #[cfg(unix)]
    #[allow(dead_code)]
    pub unsafe fn from_inherited_pipe_fds() -> Self {
        use std::os::fd::FromRawFd;
        // This constructor is for a parent-side process whose fd3 receives
        // browser output and fd4 sends browser input (tests/helpers may arrange
        // that shape explicitly; Chromium itself receives the inverse ends).
        Self::from_pipe_fds(unsafe { std::os::fd::OwnedFd::from_raw_fd(3) }, unsafe {
            std::os::fd::OwnedFd::from_raw_fd(4)
        })
    }

    /// Override the per-call response timeout.
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Send a CDP command and wait for its response result.
    /// `session_id: None` targets the browser; `Some` targets a page
    /// session (flat protocol). CDP error responses become Err with the
    /// method name and remote message. Events received while waiting
    /// are buffered for `next_event`.
    #[cfg(test)]
    pub fn call(&mut self, session_id: Option<&str>, method: &str, params: Value) -> Result<Value> {
        self.call_until(session_id, method, params, Deadline::after(self.timeout))
    }

    /// Send a CDP command and wait for its response within an absolute deadline.
    pub fn call_until(
        &mut self,
        session_id: Option<&str>,
        method: &str,
        params: Value,
        deadline: Deadline,
    ) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;

        let mut request = json!({
            "id": id,
            "method": method,
            "params": params,
        });
        if let Some(session_id) = session_id {
            request["sessionId"] = Value::String(session_id.to_string());
        }

        self.transport
            .send_json(&request, deadline)
            .with_context(|| format!("sending CDP command {method}"))?;

        loop {
            let value = match self.transport.recv_json(deadline) {
                Ok(msg) => msg,
                Err(err) if is_timeout_error(&err) => {
                    bail!("timed out waiting for {method} before command deadline")
                }
                Err(err) => {
                    return Err(err).with_context(|| format!("reading response for {method}"));
                }
            };
            if value.get("method").is_some() {
                if self.events.len() >= MAX_QUEUED_EVENTS {
                    bail!("CDP event queue exceeds {MAX_QUEUED_EVENTS} events");
                }
                self.events.push_back(event_from_value(value)?);
                continue;
            }
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                bail!("{method}: CDP error {code}: {message}");
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Attach to a target with `flatten: true`; returns the sessionId.
    #[cfg(test)]
    pub fn attach_to_target(&mut self, target_id: &str) -> Result<String> {
        self.attach_to_target_until(target_id, Deadline::after(self.timeout))
    }

    pub fn attach_to_target_until(
        &mut self,
        target_id: &str,
        deadline: Deadline,
    ) -> Result<String> {
        let result = self.call_until(
            None,
            "Target.attachToTarget",
            json!({"targetId": target_id, "flatten": true}),
            deadline,
        )?;
        result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .context("Target.attachToTarget response missing sessionId")
    }

    /// Return the next event (buffered or read from the socket),
    /// Ok(None) once `timeout` elapses with no event.
    pub fn next_event(&mut self, timeout: Duration) -> Result<Option<Event>> {
        self.next_event_until(Deadline::after(timeout))
    }

    /// Return the next event before an absolute deadline. This lets callers
    /// share one timeout budget across command calls and event polling.
    pub fn next_event_until(&mut self, deadline: Deadline) -> Result<Option<Event>> {
        if deadline.expired() {
            return Ok(None);
        }
        if let Some(event) = self.events.pop_front() {
            if deadline.expired() {
                self.events.push_front(event);
                return Ok(None);
            }
            return Ok(Some(event));
        }
        loop {
            let value = match self.transport.recv_json(deadline) {
                Ok(msg) => msg,
                Err(err) if is_timeout_error(&err) => return Ok(None),
                Err(err) => return Err(err).context("reading next CDP event"),
            };
            if value.get("method").is_some() {
                return Ok(Some(event_from_value(value)?));
            }
        }
    }

    /// Return one already-buffered event without reading the socket.
    pub fn next_buffered_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }
}

struct BrokerTransport {
    stream: std::os::unix::net::UnixStream,
}

impl CdpTransport for BrokerTransport {
    fn send_json(&mut self, value: &Value, deadline: Deadline) -> Result<()> {
        crate::broker::protocol::write_frame_until(
            &mut self.stream,
            &crate::broker::protocol::BrokerMessage::Cdp {
                message: value.clone(),
            },
            deadline,
        )
    }

    fn recv_json(&mut self, deadline: Deadline) -> Result<Value> {
        let message = crate::broker::protocol::read_frame_until(&mut self.stream, deadline)
            .map_err(|error| {
                if format!("{error:#}").contains("timed out") {
                    anyhow!(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "broker read deadline elapsed"
                    ))
                } else {
                    error
                }
            })?;
        match message {
            crate::broker::protocol::BrokerMessage::Cdp { message } => Ok(message),
            crate::broker::protocol::BrokerMessage::Error { message } => {
                bail!("broker rejected CDP message: {message}")
            }
            other => bail!("unexpected broker message while reading CDP: {other:?}"),
        }
    }

    fn close(&mut self) -> Result<()> {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        Ok(())
    }
}

impl Drop for CdpClient {
    fn drop(&mut self) {
        let _ = self.transport.close();
    }
}

struct WebSocketTransport {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
}

impl WebSocketTransport {
    fn read_with_deadline(&mut self, deadline: Deadline) -> Result<Message> {
        let remaining = remaining(deadline, "reading WebSocket message")?;
        self.set_read_timeout(Some(remaining))?;
        self.set_write_timeout(Some(remaining))?;
        self.socket.read().map_err(Into::into)
    }

    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        match self.socket.get_mut() {
            MaybeTlsStream::Plain(stream) => stream.set_read_timeout(timeout),
            _ => Ok(()),
        }
    }

    fn set_write_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        match self.socket.get_mut() {
            MaybeTlsStream::Plain(stream) => stream.set_write_timeout(timeout),
            _ => Ok(()),
        }
    }

    fn set_nonblocking(&mut self, nonblocking: bool) -> io::Result<()> {
        match self.socket.get_mut() {
            MaybeTlsStream::Plain(stream) => stream.set_nonblocking(nonblocking),
            _ => Ok(()),
        }
    }

    fn message_to_json(&mut self, msg: Message, deadline: Deadline) -> Result<Option<Value>> {
        match msg {
            Message::Text(text) => Ok(Some(
                serde_json::from_str(&text).context("parsing CDP JSON frame")?,
            )),
            Message::Ping(_) => {
                self.set_write_timeout(Some(remaining(deadline, "replying to WebSocket ping")?))?;
                let _ = self.socket.flush();
                Ok(None)
            }
            Message::Pong(_) | Message::Frame(_) | Message::Binary(_) => Ok(None),
            Message::Close(_) => bail!("connection closed by browser"),
        }
    }
}

impl CdpTransport for WebSocketTransport {
    fn send_json(&mut self, value: &Value, deadline: Deadline) -> Result<()> {
        remaining(deadline, "sending CDP command")?;
        self.set_nonblocking(true)?;
        let send_result = self.socket.send(Message::Text(value.to_string().into()));
        let send_result = match send_result {
            Ok(()) => Ok(()),
            Err(tungstenite::Error::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {
                loop {
                    remaining(deadline, "sending CDP command")?;
                    match self.socket.flush() {
                        Ok(()) => break Ok(()),
                        Err(tungstenite::Error::Io(error))
                            if error.kind() == io::ErrorKind::WouldBlock =>
                        {
                            deadline.sleep(Duration::from_millis(1));
                        }
                        Err(error) => break Err(error),
                    }
                }
            }
            Err(error) => Err(error),
        };
        self.set_nonblocking(false)?;
        send_result.map_err(Into::into)
    }

    fn recv_json(&mut self, deadline: Deadline) -> Result<Value> {
        loop {
            let msg = self.read_with_deadline(deadline)?;
            if let Some(value) = self.message_to_json(msg, deadline)? {
                return Ok(value);
            }
        }
    }

    fn close(&mut self) -> Result<()> {
        let _ = self.socket.close(None);
        Ok(())
    }
}

#[cfg(unix)]
struct PipeTransport {
    read: std::fs::File,
    write: std::fs::File,
    buf: Vec<u8>,
}

#[cfg(unix)]
impl PipeTransport {
    fn from_owned_fds(read_fd: std::os::fd::OwnedFd, write_fd: std::os::fd::OwnedFd) -> Self {
        Self {
            read: std::fs::File::from(read_fd),
            write: std::fs::File::from(write_fd),
            buf: Vec::new(),
        }
    }

    fn wait(fd: std::os::fd::RawFd, events: libc::c_short, deadline: Deadline) -> io::Result<()> {
        let Some(remaining) = deadline
            .remaining()
            .filter(|remaining| !remaining.is_zero())
        else {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "deadline elapsed"));
        };
        let timeout = remaining.as_millis().min(i32::MAX as u128) as i32;
        let mut pfd = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout) };
        if rc == 0 {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "deadline elapsed"));
        }
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        if pfd.revents & libc::POLLNVAL != 0 {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "pipe fd invalid"));
        }
        Ok(())
    }
}

#[cfg(unix)]
impl CdpTransport for PipeTransport {
    fn send_json(&mut self, value: &Value, deadline: Deadline) -> Result<()> {
        use std::os::fd::AsRawFd;
        let mut bytes = value.to_string().into_bytes();
        bytes.push(0);
        let mut written = 0;
        while written < bytes.len() {
            Self::wait(self.write.as_raw_fd(), libc::POLLOUT, deadline)?;
            match self.write.write(&bytes[written..]) {
                Ok(0) => bail!("pipe write returned zero bytes"),
                Ok(n) => written += n,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) =>
                {
                    continue;
                }
                Err(e) => return Err(e).context("writing CDP pipe message"),
            }
        }
        self.write.flush().context("flushing CDP pipe message")
    }

    fn recv_json(&mut self, deadline: Deadline) -> Result<Value> {
        use std::os::fd::AsRawFd;
        loop {
            if let Some(pos) = self.buf.iter().position(|b| *b == 0) {
                let frame: Vec<u8> = self.buf.drain(..=pos).take(pos).collect();
                return serde_json::from_slice(&frame).context("parsing CDP pipe JSON message");
            }
            if self.buf.len() >= MAX_PIPE_MESSAGE_BYTES {
                bail!("CDP pipe message exceeds {MAX_PIPE_MESSAGE_BYTES} bytes");
            }
            Self::wait(self.read.as_raw_fd(), libc::POLLIN, deadline)?;
            let mut chunk = [0u8; 8192];
            match self.read.read(&mut chunk) {
                Ok(0) => bail!("CDP pipe closed by browser"),
                Ok(n) => {
                    if self.buf.len() + n > MAX_PIPE_MESSAGE_BYTES {
                        bail!("CDP pipe message exceeds {MAX_PIPE_MESSAGE_BYTES} bytes");
                    }
                    self.buf.extend_from_slice(&chunk[..n]);
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) =>
                {
                    continue;
                }
                Err(e) => return Err(e).context("reading CDP pipe message"),
            }
        }
    }

    fn close(&mut self) -> Result<()> {
        Ok(())
    }
}

fn remaining(deadline: Deadline, operation: &str) -> Result<Duration> {
    deadline
        .remaining()
        .filter(|remaining| !remaining.is_zero())
        .with_context(|| format!("deadline elapsed while {operation}"))
}

fn event_from_value(value: Value) -> Result<Event> {
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .context("CDP event missing method")?
        .to_string();
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    let session_id = value
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(Event {
        method,
        params,
        session_id,
    })
}

fn is_timeout_error(err: &anyhow::Error) -> bool {
    if let Some(tungstenite::Error::Io(io_err)) = err.downcast_ref::<tungstenite::Error>() {
        matches!(
            io_err.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        )
    } else if let Some(io_err) = err.downcast_ref::<io::Error>() {
        matches!(
            io_err.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        )
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread::{self, JoinHandle};
    use std::time::Instant;

    fn serve<F>(script: F) -> (String, JoinHandle<()>)
    where
        F: FnOnce(WebSocket<TcpStream>) + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (ready_tx, ready_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            ready_tx.send(()).unwrap();
            let (stream, _) = listener.accept().unwrap();
            let socket = tungstenite::accept(stream).unwrap();
            script(socket);
        });
        ready_rx.recv().unwrap();
        (format!("ws://127.0.0.1:{port}"), handle)
    }

    fn read_json(socket: &mut WebSocket<TcpStream>) -> Value {
        match socket.read().unwrap() {
            Message::Text(text) => serde_json::from_str(&text).unwrap(),
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn call_returns_result() {
        let (url, handle) = serve(|mut socket| {
            let request = read_json(&mut socket);
            assert_eq!(request["id"], 1);
            assert_eq!(request["method"], "Test.ok");
            socket
                .send(Message::Text(r#"{"id":1,"result":{"ok":true}}"#.into()))
                .unwrap();
            let _ = socket.close(None);
        });
        let mut client = CdpClient::connect(&url).unwrap();
        assert_eq!(
            client.call(None, "Test.ok", json!({})).unwrap(),
            json!({"ok": true})
        );
        handle.join().unwrap();
    }

    #[test]
    fn event_interleaved_before_response_is_buffered() {
        let (url, handle) = serve(|mut socket| {
            let _ = read_json(&mut socket);
            socket
                .send(Message::Text(
                    r#"{"method":"Page.loadEventFired","params":{"ts":1},"sessionId":"s"}"#.into(),
                ))
                .unwrap();
            socket
                .send(Message::Text(r#"{"id":1,"result":{"done":true}}"#.into()))
                .unwrap();
            let _ = socket.close(None);
        });
        let mut client = CdpClient::connect(&url).unwrap();
        assert_eq!(
            client.call(None, "Page.navigate", json!({})).unwrap(),
            json!({"done": true})
        );
        let event = client
            .next_event(Duration::from_millis(50))
            .unwrap()
            .unwrap();
        assert_eq!(event.method, "Page.loadEventFired");
        assert_eq!(event.params, json!({"ts": 1}));
        assert_eq!(event.session_id.as_deref(), Some("s"));
        handle.join().unwrap();
    }

    #[test]
    fn runtime_evaluate_buffers_network_event_for_followup_drain() {
        let (url, handle) = serve(|mut socket| {
            let _ = read_json(&mut socket);
            socket
                .send(Message::Text(
                    r#"{"method":"Network.requestWillBeSent","params":{"requestId":"late"},"sessionId":"s"}"#.into(),
                ))
                .unwrap();
            socket
                .send(Message::Text(r#"{"id":1,"result":{"result":{"value":{"active":0,"seq":0,"readyState":"complete"}}}}"#.into()))
                .unwrap();
            let _ = socket.close(None);
        });
        let mut client = CdpClient::connect(&url).unwrap();
        let _ = client
            .call(Some("s"), "Runtime.evaluate", json!({}))
            .unwrap();
        let event = client.next_buffered_event().unwrap();
        assert_eq!(event.method, "Network.requestWillBeSent");
        assert_eq!(event.session_id.as_deref(), Some("s"));
        assert_eq!(event.params["requestId"], "late");
        handle.join().unwrap();
    }

    #[test]
    fn next_event_until_does_not_return_buffered_event_after_deadline() {
        let (url, handle) = serve(|mut socket| {
            let _ = read_json(&mut socket);
            socket
                .send(Message::Text(
                    r#"{"method":"Page.loadEventFired","params":{},"sessionId":"s"}"#.into(),
                ))
                .unwrap();
            socket
                .send(Message::Text(r#"{"id":1,"result":{}}"#.into()))
                .unwrap();
            thread::sleep(Duration::from_millis(200));
        });
        let mut client = CdpClient::connect(&url).unwrap();
        client.call(None, "Page.navigate", json!({})).unwrap();
        assert!(
            client
                .next_event_until(Deadline::at(Instant::now()))
                .unwrap()
                .is_none()
        );
        assert!(
            client
                .next_event(Duration::from_millis(50))
                .unwrap()
                .is_some()
        );
        handle.join().unwrap();
    }

    #[test]
    fn call_until_uses_absolute_deadline() {
        let (url, handle) = serve(|mut socket| {
            let _ = read_json(&mut socket);
            thread::sleep(Duration::from_millis(200));
        });
        let mut client = CdpClient::connect(&url).unwrap();
        let err = client
            .call_until(
                None,
                "Slow.absolute",
                json!({}),
                Deadline::after(Duration::from_millis(50)),
            )
            .unwrap_err();
        assert!(format!("{err}").contains("deadline"));
        handle.join().unwrap();
    }

    #[test]
    fn websocket_handshake_respects_absolute_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(200));
        });
        let started = Instant::now();
        let result = CdpClient::connect_until(
            &format!("ws://127.0.0.1:{port}"),
            Deadline::at(started + Duration::from_millis(60)),
        );
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_millis(250));
        handle.join().unwrap();
    }

    #[test]
    fn nested_calls_share_budget_without_resetting_it() {
        let (url, handle) = serve(|mut socket| {
            let first = read_json(&mut socket);
            thread::sleep(Duration::from_millis(1));
            socket
                .send(Message::Text(
                    format!(r#"{{"id":{},"result":{{}}}}"#, first["id"]).into(),
                ))
                .unwrap();
            let _second = read_json(&mut socket);
            thread::sleep(Duration::from_millis(100));
        });
        let mut client = CdpClient::connect(&url).unwrap();
        let started = Instant::now();
        let deadline = Deadline::at(started + Duration::from_millis(85));
        client
            .call_until(None, "First", json!({}), deadline)
            .unwrap();
        assert!(
            client
                .call_until(None, "Second", json!({}), deadline)
                .is_err()
        );
        assert!(started.elapsed() >= Duration::from_millis(50));
        assert!(started.elapsed() < Duration::from_millis(250));
        handle.join().unwrap();
    }

    #[test]
    fn cdp_error_response_mentions_method_and_message() {
        let (url, handle) = serve(|mut socket| {
            let _ = read_json(&mut socket);
            socket
                .send(Message::Text(
                    r#"{"id":1,"error":{"code":-32000,"message":"remote nope"}}"#.into(),
                ))
                .unwrap();
            let _ = socket.close(None);
        });
        let mut client = CdpClient::connect(&url).unwrap();
        let err = client
            .call(None, "Runtime.evaluate", json!({}))
            .unwrap_err();
        let text = format!("{err}");
        assert!(text.contains("Runtime.evaluate"));
        assert!(text.contains("remote nope"));
        handle.join().unwrap();
    }

    #[test]
    fn session_id_routing_and_ids_increment() {
        let (url, handle) = serve(|mut socket| {
            let first = read_json(&mut socket);
            assert_eq!(first["id"], 1);
            assert_eq!(first["sessionId"], "sess-1");
            socket
                .send(Message::Text(r#"{"id":1,"result":{}}"#.into()))
                .unwrap();
            let second = read_json(&mut socket);
            assert_eq!(second["id"], 2);
            socket
                .send(Message::Text(r#"{"id":2,"result":{"two":true}}"#.into()))
                .unwrap();
            let _ = socket.close(None);
        });
        let mut client = CdpClient::connect(&url).unwrap();
        client
            .call(Some("sess-1"), "Runtime.evaluate", json!({}))
            .unwrap();
        assert_eq!(
            client.call(None, "Browser.getVersion", json!({})).unwrap(),
            json!({"two": true})
        );
        handle.join().unwrap();
    }

    #[test]
    fn next_event_times_out() {
        let (url, handle) = serve(|_socket| {
            thread::sleep(Duration::from_millis(500));
        });
        let mut client = CdpClient::connect(&url).unwrap();
        let start = Instant::now();
        assert!(
            client
                .next_event(Duration::from_millis(200))
                .unwrap()
                .is_none()
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        handle.join().unwrap();
    }

    #[test]
    fn attach_to_target_returns_session_id() {
        let (url, handle) = serve(|mut socket| {
            let request = read_json(&mut socket);
            assert_eq!(request["method"], "Target.attachToTarget");
            assert_eq!(
                request["params"],
                json!({"targetId": "target-1", "flatten": true})
            );
            socket
                .send(Message::Text(
                    r#"{"id":1,"result":{"sessionId":"abc"}}"#.into(),
                ))
                .unwrap();
            let _ = socket.close(None);
        });
        let mut client = CdpClient::connect(&url).unwrap();
        assert_eq!(client.attach_to_target("target-1").unwrap(), "abc");
        handle.join().unwrap();
    }

    #[test]
    fn call_times_out() {
        let (url, handle) = serve(|mut socket| {
            let _ = read_json(&mut socket);
            thread::sleep(Duration::from_millis(700));
        });
        let mut client = CdpClient::connect(&url).unwrap();
        client.set_timeout(Duration::from_millis(300));
        let err = client.call(None, "Slow.method", json!({})).unwrap_err();
        assert!(format!("{err}").contains("timed out"));
        handle.join().unwrap();
    }

    #[test]
    fn debugger_url_is_bound_to_approved_authority() {
        validate_debugger_url("ws://localhost:9222/devtools/browser/1", "127.0.0.1", 9222).unwrap();
        assert!(validate_debugger_url("ws://evil.example:9222/x", "127.0.0.1", 9222).is_err());
        assert!(validate_debugger_url("ws://127.0.0.1:9333/x", "127.0.0.1", 9222).is_err());
        assert!(validate_debugger_url("wss://127.0.0.1:9222/x", "127.0.0.1", 9222).is_err());
        assert!(CdpClient::connect("ws://192.0.2.1:9222/x").is_err());
    }

    #[test]
    fn event_queue_is_bounded() {
        let (url, handle) = serve(|mut socket| {
            let _ = read_json(&mut socket);
            for index in 0..=MAX_QUEUED_EVENTS {
                socket
                    .send(Message::Text(
                        format!(r#"{{"method":"Test.event","params":{{"index":{index}}}}}"#).into(),
                    ))
                    .unwrap();
            }
        });
        let mut client = CdpClient::connect(&url).unwrap();
        let error = client.call(None, "Never.responds", json!({})).unwrap_err();
        assert!(format!("{error:#}").contains("event queue"));
        handle.join().unwrap();
    }

    #[test]
    fn oversized_websocket_message_is_rejected() {
        let (url, handle) = serve(|mut socket| {
            let _ = read_json(&mut socket);
            let oversized = "x".repeat(MAX_WEBSOCKET_MESSAGE_BYTES + 1);
            let _ = socket.send(Message::Text(oversized.into()));
        });
        let mut client = CdpClient::connect(&url).unwrap();
        let error = client.call(None, "Never.responds", json!({})).unwrap_err();
        assert!(format!("{error:#}").contains("Message too long"));
        drop(client);
        handle.join().unwrap();
    }

    #[cfg(unix)]
    fn pipe_client_and_browser() -> (
        CdpClient,
        std::os::unix::net::UnixStream,
        std::os::unix::net::UnixStream,
    ) {
        use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
        use std::os::unix::net::UnixStream;
        let (browser_write, client_read) = UnixStream::pair().unwrap();
        let (client_write, browser_read) = UnixStream::pair().unwrap();
        client_read.set_nonblocking(false).unwrap();
        client_write.set_nonblocking(false).unwrap();
        let client = CdpClient::from_pipe_fds(
            unsafe { OwnedFd::from_raw_fd(client_read.into_raw_fd()) },
            unsafe { OwnedFd::from_raw_fd(client_write.into_raw_fd()) },
        );
        (client, browser_read, browser_write)
    }

    #[cfg(unix)]
    fn read_pipe_json(read: &mut std::os::unix::net::UnixStream) -> Value {
        let mut buf = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            read.read_exact(&mut byte).unwrap();
            if byte[0] == 0 {
                break;
            }
            buf.push(byte[0]);
        }
        serde_json::from_slice(&buf).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn pipe_call_uses_nul_delimited_json() {
        let (mut client, mut browser_read, mut browser_write) = pipe_client_and_browser();
        let handle = thread::spawn(move || {
            let request = read_pipe_json(&mut browser_read);
            assert_eq!(request["method"], "Browser.getVersion");
            browser_write
                .write_all(br#"{"id":1,"result":{"product":"Fake/1"}}"#)
                .unwrap();
            browser_write.write_all(&[0]).unwrap();
        });
        assert_eq!(
            client.call(None, "Browser.getVersion", json!({})).unwrap(),
            json!({"product":"Fake/1"})
        );
        handle.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn pipe_handles_partial_and_coalesced_reads() {
        let (mut client, mut browser_read, mut browser_write) = pipe_client_and_browser();
        let handle = thread::spawn(move || {
            let _ = read_pipe_json(&mut browser_read);
            browser_write.write_all(br#"{"method":"Target.targetCreated","params":{"targetInfo":{"targetId":"t"}}}"#).unwrap();
            browser_write.write_all(&[0]).unwrap();
            browser_write
                .write_all(br#"{"id":1,"result":{"targetInfos":[]}}"#)
                .unwrap();
            browser_write.write_all(&[0]).unwrap();
        });
        assert_eq!(
            client.call(None, "Target.getTargets", json!({})).unwrap(),
            json!({"targetInfos":[]})
        );
        assert_eq!(
            client.next_buffered_event().unwrap().method,
            "Target.targetCreated"
        );
        handle.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn pipe_eof_and_timeout_are_reported() {
        let (mut client, mut browser_read, browser_write) = pipe_client_and_browser();
        drop(browser_write);
        let _ = thread::spawn(move || {
            let _ = read_pipe_json(&mut browser_read);
        });
        let err = client.call(None, "Browser.close", json!({})).unwrap_err();
        assert!(format!("{err:#}").contains("closed"));
    }

    #[cfg(all(test, unix, any(target_os = "macos", target_os = "linux")))]
    #[test]
    #[ignore = "requires Chrome/Chromium in PATH and launches a real browser with --remote-debugging-pipe"]
    fn real_chrome_remote_debugging_pipe_smoke() {
        use std::os::fd::{FromRawFd, OwnedFd};
        use std::os::unix::process::CommandExt;
        use std::process::{Child, Command, Stdio};
        let chrome = std::env::var("RDNY_CHROME").unwrap_or_else(|_| "chromium".to_string());

        fn wait_exited(child: &mut Child) {
            let deadline = Deadline::after(Duration::from_secs(10));
            loop {
                if child.try_wait().unwrap().is_some() {
                    return;
                }
                assert!(!deadline.expired(), "Chrome did not exit after pipe close");
                deadline.sleep(Duration::from_millis(50));
            }
        }

        let launch = || {
            let temp = tempfile::tempdir().unwrap();
            let mut to_chrome = [0; 2];
            let mut from_chrome = [0; 2];
            unsafe {
                assert_eq!(libc::pipe(to_chrome.as_mut_ptr()), 0);
                assert_eq!(libc::pipe(from_chrome.as_mut_ptr()), 0);
            }
            let mut command = Command::new(&chrome);
            command
                .arg("--headless=new")
                .arg("--remote-debugging-pipe")
                .arg(format!("--user-data-dir={}", temp.path().display()))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            unsafe {
                command.pre_exec(move || {
                    if libc::dup2(to_chrome[0], 3) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::dup2(from_chrome[1], 4) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let child = command.spawn().unwrap();
            unsafe {
                libc::close(to_chrome[0]);
                libc::close(from_chrome[1]);
            }
            let mut client =
                CdpClient::from_pipe_fds(unsafe { OwnedFd::from_raw_fd(from_chrome[0]) }, unsafe {
                    OwnedFd::from_raw_fd(to_chrome[1])
                });
            client.set_timeout(Duration::from_secs(10));
            (temp, child, client)
        };

        let (_temp, mut child, mut client) = launch();
        client.set_timeout(Duration::from_secs(10));
        let version = client.call(None, "Browser.getVersion", json!({})).unwrap();
        assert!(version.get("product").is_some());
        let targets = client.call(None, "Target.getTargets", json!({})).unwrap();
        assert!(targets.get("targetInfos").is_some());
        let created = client
            .call(None, "Target.createTarget", json!({"url":"about:blank"}))
            .unwrap();
        assert!(created.get("targetId").is_some());
        let _ = client.call(None, "Browser.close", json!({})).unwrap();
        drop(client);
        wait_exited(&mut child);

        let (_temp, mut child, client) = launch();
        drop(client);
        wait_exited(&mut child);
    }
}
