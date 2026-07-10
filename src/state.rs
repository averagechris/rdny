//! Session state persistence.
//!
//! `rdny start`/`rdny connect` write a state file; every other command
//! loads it to find the running browser. Location:
//! `$RDNY_STATE_DIR` override > `$XDG_STATE_HOME/rdny` >
//! macOS `~/Library/Application Support/rdny` > `~/.local/state/rdny`.

#[path = "state/storage.rs"]
#[allow(dead_code)]
mod storage;

use std::{
    env,
    fs::File,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use storage::StateStore;
pub(crate) use storage::{Generation, Inspection, SecureDir};

const STATE_FILE: &str = "state.json";

/// Persisted viewport/mobile emulation override.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewportOverride {
    pub width: u32,
    pub height: u32,
    #[serde(default = "default_viewport_scale")]
    pub scale: f64,
    pub mobile: bool,
}

fn default_viewport_scale() -> f64 {
    1.0
}

impl ViewportOverride {
    pub fn cdp_params(&self) -> Value {
        json!({
            "width": self.width,
            "height": self.height,
            "deviceScaleFactor": self.scale,
            "mobile": self.mobile,
        })
    }
}

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
    /// Human-readable instance label, set by `rdny start --label`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Optional persisted viewport/mobile emulation override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub viewport: Option<ViewportOverride>,
    /// Whether commands should collect CDP screencast frames.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recording: bool,
}

/// Directory where video frames are accumulated while recording.
pub(crate) fn frames_dir() -> Result<SecureDir> {
    open_store()?.subdir("frames")
}

pub(crate) fn remove_frames_dir() -> Result<()> {
    open_store()?.remove_subdir("frames")
}

pub(crate) struct BrowserStorage {
    // Keep both validated directory descriptors alive throughout launch.
    _store: StateStore,
    pub(crate) profile: SecureDir,
    pub(crate) log: File,
}

pub(crate) fn browser_storage() -> Result<BrowserStorage> {
    browser_storage_at(&resolved_state_dir()?)
}

pub(crate) fn browser_storage_at(path: &Path) -> Result<BrowserStorage> {
    let store = StateStore::open(path)?;
    let profile = store.subdir("chrome-profile")?;
    let log = store.create_file("chrome.log", true)?;
    Ok(BrowserStorage {
        _store: store,
        profile,
        log,
    })
}

/// Resolve the rdny state directory (created if missing).
pub fn state_dir() -> Result<PathBuf> {
    let dir = resolve_state_dir(
        env::var_os("RDNY_STATE_DIR"),
        env::var_os("XDG_STATE_HOME"),
        env::var_os("HOME"),
    )?;
    StateStore::open(&dir)
        .with_context(|| format!("opening secure state dir {}", dir.display()))?;
    Ok(dir)
}

fn resolved_state_dir() -> Result<PathBuf> {
    resolve_state_dir(
        env::var_os("RDNY_STATE_DIR"),
        env::var_os("XDG_STATE_HOME"),
        env::var_os("HOME"),
    )
}

fn open_store() -> Result<StateStore> {
    let dir = resolved_state_dir()?;
    StateStore::open(&dir).with_context(|| format!("opening secure state dir {}", dir.display()))
}

pub(crate) fn open_store_at(path: &Path) -> Result<StateStore> {
    StateStore::open_existing(path)
}

/// Resolve the default rdny state directory, deliberately ignoring RDNY_STATE_DIR.
pub fn default_state_dir() -> Result<PathBuf> {
    resolve_default_state_dir(env::var_os("XDG_STATE_HOME"), env::var_os("HOME"))
}

pub fn resolve_state_dir(
    rdny_state_dir: Option<impl Into<PathBuf>>,
    xdg_state_home: Option<impl Into<PathBuf>>,
    home: Option<impl Into<PathBuf>>,
) -> Result<PathBuf> {
    if let Some(dir) = rdny_state_dir {
        storage::normalize_absolute(&dir.into())
    } else {
        resolve_default_state_dir(xdg_state_home, home)
    }
}

pub fn resolve_default_state_dir(
    xdg_state_home: Option<impl Into<PathBuf>>,
    home: Option<impl Into<PathBuf>>,
) -> Result<PathBuf> {
    if let Some(xdg) = xdg_state_home {
        storage::normalize_absolute(&xdg.into().join("rdny"))
    } else {
        let home = home
            .map(Into::into)
            .context("HOME is not set; cannot resolve rdny state dir")?;
        if cfg!(target_os = "macos") {
            storage::normalize_absolute(
                &home
                    .join("Library")
                    .join("Application Support")
                    .join("rdny"),
            )
        } else {
            storage::normalize_absolute(&home.join(".local").join("state").join("rdny"))
        }
    }
}

pub(crate) fn capture_initial_cwd() -> Result<()> {
    storage::capture_initial_cwd()
}

/// Load the session state, Ok(None) when no state file exists.
pub fn load() -> Result<Option<SessionState>> {
    match inspect()? {
        StateInspection::Missing => Ok(None),
        StateInspection::Valid(state) => Ok(Some(state)),
        StateInspection::Malformed(message) => {
            anyhow::bail!("state file contains malformed JSON: {message}")
        }
        StateInspection::Incompatible(message) => {
            anyhow::bail!("state file has incompatible schema: {message}")
        }
    }
}

/// Explicitly replace the lifecycle state while preserving unknown fields.
/// Incremental command changes must use [`update`] instead.
pub fn replace(state: &SessionState) -> Result<()> {
    let store = open_store()?;
    store.transaction(STATE_FILE, Duration::from_secs(5), |current| {
        merge_state(current, state)
    })?;
    Ok(())
}

/// Lock, load the latest state, apply one incremental mutation, and atomically
/// persist it without dropping fields written by concurrent commands or newer
/// rdny versions.
pub fn update<R>(mutate: impl FnOnce(&mut SessionState) -> Result<R>) -> Result<R> {
    let store = open_store()?;
    let mut result = None;
    store.transaction(STATE_FILE, Duration::from_secs(5), |current| {
        let current = current.context("no browser session; run `rdny start`")?;
        let mut state: SessionState = serde_json::from_value(current.clone())
            .context("state file has incompatible schema")?;
        result = Some(mutate(&mut state)?);
        merge_state(Some(current), &state)
    })?;
    Ok(result.expect("transaction mutation ran"))
}

pub fn update_if_present<R>(
    mutate: impl FnOnce(&mut SessionState) -> Result<R>,
) -> Result<Option<R>> {
    let store = open_store()?;
    let mut result = None;
    store.transaction_optional(STATE_FILE, Duration::from_secs(5), |current| {
        let Some(current) = current else {
            return Ok(None);
        };
        let mut state: SessionState = serde_json::from_value(current.clone())
            .context("state file has incompatible schema")?;
        result = Some(mutate(&mut state)?);
        merge_state(Some(current), &state).map(Some)
    })?;
    Ok(result)
}

fn merge_state(current: Option<Value>, state: &SessionState) -> Result<Value> {
    let mut next = match current {
        Some(Value::Object(map)) => Value::Object(map),
        _ => Value::Object(Default::default()),
    };
    let state_value = serde_json::to_value(state).context("serializing session state")?;
    if let (Value::Object(dst), Value::Object(src)) = (&mut next, state_value) {
        for key in [
            "ws_url",
            "host",
            "port",
            "pid",
            "user_data_dir",
            "browser_path",
            "target_id",
            "label",
            "viewport",
            "recording",
        ] {
            dst.remove(key);
        }
        for (key, value) in src {
            dst.insert(key, value);
        }
    }
    Ok(next)
}

/// Remove the state file if present.
pub fn clear() -> Result<()> {
    open_store()?.remove(STATE_FILE)
}

#[derive(Debug)]
enum StateInspection {
    Missing,
    Valid(SessionState),
    Malformed(String),
    Incompatible(String),
}

fn inspect() -> Result<StateInspection> {
    let dir = resolved_state_dir()?;
    let store = match StateStore::open_existing(&dir) {
        Ok(store) => store,
        Err(err)
            if err
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(StateInspection::Missing);
        }
        Err(err) => return Err(err),
    };
    Ok(match store.inspect(STATE_FILE)? {
        Inspection::Missing => StateInspection::Missing,
        Inspection::Valid(v, _) => StateInspection::Valid(v),
        Inspection::Malformed(v) => StateInspection::Malformed(v.message().to_owned()),
        Inspection::Incompatible(e, _) => StateInspection::Incompatible(e),
    })
}

#[allow(dead_code)] // Recovery command wiring is a follow-up; keep the operation internal.
fn quarantine_malformed() -> Result<Option<PathBuf>> {
    let store = open_store()?;
    match store.inspect::<SessionState>(STATE_FILE)? {
        Inspection::Malformed(observed) => store.quarantine_malformed(STATE_FILE, &observed),
        Inspection::Missing => Ok(None),
        Inspection::Valid(_, _) => anyhow::bail!("state is valid; refusing to quarantine"),
        Inspection::Incompatible(e, _) => {
            anyhow::bail!("state schema is incompatible, not malformed: {e}")
        }
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
    use std::{process::Command, sync::Mutex};

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
            label: None,
            viewport: None,
            recording: false,
        }
    }

    #[test]
    fn update_process_helper() {
        let Some(root) = env::var_os("RDNY_UPDATE_PROCESS_ROOT") else {
            return;
        };
        let role = env::var("RDNY_UPDATE_PROCESS_ROLE").unwrap();
        let barrier = PathBuf::from(&root).join("go");
        std::fs::write(PathBuf::from(&root).join(format!("ready-{role}")), b"ready").unwrap();
        while !barrier.exists() {
            std::thread::sleep(Duration::from_millis(2));
        }
        update(|state| {
            std::thread::sleep(Duration::from_millis(40));
            match role.as_str() {
                "label" => state.label = Some("concurrent".into()),
                "recording" => state.recording = true,
                _ => unreachable!(),
            }
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn label_round_trips_in_state_json() {
        let mut state = sample_state();
        state.label = Some("work".to_string());
        let raw = serde_json::to_string(&state).unwrap();
        assert!(raw.contains("label"));
        assert_eq!(serde_json::from_str::<SessionState>(&raw).unwrap(), state);
    }

    #[test]
    fn viewport_round_trips_in_state_json() {
        let mut state = sample_state();
        state.viewport = Some(ViewportOverride {
            width: 375,
            height: 812,
            scale: 2.0,
            mobile: true,
        });

        let raw = serde_json::to_string(&state).unwrap();
        assert!(raw.contains("viewport"));
        assert_eq!(serde_json::from_str::<SessionState>(&raw).unwrap(), state);
    }

    #[test]
    fn old_state_json_without_viewport_deserializes() {
        let raw = r#"{
            "ws_url":"ws://127.0.0.1:9222/devtools/browser/abc",
            "host":"127.0.0.1",
            "port":9222,
            "pid":123,
            "user_data_dir":"/tmp/rdny-profile",
            "browser_path":"/Applications/Google Chrome.app",
            "target_id":"target-1"
        }"#;

        let state = serde_json::from_str::<SessionState>(raw).unwrap();
        assert_eq!(state.viewport, None);
        assert_eq!(state.label, None);
        assert!(!state.recording);
    }

    #[test]
    fn recording_round_trips_and_skips_false() {
        let state = sample_state();
        let raw = serde_json::to_string(&state).unwrap();
        assert!(!raw.contains("recording"));

        let mut state = state;
        state.recording = true;
        let raw = serde_json::to_string(&state).unwrap();
        assert!(raw.contains("recording"));
        assert_eq!(serde_json::from_str::<SessionState>(&raw).unwrap(), state);
    }

    #[test]
    fn round_trip_state_with_env_override() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous = env::var("RDNY_STATE_DIR").ok();
        let temp = tempfile::tempdir_in(".").unwrap();
        unsafe { env::set_var("RDNY_STATE_DIR", temp.path()) };

        assert_eq!(load().unwrap(), None);
        let err = require().unwrap_err();
        assert!(format!("{err}").contains("rdny start"));

        let state = sample_state();
        replace(&state).unwrap();
        assert_eq!(load().unwrap(), Some(state));
        clear().unwrap();
        assert_eq!(load().unwrap(), None);

        if let Some(previous) = previous {
            unsafe { env::set_var("RDNY_STATE_DIR", previous) };
        } else {
            unsafe { env::remove_var("RDNY_STATE_DIR") };
        }
    }

    #[test]
    fn save_preserves_unknown_fields_but_clears_omitted_known_fields() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous = env::var_os("RDNY_STATE_DIR");
        let temp = tempfile::tempdir_in(".").unwrap();
        unsafe { env::set_var("RDNY_STATE_DIR", temp.path()) };
        std::fs::write(
            temp.path().join(STATE_FILE),
            serde_json::to_vec(&json!({
                "ws_url":"old", "host":"old", "port":1,
                "pid":null, "user_data_dir":null, "browser_path":null,
                "target_id":null, "label":"stale",
                "viewport":{"width":1,"height":1,"mobile":false},
                "recording":true, "future_field":{"keep":true}
            }))
            .unwrap(),
        )
        .unwrap();

        replace(&sample_state()).unwrap();
        let raw: Value =
            serde_json::from_slice(&std::fs::read(temp.path().join(STATE_FILE)).unwrap()).unwrap();
        assert!(raw.get("label").is_none());
        assert!(raw.get("viewport").is_none(), "viewport must reset to None");
        assert!(
            raw.get("recording").is_none(),
            "false recording must replace stale true"
        );
        assert_eq!(raw["future_field"]["keep"], true);

        if let Some(previous) = previous {
            unsafe { env::set_var("RDNY_STATE_DIR", previous) }
        } else {
            unsafe { env::remove_var("RDNY_STATE_DIR") }
        }
    }

    #[test]
    fn process_updates_of_independent_fields_do_not_overwrite() {
        let temp = tempfile::tempdir_in(".").unwrap();
        StateStore::open(temp.path())
            .unwrap()
            .write_json(STATE_FILE, &sample_state())
            .unwrap();
        let exe = std::env::current_exe().unwrap();
        let mut children: Vec<_> = ["label", "recording"]
            .into_iter()
            .map(|role| {
                Command::new(&exe)
                    .arg("state::tests::update_process_helper")
                    .arg("--exact")
                    .env("RDNY_STATE_DIR", temp.path())
                    .env("RDNY_UPDATE_PROCESS_ROOT", temp.path())
                    .env("RDNY_UPDATE_PROCESS_ROLE", role)
                    .spawn()
                    .unwrap()
            })
            .collect();
        while ["label", "recording"]
            .into_iter()
            .any(|role| !temp.path().join(format!("ready-{role}")).exists())
        {
            std::thread::sleep(Duration::from_millis(2));
        }
        std::fs::write(temp.path().join("go"), b"go").unwrap();
        for child in &mut children {
            assert!(child.wait().unwrap().success());
        }
        let state: SessionState = StateStore::open(temp.path())
            .unwrap()
            .read_json(STATE_FILE)
            .unwrap()
            .unwrap();
        assert_eq!(state.label.as_deref(), Some("concurrent"));
        assert!(state.recording);
    }
}
