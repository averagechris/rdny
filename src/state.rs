//! Secure, transactional session state and persistent instance registry.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, thread};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const LOCK_POLL: Duration = Duration::from_millis(25);
const PRIVATE_DIR_MODE: u32 = 0o700;
const PRIVATE_FILE_MODE: u32 = 0o600;

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

/// Identity recorded for a process rdny launched. Optional for compatibility
/// with state written by older rdny versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub birth_token: String,
    pub executable: PathBuf,
    pub profile: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    pub ws_url: String,
    pub host: String,
    pub port: u16,
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_identity: Option<ProcessIdentity>,
    pub user_data_dir: Option<PathBuf>,
    pub browser_path: Option<PathBuf>,
    pub target_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub viewport: Option<ViewportOverride>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recording: bool,
}

#[derive(Debug)]
pub enum StateFile {
    Missing,
    Valid(Box<SessionState>),
    Corrupt(String),
}

/// Exclusive interprocess transaction for one state directory. The lock is
/// held until this value is dropped.
pub struct Transaction {
    dir: PathBuf,
    _lock: File,
}

impl Transaction {
    pub fn begin() -> Result<Self> {
        Self::begin_in(&state_dir_path()?, LOCK_TIMEOUT)
    }

    pub fn begin_in(dir: &Path, timeout: Duration) -> Result<Self> {
        ensure_private_dir(dir)?;
        let lock_path = dir.join("state.lock");
        let lock = open_private(&lock_path, true, false)?;
        acquire_lock(&lock, &lock_path, timeout)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            _lock: lock,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn inspect(&self) -> Result<StateFile> {
        read_state_file(&self.dir.join("state.json"))
    }

    pub fn load(&self) -> Result<Option<SessionState>> {
        match self.inspect()? {
            StateFile::Missing => Ok(None),
            StateFile::Valid(state) => Ok(Some(*state)),
            StateFile::Corrupt(reason) => bail!(
                "corrupt state file in {}: {reason}; run `rdny cleanup` to quarantine it",
                self.dir.display()
            ),
        }
    }

    pub fn require(&self) -> Result<SessionState> {
        self.load()?.ok_or_else(|| {
            crate::hint::hint_error(
                "no browser session",
                "run `rdny start` (or `rdny connect <host:port>`)",
                None,
            )
        })
    }

    pub fn save(&self, state: &SessionState) -> Result<()> {
        write_json_atomic(&self.dir, "state.json", state).context("persisting session state")
    }

    /// Persist a newly created/adopted lifecycle state and then make it
    /// discoverable. If registry persistence fails, the state remains usable via
    /// this exact state dir and a later `rdny start/connect` or `rdny cleanup`
    /// can retry registration; state mutations intentionally do not register.
    pub fn save_and_register(&self, state: &SessionState) -> Result<()> {
        self.save(state)?;
        register_dir(&self.dir).map(|_| ())
    }

    pub fn clear(&self) -> Result<()> {
        // Clear state before unregistering so a registry failure leaves a
        // discoverable missing entry rather than undiscoverable custom state.
        self.clear_state_file()?;
        unregister_dir(&self.dir)
    }

    /// Remove only this transaction's state file. Instance cleanup updates the
    /// registry once, explicitly, after processing all discovered entries.
    pub(crate) fn clear_state_file(&self) -> Result<()> {
        remove_if_exists(&self.dir.join("state.json"))
    }

    pub(crate) fn unregister(&self) -> Result<()> {
        unregister_dir(&self.dir)
    }

    pub fn quarantine_corrupt(&self) -> Result<Option<PathBuf>> {
        let StateFile::Corrupt(_) = self.inspect()? else {
            return Ok(None);
        };
        for nonce in 0..1000_u32 {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let destination = self.dir.join(format!("state.corrupt-{stamp}-{nonce}"));
            let placeholder = match open_private(&destination, true, true) {
                Ok(file) => file,
                Err(err)
                    if err
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|io| io.kind() == ErrorKind::AlreadyExists) =>
                {
                    continue;
                }
                Err(err) => return Err(err),
            };
            drop(placeholder);
            fs::rename(self.dir.join("state.json"), &destination).with_context(|| {
                format!("quarantining corrupt state as {}", destination.display())
            })?;
            sync_dir(&self.dir)?;
            return Ok(Some(destination));
        }
        bail!("could not choose a unique corrupt-state quarantine name")
    }
}

pub fn transaction() -> Result<Transaction> {
    Transaction::begin()
}

pub fn inspect() -> Result<StateFile> {
    transaction()?.inspect()
}

pub fn load() -> Result<Option<SessionState>> {
    transaction()?.load()
}

pub fn require() -> Result<SessionState> {
    transaction()?.require()
}

pub fn frames_dir() -> Result<PathBuf> {
    let dir = state_dir_path()?.join("frames");
    ensure_private_dir(&dir)?;
    Ok(dir)
}

/// Create or validate an owner-private directory used by session artifacts.
pub fn secure_dir(path: &Path) -> Result<()> {
    ensure_private_dir(path)
}

/// Open a private regular output file without following symlinks.
pub fn secure_output(path: &Path, truncate: bool) -> Result<File> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        validate_private_file(path, &metadata)?;
    }
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create(true)
        .truncate(truncate)
        .mode(PRIVATE_FILE_MODE)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options
        .open(path)
        .with_context(|| format!("opening private output {}", path.display()))?;
    file.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
    Ok(file)
}

/// Validate and open a private regular input without following symlinks.
pub fn secure_input(path: &Path) -> Result<File> {
    open_private(path, false, false)
}

/// Resolve the current state directory without creating it.
pub fn resolved_state_dir() -> Result<PathBuf> {
    state_dir_path()
}

fn state_dir_path() -> Result<PathBuf> {
    resolve_state_dir(
        env::var_os("RDNY_STATE_DIR"),
        env::var_os("XDG_STATE_HOME"),
        env::var_os("HOME"),
    )
}

pub fn default_state_dir() -> Result<PathBuf> {
    resolve_default_state_dir(env::var_os("XDG_STATE_HOME"), env::var_os("HOME"))
}

pub fn resolve_state_dir(
    rdny_state_dir: Option<impl Into<PathBuf>>,
    xdg_state_home: Option<impl Into<PathBuf>>,
    home: Option<impl Into<PathBuf>>,
) -> Result<PathBuf> {
    if let Some(dir) = rdny_state_dir {
        normalize_absolute(dir.into())
    } else {
        resolve_default_state_dir(xdg_state_home, home)
    }
}

pub fn resolve_default_state_dir(
    xdg_state_home: Option<impl Into<PathBuf>>,
    home: Option<impl Into<PathBuf>>,
) -> Result<PathBuf> {
    if let Some(xdg) = xdg_state_home {
        normalize_absolute(xdg.into().join("rdny"))
    } else {
        let home = home
            .map(Into::into)
            .context("HOME is not set; cannot resolve rdny state dir")?;
        if cfg!(target_os = "macos") {
            normalize_absolute(
                home.join("Library")
                    .join("Application Support")
                    .join("rdny"),
            )
        } else {
            normalize_absolute(home.join(".local").join("state").join("rdny"))
        }
    }
}

fn normalize_absolute(path: PathBuf) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path
    } else {
        env::current_dir()
            .context("resolving current directory for state dir")?
            .join(path)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryEntry {
    Path(PathBuf),
    Malformed(String),
}

/// Read every registry item without silently discarding malformed entries.
pub fn registry_entries() -> Result<Vec<RegistryEntry>> {
    let root = registry_root()?;
    let _lock = RegistryLock::begin(&root)?;
    read_registry(&root.join("registry.json"))
}

pub(crate) fn unregister_dir_for_cleanup(dir: &Path) -> Result<()> {
    unregister_dir(dir)
}

#[cfg(test)]
fn prune_registry_at(root: &Path, keep: impl Fn(&Path) -> bool) -> Result<()> {
    ensure_private_dir(root)?;
    let _lock = RegistryLock::begin(root)?;
    let entries = read_registry(&root.join("registry.json"))?;
    let paths: Vec<PathBuf> = entries
        .into_iter()
        .filter_map(|entry| match entry {
            RegistryEntry::Path(path) if keep(&path) => Some(path),
            _ => None,
        })
        .collect();
    write_registry(root, &paths)
}

fn registry_root() -> Result<PathBuf> {
    let state_root = default_state_dir()?;
    ensure_private_dir(&state_root)?;
    let root = state_root.join("registry");
    ensure_private_dir(&root)?;
    Ok(root)
}

struct RegistryLock {
    _file: File,
}

impl RegistryLock {
    fn begin(root: &Path) -> Result<Self> {
        let path = root.join("registry.lock");
        let file = open_private(&path, true, false)?;
        acquire_lock(&file, &path, LOCK_TIMEOUT)?;
        Ok(Self { _file: file })
    }
}

fn register_dir(dir: &Path) -> Result<bool> {
    let root = registry_root()?;
    register_dir_at(&root, dir)
}

fn register_dir_at(root: &Path, dir: &Path) -> Result<bool> {
    ensure_private_dir(root)?;
    let _lock = RegistryLock::begin(root)?;
    let entries = read_registry(&root.join("registry.json"))?;
    reject_malformed_registry(&entries, "registering a state directory")?;
    let mut paths: Vec<PathBuf> = entries
        .into_iter()
        .map(|entry| match entry {
            RegistryEntry::Path(path) => path,
            RegistryEntry::Malformed(_) => unreachable!("checked above"),
        })
        .collect();
    let added = !paths.iter().any(|path| same_path(path, dir));
    if added {
        paths.push(dir.to_path_buf());
    }
    write_registry(root, &paths)?;
    Ok(added)
}

fn unregister_dir(dir: &Path) -> Result<()> {
    let root = registry_root()?;
    unregister_dir_at(&root, dir)
}

fn unregister_dir_at(root: &Path, dir: &Path) -> Result<()> {
    ensure_private_dir(root)?;
    let _lock = RegistryLock::begin(root)?;
    let entries = read_registry(&root.join("registry.json"))?;
    reject_malformed_registry(&entries, "unregistering a state directory")?;
    let mut changed = false;
    let paths: Vec<PathBuf> = entries
        .into_iter()
        .filter_map(|entry| match entry {
            RegistryEntry::Path(path) if same_path(&path, dir) => {
                changed = true;
                None
            }
            RegistryEntry::Path(path) => Some(path),
            RegistryEntry::Malformed(_) => unreachable!("checked above"),
        })
        .collect();
    if changed {
        write_registry(root, &paths)?;
    }
    Ok(())
}

fn reject_malformed_registry(entries: &[RegistryEntry], action: &str) -> Result<()> {
    if let Some(RegistryEntry::Malformed(reason)) = entries
        .iter()
        .find(|entry| matches!(entry, RegistryEntry::Malformed(_)))
    {
        bail!(
            "cannot continue {action}: instance registry is malformed ({reason}); run `rdny cleanup` to prune malformed registry entries"
        );
    }
    Ok(())
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn read_registry(path: &Path) -> Result<Vec<RegistryEntry>> {
    let raw = match read_private(path)? {
        Some(raw) => raw,
        None => return Ok(Vec::new()),
    };
    let value: Value = match serde_json::from_slice(&raw) {
        Ok(value) => value,
        Err(err) => {
            return Ok(vec![RegistryEntry::Malformed(format!(
                "{}: {err}",
                path.display()
            ))]);
        }
    };
    let Some(items) = value.get("paths").and_then(Value::as_array) else {
        return Ok(vec![RegistryEntry::Malformed(format!(
            "{}: expected an object containing a paths array",
            path.display()
        ))]);
    };
    Ok(items
        .iter()
        .enumerate()
        .map(|(index, item)| match item.as_str() {
            Some(path) if Path::new(path).is_absolute() => RegistryEntry::Path(path.into()),
            Some(_) => RegistryEntry::Malformed(format!("registry entry {index} is not absolute")),
            None => RegistryEntry::Malformed(format!("registry entry {index} is not a string")),
        })
        .collect())
}

fn write_registry(root: &Path, paths: &[PathBuf]) -> Result<()> {
    let strings: Vec<_> = paths.iter().map(|path| path.to_string_lossy()).collect();
    write_json_atomic(root, "registry.json", &json!({ "paths": strings }))
        .context("persisting instance registry")
}

fn read_state_file(path: &Path) -> Result<StateFile> {
    let Some(raw) = read_private(path)? else {
        return Ok(StateFile::Missing);
    };
    match serde_json::from_slice(&raw) {
        Ok(state) => Ok(StateFile::Valid(Box::new(state))),
        Err(err) => Ok(StateFile::Corrupt(format!("{}: {err}", path.display()))),
    }
}

fn read_private(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_private_file(path, &metadata)?,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("inspecting {}", path.display())),
    }
    let mut file = open_private(path, false, false)?;
    let mut raw = Vec::new();
    file.read_to_end(&mut raw)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(Some(raw))
}

fn write_json_atomic(dir: &Path, name: &str, value: &impl Serialize) -> Result<()> {
    ensure_private_dir(dir)?;
    if std::env::var_os("RDNY_TEST_FAIL_WRITE").is_some_and(|v| v == name) {
        bail!("injected persistence failure for {name}");
    }
    let destination = dir.join(name);
    if let Ok(metadata) = fs::symlink_metadata(&destination) {
        validate_private_file(&destination, &metadata)?;
    }
    let raw = serde_json::to_vec_pretty(value).context("serializing JSON")?;
    for attempt in 0..100_u32 {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temp = dir.join(format!(
            ".{name}.tmp-{}-{nonce}-{attempt}",
            std::process::id()
        ));
        let mut file = match open_private(&temp, true, true) {
            Ok(file) => file,
            Err(err)
                if err
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == ErrorKind::AlreadyExists) =>
            {
                continue;
            }
            Err(err) => return Err(err),
        };
        let result = (|| -> Result<()> {
            file.write_all(&raw)
                .with_context(|| format!("writing {}", temp.display()))?;
            file.flush()
                .with_context(|| format!("flushing {}", temp.display()))?;
            file.sync_all()
                .with_context(|| format!("syncing {}", temp.display()))?;
            fs::rename(&temp, &destination).with_context(|| {
                format!(
                    "atomically replacing {} with {}",
                    destination.display(),
                    temp.display()
                )
            })?;
            sync_dir(dir)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        return result;
    }
    bail!(
        "could not create a unique temporary file in {}",
        dir.display()
    )
}

fn open_private(path: &Path, create: bool, exclusive: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(create)
        .create(create)
        .mode(PRIVATE_FILE_MODE);
    if exclusive {
        options.create_new(true);
    }
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options
        .open(path)
        .with_context(|| format!("opening {} without following symlinks", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspecting {}", path.display()))?;
    validate_private_file(path, &metadata)?;
    if create {
        file.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))
            .with_context(|| format!("securing {}", path.display()))?;
    }
    Ok(file)
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    validate_existing_ancestors(path)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_private_dir(path, &metadata),
        Err(err) if err.kind() == ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(PRIVATE_DIR_MODE);
            builder
                .create(path)
                .with_context(|| format!("creating {}", path.display()))?;
            fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIR_MODE))
                .with_context(|| format!("securing {}", path.display()))?;
            let metadata = fs::symlink_metadata(path)
                .with_context(|| format!("inspecting {}", path.display()))?;
            validate_private_dir(path, &metadata)
        }
        Err(err) => Err(err).with_context(|| format!("inspecting {}", path.display())),
    }
}

fn validate_existing_ancestors(path: &Path) -> Result<()> {
    let mut cur = PathBuf::new();
    for component in path.components() {
        cur.push(component.as_os_str());
        if cur == path {
            break;
        }
        match fs::symlink_metadata(&cur) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    if is_trusted_platform_alias(&cur) {
                        continue;
                    }
                    bail!(
                        "unsafe path {}: ancestor symlinks are not allowed",
                        cur.display()
                    );
                }
                if !metadata.is_dir() {
                    bail!("unsafe path {}: ancestor is not a directory", cur.display());
                }
                if metadata.uid() != unsafe { libc::geteuid() }
                    && metadata.mode() & 0o002 != 0
                    && metadata.mode() & libc::S_ISVTX as u32 == 0
                {
                    bail!("unsafe path {}: mutable untrusted ancestor", cur.display());
                }
            }
            Err(err) if err.kind() == ErrorKind::NotFound => break,
            Err(err) => {
                return Err(err).with_context(|| format!("inspecting ancestor {}", cur.display()));
            }
        }
    }
    Ok(())
}

fn is_trusted_platform_alias(path: &Path) -> bool {
    cfg!(target_os = "macos") && path == Path::new("/var")
}

pub fn validate_private_dir(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    validate_owner_and_mode(path, metadata, true)
}

fn validate_private_file(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    validate_owner_and_mode(path, metadata, false)
}

fn validate_owner_and_mode(path: &Path, metadata: &fs::Metadata, directory: bool) -> Result<()> {
    if metadata.file_type().is_symlink() {
        bail!("unsafe path {}: symlinks are not allowed", path.display());
    }
    if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
        bail!("unsafe path {}: wrong file type", path.display());
    }
    if !directory && metadata.nlink() != 1 {
        bail!(
            "unsafe path {}: mutable hard links are not allowed",
            path.display()
        );
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        bail!(
            "unsafe path {}: not owned by the effective user",
            path.display()
        );
    }
    if metadata.mode() & 0o077 != 0 {
        bail!(
            "unsafe permissions on {}: expected owner-private {:04o}, found {:04o}",
            path.display(),
            if directory {
                PRIVATE_DIR_MODE
            } else {
                PRIVATE_FILE_MODE
            },
            metadata.mode() & 0o777
        );
    }
    Ok(())
}

fn acquire_lock(file: &File, path: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
            return Err(err).with_context(|| format!("locking {}", path.display()));
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out after {:.1}s waiting for state lock {}; another rdny command may be running",
                timeout.as_secs_f64(),
                path.display()
            );
        }
        thread::sleep(LOCK_POLL);
    }
}

use std::os::fd::AsRawFd;

fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("syncing directory {}", dir.display()))
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                sync_dir(parent)?;
            }
            Ok(())
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn sample_state() -> SessionState {
        SessionState {
            ws_url: "ws://127.0.0.1:9222/devtools/browser/abc".into(),
            host: "127.0.0.1".into(),
            port: 9222,
            pid: Some(123),
            process_identity: None,
            user_data_dir: Some("/tmp/rdny-profile".into()),
            browser_path: Some("/Applications/Google Chrome.app".into()),
            target_id: Some("target-1".into()),
            label: None,
            viewport: None,
            recording: false,
        }
    }

    #[test]
    fn atomic_state_round_trip_and_permissions() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("state");
        let tx = Transaction::begin_in(&dir, Duration::from_secs(1)).unwrap();
        write_json_atomic(&dir, "state.json", &sample_state()).unwrap();
        assert_eq!(tx.load().unwrap(), Some(sample_state()));
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        assert_eq!(
            fs::metadata(dir.join("state.json")).unwrap().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn malformed_and_unknown_fields_are_distinguished() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("state");
        let tx = Transaction::begin_in(&dir, Duration::from_secs(1)).unwrap();
        let mut value = serde_json::to_value(sample_state()).unwrap();
        value["future"] = json!({"works": true});
        write_json_atomic(&dir, "state.json", &value).unwrap();
        assert!(matches!(tx.inspect().unwrap(), StateFile::Valid(_)));
        fs::write(dir.join("state.json"), "{\"port\":").unwrap();
        fs::set_permissions(dir.join("state.json"), fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(tx.inspect().unwrap(), StateFile::Corrupt(_)));
        let quarantined = tx.quarantine_corrupt().unwrap().unwrap();
        assert!(quarantined.exists());
        assert!(!dir.join("state.json").exists());
    }

    #[test]
    fn rejects_symlinks_and_permissive_paths() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        fs::create_dir(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
        let link = temp.path().join("link");
        symlink(&real, &link).unwrap();
        assert!(Transaction::begin_in(&link, Duration::from_millis(1)).is_err());
        fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Transaction::begin_in(&real, Duration::from_millis(1)).is_err());
        assert_eq!(fs::metadata(&real).unwrap().mode() & 0o777, 0o755);
    }

    #[test]
    fn rejects_symlinked_ancestors() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        fs::create_dir(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
        let link = temp.path().join("ancestor-link");
        symlink(&real, &link).unwrap();
        let err = match Transaction::begin_in(&link.join("state"), Duration::from_millis(1)) {
            Ok(_) => panic!("symlinked ancestor was accepted"),
            Err(err) => err,
        };
        assert!(format!("{err:#}").contains("ancestor symlinks"));
    }

    #[test]
    fn rejects_prepositioned_state_and_lock_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("state");
        ensure_private_dir(&dir).unwrap();
        let victim = temp.path().join("victim");
        fs::write(&victim, "do not touch").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o600)).unwrap();

        symlink(&victim, dir.join("state.lock")).unwrap();
        assert!(Transaction::begin_in(&dir, Duration::from_millis(1)).is_err());
        fs::remove_file(dir.join("state.lock")).unwrap();

        let tx = Transaction::begin_in(&dir, Duration::from_millis(100)).unwrap();
        symlink(&victim, dir.join("state.json")).unwrap();
        assert!(tx.inspect().is_err());
        assert!(write_json_atomic(&dir, "state.json", &sample_state()).is_err());
        assert_eq!(fs::read_to_string(&victim).unwrap(), "do not touch");
    }

    #[test]
    fn lock_timeout_is_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("state");
        let _first = Transaction::begin_in(&dir, Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        let err = Transaction::begin_in(&dir, Duration::from_millis(80))
            .err()
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(format!("{err:#}").contains("timed out"));
    }

    #[test]
    fn concurrent_registry_updates_do_not_lose_custom_directories() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("registry");
        ensure_private_dir(&root).unwrap();
        let mut threads = Vec::new();
        for index in 0..12 {
            let root = root.clone();
            let dir = temp.path().join(format!("custom-{index}"));
            threads.push(std::thread::spawn(move || {
                register_dir_at(&root, &dir).unwrap()
            }));
        }
        for thread in threads {
            assert!(thread.join().unwrap());
        }
        let entries = read_registry(&root.join("registry.json")).unwrap();
        assert_eq!(
            entries
                .iter()
                .filter(|entry| matches!(entry, RegistryEntry::Path(_)))
                .count(),
            12
        );
    }

    #[test]
    fn ordinary_registry_updates_preserve_and_reject_malformed_entries() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("registry");
        ensure_private_dir(&root).unwrap();
        let existing = temp.path().join("existing");
        write_json_atomic(&root, "registry.json", &json!({"paths": [existing, 7]})).unwrap();
        let before = fs::read(root.join("registry.json")).unwrap();

        let register_err = register_dir_at(&root, &temp.path().join("new")).unwrap_err();
        assert!(format!("{register_err:#}").contains("rdny cleanup"));
        assert_eq!(fs::read(root.join("registry.json")).unwrap(), before);

        let unregister_err = unregister_dir_at(&root, &existing).unwrap_err();
        assert!(format!("{unregister_err:#}").contains("rdny cleanup"));
        assert_eq!(fs::read(root.join("registry.json")).unwrap(), before);

        prune_registry_at(&root, |path| path == existing).unwrap();
        assert_eq!(
            read_registry(&root.join("registry.json")).unwrap(),
            vec![RegistryEntry::Path(existing)]
        );
    }

    #[test]
    fn relative_state_dir_resolves_to_stable_absolute_path() {
        let temp = tempfile::tempdir().unwrap();
        let before = env::current_dir().unwrap();
        env::set_current_dir(temp.path()).unwrap();
        let cwd = env::current_dir().unwrap();
        let resolved =
            resolve_state_dir(Some("custom/../state"), None::<PathBuf>, None::<PathBuf>).unwrap();
        env::set_current_dir(&before).unwrap();
        assert_eq!(resolved, cwd.join("state"));
    }

    #[test]
    fn state_mutation_does_not_register_until_lifecycle_save() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("state");
        let registry = temp.path().join("registry");
        ensure_private_dir(&registry).unwrap();
        let tx = Transaction::begin_in(&dir, Duration::from_secs(1)).unwrap();
        tx.save(&sample_state()).unwrap();
        assert!(
            read_registry(&registry.join("registry.json"))
                .unwrap()
                .is_empty()
        );
        register_dir_at(&registry, &dir).unwrap();
        assert_eq!(
            read_registry(&registry.join("registry.json")).unwrap(),
            vec![RegistryEntry::Path(dir)]
        );
    }

    #[test]
    fn targeted_unregister_preserves_concurrent_registration() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("registry");
        ensure_private_dir(&root).unwrap();
        let stale = temp.path().join("stale");
        let fresh = temp.path().join("fresh");
        register_dir_at(&root, &stale).unwrap();
        register_dir_at(&root, &fresh).unwrap();
        unregister_dir_at(&root, &stale).unwrap();
        assert_eq!(
            read_registry(&root.join("registry.json")).unwrap(),
            vec![RegistryEntry::Path(fresh)]
        );
    }

    #[test]
    fn recursively_created_private_directories_are_mode_0700() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let registry = state.join("registry");
        let profile = state.join("chrome-profile");
        let frames = state.join("frames");
        for dir in [&state, &registry, &profile, &frames] {
            ensure_private_dir(dir).unwrap();
            assert_eq!(fs::metadata(dir).unwrap().mode() & 0o777, 0o700);
        }
    }
}
