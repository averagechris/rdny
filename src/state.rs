//! Session state persistence.
//!
//! `rdny start`/`rdny connect` write a state file; every other command
//! loads it to find the running browser. Location:
//! `$RDNY_STATE_DIR` override > `$XDG_STATE_HOME/rdny` >
//! macOS `~/Library/Application Support/rdny` > `~/.local/state/rdny`.

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Persisted session record (state.json in the state dir).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    /// Browser-level WebSocket debugger URL from /json/version.
    pub ws_url: String,
    pub host: String,
    pub port: u16,
    /// PID of the browser we launched; None for `rdny connect` sessions.
    pub pid: Option<u32>,
    /// user-data-dir we created for launched sessions.
    pub user_data_dir: Option<PathBuf>,
    /// Browser binary used for launched sessions.
    pub browser_path: Option<PathBuf>,
    /// Current page target id (set at start, updated by `rdny page`).
    pub target_id: Option<String>,
}

/// Resolve the rdny state directory (created if missing).
pub fn state_dir() -> Result<PathBuf> {
    anyhow::bail!("unimplemented: state_dir")
}

/// Load the session state, Ok(None) when no state file exists.
pub fn load() -> Result<Option<SessionState>> {
    anyhow::bail!("unimplemented: state::load")
}

/// Persist the session state atomically (write temp + rename).
pub fn save(_state: &SessionState) -> Result<()> {
    anyhow::bail!("unimplemented: state::save")
}

/// Remove the state file if present.
pub fn clear() -> Result<()> {
    anyhow::bail!("unimplemented: state::clear")
}

/// Load state or fail with an actionable hint (no session -> tell the
/// user to run `rdny start`).
pub fn require() -> Result<SessionState> {
    anyhow::bail!("unimplemented: state::require")
}
