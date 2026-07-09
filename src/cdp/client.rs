//! Blocking CDP WebSocket client (flat session protocol).

use std::collections::VecDeque;
use std::io;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

/// Default per-call response timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

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
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    events: VecDeque<Event>,
    timeout: Duration,
}

impl CdpClient {
    /// Connect to a ws:// debugger URL (browser-level endpoint).
    pub fn connect(ws_url: &str) -> Result<Self> {
        let (socket, _) = tungstenite::connect(ws_url)
            .with_context(|| format!("connecting to browser WebSocket at {ws_url}"))?;
        match socket.get_ref() {
            MaybeTlsStream::Plain(_) => Ok(Self {
                socket,
                next_id: 1,
                events: VecDeque::new(),
                timeout: DEFAULT_TIMEOUT,
            }),
            _ => bail!("CDP client supports ws:// only"),
        }
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
    pub fn call(&mut self, session_id: Option<&str>, method: &str, params: Value) -> Result<Value> {
        self.call_until(session_id, method, params, Instant::now() + self.timeout)
    }

    /// Send a CDP command and wait for its response within an absolute deadline.
    pub fn call_until(
        &mut self,
        session_id: Option<&str>,
        method: &str,
        params: Value,
        deadline: Instant,
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

        self.socket
            .send(Message::Text(request.to_string().into()))
            .with_context(|| format!("sending CDP command {method}"))?;

        loop {
            let msg = match self.read_with_deadline(deadline) {
                Ok(msg) => msg,
                Err(err) if is_timeout_error(&err) => {
                    bail!("timed out waiting for {method} before command deadline")
                }
                Err(err) => {
                    return Err(err).with_context(|| format!("reading response for {method}"));
                }
            };
            let Some(value) = self.message_to_json(msg)? else {
                continue;
            };
            if value.get("method").is_some() {
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
    pub fn attach_to_target(&mut self, target_id: &str) -> Result<String> {
        let result = self.call(
            None,
            "Target.attachToTarget",
            json!({"targetId": target_id, "flatten": true}),
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
        self.next_event_until(Instant::now() + timeout)
    }

    /// Return the next event before an absolute deadline. This lets callers
    /// share one timeout budget across command calls and event polling.
    pub fn next_event_until(&mut self, deadline: Instant) -> Result<Option<Event>> {
        if Instant::now() >= deadline {
            return Ok(None);
        }
        if let Some(event) = self.events.pop_front() {
            if Instant::now() >= deadline {
                self.events.push_front(event);
                return Ok(None);
            }
            return Ok(Some(event));
        }
        loop {
            let msg = match self.read_with_deadline(deadline) {
                Ok(msg) => msg,
                Err(err) if is_timeout_error(&err) => return Ok(None),
                Err(err) => return Err(err).context("reading next CDP event"),
            };
            let Some(value) = self.message_to_json(msg)? else {
                continue;
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

    fn read_with_deadline(&mut self, deadline: Instant) -> Result<Message> {
        let now = Instant::now();
        if now >= deadline {
            return Err(anyhow!(io::Error::new(
                io::ErrorKind::TimedOut,
                "deadline elapsed"
            )));
        }
        let remaining = deadline - now;
        self.set_read_timeout(Some(remaining))?;
        self.socket.read().map_err(Into::into)
    }

    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        match self.socket.get_mut() {
            MaybeTlsStream::Plain(stream) => stream.set_read_timeout(timeout),
            _ => Ok(()),
        }
    }

    fn message_to_json(&mut self, msg: Message) -> Result<Option<Value>> {
        match msg {
            Message::Text(text) => Ok(Some(
                serde_json::from_str(&text).context("parsing CDP JSON frame")?,
            )),
            Message::Ping(_) => {
                let _ = self.socket.flush();
                Ok(None)
            }
            Message::Pong(_) | Message::Frame(_) | Message::Binary(_) => Ok(None),
            Message::Close(_) => bail!("connection closed by browser"),
        }
    }
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
        assert!(client.next_event_until(Instant::now()).unwrap().is_none());
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
                Instant::now() + Duration::from_millis(50),
            )
            .unwrap_err();
        assert!(format!("{err}").contains("deadline"));
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
}
