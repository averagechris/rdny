//! Browser discovery, launch, and lifecycle (start/connect/stop/status).

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::state::SessionState;

/// Options for `rdny start`.
#[derive(Debug, Clone, Default)]
pub struct LaunchOpts {
    /// Run with a visible window instead of headless.
    pub show: bool,
    /// Ignore TLS certificate errors.
    pub insecure: bool,
    /// Extra Chrome args (from RDNY_CHROME_ARGS), already split.
    pub extra_args: Vec<String>,
}

/// Discover a browser binary: RDNY_CHROME env var, then well-known
/// locations, else an actionable hint error.
pub fn discover() -> Result<PathBuf> {
    anyhow::bail!("unimplemented: browser::discover")
}

/// Launch the browser with a remote debugging port and verify it is
/// reachable (probe /json/version). `data_root` is the rdny state dir
/// (from `state::state_dir()`); the profile dir and chrome.log live
/// under it. Returns the session to persist.
pub fn launch(_opts: &LaunchOpts, _data_root: &Path) -> Result<SessionState> {
    anyhow::bail!("unimplemented: browser::launch")
}

/// Attach to an already-running browser at host:port.
pub fn connect(_host: &str, _port: u16) -> Result<SessionState> {
    anyhow::bail!("unimplemented: browser::connect")
}

/// Stop the session's browser and report whether it was running.
pub fn stop(_state: &SessionState) -> Result<()> {
    anyhow::bail!("unimplemented: browser::stop")
}

/// Health of the recorded session.
#[derive(Debug, Clone)]
pub enum BrowserStatus {
    /// Browser answers /json/version.
    Running { browser: String },
    /// State file exists but the browser is gone.
    Stale,
}

/// Probe the session's browser.
pub fn status(_state: &SessionState) -> Result<BrowserStatus> {
    anyhow::bail!("unimplemented: browser::status")
}
