use std::{
    ffi::{CStr, CString, OsStr},
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::ffi::OsStrExt,
        unix::fs::{MetadataExt, PermissionsExt},
    },
    path::{Component, Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
static UNIQUE: AtomicU64 = AtomicU64::new(0);
static INITIAL_CWD: OnceLock<PathBuf> = OnceLock::new();
#[cfg(target_os = "macos")]
static UMASK_LOCK: Mutex<()> = Mutex::new(());

pub(super) fn capture_initial_cwd() -> Result<()> {
    if INITIAL_CWD.get().is_none() {
        let cwd = std::env::current_dir()?;
        let _ = INITIAL_CWD.set(cwd);
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) enum Inspection<T> {
    Missing,
    Valid(T, Generation),
    Malformed(Malformed),
    Incompatible(String, Generation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Generation(Vec<u8>);

#[derive(Debug)]
pub(crate) struct Malformed {
    generation: Generation,
    message: String,
}

impl Malformed {
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

pub(crate) struct StateStore {
    root: PathBuf,
    dir: File,
}

/// A directory kept open so all creation and writes remain relative to the
/// validated inode. The path is only for programs (Chrome/ffmpeg) that require
/// a pathname; rdny itself never uses it to create children.
pub(crate) struct SecureDir {
    state_root: PathBuf,
    path: PathBuf,
    dir: File,
}

pub(crate) struct AdvisoryLock(File);

impl SecureDir {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Validate the cross-UID trust boundary required before exposing a real
    /// path to Chrome or ffmpeg. Same-UID mutation is deliberately outside
    /// this policy; lifecycle serialization remains #130/#131 work.
    pub(crate) fn validate_external_path(&self) -> Result<()> {
        validate_external_dir_path(&self.state_root, &self.path)
    }

    pub(crate) fn subdir(&self, name: &str) -> Result<SecureDir> {
        validate_name(name)?;
        let dir = open_or_create_dir(self.dir.as_raw_fd(), name)?;
        Ok(SecureDir {
            state_root: self.state_root.clone(),
            path: self.path.join(name),
            dir,
        })
    }

    pub(crate) fn create_subdir_exclusive(&self, name: &str) -> Result<Option<SecureDir>> {
        validate_name(name)?;
        match mkdirat(self.dir.as_raw_fd(), name, 0o700) {
            Ok(()) => {
                let dir = file_from_openat(
                    self.dir.as_raw_fd(),
                    name,
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                    0,
                )?;
                validate_owned_directory(&dir)?;
                self.dir.sync_all()?;
                Ok(Some(SecureDir {
                    state_root: self.state_root.clone(),
                    path: self.path.join(name),
                    dir,
                }))
            }
            Err(err) if err.raw_os_error() == Some(libc::EEXIST) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    pub(crate) fn remove_flat_subdir(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        let dir = match file_from_openat(
            self.dir.as_raw_fd(),
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        ) {
            Ok(v) => v,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        validate_owned_directory(&dir)?;
        let child = SecureDir {
            state_root: self.state_root.clone(),
            path: self.path.join(name),
            dir,
        };
        for entry in child.names()? {
            let file = file_from_openat(
                child.dir.as_raw_fd(),
                &entry,
                libc::O_RDONLY | libc::O_NOFOLLOW,
                0,
            )?;
            validate_regular(&file, 0o600)?;
            drop(file);
            unlinkat(child.dir.as_raw_fd(), &entry, 0)?;
        }
        drop(child);
        unlinkat(self.dir.as_raw_fd(), name, libc::AT_REMOVEDIR)?;
        self.dir.sync_all()?;
        Ok(())
    }

    pub(crate) fn create_file(&self, name: &str, truncate: bool) -> Result<File> {
        validate_name(name)?;
        let mut flags = libc::O_WRONLY | libc::O_CREAT | libc::O_NOFOLLOW;
        if truncate {
            flags |= libc::O_TRUNC;
        }
        let file = file_from_openat(self.dir.as_raw_fd(), name, flags, 0o600)
            .with_context(|| format!("opening secure file {name}"))?;
        validate_regular(&file, 0o600)?;
        Ok(file)
    }

    pub(crate) fn open_file(&self, name: &str) -> Result<File> {
        validate_name(name)?;
        let file = file_from_openat(
            self.dir.as_raw_fd(),
            name,
            libc::O_RDONLY | libc::O_NOFOLLOW,
            0,
        )?;
        validate_regular(&file, 0o600)?;
        Ok(file)
    }

    pub(crate) fn read_string(&self, name: &str) -> Result<String> {
        let mut file = self.open_file(name)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        Ok(contents)
    }

    pub(crate) fn write_file(&self, name: &str, bytes: &[u8]) -> Result<()> {
        let mut file = self.create_file(name, true)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    }

    pub(crate) fn remove_file(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        match unlinkat(self.dir.as_raw_fd(), name, 0) {
            Ok(()) => Ok(()),
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    pub(crate) fn try_advisory_lock(&self, name: &str) -> Result<Option<AdvisoryLock>> {
        validate_name(name)?;
        let file = file_from_openat(
            self.dir.as_raw_fd(),
            name,
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW,
            0o600,
        )?;
        validate_regular(&file, 0o600)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            Ok(Some(AdvisoryLock(file)))
        } else {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                Ok(None)
            } else {
                Err(err.into())
            }
        }
    }

    pub(crate) fn regular_paths_with_suffix(&self, suffix: &str) -> Result<Vec<PathBuf>> {
        let mut paths = Vec::new();
        for name in self.names()? {
            if !name.ends_with(suffix) {
                continue;
            }
            let file = file_from_openat(
                self.dir.as_raw_fd(),
                &name,
                libc::O_RDONLY | libc::O_NOFOLLOW,
                0,
            )?;
            validate_regular(&file, 0o600)?;
            paths.push(self.path.join(name));
        }
        Ok(paths)
    }

    fn names(&self) -> Result<Vec<String>> {
        let fd = unsafe { libc::dup(self.dir.as_raw_fd()) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let raw = unsafe { libc::fdopendir(fd) };
        if raw.is_null() {
            unsafe {
                libc::close(fd);
            }
            return Err(std::io::Error::last_os_error().into());
        }
        let dir = Dir(raw);
        let mut names = Vec::new();
        loop {
            clear_errno();
            let entry = unsafe { libc::readdir(dir.0) };
            if entry.is_null() {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error().unwrap_or(0) != 0 {
                    return Err(err.into());
                }
                break;
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_string_lossy();
            if name != "." && name != ".." {
                names.push(name.into_owned());
            }
        }
        Ok(names)
    }
}

impl Drop for AdvisoryLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

struct Dir(*mut libc::DIR);
impl Drop for Dir {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}

fn clear_errno() {
    #[cfg(target_os = "macos")]
    unsafe {
        *libc::__error() = 0;
    }
    #[cfg(not(target_os = "macos"))]
    unsafe {
        *libc::__errno_location() = 0;
    }
}

impl StateStore {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        open_secure_dir(path, true)
    }
    pub(crate) fn open_existing(path: &Path) -> Result<Self> {
        open_secure_dir(path, false)
    }
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn subdir(&self, name: &str) -> Result<SecureDir> {
        validate_name(name)?;
        let dir = open_or_create_dir(self.dir.as_raw_fd(), name)?;
        Ok(SecureDir {
            state_root: self.root.clone(),
            path: self.root.join(name),
            dir,
        })
    }

    pub(crate) fn create_file(&self, name: &str, truncate: bool) -> Result<File> {
        SecureDir {
            state_root: self.root.clone(),
            path: self.root.clone(),
            dir: self.dir.try_clone()?,
        }
        .create_file(name, truncate)
    }

    pub(crate) fn inspect<T: DeserializeOwned>(&self, name: &str) -> Result<Inspection<T>> {
        let Some(raw) = self.read_bytes(name)? else {
            return Ok(Inspection::Missing);
        };
        let generation = Generation(raw.clone());
        match serde_json::from_slice(&raw) {
            Ok(value) => Ok(Inspection::Valid(value, generation)),
            Err(err) if err.is_data() => Ok(Inspection::Incompatible(err.to_string(), generation)),
            Err(err) => Ok(Inspection::Malformed(Malformed {
                generation,
                message: err.to_string(),
            })),
        }
    }

    pub(crate) fn read_json<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>> {
        match self.inspect(name)? {
            Inspection::Missing => Ok(None),
            Inspection::Valid(v, _) => Ok(Some(v)),
            Inspection::Malformed(v) => {
                bail!("state file {name} contains malformed JSON: {}", v.message)
            }
            Inspection::Incompatible(e, _) => {
                bail!("state file {name} has incompatible schema: {e}")
            }
        }
    }

    fn read_value(&self, name: &str) -> Result<Option<Value>> {
        self.read_json(name)
    }

    fn read_bytes(&self, name: &str) -> Result<Option<Vec<u8>>> {
        validate_name(name)?;
        let mut file = match file_from_openat(
            self.dir.as_raw_fd(),
            name,
            libc::O_RDONLY | libc::O_NOFOLLOW,
            0,
        ) {
            Ok(file) => file,
            Err(err) if err.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("opening state file {name}")),
        };
        validate_regular(&file, 0o600).with_context(|| format!("validating state file {name}"))?;
        let mut raw = Vec::new();
        file.read_to_end(&mut raw)
            .with_context(|| format!("reading state file {name}"))?;
        Ok(Some(raw))
    }

    pub(crate) fn transaction<F>(&self, name: &str, timeout: Duration, update: F) -> Result<()>
    where
        F: FnOnce(Option<Value>) -> Result<Value>,
    {
        validate_name(name)?;
        let _lock = self.lock(timeout)?;
        let next = update(self.read_value(name)?)?;
        self.write_json_locked(name, &next, FailurePoint::None)
    }

    pub(crate) fn transaction_optional<F>(
        &self,
        name: &str,
        timeout: Duration,
        update: F,
    ) -> Result<()>
    where
        F: FnOnce(Option<Value>) -> Result<Option<Value>>,
    {
        validate_name(name)?;
        let _lock = self.lock(timeout)?;
        if let Some(next) = update(self.read_value(name)?)? {
            self.write_json_locked(name, &next, FailurePoint::None)?;
        }
        Ok(())
    }

    pub(crate) fn write_json<T: Serialize>(&self, name: &str, value: &T) -> Result<()> {
        let _lock = self.lock(DEFAULT_TIMEOUT)?;
        self.write_json_locked(name, value, FailurePoint::None)
    }

    fn write_json_locked<T: Serialize>(
        &self,
        name: &str,
        value: &T,
        fail: FailurePoint,
    ) -> Result<()> {
        validate_name(name)?;
        let raw = serde_json::to_vec_pretty(value).context("serializing state JSON")?;
        let tmp = unique_name(&format!(".{name}.tmp"));
        let mut file = file_from_openat(
            self.dir.as_raw_fd(),
            &tmp,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            0o600,
        )
        .with_context(|| format!("creating temp state file {tmp}"))?;
        let result = (|| -> Result<()> {
            validate_regular(&file, 0o600)?;
            fail.check(FailurePoint::AfterCreate)?;
            file.write_all(&raw)?;
            fail.check(FailurePoint::AfterWrite)?;
            file.sync_all()?;
            fail.check(FailurePoint::AfterFileSync)?;
            drop(file);
            fail.check(FailurePoint::BeforeRename)?;
            renameat(self.dir.as_raw_fd(), &tmp, self.dir.as_raw_fd(), name)?;
            fail.check(FailurePoint::AfterRename)?;
            self.dir.sync_all().context("syncing state directory")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = unlinkat(self.dir.as_raw_fd(), &tmp, 0);
        }
        result
    }

    pub(crate) fn remove(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        let _lock = self.lock(DEFAULT_TIMEOUT)?;
        match unlinkat(self.dir.as_raw_fd(), name, 0) {
            Ok(()) => self.dir.sync_all().context("syncing state directory"),
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing state file {name}")),
        }
    }

    pub(crate) fn remove_if_generation<T, F>(
        &self,
        name: &str,
        observed: &Generation,
        predicate: F,
    ) -> Result<bool>
    where
        T: DeserializeOwned,
        F: FnOnce(&T) -> Result<bool>,
    {
        validate_name(name)?;
        let _lock = self.lock(DEFAULT_TIMEOUT)?;
        let Some(raw) = self.read_bytes(name)? else {
            return Ok(false);
        };
        if raw != observed.0 {
            return Ok(false);
        }
        let value: T = serde_json::from_slice(&raw).context("revalidating state before removal")?;
        if !predicate(&value)? {
            return Ok(false);
        }
        unlinkat(self.dir.as_raw_fd(), name, 0)?;
        self.dir.sync_all()?;
        Ok(true)
    }

    pub(crate) fn remove_subdir(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        let _lock = self.lock(DEFAULT_TIMEOUT)?;
        let dir = match file_from_openat(
            self.dir.as_raw_fd(),
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        ) {
            Ok(v) => v,
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let child = SecureDir {
            state_root: self.root.clone(),
            path: self.root.join(name),
            dir,
        };
        for entry in child.names()? {
            // The frames directory is deliberately flat. Refuse unexpected
            // directories rather than recursively traversing attacker input.
            let file = file_from_openat(
                child.dir.as_raw_fd(),
                &entry,
                libc::O_RDONLY | libc::O_NOFOLLOW,
                0,
            )?;
            validate_regular(&file, 0o600)?;
            drop(file);
            unlinkat(child.dir.as_raw_fd(), &entry, 0)?;
        }
        drop(child);
        unlinkat(self.dir.as_raw_fd(), name, libc::AT_REMOVEDIR)?;
        self.dir.sync_all()?;
        Ok(())
    }

    pub(crate) fn quarantine_malformed(
        &self,
        name: &str,
        observed: &Malformed,
    ) -> Result<Option<PathBuf>> {
        validate_name(name)?;
        let _lock = self.lock(DEFAULT_TIMEOUT)?;
        let Some(current) = self.read_bytes(name)? else {
            return Ok(None);
        };
        if current != observed.generation.0 {
            bail!("state changed since inspection; refusing to quarantine")
        }
        // The rename itself is atomic and no-replace. If the process crashes
        // after rename but before directory fsync, either the source or the
        // quarantine name may be visible after reboot, never an overwritten
        // pre-existing quarantine file.
        for _ in 0..100 {
            let dest = unique_name(&format!("{name}.quarantine"));
            match rename_noreplace(self.dir.as_raw_fd(), name, &dest) {
                Ok(()) => {
                    self.dir
                        .sync_all()
                        .context("syncing state directory after quarantine")?;
                    return Ok(Some(self.root.join(dest)));
                }
                Err(e) if e.raw_os_error() == Some(libc::EEXIST) => continue,
                Err(e) => return Err(e).context("atomically renaming malformed state"),
            }
        }
        bail!("could not allocate unique quarantine name")
    }

    fn lock(&self, timeout: Duration) -> Result<LockGuard> {
        let file = file_from_openat(
            self.dir.as_raw_fd(),
            ".lock",
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW,
            0o600,
        )
        .context("opening state lock file")?;
        validate_regular(&file, 0o600).context("validating state lock file")?;
        let start = Instant::now();
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(LockGuard(file));
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EWOULDBLOCK)
                && err.raw_os_error() != Some(libc::EINTR)
            {
                return Err(err).context("locking state");
            }
            let elapsed = start.elapsed();
            let Some(remaining) = timeout.checked_sub(elapsed) else {
                bail!(
                    "timed out after {timeout:?} waiting for rdny state lock; another rdny process may be updating state"
                )
            };
            if remaining.is_zero() {
                bail!(
                    "timed out after {timeout:?} waiting for rdny state lock; another rdny process may be updating state"
                )
            }
            thread::sleep(remaining.min(Duration::from_millis(25)));
        }
    }
}

struct LockGuard(File);
impl Drop for LockGuard {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub(super) fn normalize_absolute(path: &Path) -> Result<PathBuf> {
    capture_initial_cwd()?;
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        INITIAL_CWD.get().expect("initial cwd captured").join(path)
    };
    let mut parts: Vec<&OsStr> = Vec::new();
    for component in absolute.components() {
        match component {
            Component::RootDir => parts.clear(),
            Component::CurDir => {}
            Component::Normal(p) => parts.push(p),
            Component::ParentDir => {
                if parts.pop().is_none() {
                    bail!("state path escapes filesystem root")
                }
            }
            Component::Prefix(_) => bail!("unsupported state path prefix"),
        }
    }
    let mut out = PathBuf::from("/");
    for p in parts {
        out.push(p);
    }
    #[cfg(target_os = "macos")]
    if out.starts_with("/var") || out.starts_with("/tmp") {
        out = Path::new("/private").join(out.strip_prefix("/").expect("absolute path"));
    }
    Ok(out)
}

fn open_secure_dir(path: &Path, create: bool) -> Result<StateStore> {
    let root = normalize_absolute(path)?;
    let mut current = file_from_open(
        "/",
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        0,
    )?;
    for component in root.components() {
        let Component::Normal(part) = component else {
            continue;
        };
        validate_component(part)?;
        let name = part.to_str().context("state path must be valid UTF-8")?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;
        let (next, created) = match file_from_openat(current.as_raw_fd(), name, flags, 0) {
            Ok(dir) => (dir, false),
            Err(e) if create && e.raw_os_error() == Some(libc::ENOENT) => {
                mkdir_private(current.as_raw_fd(), name).with_context(|| {
                    format!(
                        "creating state directory component {}",
                        part.to_string_lossy()
                    )
                })?;
                // Open immediately and perform every subsequent operation on
                // the descriptor, never on a followable pathname.
                #[cfg(target_os = "macos")]
                {
                    // An extreme umask can create mode 000. O_EVTONLY still
                    // gives us an inode-bound descriptor so we can fchmod it
                    // without ever following the name.
                    let inode = file_from_openat(
                        current.as_raw_fd(),
                        name,
                        libc::O_EVTONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                        0,
                    )?;
                    validate_owned_directory(&inode)?;
                    inode.set_permissions(std::fs::Permissions::from_mode(0o700))?;
                }
                #[cfg(not(target_os = "macos"))]
                {
                    // O_PATH bypasses mode-000 lookup restrictions. chmod via
                    // procfs addresses that open descriptor, not the original
                    // replaceable directory name.
                    let inode = file_from_openat(
                        current.as_raw_fd(),
                        name,
                        libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                        0,
                    )?;
                    validate_owned_directory(&inode)?;
                    let fd_path = format!("/proc/self/fd/{}", inode.as_raw_fd());
                    let rc = unsafe { libc::chmod(cstr(&fd_path)?.as_ptr(), 0o700) };
                    if rc != 0 {
                        return Err(std::io::Error::last_os_error().into());
                    }
                }
                (file_from_openat(current.as_raw_fd(), name, flags, 0)?, true)
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "opening secure state directory component {}",
                        part.to_string_lossy()
                    )
                });
            }
        };
        validate_directory(&next)?;
        if created {
            validate_owned_directory(&next)?;
            next.set_permissions(std::fs::Permissions::from_mode(0o700))?;
        }
        current = next;
    }
    validate_owned_directory(&current)?;
    current.set_permissions(std::fs::Permissions::from_mode(0o700))?;
    Ok(StateStore { root, dir: current })
}

fn validate_external_dir_path(state_root: &Path, target: &Path) -> Result<()> {
    let state_root = normalize_absolute(state_root)?;
    let target = normalize_absolute(target)?;
    if !target.starts_with(&state_root) {
        bail!("external path is outside the state root")
    }
    let euid = unsafe { libc::geteuid() };
    let mut current = file_from_open(
        "/",
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        0,
    )?;
    let mut built = PathBuf::from("/");
    validate_trusted_ancestor(&current, &built, false, euid)?;
    for component in target.components() {
        let Component::Normal(part) = component else {
            continue;
        };
        let name = part.to_str().context("external path must be valid UTF-8")?;
        current = file_from_openat(
            current.as_raw_fd(),
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )
        .with_context(|| {
            format!(
                "opening trusted external ancestor {}",
                built.join(part).display()
            )
        })?;
        built.push(part);
        let private = built == state_root || built == target;
        validate_trusted_ancestor(&current, &built, private, euid)?;
    }
    Ok(())
}

fn validate_trusted_ancestor(file: &File, path: &Path, private: bool, euid: u32) -> Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_dir() {
        bail!(
            "external path ancestor {} is not a directory",
            path.display()
        )
    }
    let uid = metadata.uid();
    if uid != 0 && uid != euid {
        bail!(
            "external path ancestor {} is owned by untrusted uid {uid}",
            path.display()
        )
    }
    let mode = metadata.mode();
    let writable = mode & 0o022 != 0;
    let root_sticky = uid == 0 && mode & libc::S_ISVTX as u32 != 0;
    if writable && !root_sticky {
        bail!(
            "external path ancestor {} is group/other writable",
            path.display()
        )
    }
    if private && (uid != euid || mode & 0o777 != 0o700) {
        bail!(
            "external state directory {} must be current-user-owned mode 0700",
            path.display()
        )
    }
    Ok(())
}

fn open_or_create_dir(parent: RawFd, name: &str) -> Result<File> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;
    let dir = match file_from_openat(parent, name, flags, 0) {
        Ok(v) => v,
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
            mkdirat(parent, name, 0o700)?;
            file_from_openat(parent, name, flags, 0)?
        }
        Err(e) => return Err(e.into()),
    };
    validate_owned_directory(&dir)?;
    dir.set_permissions(std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.as_bytes().contains(&0) {
        bail!("store name must be one normal path component")
    }
    let mut c = Path::new(name).components();
    if !matches!(c.next(), Some(Component::Normal(_))) || c.next().is_some() {
        bail!("store name must be one normal path component")
    }
    Ok(())
}
fn validate_component(name: &OsStr) -> Result<()> {
    if name.as_bytes().is_empty()
        || name.as_bytes().contains(&0)
        || name == "."
        || name == ".."
        || name.as_bytes().contains(&b'/')
    {
        bail!("state path contains invalid component")
    }
    Ok(())
}
fn validate_directory(file: &File) -> Result<()> {
    let st = file.metadata()?;
    if !st.is_dir() {
        bail!("not a directory")
    }
    Ok(())
}
fn validate_owned_directory(file: &File) -> Result<()> {
    validate_directory(file)?;
    let st = file.metadata()?;
    if st.uid() != unsafe { libc::geteuid() } {
        bail!("refusing directory owned by uid {}", st.uid())
    }
    Ok(())
}
fn validate_regular(file: &File, mode: u32) -> Result<()> {
    let st = file.metadata()?;
    if !st.is_file() {
        bail!("not a regular file")
    }
    if st.nlink() != 1 {
        bail!("refusing hard-linked file")
    }
    if st.uid() != unsafe { libc::geteuid() } {
        bail!("refusing file owned by uid {}", st.uid())
    }
    file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn cstr(s: &str) -> std::io::Result<CString> {
    CString::new(s).map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))
}
fn file_from_open(path: &str, flags: i32, mode: u32) -> std::io::Result<File> {
    let fd = unsafe { libc::open(cstr(path)?.as_ptr(), flags, mode) };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}
fn file_from_openat(dir: RawFd, name: &str, flags: i32, mode: u32) -> std::io::Result<File> {
    let fd = unsafe { libc::openat(dir, cstr(name)?.as_ptr(), flags, mode) };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}
fn mkdirat(dir: RawFd, name: &str, mode: u32) -> std::io::Result<()> {
    if unsafe { libc::mkdirat(dir, cstr(name)?.as_ptr(), mode as libc::mode_t) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}
fn mkdir_private(dir: RawFd, name: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        // macOS has no O_PATH/fchmodat(AT_EMPTY_PATH). Requesting only 0700
        // makes this umask-neutral window non-permissive; the mutex prevents
        // concurrent rdny creations from observing the temporary umask.
        let _guard = UMASK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = unsafe { libc::umask(0) };
        let result = mkdirat(dir, name, 0o700);
        unsafe {
            libc::umask(old);
        }
        result
    }
    #[cfg(not(target_os = "macos"))]
    {
        mkdirat(dir, name, 0o700)
    }
}
fn renameat(a: RawFd, old: &str, b: RawFd, new: &str) -> std::io::Result<()> {
    if unsafe { libc::renameat(a, cstr(old)?.as_ptr(), b, cstr(new)?.as_ptr()) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}
fn unlinkat(dir: RawFd, name: &str, flags: i32) -> std::io::Result<()> {
    if unsafe { libc::unlinkat(dir, cstr(name)?.as_ptr(), flags) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}
#[cfg(target_os = "macos")]
fn rename_noreplace(dir: RawFd, old: &str, new: &str) -> std::io::Result<()> {
    if unsafe {
        libc::renameatx_np(
            dir,
            cstr(old)?.as_ptr(),
            dir,
            cstr(new)?.as_ptr(),
            libc::RENAME_EXCL,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}
#[cfg(target_os = "linux")]
fn rename_noreplace(dir: RawFd, old: &str, new: &str) -> std::io::Result<()> {
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            dir,
            cstr(old)?.as_ptr(),
            dir,
            cstr(new)?.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn rename_noreplace(_dir: RawFd, _old: &str, _new: &str) -> std::io::Result<()> {
    Err(std::io::Error::from_raw_os_error(libc::ENOTSUP))
}
fn unique_name(prefix: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = UNIQUE.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}.{}.{}.{}", std::process::id(), now, seq)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FailurePoint {
    None,
    AfterCreate,
    AfterWrite,
    AfterFileSync,
    BeforeRename,
    AfterRename,
}
impl FailurePoint {
    fn check(self, at: Self) -> Result<()> {
        if self == at {
            bail!("injected atomic write failure")
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::{fs, os::unix::fs::symlink, process::Command, sync::Arc, time::Instant};

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir_in(".").unwrap()
    }

    #[test]
    fn process_helper() {
        let Some(root) = std::env::var_os("RDNY_STATE_PROCESS_HELPER") else {
            return;
        };
        let Some(barrier) = std::env::var_os("RDNY_STATE_BARRIER") else {
            return;
        };
        let id = std::env::var("RDNY_STATE_PROCESS_ID").unwrap();
        fs::write(
            Path::new(&barrier).with_extension(format!("ready-{id}")),
            b"ready",
        )
        .unwrap();
        while !Path::new(&barrier).exists() {
            thread::sleep(Duration::from_millis(2));
        }
        StateStore::open(Path::new(&root))
            .unwrap()
            .transaction("state.json", Duration::from_secs(3), |cur| {
                let mut v = cur.unwrap();
                v["count"] = (v["count"].as_u64().unwrap() + 1).into();
                thread::sleep(Duration::from_millis(30));
                Ok(v)
            })
            .unwrap();
    }

    #[test]
    fn umask_helper() {
        let Some(root) = std::env::var_os("RDNY_STATE_UMASK_HELPER") else {
            return;
        };
        unsafe {
            libc::umask(0o777);
        }
        StateStore::open(Path::new(&root))
            .unwrap()
            .write_json("state.json", &serde_json::json!({"a":1}))
            .unwrap();
    }

    #[test]
    fn names_reject_traversal() {
        let t = tempdir();
        let s = StateStore::open(t.path()).unwrap();
        for name in ["a/b", ".", "..", "a\0b", ""] {
            assert!(s.write_json(name, &1).is_err(), "{name:?}");
        }
    }

    #[test]
    fn relative_paths_are_normalized_not_skipped() {
        let cwd = std::env::current_dir().unwrap();
        let a = normalize_absolute(Path::new("../state")).unwrap();
        assert_eq!(a, cwd.parent().unwrap().join("state"));
        assert_eq!(
            normalize_absolute(Path::new("foo/../bar")).unwrap(),
            cwd.join("bar")
        );
        assert_eq!(
            normalize_absolute(Path::new("..")).unwrap(),
            cwd.parent().unwrap()
        );
    }

    #[test]
    fn lock_and_files_are_private_despite_umask_in_second_process() {
        let t = tempdir();
        let root = t.path().join("state");
        let status = Command::new(std::env::current_exe().unwrap())
            .arg("state::storage::tests::umask_helper")
            .arg("--exact")
            .env("RDNY_STATE_UMASK_HELPER", &root)
            .status()
            .unwrap();
        assert!(status.success());
        for (name, mode) in [("state.json", 0o600), (".lock", 0o600)] {
            assert_eq!(
                fs::metadata(root.join(name)).unwrap().permissions().mode() & 0o777,
                mode
            );
        }
    }

    #[test]
    fn refuses_symlink_and_hardlink_lock_and_state() {
        let t = tempdir();
        let s = StateStore::open(t.path()).unwrap();
        fs::write(t.path().join("target"), "{}").unwrap();
        symlink("target", t.path().join("state.json")).unwrap();
        assert!(s.read_value("state.json").is_err());
        fs::remove_file(t.path().join("state.json")).unwrap();
        fs::hard_link(t.path().join("target"), t.path().join("state.json")).unwrap();
        assert!(s.read_value("state.json").is_err());
        fs::hard_link(t.path().join("target"), t.path().join(".lock")).unwrap();
        assert!(s.write_json("other", &1).is_err());
    }

    #[test]
    fn descriptor_bound_subdir_rejects_name_replacement() {
        let t = tempdir();
        let s = StateStore::open(t.path()).unwrap();
        let held = s.subdir("frames").unwrap();
        fs::rename(t.path().join("frames"), t.path().join("moved")).unwrap();
        symlink("moved", t.path().join("frames")).unwrap();
        // Existing handle still targets the original inode; a fresh lookup
        // refuses the replacement symlink.
        held.write_file("frame.jpg", b"ok").unwrap();
        assert!(t.path().join("moved/frame.jpg").exists());
        assert!(s.subdir("frames").is_err());
    }

    #[test]
    fn external_path_accepts_root_sticky_and_user_private_chain() {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let store = StateStore::open(temp.path()).unwrap();
        let profile = store.subdir("profile").unwrap();
        profile.validate_external_path().unwrap();
    }

    #[test]
    fn external_path_rejects_writable_user_ancestor() {
        let temp = tempdir();
        let writable = temp.path().join("writable");
        fs::create_dir(&writable).unwrap();
        fs::set_permissions(&writable, fs::Permissions::from_mode(0o777)).unwrap();
        let store = StateStore::open(&writable.join("state")).unwrap();
        let profile = store.subdir("profile").unwrap();
        let err = profile.validate_external_path().unwrap_err().to_string();
        assert!(err.contains("group/other writable"), "{err}");
    }

    #[test]
    fn remove_waits_for_the_state_lock() {
        let t = tempdir();
        let s = Arc::new(StateStore::open(t.path()).unwrap());
        s.write_json("state.json", &1).unwrap();
        let guard = s.lock(Duration::from_secs(1)).unwrap();
        let worker = {
            let s = Arc::clone(&s);
            thread::spawn(move || s.remove("state.json").unwrap())
        };
        thread::sleep(Duration::from_millis(40));
        assert!(t.path().join("state.json").exists());
        drop(guard);
        worker.join().unwrap();
        assert!(!t.path().join("state.json").exists());
    }

    #[test]
    fn process_transactions_overlap_and_serialize() {
        let t = tempdir();
        let barrier = t.path().join("go");
        StateStore::open(t.path())
            .unwrap()
            .write_json("state.json", &serde_json::json!({"count":0}))
            .unwrap();
        let exe = std::env::current_exe().unwrap();
        let mut children: Vec<_> = (0..5)
            .map(|id| {
                Command::new(&exe)
                    .arg("state::storage::tests::process_helper")
                    .arg("--exact")
                    .env("RDNY_STATE_PROCESS_HELPER", t.path())
                    .env("RDNY_STATE_BARRIER", &barrier)
                    .env("RDNY_STATE_PROCESS_ID", id.to_string())
                    .spawn()
                    .unwrap()
            })
            .collect();
        while (0..5).any(|id| !barrier.with_extension(format!("ready-{id}")).exists()) {
            thread::sleep(Duration::from_millis(2));
        }
        fs::write(&barrier, "go").unwrap();
        for child in &mut children {
            assert!(child.wait().unwrap().success());
        }
        let v: Value = StateStore::open(t.path())
            .unwrap()
            .read_json("state.json")
            .unwrap()
            .unwrap();
        assert_eq!(v["count"], 5);
    }

    #[test]
    fn timeout_respects_remaining_duration() {
        let t = tempdir();
        let s = StateStore::open(t.path()).unwrap();
        let _g = s.lock(Duration::from_secs(1)).unwrap();
        let start = Instant::now();
        assert!(s.lock(Duration::from_millis(7)).is_err());
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(7));
        assert!(elapsed < Duration::from_millis(40), "{elapsed:?}");
    }

    #[test]
    fn failure_points_preserve_or_replace_and_clean_temp() {
        let t = tempdir();
        let s = StateStore::open(t.path()).unwrap();
        s.write_json("state.json", &serde_json::json!({"old":true}))
            .unwrap();
        for point in [
            FailurePoint::AfterCreate,
            FailurePoint::AfterWrite,
            FailurePoint::AfterFileSync,
            FailurePoint::BeforeRename,
        ] {
            let _g = s.lock(Duration::from_secs(1)).unwrap();
            assert!(
                s.write_json_locked("state.json", &serde_json::json!({"new":true}), point)
                    .is_err()
            );
            drop(_g);
            let v: Value = s.read_json("state.json").unwrap().unwrap();
            assert_eq!(v["old"], true);
        }
        let _g = s.lock(Duration::from_secs(1)).unwrap();
        assert!(
            s.write_json_locked(
                "state.json",
                &serde_json::json!({"new":true}),
                FailurePoint::AfterRename
            )
            .is_err()
        );
        drop(_g);
        let v: Value = s.read_json("state.json").unwrap().unwrap();
        assert_eq!(v["new"], true);
        assert!(
            !fs::read_dir(t.path())
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().contains(".tmp."))
        );
    }

    #[test]
    fn quarantine_requires_same_malformed_generation() {
        let t = tempdir();
        let s = StateStore::open(t.path()).unwrap();
        fs::write(t.path().join("state.json"), "{").unwrap();
        let Inspection::Malformed(bad) = s.inspect::<Value>("state.json").unwrap() else {
            panic!()
        };
        fs::write(t.path().join("state.json"), "[]").unwrap();
        assert!(s.quarantine_malformed("state.json", &bad).is_err());
        fs::write(t.path().join("state.json"), "{").unwrap();
        let q = s.quarantine_malformed("state.json", &bad).unwrap().unwrap();
        assert!(q.exists());
    }

    #[test]
    fn no_replace_quarantine_rename_preserves_collision() {
        let t = tempdir();
        let s = StateStore::open(t.path()).unwrap();
        fs::write(t.path().join("source"), b"malformed").unwrap();
        fs::write(t.path().join("destination"), b"existing").unwrap();
        let err = rename_noreplace(s.dir.as_raw_fd(), "source", "destination").unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EEXIST));
        assert_eq!(fs::read(t.path().join("source")).unwrap(), b"malformed");
        assert_eq!(fs::read(t.path().join("destination")).unwrap(), b"existing");
    }

    #[test]
    fn inspection_distinguishes_malformed_incompatible_and_unsafe() {
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct Schema {
            required: u64,
        }

        let t = tempdir();
        let s = StateStore::open(t.path()).unwrap();
        fs::write(t.path().join("state.json"), "{").unwrap();
        assert!(matches!(
            s.inspect::<Schema>("state.json").unwrap(),
            Inspection::Malformed(_)
        ));
        fs::write(t.path().join("state.json"), "{}").unwrap();
        assert!(matches!(
            s.inspect::<Schema>("state.json").unwrap(),
            Inspection::Incompatible(..)
        ));
        fs::remove_file(t.path().join("state.json")).unwrap();
        symlink("missing", t.path().join("state.json")).unwrap();
        assert!(s.inspect::<Schema>("state.json").is_err());
    }
}
