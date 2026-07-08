//! Blocking CDP WebSocket client (flat session protocol).

use std::time::Duration;

use anyhow::Result;
use serde_json::Value;

/// Default per-call response timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// A CDP event received out-of-band.
#[derive(Debug, Clone)]
pub struct Event {
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

/// Blocking connection to a browser WebSocket debugger URL.
pub struct CdpClient {
    _private: (),
}

impl CdpClient {
    /// Connect to a ws:// debugger URL (browser-level endpoint).
    pub fn connect(_ws_url: &str) -> Result<Self> {
        anyhow::bail!("unimplemented: CdpClient::connect")
    }

    /// Override the per-call response timeout.
    pub fn set_timeout(&mut self, _timeout: Duration) {}

    /// Send a CDP command and wait for its response result.
    /// `session_id: None` targets the browser; `Some` targets a page
    /// session (flat protocol). CDP error responses become Err with the
    /// method name and remote message. Events received while waiting
    /// are buffered for `next_event`.
    pub fn call(
        &mut self,
        _session_id: Option<&str>,
        _method: &str,
        _params: Value,
    ) -> Result<Value> {
        anyhow::bail!("unimplemented: CdpClient::call")
    }

    /// Attach to a target with `flatten: true`; returns the sessionId.
    pub fn attach_to_target(&mut self, _target_id: &str) -> Result<String> {
        anyhow::bail!("unimplemented: CdpClient::attach_to_target")
    }

    /// Return the next event (buffered or read from the socket),
    /// Ok(None) once `timeout` elapses with no event.
    pub fn next_event(&mut self, _timeout: Duration) -> Result<Option<Event>> {
        anyhow::bail!("unimplemented: CdpClient::next_event")
    }
}
