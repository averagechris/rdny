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
    /// Authenticated local broker endpoint for new managed sessions. Absent for
    /// legacy managed TCP state and externally attached sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<BrokerEndpoint>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerEndpoint {
    pub socket: PathBuf,
    pub token: Vec<u8>,
    pub version: u32,
    pub broker_pid: u32,
    pub broker_identity: ProcessIdentity,
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
    pub(crate) broker: SecureDir,
    pub(crate) log: File,
}

pub(crate) fn browser_storage() -> Result<BrowserStorage> {
    browser_storage_at(&resolved_state_dir()?)
}

pub(crate) fn browser_storage_at(path: &Path) -> Result<BrowserStorage> {
    let store = StateStore::open(path)?;
    let profile = store.subdir("chrome-profile")?;
    let broker = store.secure_root()?;
    let log = store.create_file("chrome.log", true)?;
    Ok(BrowserStorage {
        _store: store,
        profile,
        broker,
        log,
    })
}

pub(crate) struct LifecycleLock {
    dir: PathBuf,
    _lock: storage::LockGuard,
}

pub(crate) fn lifecycle_lock() -> Result<LifecycleLock> {
    lifecycle_lock_at(&resolved_state_dir()?)
}

pub(crate) fn lifecycle_lock_at(path: &Path) -> Result<LifecycleLock> {
    let store = StateStore::open(path)?;
    Ok(LifecycleLock {
        dir: storage::normalize_absolute(path)?,
        _lock: store
            .lock_named(".lifecycle.lock", Duration::from_secs(120))
            .context("acquiring rdny lifecycle lock for start/connect/stop/clear")?,
    })
}

pub(crate) fn inspect_for_lifecycle(lock: &LifecycleLock) -> Result<Inspection<SessionState>> {
    StateStore::open(&lock.dir)?.inspect(STATE_FILE)
}

/// Atomic ownership publication/removal transaction. Lock order is always the
/// per-directory lifecycle lock (held by the caller), current state lock, then
/// the global registry lock. Both data locks remain held through exact-byte
/// snapshots, repair, writes, commit, or rollback.
struct LifecycleTransaction {
    state: StateStore,
    registry: StateStore,
    _state_lock: storage::LockGuard,
    _registry_lock: storage::LockGuard,
    state_prior: Option<Vec<u8>>,
    registry_prior: Option<Vec<u8>>,
}

impl LifecycleTransaction {
    fn begin(lifecycle: &LifecycleLock) -> Result<Self> {
        let state = StateStore::open(&lifecycle.dir)?;
        let state_lock = state.lock_named(".state.lock", Duration::from_secs(120))?;
        let registry = default_store()?;
        let registry_lock = registry.lock_named(".registry.lock", Duration::from_secs(120))?;
        let state_prior = state.raw_snapshot_locked(STATE_FILE)?;
        let registry_prior = registry.raw_snapshot_locked(REGISTRY_FILE)?;
        Ok(Self {
            state,
            registry,
            _state_lock: state_lock,
            _registry_lock: registry_lock,
            state_prior,
            registry_prior,
        })
    }

    fn rollback<R>(&self, original: anyhow::Error) -> Result<R> {
        let state_result = self
            .state
            .raw_restore_locked(STATE_FILE, self.state_prior.as_deref());
        let registry_result = self
            .registry
            .raw_restore_locked(REGISTRY_FILE, self.registry_prior.as_deref());
        if state_result.is_err() || registry_result.is_err() {
            anyhow::bail!(
                "{original:#}; lifecycle rollback also failed: state={:?}, registry={:?}",
                state_result.err().map(|e| format!("{e:#}")),
                registry_result.err().map(|e| format!("{e:#}")),
            );
        }
        Err(original)
    }

    fn commit(self) {}
}

fn lifecycle_transaction<R>(
    lifecycle: &LifecycleLock,
    operation: impl FnOnce(&LifecycleTransaction) -> Result<R>,
) -> Result<R> {
    let transaction = LifecycleTransaction::begin(lifecycle)?;
    match operation(&transaction) {
        Ok(value) => {
            transaction.commit();
            Ok(value)
        }
        Err(err) => transaction.rollback(err),
    }
}

pub(crate) fn clear_observed_lifecycle(
    lifecycle: &LifecycleLock,
    instance_id: Option<&str>,
    generation: &Generation,
) -> Result<bool> {
    lifecycle_transaction(lifecycle, |tx| {
        let Inspection::Valid(state, observed) =
            tx.state.inspect_locked::<SessionState>(STATE_FILE)?
        else {
            return Ok(false);
        };
        if observed.bytes() != generation.bytes()
            || instance_id.is_some_and(|id| state.instance_id.as_deref() != Some(id))
        {
            return Ok(false);
        }
        tx.state.remove_locked(STATE_FILE)?;
        if let Some(id) = instance_id {
            unregister_state_dir_locked(&tx.registry, &lifecycle.dir, id)?;
        }
        Ok(true)
    })
}

pub(crate) fn prune_stale_registry_locked(
    lifecycle: &LifecycleLock,
    instance_id: &str,
) -> Result<bool> {
    lifecycle_transaction(lifecycle, |tx| {
        if matches!(
            tx.state.inspect_locked::<SessionState>(STATE_FILE)?,
            Inspection::Valid(state, _) if state.instance_id.as_deref() == Some(instance_id)
        ) {
            return Ok(false);
        }
        unregister_state_dir_locked(&tx.registry, &lifecycle.dir, instance_id)?;
        Ok(true)
    })
}

pub(crate) fn quarantine_malformed_locked(
    lifecycle: &LifecycleLock,
    registry_instance_id: Option<&str>,
) -> Result<Option<PathBuf>> {
    lifecycle_transaction(lifecycle, |tx| {
        let result = match tx.state.inspect_locked::<SessionState>(STATE_FILE)? {
            Inspection::Malformed(malformed) => tx
                .state
                .quarantine_malformed_locked(STATE_FILE, &malformed)?,
            _ => None,
        };
        if result.is_some()
            && let Some(instance_id) = registry_instance_id
        {
            unregister_state_dir_locked(&tx.registry, &lifecycle.dir, instance_id)?;
        }
        Ok(result)
    })
}

pub(crate) fn replace_lifecycle(lifecycle: &LifecycleLock, state: &SessionState) -> Result<()> {
    replace_lifecycle_with_hook(lifecycle, state, || Ok(()))
}

pub(crate) fn replace_lifecycle_and_commit(
    lifecycle: &LifecycleLock,
    state: &SessionState,
    commit: impl FnOnce() -> Result<()>,
) -> Result<()> {
    replace_lifecycle_with_hook(lifecycle, state, commit)
}

fn replace_lifecycle_with_hook(
    lifecycle: &LifecycleLock,
    state: &SessionState,
    after_publication: impl FnOnce() -> Result<()>,
) -> Result<()> {
    replace_lifecycle_with_injections(lifecycle, state, || Ok(()), || Ok(()), after_publication)
}

fn replace_lifecycle_with_injections(
    lifecycle: &LifecycleLock,
    state: &SessionState,
    after_state_write: impl FnOnce() -> Result<()>,
    after_registry_write: impl FnOnce() -> Result<()>,
    after_publication: impl FnOnce() -> Result<()>,
) -> Result<()> {
    lifecycle_transaction(lifecycle, |tx| {
        // Repair is part of the transaction: malformed prior bytes are restored
        // exactly if any later publication step fails.
        let mut registry = registry_locked(&tx.registry)?;
        let current = match tx.state.inspect_locked::<Value>(STATE_FILE)? {
            Inspection::Missing => None,
            Inspection::Valid(value, _) => Some(value),
            Inspection::Malformed(m) => {
                anyhow::bail!("state file contains malformed JSON: {}", m.message())
            }
            Inspection::Incompatible(e, _) => {
                anyhow::bail!("state file has incompatible schema: {e}")
            }
        };
        let instance_id = state.instance_id.clone().unwrap_or_else(new_instance_id);
        let mut next = state.clone();
        next.instance_id = Some(instance_id.clone());
        let merged = merge_state(current, &next)?;
        tx.state.write_json_locked_normal(STATE_FILE, &merged)?;
        after_state_write()?;
        let dir = lifecycle.dir.clone();
        registry
            .entries
            .retain(|entry| entry.dir != dir && entry.instance_id != instance_id);
        registry.entries.push(RegistryEntry { dir, instance_id });
        registry.entries.sort_by(|a, b| a.dir.cmp(&b.dir));
        registry.dirs.clear();
        tx.registry
            .write_json_locked_normal(REGISTRY_FILE, &registry)?;
        after_registry_write()?;
        // Both ownership records are durable while the broker is still armed.
        // This is deliberately the final fallible transaction action: a failed
        // commit/ack restores both exact snapshots before dropping the armed
        // broker guard, while success has no later operation that can roll back.
        after_publication()?;
        Ok(())
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

pub(crate) fn resolved_state_dir() -> Result<PathBuf> {
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
#[allow(dead_code)]
pub fn replace(state: &SessionState) -> Result<()> {
    let lifecycle = lifecycle_lock()?;
    replace_lifecycle(&lifecycle, state)
}

pub(crate) fn new_instance_id() -> String {
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

#[cfg(test)]
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

#[cfg(test)]
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
    let dir = default_state_dir()?;
    let store = match StateStore::open_existing(&dir) {
        Ok(store) => store,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok((Vec::new(), Vec::new()));
        }
        Err(error) => return Err(error),
    };
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

#[cfg(test)]
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

fn registry_locked(store: &StateStore) -> Result<InstanceRegistry> {
    Ok(match store.inspect_locked::<Value>(REGISTRY_FILE)? {
        Inspection::Missing => InstanceRegistry::default(),
        Inspection::Valid(value, _) => registry_from_valid_value(value).unwrap_or_default(),
        Inspection::Incompatible(_, _) => InstanceRegistry::default(),
        Inspection::Malformed(malformed) => {
            store.quarantine_malformed_locked(REGISTRY_FILE, &malformed)?;
            InstanceRegistry::default()
        }
    })
}

fn unregister_state_dir_locked(
    registry_store: &StateStore,
    dir: &Path,
    instance_id: &str,
) -> Result<()> {
    let dir = storage::normalize_absolute(dir)?;
    let mut registry = registry_locked(registry_store)?;
    registry
        .entries
        .retain(|entry| !(entry.dir == dir && entry.instance_id == instance_id));
    registry.dirs.retain(|entry| entry != &dir);
    registry_store.write_json_locked_normal(REGISTRY_FILE, &registry)?;
    Ok(())
}

#[cfg(test)]
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
            "endpoint",
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
#[allow(dead_code)]
pub fn clear() -> Result<()> {
    let lifecycle = lifecycle_lock()?;
    clear_locked(&lifecycle)
}

pub(crate) fn clear_locked(lifecycle: &LifecycleLock) -> Result<()> {
    lifecycle_transaction(lifecycle, |tx| {
        match tx.state.inspect_locked::<SessionState>(STATE_FILE)? {
            Inspection::Missing => {}
            Inspection::Valid(state, _) => {
                tx.state.remove_locked(STATE_FILE)?;
                if let Some(instance_id) = state.instance_id {
                    unregister_state_dir_locked(&tx.registry, &lifecycle.dir, &instance_id)?;
                }
            }
            Inspection::Malformed(malformed) => {
                tx.state
                    .quarantine_malformed_locked(STATE_FILE, &malformed)?;
            }
            Inspection::Incompatible(e, _) => {
                anyhow::bail!("state file has incompatible schema: {e}")
            }
        }
        Ok(())
    })
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
            endpoint: None,
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
    fn publication_failures_rollback_exact_state_and_registry_before_commit() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let old_state = env::var_os("RDNY_STATE_DIR");
        let old_xdg = env::var_os("XDG_STATE_HOME");
        for stage in 0..3 {
            let temp = tempfile::tempdir().unwrap();
            let state_dir = temp.path().join("state");
            let xdg = temp.path().join("xdg");
            unsafe {
                env::set_var("RDNY_STATE_DIR", &state_dir);
                env::set_var("XDG_STATE_HOME", &xdg);
            }
            let lifecycle = lifecycle_lock().unwrap();
            let state_path = state_dir.join(STATE_FILE);
            let registry_path = xdg.join("rdny").join(REGISTRY_FILE);
            let result = replace_lifecycle_with_injections(
                &lifecycle,
                &sample_state(),
                || {
                    if stage == 0 {
                        anyhow::bail!("injected state write failure")
                    }
                    Ok(())
                },
                || {
                    if stage == 1 {
                        anyhow::bail!("injected registry write failure")
                    }
                    Ok(())
                },
                || {
                    assert!(state_path.is_file(), "state must precede broker commit");
                    assert!(
                        registry_path.is_file(),
                        "registry must precede broker commit"
                    );
                    if stage == 2 {
                        anyhow::bail!("injected broker commit failure")
                    }
                    Ok(())
                },
            );
            assert!(result.is_err());
            assert!(
                !state_path.exists(),
                "state rollback failed at stage {stage}"
            );
            assert!(
                !registry_path.exists(),
                "registry rollback failed at stage {stage}"
            );
        }
        unsafe {
            match old_state {
                Some(value) => env::set_var("RDNY_STATE_DIR", value),
                None => env::remove_var("RDNY_STATE_DIR"),
            }
            match old_xdg {
                Some(value) => env::set_var("XDG_STATE_HOME", value),
                None => env::remove_var("XDG_STATE_HOME"),
            }
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
    fn lifecycle_overlap_process_helper() {
        let Some(root) = env::var_os("RDNY_LIFECYCLE_TEST_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let role = env::var("RDNY_LIFECYCLE_TEST_ROLE").unwrap();
        let marker = |name: &str| root.join(name);
        let wait = |name: &str| {
            while !marker(name).exists() {
                std::thread::sleep(Duration::from_millis(2));
            }
        };
        let state_for = |label: &str| {
            let mut state = sample_state();
            state.pid = None;
            state.label = Some(label.to_string());
            state
        };
        match role.as_str() {
            role if role.starts_with("owner-claim-") => {
                let lifecycle = lifecycle_lock().unwrap();
                std::fs::write(marker("owner-lifecycle-held"), b"held").unwrap();
                wait("release-owner");
                if matches!(
                    inspect_for_lifecycle(&lifecycle).unwrap(),
                    Inspection::Missing
                ) {
                    replace_lifecycle(&lifecycle, &state_for(role)).unwrap();
                    std::fs::write(marker(&format!("won-{role}")), b"won").unwrap();
                }
            }
            role if role.starts_with("contender-claim-") => {
                let lifecycle = lifecycle_lock().unwrap();
                if matches!(
                    inspect_for_lifecycle(&lifecycle).unwrap(),
                    Inspection::Missing
                ) {
                    replace_lifecycle(&lifecycle, &state_for(role)).unwrap();
                    std::fs::write(marker(&format!("won-{role}")), b"won").unwrap();
                }
            }
            "replacement" => {
                let lifecycle = lifecycle_lock().unwrap();
                replace_lifecycle(&lifecycle, &state_for("replacement")).unwrap();
            }
            "stop" => {
                let lifecycle = lifecycle_lock().unwrap();
                let Inspection::Valid(state, generation) =
                    inspect_for_lifecycle(&lifecycle).unwrap()
                else {
                    panic!("missing stop state")
                };
                std::fs::write(marker("stop-locked"), b"locked").unwrap();
                wait("release-stop");
                clear_observed_lifecycle(&lifecycle, state.instance_id.as_deref(), &generation)
                    .unwrap();
            }
            "cleanup" => {
                let expected = std::fs::read(marker("observed-state")).unwrap();
                let lifecycle = lifecycle_lock().unwrap();
                if let Inspection::Valid(state, generation) =
                    inspect_for_lifecycle(&lifecycle).unwrap()
                    && generation.bytes() == expected
                {
                    clear_observed_lifecycle(&lifecycle, state.instance_id.as_deref(), &generation)
                        .unwrap();
                }
            }
            "start-owner" => {
                let lifecycle = lifecycle_lock().unwrap();
                std::fs::write(marker("start-lifecycle-held"), b"held").unwrap();
                wait("release-start");
                replace_lifecycle(&lifecycle, &state_for("started")).unwrap();
            }
            "rollback" => {
                let lifecycle = lifecycle_lock().unwrap();
                let error = replace_lifecycle_with_hook(&lifecycle, &state_for("doomed"), || {
                    std::fs::write(marker("rollback-state-written"), b"written")?;
                    wait("allow-rollback");
                    anyhow::bail!("injected publication failure")
                })
                .unwrap_err();
                assert!(format!("{error:#}").contains("injected publication failure"));
                let state_bytes = std::fs::read(resolved_state_dir().unwrap().join(STATE_FILE))
                    .unwrap_or_default();
                let registry_bytes =
                    std::fs::read(default_state_dir().unwrap().join(REGISTRY_FILE))
                        .unwrap_or_default();
                std::fs::write(marker("rollback-state-snapshot"), state_bytes).unwrap();
                std::fs::write(marker("rollback-registry-snapshot"), registry_bytes).unwrap();
            }
            "update" => {
                update(|state| {
                    state.viewport = Some(ViewportOverride {
                        width: 777,
                        height: 888,
                        scale: 1.0,
                        mobile: false,
                    });
                    Ok(())
                })
                .unwrap();
            }
            "publish-other" => {
                let lifecycle = lifecycle_lock().unwrap();
                replace_lifecycle(&lifecycle, &state_for("other")).unwrap();
            }
            _ => panic!("unknown lifecycle helper role {role}"),
        }
        std::fs::write(marker(&format!("done-{role}")), b"done").unwrap();
    }

    fn spawn_lifecycle_role(
        root: &Path,
        xdg: &Path,
        state_dir: &Path,
        role: &str,
    ) -> std::process::Child {
        spawn_lifecycle_role_with_contention(root, xdg, state_dir, role, None, None)
    }

    fn spawn_lifecycle_role_with_contention(
        root: &Path,
        xdg: &Path,
        state_dir: &Path,
        role: &str,
        lock_name: Option<&str>,
        contention_marker: Option<&Path>,
    ) -> std::process::Child {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .arg("state::tests::lifecycle_overlap_process_helper")
            .arg("--exact")
            .env_remove("RDNY_TEST_CONTENTION_LOCK")
            .env_remove("RDNY_TEST_CONTENTION_MARKER")
            .env_remove("RDNY_TEST_CRASH_BEFORE_BROKER_COMMIT")
            .env("XDG_STATE_HOME", xdg)
            .env("RDNY_STATE_DIR", state_dir)
            .env("RDNY_LIFECYCLE_TEST_ROOT", root)
            .env("RDNY_LIFECYCLE_TEST_ROLE", role)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let (Some(lock_name), Some(marker)) = (lock_name, contention_marker) {
            command
                .env("RDNY_TEST_CONTENTION_LOCK", lock_name)
                .env("RDNY_TEST_CONTENTION_MARKER", marker);
        }
        command.spawn().unwrap()
    }

    fn wait_for(path: &Path) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !path.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn initialize_lifecycle(root: &Path, xdg: &Path, state_dir: &Path) {
        let child = spawn_lifecycle_role(root, xdg, state_dir, "replacement");
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "replacement helper failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::remove_file(root.join("done-replacement")).unwrap();
    }

    fn assert_child_blocked(child: &mut std::process::Child, done: &Path) {
        assert!(
            child.try_wait().unwrap().is_none(),
            "contender exited before lock release"
        );
        assert!(
            !done.exists(),
            "contender reported completion before lock release"
        );
    }

    fn assert_single_lifecycle_owner(owner_kind: &str, contender_kind: &str) {
        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg");
        let state_dir = temp.path().join("state");
        StateStore::open(&state_dir).unwrap();
        StateStore::open(&xdg.join("rdny")).unwrap();
        let owner = format!("owner-claim-{owner_kind}");
        let contender = format!("contender-claim-{contender_kind}");
        let mut owner_child = spawn_lifecycle_role(temp.path(), &xdg, &state_dir, &owner);
        wait_for(&temp.path().join("owner-lifecycle-held"));
        let contention = temp.path().join("contender-lifecycle-contended");
        let mut contender_child = spawn_lifecycle_role_with_contention(
            temp.path(),
            &xdg,
            &state_dir,
            &contender,
            Some(".lifecycle.lock"),
            Some(&contention),
        );
        wait_for(&contention);
        assert_child_blocked(
            &mut contender_child,
            &temp.path().join(format!("done-{contender}")),
        );
        std::fs::write(temp.path().join("release-owner"), b"release").unwrap();
        assert!(owner_child.wait().unwrap().success());
        assert!(contender_child.wait().unwrap().success());
        let winners = [&owner, &contender]
            .into_iter()
            .filter(|role| temp.path().join(format!("won-{role}")).exists())
            .count();
        assert_eq!(
            winners, 1,
            "exactly one fake lifecycle interface owns the dir"
        );
        let state: SessionState =
            serde_json::from_slice(&std::fs::read(state_dir.join(STATE_FILE)).unwrap()).unwrap();
        let registry: InstanceRegistry =
            serde_json::from_slice(&std::fs::read(xdg.join("rdny").join(REGISTRY_FILE)).unwrap())
                .unwrap();
        assert!(state.instance_id.is_some());
        assert_eq!(registry.entries.len(), 1);
        assert_eq!(registry.entries[0].instance_id, state.instance_id.unwrap());
    }

    #[test]
    fn separate_process_start_start_has_one_owner() {
        assert_single_lifecycle_owner("start-a", "start-b");
    }

    #[test]
    fn separate_process_start_connect_has_one_owner() {
        assert_single_lifecycle_owner("start", "connect");
    }

    #[test]
    fn separate_process_connect_connect_has_one_owner() {
        assert_single_lifecycle_owner("connect-a", "connect-b");
    }

    #[test]
    fn separate_process_stop_then_replacement_preserves_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg");
        let state_dir = temp.path().join("state");
        initialize_lifecycle(temp.path(), &xdg, &state_dir);
        let mut stop = spawn_lifecycle_role(temp.path(), &xdg, &state_dir, "stop");
        wait_for(&temp.path().join("stop-locked"));
        let contention = temp.path().join("replacement-lifecycle-contended");
        let mut replacement = spawn_lifecycle_role_with_contention(
            temp.path(),
            &xdg,
            &state_dir,
            "replacement",
            Some(".lifecycle.lock"),
            Some(&contention),
        );
        wait_for(&contention);
        assert_child_blocked(&mut replacement, &temp.path().join("done-replacement"));
        std::fs::write(temp.path().join("release-stop"), b"go").unwrap();
        assert!(stop.wait().unwrap().success());
        assert!(replacement.wait().unwrap().success());
        let state: SessionState =
            serde_json::from_slice(&std::fs::read(state_dir.join(STATE_FILE)).unwrap()).unwrap();
        assert_eq!(state.label.as_deref(), Some("replacement"));
    }

    #[test]
    fn separate_process_cleanup_vs_start_preserves_started_owner() {
        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg");
        let state_dir = temp.path().join("state");
        initialize_lifecycle(temp.path(), &xdg, &state_dir);
        std::fs::copy(
            state_dir.join(STATE_FILE),
            temp.path().join("observed-state"),
        )
        .unwrap();
        let mut start = spawn_lifecycle_role(temp.path(), &xdg, &state_dir, "start-owner");
        wait_for(&temp.path().join("start-lifecycle-held"));
        let contention = temp.path().join("cleanup-lifecycle-contended");
        let mut cleanup = spawn_lifecycle_role_with_contention(
            temp.path(),
            &xdg,
            &state_dir,
            "cleanup",
            Some(".lifecycle.lock"),
            Some(&contention),
        );
        wait_for(&contention);
        assert_child_blocked(&mut cleanup, &temp.path().join("done-cleanup"));
        std::fs::write(temp.path().join("release-start"), b"release").unwrap();
        assert!(start.wait().unwrap().success());
        assert!(cleanup.wait().unwrap().success());
        let state: SessionState =
            serde_json::from_slice(&std::fs::read(state_dir.join(STATE_FILE)).unwrap()).unwrap();
        assert_eq!(state.label.as_deref(), Some("started"));
    }

    #[test]
    fn lifecycle_rollback_then_concurrent_state_update_keeps_update_and_exact_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg");
        let state_dir = temp.path().join("state");
        initialize_lifecycle(temp.path(), &xdg, &state_dir);
        let prior_state = std::fs::read(state_dir.join(STATE_FILE)).unwrap();
        let prior_registry = std::fs::read(xdg.join("rdny").join(REGISTRY_FILE)).unwrap();
        let mut rollback = spawn_lifecycle_role(temp.path(), &xdg, &state_dir, "rollback");
        wait_for(&temp.path().join("rollback-state-written"));
        let contention = temp.path().join("update-state-contended");
        let mut update = spawn_lifecycle_role_with_contention(
            temp.path(),
            &xdg,
            &state_dir,
            "update",
            Some(".state.lock"),
            Some(&contention),
        );
        wait_for(&contention);
        assert_child_blocked(&mut update, &temp.path().join("done-update"));
        std::fs::write(temp.path().join("allow-rollback"), b"go").unwrap();
        assert!(rollback.wait().unwrap().success());
        assert!(update.wait().unwrap().success());
        assert_eq!(
            std::fs::read(temp.path().join("rollback-state-snapshot")).unwrap(),
            prior_state
        );
        assert_eq!(
            std::fs::read(temp.path().join("rollback-registry-snapshot")).unwrap(),
            prior_registry
        );
        let state: SessionState =
            serde_json::from_slice(&std::fs::read(state_dir.join(STATE_FILE)).unwrap()).unwrap();
        assert_eq!(state.viewport.as_ref().map(|v| v.width), Some(777));
        assert_eq!(state.label.as_deref(), Some("replacement"));
    }

    #[test]
    fn lifecycle_rollback_then_other_custom_dir_publication_keeps_both_registry_entries() {
        let temp = tempfile::tempdir().unwrap();
        let xdg = temp.path().join("xdg");
        let state_a = temp.path().join("state-a");
        let state_b = temp.path().join("state-b");
        initialize_lifecycle(temp.path(), &xdg, &state_a);
        let prior_state = std::fs::read(state_a.join(STATE_FILE)).unwrap();
        let prior_registry = std::fs::read(xdg.join("rdny").join(REGISTRY_FILE)).unwrap();
        let mut rollback = spawn_lifecycle_role(temp.path(), &xdg, &state_a, "rollback");
        wait_for(&temp.path().join("rollback-state-written"));
        let contention = temp.path().join("publish-registry-contended");
        let mut publish = spawn_lifecycle_role_with_contention(
            temp.path(),
            &xdg,
            &state_b,
            "publish-other",
            Some(".registry.lock"),
            Some(&contention),
        );
        wait_for(&contention);
        assert_child_blocked(&mut publish, &temp.path().join("done-publish-other"));
        std::fs::write(temp.path().join("allow-rollback"), b"go").unwrap();
        assert!(rollback.wait().unwrap().success());
        assert!(publish.wait().unwrap().success());
        assert_eq!(
            std::fs::read(temp.path().join("rollback-state-snapshot")).unwrap(),
            prior_state
        );
        assert_eq!(
            std::fs::read(temp.path().join("rollback-registry-snapshot")).unwrap(),
            prior_registry
        );
        let registry: InstanceRegistry =
            serde_json::from_slice(&std::fs::read(xdg.join("rdny").join(REGISTRY_FILE)).unwrap())
                .unwrap();
        assert_eq!(registry.entries.len(), 2);
        let state_a = storage::normalize_absolute(&state_a).unwrap();
        let state_b = storage::normalize_absolute(&state_b).unwrap();
        assert!(registry.entries.iter().any(|entry| entry.dir == state_a));
        assert!(registry.entries.iter().any(|entry| entry.dir == state_b));
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
