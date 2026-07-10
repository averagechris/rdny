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
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::process_identity::ProcessIdentity;
use storage::AdvisoryLockMode;
use storage::StateStore;
pub(crate) use storage::{AdvisoryLock, Generation, Inspection, SecureDir};

const STATE_FILE: &str = "state.json";
const REGISTRY_FILE: &str = "instances.json";

#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
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
    /// Stable lifecycle identity used to couple state and registry entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    /// Browser-level WebSocket debugger URL from /json/version.
    pub ws_url: String,
    pub host: String,
    pub port: u16,
    /// PID of the browser we launched; None for `rdny connect` sessions.
    pub pid: Option<u32>,
    /// Robust identity captured at launch. Missing in legacy PID-only state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_identity: Option<ProcessIdentity>,
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
    /// Current recording id. Missing in older state; when `recording` is true
    /// without this field, frames live in the legacy `frames/` directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_id: Option<String>,
    /// Current recording frame directory, absolute or relative to the state dir.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_frames_dir: Option<PathBuf>,
    /// Last failed/incomplete recording kept for explicit `stop-video` retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recoverable_recording: Option<RecoverableRecording>,
    /// Failed/incomplete recordings kept for explicit deterministic retry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recoverable_recordings: Vec<RecoverableRecording>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instrumentation: Option<Box<InstrumentationState>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecoverableRecording {
    pub id: String,
    pub frames_dir: PathBuf,
    #[serde(default)]
    pub status: RecordingStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RecordingStatus {
    #[default]
    Recoverable,
    Assembling,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstrumentationState {
    pub target_id: String,
    pub version: u32,
    pub script_id: String,
}

/// Directory where video frames are accumulated while recording.
pub(crate) fn frames_dir() -> Result<SecureDir> {
    open_store()?.subdir("frames")
}

pub(crate) fn remove_frames_dir() -> Result<()> {
    open_store()?.remove_subdir("frames")
}

pub(crate) fn recordings_dir() -> Result<SecureDir> {
    open_store()?.subdir("recordings")
}

pub(crate) fn create_recording_frames_dir(id: &str) -> Result<Option<SecureDir>> {
    let recordings = recordings_dir()?;
    let Some(recording) = recordings.create_subdir_exclusive(id)? else {
        return Ok(None);
    };
    Ok(Some(recording.subdir("frames")?))
}

pub(crate) fn remove_recording_dir(id: &str) -> Result<()> {
    let recordings = recordings_dir()?;
    recordings.subdir(id)?.remove_flat_subdir("frames")?;
    recordings.remove_flat_subdir(id)
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

pub(crate) fn recording_frames_dir(state: &SessionState) -> Result<SecureDir> {
    if let Some(dir) = &state.recording_frames_dir {
        open_frames_dir_from_path(dir)
    } else {
        frames_dir()
    }
}

pub(crate) fn open_frames_dir_from_path(path: &Path) -> Result<SecureDir> {
    let root = state_dir()?;
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let relative = absolute.strip_prefix(&root).with_context(|| {
        format!(
            "recording frames path {} is outside state dir",
            absolute.display()
        )
    })?;
    if relative == Path::new("frames") {
        return frames_dir();
    }
    let components: Vec<_> = relative.components().collect();
    if components.len() == 3
        && components[0].as_os_str() == "recordings"
        && components[2].as_os_str() == "frames"
    {
        let id = components[1].as_os_str().to_string_lossy();
        return recordings_dir()?.subdir(&id)?.subdir("frames");
    }
    anyhow::bail!("unsupported recording frames path {}", absolute.display())
}

pub(crate) struct RecordingLease {
    _lock: AdvisoryLock,
}

pub(crate) fn try_recording_lease(frames_path: &Path) -> Result<Option<RecordingLease>> {
    let frames = open_frames_dir_from_path(frames_path)?;
    Ok(frames
        .try_advisory_lock(".recording.lock", AdvisoryLockMode::Exclusive)?
        .map(|lock| RecordingLease { _lock: lock }))
}

pub(crate) fn recording_lease(frames_path: &Path) -> Result<RecordingLease> {
    let frames = open_frames_dir_from_path(frames_path)?;
    frames
        .advisory_lock(
            ".recording.lock",
            AdvisoryLockMode::Exclusive,
            Duration::from_secs(5),
        )
        .map(|lock| RecordingLease { _lock: lock })
}

/// Acquire a writer lease while holding the state lock and only if this frame
/// directory is still the active recording. This state -> recording-lease
/// ordering is shared with deactivation: once deactivation owns the state lock,
/// no stale page session can begin another write.
pub(crate) fn active_recording_writer_lease(frames_path: &Path) -> Result<Option<RecordingLease>> {
    let frames = open_frames_dir_from_path(frames_path)?;
    let expected = storage::normalize_absolute(frames.path())?;
    let legacy = frames_dir()?.path().to_path_buf();
    let store = open_store()?;
    let mut selected = None;
    store.inspect_transaction::<SessionState, _>(
        STATE_FILE,
        Duration::from_secs(5),
        |inspection| {
            let Inspection::Valid(state, _) = inspection else {
                return Ok(None);
            };
            let active_path = state
                .recording_frames_dir
                .as_deref()
                .unwrap_or(legacy.as_path());
            if state.recording && storage::normalize_absolute(active_path)? == expected {
                selected = Some(RecordingLease {
                    _lock: frames.advisory_lock(
                        ".recording.lock",
                        AdvisoryLockMode::Shared,
                        Duration::from_secs(5),
                    )?,
                });
            }
            Ok(None)
        },
    )?;
    Ok(selected)
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
        StateInspection::Valid(state) => Ok(Some(*state)),
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
    // Lifecycle lock order is deliberately non-nested: repair registry, mutate
    // state, then publish the resulting state generation to the registry. Clear
    // and cleanup likewise release the state lock before pruning the matching
    // registry generation, so separate processes cannot deadlock on registry vs
    // state locks and concurrent re-registers with a newer generation survive.
    repair_registry_for_lifecycle()?;
    let store = open_store()?;
    let instance_id = new_instance_id();
    let generation =
        store.transaction_generation(STATE_FILE, Duration::from_secs(5), |current| {
            let mut state = state.clone();
            state.instance_id = Some(instance_id.clone());
            merge_state(current, &state)
        })?;
    publish_current_state_dir_if_current(&instance_id, &generation)?;
    Ok(())
}

fn new_instance_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{:032x}{:08x}{:016x}",
        now,
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct InstanceRegistry {
    #[serde(default)]
    entries: Vec<RegistryEntry>,
    #[serde(default)]
    dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct RegistryEntry {
    pub(crate) dir: PathBuf,
    pub(crate) instance_id: String,
}

fn default_store() -> Result<StateStore> {
    let dir = default_state_dir()?;
    StateStore::open(&dir)
        .with_context(|| format!("opening secure default state dir {}", dir.display()))
}

fn publish_current_state_dir_if_current(instance_id: &str, generation: &Generation) -> Result<()> {
    let dir = resolved_state_dir()?;
    let store = StateStore::open(&dir)?;
    // Lifecycle publication lock order is state -> registry. No lifecycle code
    // holds the registry lock while opening/locking a state dir, avoiding cycles.
    store.inspect_transaction::<SessionState, _>(STATE_FILE, Duration::from_secs(5), |inspection| {
        let Inspection::Valid(state, observed_generation) = inspection else {
            return Ok(None);
        };
        if state.instance_id.as_deref() == Some(instance_id)
            && observed_generation.bytes() == generation.bytes()
        {
            register_state_dir(&dir, instance_id)?;
        }
        Ok(None)
    })
}

pub(crate) fn register_state_dir(dir: &Path, instance_id: &str) -> Result<()> {
    let dir = storage::normalize_absolute(dir)?;
    let store = default_store()?;
    registry_transaction(&store, |reg| {
        reg.entries
            .retain(|entry| entry.dir != dir && entry.instance_id != instance_id);
        reg.entries.push(RegistryEntry {
            dir: dir.clone(),
            instance_id: instance_id.to_string(),
        });
        reg.entries.sort_by(|a, b| a.dir.cmp(&b.dir));
        reg.dirs.clear();
        Ok(true)
    })
}

pub(crate) fn unregister_state_dir_if_observed(dir: &Path, instance_id: &str) -> Result<()> {
    unregister_state_dir_if_observed_id(dir, instance_id)
}

pub(crate) fn unregister_state_dir_if_observed_id(dir: &Path, instance_id: &str) -> Result<()> {
    let dir = storage::normalize_absolute(dir)?;
    let store = default_store()?;
    registry_transaction(&store, |reg| {
        let before = reg.entries.len() + reg.dirs.len();
        reg.entries
            .retain(|entry| !(entry.dir == dir && entry.instance_id == instance_id));
        reg.dirs.retain(|entry| entry != &dir);
        Ok(reg.entries.len() + reg.dirs.len() != before)
    })
}

fn registry_from_valid_value(value: Value) -> Result<InstanceRegistry> {
    let mut reg: InstanceRegistry =
        serde_json::from_value(value).context("instance registry has incompatible schema")?;
    reg.entries = reg
        .entries
        .into_iter()
        .filter_map(|mut entry| {
            entry.dir = storage::normalize_absolute(&entry.dir).ok()?;
            Some(entry)
        })
        .collect();
    reg.entries.sort_by(|a, b| a.dir.cmp(&b.dir));
    reg.entries
        .dedup_by(|a, b| a.dir == b.dir && a.instance_id == b.instance_id);
    reg.dirs.clear();
    Ok(reg)
}

pub(crate) fn registered_state_dirs() -> Result<(Vec<RegistryEntry>, Vec<String>)> {
    let store = default_store()?;
    match store.inspect::<InstanceRegistry>(REGISTRY_FILE)? {
        Inspection::Missing => Ok((Vec::new(), Vec::new())),
        Inspection::Valid(reg, _) => Ok((valid_registry_entries(reg), Vec::new())),
        Inspection::Incompatible(e, _) => {
            Ok((Vec::new(), vec![format!("registry incompatible: {e}")]))
        }
        Inspection::Malformed(m) => Ok((
            Vec::new(),
            vec![format!("registry malformed: {}", m.message())],
        )),
    }
}

fn valid_registry_entries(reg: InstanceRegistry) -> Vec<RegistryEntry> {
    registry_from_valid_value(serde_json::to_value(reg).expect("registry serializes"))
        .map(|reg| reg.entries)
        .unwrap_or_default()
}

fn registry_transaction(
    store: &StateStore,
    mutate: impl FnOnce(&mut InstanceRegistry) -> Result<bool>,
) -> Result<()> {
    store.inspect_transaction::<Value, _>(REGISTRY_FILE, Duration::from_secs(5), |inspection| {
        let mut reg = match inspection {
            Inspection::Missing => InstanceRegistry::default(),
            Inspection::Valid(value, _) => registry_from_valid_value(value).unwrap_or_default(),
            Inspection::Incompatible(_, _) | Inspection::Malformed(_) => {
                InstanceRegistry::default()
            }
        };
        let changed = mutate(&mut reg)?;
        if changed {
            serde_json::to_value(reg)
                .context("serializing instance registry")
                .map(Some)
        } else {
            Ok(None)
        }
    })
}

fn repair_registry_for_lifecycle() -> Result<()> {
    registry_transaction(&default_store()?, |_| Ok(true))
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
            "instance_id",
            "host",
            "port",
            "pid",
            "process_identity",
            "user_data_dir",
            "browser_path",
            "target_id",
            "label",
            "viewport",
            "recording",
            "recording_id",
            "recording_frames_dir",
            "recoverable_recording",
            "recoverable_recordings",
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
    repair_registry_for_lifecycle()?;
    let dir = resolved_state_dir()?;
    let store = open_store()?;
    let removed_instance = match store.inspect::<SessionState>(STATE_FILE)? {
        Inspection::Valid(state, generation) => {
            if store
                .remove_if_generation::<SessionState, _>(STATE_FILE, &generation, |_| Ok(true))?
            {
                state.instance_id
            } else {
                None
            }
        }
        Inspection::Missing => None,
        Inspection::Malformed(malformed) => {
            store.quarantine_malformed(STATE_FILE, &malformed)?;
            None
        }
        Inspection::Incompatible(e, _) => anyhow::bail!("state file has incompatible schema: {e}"),
    };
    if let Some(instance_id) = removed_instance {
        unregister_state_dir_if_observed(&dir, &instance_id)?;
    }
    Ok(())
}

#[derive(Debug)]
enum StateInspection {
    Missing,
    Valid(Box<SessionState>),
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
        Inspection::Valid(v, _) => StateInspection::Valid(Box::new(v)),
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
    use std::process::Command;
    fn sample_state() -> SessionState {
        SessionState {
            instance_id: None,
            ws_url: "ws://127.0.0.1:9222/devtools/browser/abc".to_string(),
            host: "127.0.0.1".to_string(),
            port: 9222,
            pid: Some(123),
            process_identity: None,
            user_data_dir: Some(PathBuf::from("/tmp/rdny-profile")),
            browser_path: Some(PathBuf::from("/Applications/Google Chrome.app")),
            target_id: Some("target-1".to_string()),
            label: None,
            viewport: None,
            recording: false,
            recording_id: None,
            recording_frames_dir: None,
            recoverable_recording: None,
            recoverable_recordings: Vec::new(),
            instrumentation: None,
        }
    }

    #[test]
    fn registry_process_helper() {
        let Some(root) = env::var_os("RDNY_REGISTRY_PROCESS_ROOT") else {
            return;
        };
        let role = env::var("RDNY_REGISTRY_PROCESS_ROLE").unwrap();
        let root = PathBuf::from(root);
        std::fs::write(root.join(format!("ready-{role}")), b"ready").unwrap();
        while !root.join("go").exists() {
            std::thread::sleep(Duration::from_millis(2));
        }
        let dir = root.join(format!("state-{role}"));
        let store = StateStore::open(&dir).unwrap();
        store.write_json(STATE_FILE, &sample_state()).unwrap();
        let Inspection::Valid(_, generation) = store.inspect::<SessionState>(STATE_FILE).unwrap()
        else {
            unreachable!();
        };
        let _ = generation;
        register_state_dir(&dir, &format!("helper-{role}")).unwrap();
    }

    #[test]
    fn relative_register_process_helper() {
        if env::var_os("RDNY_RELATIVE_REGISTER_HELPER").is_none() {
            return;
        }
        replace(&sample_state()).unwrap();
    }

    #[test]
    fn reregister_process_helper() {
        let Some(root) = env::var_os("RDNY_REREGISTER_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        std::fs::write(root.join("ready"), b"ready").unwrap();
        while !root.join("go").exists() {
            std::thread::sleep(Duration::from_millis(2));
        }
        replace(&sample_state()).unwrap();
    }

    #[test]
    fn late_publish_process_helper() {
        let Some(root) = env::var_os("RDNY_LATE_PUBLISH_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let instance_id = "late-a".to_string();
        let store = open_store().unwrap();
        let generation = store
            .transaction_generation(STATE_FILE, Duration::from_secs(5), |current| {
                let mut state = sample_state();
                state.instance_id = Some(instance_id.clone());
                merge_state(current, &state)
            })
            .unwrap();
        std::fs::write(root.join("ready"), b"ready").unwrap();
        while !root.join("go").exists() {
            std::thread::sleep(Duration::from_millis(2));
        }
        publish_current_state_dir_if_current(&instance_id, &generation).unwrap();
    }

    #[test]
    fn registry_preserves_concurrent_process_registrations() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous_xdg = env::var_os("XDG_STATE_HOME");
        let temp = tempfile::tempdir().unwrap();
        unsafe {
            env::set_var("XDG_STATE_HOME", temp.path().join("xdg"));
        }
        StateStore::open(&default_state_dir().unwrap()).unwrap();
        let exe = std::env::current_exe().unwrap();
        let mut children: Vec<_> = ["a", "b"]
            .into_iter()
            .map(|role| {
                Command::new(&exe)
                    .arg("state::tests::registry_process_helper")
                    .arg("--exact")
                    .env("XDG_STATE_HOME", temp.path().join("xdg"))
                    .env("RDNY_REGISTRY_PROCESS_ROOT", temp.path())
                    .env("RDNY_REGISTRY_PROCESS_ROLE", role)
                    .spawn()
                    .unwrap()
            })
            .collect();
        while ["a", "b"]
            .into_iter()
            .any(|r| !temp.path().join(format!("ready-{r}")).exists())
        {
            std::thread::sleep(Duration::from_millis(2));
        }
        std::fs::write(temp.path().join("go"), b"go").unwrap();
        for child in &mut children {
            assert!(child.wait().unwrap().success());
        }
        let (dirs, diagnostics) = registered_state_dirs().unwrap();
        assert!(diagnostics.is_empty());
        assert!(
            dirs.iter().any(
                |e| e.dir == storage::normalize_absolute(&temp.path().join("state-a")).unwrap()
            )
        );
        assert!(
            dirs.iter().any(
                |e| e.dir == storage::normalize_absolute(&temp.path().join("state-b")).unwrap()
            )
        );
        if let Some(v) = previous_xdg {
            unsafe { env::set_var("XDG_STATE_HOME", v) }
        } else {
            unsafe { env::remove_var("XDG_STATE_HOME") }
        }
    }

    #[test]
    fn relative_state_dir_registers_absolute_for_other_cwd() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous_xdg = env::var_os("XDG_STATE_HOME");
        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("state::tests::relative_register_process_helper")
            .arg("--exact")
            .current_dir(temp.path())
            .env("XDG_STATE_HOME", &xdg)
            .env("RDNY_STATE_DIR", "relative-state")
            .env("RDNY_RELATIVE_REGISTER_HELPER", "1")
            .spawn()
            .unwrap();
        assert!(child.wait().unwrap().success());
        unsafe {
            env::set_var("XDG_STATE_HOME", &xdg);
        }
        let (dirs, _) = registered_state_dirs().unwrap();
        assert!(
            dirs.iter().any(|e| e.dir
                == storage::normalize_absolute(&temp.path().join("relative-state")).unwrap())
        );
        if let Some(v) = previous_xdg {
            unsafe { env::set_var("XDG_STATE_HOME", v) }
        } else {
            unsafe { env::remove_var("XDG_STATE_HOME") }
        }
    }

    #[test]
    fn malformed_registry_is_not_salvaged_from_unrelated_strings() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous_xdg = env::var_os("XDG_STATE_HOME");
        let temp = tempfile::tempdir().unwrap();
        unsafe {
            env::set_var("XDG_STATE_HOME", temp.path().join("xdg"));
        }
        let default = default_state_dir().unwrap();
        use std::io::Write;
        let mut file = StateStore::open(&default)
            .unwrap()
            .create_file(REGISTRY_FILE, true)
            .unwrap();
        file.write_all(br#"{"dirs":["/valid/one", bad, "/valid/two"]}"#)
            .unwrap();
        file.sync_all().unwrap();
        let (dirs, diagnostics) = registered_state_dirs().unwrap();
        assert!(diagnostics.iter().any(|d| d.contains("registry malformed")));
        assert!(dirs.is_empty());
        repair_registry_for_lifecycle().unwrap();
        let (dirs, diagnostics) = registered_state_dirs().unwrap();
        assert!(dirs.is_empty());
        assert!(diagnostics.is_empty());
        assert!(std::fs::read_dir(&default).unwrap().flatten().any(|e| {
            e.file_name()
                .to_string_lossy()
                .contains("instances.json.quarantine")
        }));
        if let Some(v) = previous_xdg {
            unsafe { env::set_var("XDG_STATE_HOME", v) }
        } else {
            unsafe { env::remove_var("XDG_STATE_HOME") }
        }
    }

    #[test]
    fn lifecycle_register_quarantines_malformed_registry_then_registers() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous_xdg = env::var_os("XDG_STATE_HOME");
        let previous_state = env::var_os("RDNY_STATE_DIR");
        let temp = tempfile::tempdir().unwrap();
        unsafe {
            env::set_var("XDG_STATE_HOME", temp.path().join("xdg"));
            env::set_var("RDNY_STATE_DIR", temp.path().join("state"));
        }
        let default = default_state_dir().unwrap();
        use std::io::Write;
        let mut file = StateStore::open(&default)
            .unwrap()
            .create_file(REGISTRY_FILE, true)
            .unwrap();
        file.write_all(b"not json").unwrap();
        file.sync_all().unwrap();
        replace(&sample_state()).unwrap();
        let (entries, diagnostics) = registered_state_dirs().unwrap();
        assert!(diagnostics.is_empty());
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].dir,
            storage::normalize_absolute(&temp.path().join("state")).unwrap()
        );
        assert!(std::fs::read_dir(&default).unwrap().flatten().any(|e| {
            e.file_name()
                .to_string_lossy()
                .contains("instances.json.quarantine")
        }));
        if let Some(v) = previous_state {
            unsafe { env::set_var("RDNY_STATE_DIR", v) }
        } else {
            unsafe { env::remove_var("RDNY_STATE_DIR") }
        }
        if let Some(v) = previous_xdg {
            unsafe { env::set_var("XDG_STATE_HOME", v) }
        } else {
            unsafe { env::remove_var("XDG_STATE_HOME") }
        }
    }

    #[test]
    fn clear_unregister_preserves_concurrent_process_reregister() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous_xdg = env::var_os("XDG_STATE_HOME");
        let previous_state = env::var_os("RDNY_STATE_DIR");
        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg");
        let state_dir = temp.path().join("state");
        unsafe {
            env::set_var("XDG_STATE_HOME", &xdg);
            env::set_var("RDNY_STATE_DIR", &state_dir);
        }
        replace(&sample_state()).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("state::tests::reregister_process_helper")
            .arg("--exact")
            .env("XDG_STATE_HOME", &xdg)
            .env("RDNY_STATE_DIR", &state_dir)
            .env("RDNY_REREGISTER_ROOT", temp.path())
            .spawn()
            .unwrap();
        while !temp.path().join("ready").exists() {
            std::thread::sleep(Duration::from_millis(2));
        }
        clear().unwrap();
        std::fs::write(temp.path().join("go"), b"go").unwrap();
        assert!(child.wait().unwrap().success());
        let (entries, _) = registered_state_dirs().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].dir,
            storage::normalize_absolute(&state_dir).unwrap()
        );
        if let Some(v) = previous_state {
            unsafe { env::set_var("RDNY_STATE_DIR", v) }
        } else {
            unsafe { env::remove_var("RDNY_STATE_DIR") }
        }
        if let Some(v) = previous_xdg {
            unsafe { env::set_var("XDG_STATE_HOME", v) }
        } else {
            unsafe { env::remove_var("XDG_STATE_HOME") }
        }
    }

    #[test]
    fn late_publish_after_replacement_does_not_overwrite_registry() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous_xdg = env::var_os("XDG_STATE_HOME");
        let previous_state = env::var_os("RDNY_STATE_DIR");
        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg");
        let state_dir = temp.path().join("state");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("state::tests::late_publish_process_helper")
            .arg("--exact")
            .env("XDG_STATE_HOME", &xdg)
            .env("RDNY_STATE_DIR", &state_dir)
            .env("RDNY_LATE_PUBLISH_ROOT", temp.path())
            .spawn()
            .unwrap();
        while !temp.path().join("ready").exists() {
            std::thread::sleep(Duration::from_millis(2));
        }
        unsafe {
            env::set_var("XDG_STATE_HOME", &xdg);
            env::set_var("RDNY_STATE_DIR", &state_dir);
        }
        replace(&sample_state()).unwrap();
        let current = load().unwrap().unwrap();
        let current_id = current.instance_id.clone().unwrap();
        assert_ne!(current_id, "late-a");
        std::fs::write(temp.path().join("go"), b"go").unwrap();
        assert!(child.wait().unwrap().success());
        let (entries, diagnostics) = registered_state_dirs().unwrap();
        assert!(diagnostics.is_empty());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].instance_id, current_id);
        if let Some(v) = previous_state {
            unsafe { env::set_var("RDNY_STATE_DIR", v) }
        } else {
            unsafe { env::remove_var("RDNY_STATE_DIR") }
        }
        if let Some(v) = previous_xdg {
            unsafe { env::set_var("XDG_STATE_HOME", v) }
        } else {
            unsafe { env::remove_var("XDG_STATE_HOME") }
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
        assert_eq!(state.recording_id, None);
        assert_eq!(state.recording_frames_dir, None);
        assert_eq!(state.recoverable_recording, None);
        assert!(state.recoverable_recordings.is_empty());
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
        let _guard = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let previous = env::var("RDNY_STATE_DIR").ok();
        let previous_xdg = env::var_os("XDG_STATE_HOME");
        let temp = tempfile::tempdir_in(".").unwrap();
        unsafe {
            env::set_var("RDNY_STATE_DIR", temp.path().join("state"));
            env::set_var("XDG_STATE_HOME", temp.path().join("xdg"));
        }

        assert_eq!(load().unwrap(), None);
        let err = require().unwrap_err();
        assert!(format!("{err}").contains("rdny start"));

        let state = sample_state();
        replace(&state).unwrap();
        let loaded = load().unwrap().unwrap();
        assert!(loaded.instance_id.is_some());
        let mut expected = state;
        expected.instance_id = loaded.instance_id.clone();
        assert_eq!(loaded, expected);
        clear().unwrap();
        assert_eq!(load().unwrap(), None);

        if let Some(previous) = previous {
            unsafe { env::set_var("RDNY_STATE_DIR", previous) };
        } else {
            unsafe { env::remove_var("RDNY_STATE_DIR") };
        }
        if let Some(previous) = previous_xdg {
            unsafe { env::set_var("XDG_STATE_HOME", previous) };
        } else {
            unsafe { env::remove_var("XDG_STATE_HOME") };
        }
    }

    #[test]
    fn default_state_lifecycle_does_not_reenter_registry_lock() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let previous_state = env::var_os("RDNY_STATE_DIR");
        let previous_xdg = env::var_os("XDG_STATE_HOME");
        let temp = tempfile::tempdir().unwrap();
        unsafe {
            env::remove_var("RDNY_STATE_DIR");
            env::set_var("XDG_STATE_HOME", temp.path());
        }

        let started = std::time::Instant::now();
        replace(&sample_state()).unwrap();
        let loaded = load().unwrap().unwrap();
        assert!(loaded.instance_id.is_some());
        let default = default_state_dir().unwrap();
        assert!(default.join("state.json").is_file());
        assert!(default.join("instances.json").is_file());
        assert!(default.join(".state.lock").is_file());
        assert!(default.join(".registry.lock").is_file());
        clear().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));

        if let Some(value) = previous_state {
            unsafe { env::set_var("RDNY_STATE_DIR", value) };
        } else {
            unsafe { env::remove_var("RDNY_STATE_DIR") };
        }
        if let Some(value) = previous_xdg {
            unsafe { env::set_var("XDG_STATE_HOME", value) };
        } else {
            unsafe { env::remove_var("XDG_STATE_HOME") };
        }
    }

    #[test]
    fn save_preserves_unknown_fields_but_clears_omitted_known_fields() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let previous = env::var_os("RDNY_STATE_DIR");
        let previous_xdg = env::var_os("XDG_STATE_HOME");
        let temp = tempfile::tempdir_in(".").unwrap();
        unsafe {
            env::set_var("RDNY_STATE_DIR", temp.path().join("state"));
            env::set_var("XDG_STATE_HOME", temp.path().join("xdg"));
        }
        std::fs::create_dir_all(temp.path().join("state")).unwrap();
        std::fs::write(
            temp.path().join("state").join(STATE_FILE),
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
        let raw: Value = serde_json::from_slice(
            &std::fs::read(temp.path().join("state").join(STATE_FILE)).unwrap(),
        )
        .unwrap();
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
        if let Some(previous) = previous_xdg {
            unsafe { env::set_var("XDG_STATE_HOME", previous) }
        } else {
            unsafe { env::remove_var("XDG_STATE_HOME") }
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
