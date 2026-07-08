//! Session state persistence.
//!
//! `rdny start`/`rdny connect` write a state file; every other command
//! loads it to find the running browser. Location:
//! `$RDNY_STATE_DIR` override > `$XDG_STATE_HOME/rdny` >
//! macOS `~/Library/Application Support/rdny` > `~/.local/state/rdny`.

use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result};
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
    let dir = if let Ok(dir) = env::var("RDNY_STATE_DIR") {
        PathBuf::from(dir)
    } else if let Ok(xdg) = env::var("XDG_STATE_HOME") {
        PathBuf::from(xdg).join("rdny")
    } else {
        let home = env::var("HOME").context("HOME is not set; cannot resolve rdny state dir")?;
        if cfg!(target_os = "macos") {
            PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("rdny")
        } else {
            PathBuf::from(home)
                .join(".local")
                .join("state")
                .join("rdny")
        }
    };
    fs::create_dir_all(&dir).with_context(|| format!("creating state dir {}", dir.display()))?;
    Ok(dir)
}

/// Load the session state, Ok(None) when no state file exists.
pub fn load() -> Result<Option<SessionState>> {
    let path = state_dir()?.join("state.json");
    match fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw)
            .with_context(|| format!("parsing state file {}", path.display()))
            .map(Some),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading state file {}", path.display())),
    }
}

/// Persist the session state atomically (write temp + rename).
pub fn save(state: &SessionState) -> Result<()> {
    let dir = state_dir()?;
    let path = dir.join("state.json");
    let tmp = dir.join("state.json.tmp");
    let raw = serde_json::to_string_pretty(state).context("serializing session state")?;
    fs::write(&tmp, raw).with_context(|| format!("writing temp state file {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| {
        format!(
            "renaming temp state file {} to {}",
            tmp.display(),
            path.display()
        )
    })?;
    Ok(())
}

/// Remove the state file if present.
pub fn clear() -> Result<()> {
    let path = state_dir()?.join("state.json");
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing state file {}", path.display())),
    }
}

/// Load state or fail with an actionable hint (no session -> tell the
/// user to run `rdny start`).
pub fn require() -> Result<SessionState> {
    load()?.ok_or_else(|| {
        crate::hint::hint_error(
            "no browser session",
            "run `rdny start` (or `rdny connect <host:port>`)",
            None,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn sample_state() -> SessionState {
        SessionState {
            ws_url: "ws://127.0.0.1:9222/devtools/browser/abc".to_string(),
            host: "127.0.0.1".to_string(),
            port: 9222,
            pid: Some(123),
            user_data_dir: Some(PathBuf::from("/tmp/rdny-profile")),
            browser_path: Some(PathBuf::from("/Applications/Google Chrome.app")),
            target_id: Some("target-1".to_string()),
        }
    }

    #[test]
    fn round_trip_state_with_env_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous = env::var("RDNY_STATE_DIR").ok();
        let temp = tempfile::tempdir().unwrap();
        unsafe { env::set_var("RDNY_STATE_DIR", temp.path()) };

        assert_eq!(load().unwrap(), None);
        let err = require().unwrap_err();
        assert!(format!("{err}").contains("rdny start"));

        let state = sample_state();
        save(&state).unwrap();
        assert_eq!(load().unwrap(), Some(state));
        clear().unwrap();
        assert_eq!(load().unwrap(), None);

        if let Some(previous) = previous {
            unsafe { env::set_var("RDNY_STATE_DIR", previous) };
        } else {
            unsafe { env::remove_var("RDNY_STATE_DIR") };
        }
    }
}
