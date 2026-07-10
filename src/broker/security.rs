use anyhow::{Context, Result, bail};
use std::{
    fs,
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
};

use crate::state::SecureDir;

pub(crate) trait CredentialProvider: Send + Sync {
    fn current_uid(&self) -> u32;
}
#[derive(Debug, Default)]
pub(crate) struct OsCredentialProvider;
impl CredentialProvider for OsCredentialProvider {
    fn current_uid(&self) -> u32 {
        unsafe { libc::geteuid() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PeerCredentials {
    pub uid: u32,
    pub gid: u32,
    pub pid: Option<u32>,
}

pub(crate) fn socket_path(dir: &SecureDir, instance_id: &str) -> Result<PathBuf> {
    if !instance_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("invalid broker instance id");
    }
    // The full lifecycle id remains in the authenticated handshake/state. A
    // The compact 64-bit pathname suffix keeps normal macOS state roots below
    // sun_path's unusually small limit. A collision fails closed; the full id
    // and 256-bit token remain the authentication boundary.
    let socket_id: String = instance_id.chars().take(16).collect();
    let path = dir.path().join(format!("b-{socket_id}.s"));
    let bytes = path.as_os_str().as_encoded_bytes().len();
    if bytes >= sockaddr_un_path_cap() {
        bail!(
            "broker socket path is {bytes} bytes but this platform permits fewer than {} bytes; choose a shorter absolute --state-dir or RDNY_STATE_DIR (no TCP fallback is used)",
            sockaddr_un_path_cap()
        );
    }
    Ok(path)
}

fn sockaddr_un_path_cap() -> usize {
    std::mem::size_of::<libc::sockaddr_un>() - 2
}

pub(crate) fn bind(
    dir: &SecureDir,
    instance_id: &str,
    creds: &dyn CredentialProvider,
) -> Result<(UnixListener, PathBuf)> {
    dir.validate_external_path()?;
    let path = socket_path(dir, instance_id)?;
    // A pathname here is ownership evidence, not disposable debris. Lifecycle
    // code must explicitly validate and remove stale sockets before spawning.
    if fs::symlink_metadata(&path).is_ok() {
        bail!(
            "broker socket already exists at {}; run `rdny status` and `rdny cleanup` after verifying the recorded broker identity",
            path.display()
        );
    }
    let old = unsafe { libc::umask(0o177) };
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("binding broker socket {}", path.display()));
    unsafe { libc::umask(old) };
    let listener = listener?;
    if let Err(error) = validate_socket_path(&path, creds) {
        let _ = fs::remove_file(&path);
        return Err(error);
    }
    Ok((listener, path))
}

pub(crate) fn connect(path: &Path, creds: &dyn CredentialProvider) -> Result<UnixStream> {
    validate_socket_path(path, creds)?;
    let stream = UnixStream::connect(path).context("connecting broker socket")?;
    validate_socket_path(path, creds)?;
    Ok(stream)
}

pub(crate) fn verify_peer(
    stream: &UnixStream,
    expected_uid: u32,
    expected_pid: Option<u32>,
) -> Result<()> {
    let peer = peer_credentials(stream)?;
    if peer.uid != expected_uid {
        bail!("broker peer UID mismatch");
    }
    if let (Some(expected), Some(actual)) = (expected_pid, peer.pid)
        && expected != actual
    {
        bail!("broker peer PID mismatch");
    }
    Ok(())
}

pub(crate) fn validate_socket_path(path: &Path, creds: &dyn CredentialProvider) -> Result<()> {
    let md = fs::symlink_metadata(path).context("stat broker socket")?;
    if !md.file_type().is_socket() {
        bail!("broker path is not a socket");
    }
    // Mode 0600 makes the owning UID the authorization boundary. macOS may
    // assign a state-directory socket the directory's inherited group (for
    // example wheel) rather than the process egid, so group equality is not a
    // portable security invariant.
    if md.uid() != creds.current_uid() {
        bail!("broker socket owner mismatch");
    }
    if md.permissions().mode() & 0o777 != 0o600 {
        bail!("broker socket mode must be 0600");
    }
    Ok(())
}

pub(crate) fn peer_credentials(stream: &UnixStream) -> Result<PeerCredentials> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let mut ucred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut ucred as *mut _ as *mut _,
                &mut len,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("SO_PEERCRED");
        }
        Ok(PeerCredentials {
            uid: ucred.uid,
            gid: ucred.gid,
            pid: Some(ucred.pid as u32),
        })
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        let mut uid = 0;
        let mut gid = 0;
        let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("getpeereid");
        }
        Ok(PeerCredentials {
            uid,
            gid,
            pid: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::net::UnixListener, thread};

    struct FakeCreds {
        uid: u32,
    }
    impl CredentialProvider for FakeCreds {
        fn current_uid(&self) -> u32 {
            self.uid
        }
    }
    fn real() -> FakeCreds {
        FakeCreds {
            uid: unsafe { libc::geteuid() },
        }
    }

    #[test]
    fn rejects_non_socket_and_long_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not.sock");
        fs::write(&file, b"x").unwrap();
        assert!(validate_socket_path(&file, &real()).is_err());
        let long = dir.path().join("x".repeat(200));
        assert!(
            long.as_os_str().as_encoded_bytes().len() >= sockaddr_un_path_cap()
                || UnixListener::bind(&long).is_err()
        );
    }

    #[test]
    fn validates_owner_mode_type_and_peer_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        validate_socket_path(&path, &real()).unwrap();
        assert!(
            validate_socket_path(
                &path,
                &FakeCreds {
                    uid: real().uid + 1,
                }
            )
            .is_err()
        );
        let t = thread::spawn(move || peer_credentials(&listener.accept().unwrap().0).unwrap());
        let stream = UnixStream::connect(&path).unwrap();
        let server_seen = t.join().unwrap();
        let client_seen = peer_credentials(&stream).unwrap();
        assert_eq!(server_seen.uid, real().uid);
        assert_eq!(client_seen.uid, real().uid);
    }

    #[test]
    fn bind_never_unlinks_an_existing_socket() {
        let temp = tempfile::tempdir().unwrap();
        let storage = crate::state::browser_storage_at(temp.path()).unwrap();
        let (_, path) = bind(&storage.profile, "instance", &real()).unwrap();
        assert!(bind(&storage.profile, "instance", &real()).is_err());
        assert!(path.exists());
    }

    #[test]
    fn overlong_state_path_is_rejected_before_bind() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("x".repeat(100));
        let storage = crate::state::browser_storage_at(&root).unwrap();
        let error = socket_path(&storage.profile, "instance").unwrap_err();
        assert!(format!("{error:#}").contains("shorter absolute --state-dir"));
    }
}
