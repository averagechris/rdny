//! Focused managed-process identity and safe signaling.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_time: u64,
    pub exe: PathBuf,
    pub user_data_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub argv: Vec<OsString>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessClass {
    ManagedMatching,
    ManagedDead,
    PidReusedOrUnrelated,
    LegacyUnverifiable,
    AttachedReachable,
    AttachedDead,
}

pub fn validate_persisted(
    pid: Option<u32>,
    _browser_path: Option<&Path>,
    user_data_dir: Option<&Path>,
    id: &ProcessIdentity,
) -> Result<()> {
    if pid != Some(id.pid) {
        bail!("process_identity pid disagrees with state pid");
    }
    if user_data_dir.map(normalize) != id.user_data_dir.as_deref().map(normalize) {
        bail!("process_identity profile disagrees with user_data_dir");
    }
    validate_profile_arg(&id.argv, id.user_data_dir.as_deref())?;
    Ok(())
}

pub fn capture(
    pid: u32,
    _launcher_exe: &Path,
    user_data_dir: Option<&Path>,
) -> Result<ProcessIdentity> {
    let observed = observe(pid)?.context("launched browser exited before identity capture")?;
    if let Some(profile) = user_data_dir {
        validate_profile_arg(&observed.argv, Some(profile))?;
    }
    Ok(ProcessIdentity {
        pid,
        start_time: observed.start_time,
        exe: normalize(&observed.exe),
        user_data_dir: user_data_dir.map(Path::to_path_buf),
        argv: observed.argv,
    })
}

pub fn classify(
    pid: Option<u32>,
    id: Option<&ProcessIdentity>,
    port_reachable: bool,
) -> ProcessClass {
    match (pid, id) {
        (None, _) if port_reachable => ProcessClass::AttachedReachable,
        (None, _) => ProcessClass::AttachedDead,
        (Some(_), None) => ProcessClass::LegacyUnverifiable,
        (Some(pid), Some(id)) if pid != id.pid => ProcessClass::PidReusedOrUnrelated,
        (Some(pid), Some(id)) => match observe(pid) {
            Ok(None) => ProcessClass::ManagedDead,
            Ok(Some(_)) => match matches_identity(pid, id) {
                Ok(true) => ProcessClass::ManagedMatching,
                Ok(false) => ProcessClass::PidReusedOrUnrelated,
                Err(_) => ProcessClass::LegacyUnverifiable,
            },
            Err(_) => ProcessClass::LegacyUnverifiable,
        },
    }
}

pub fn matches_identity(pid: u32, id: &ProcessIdentity) -> Result<bool> {
    let Some(observed) = observe(pid)? else {
        return Ok(false);
    };
    Ok(observed.start_time == id.start_time
        && executable_matches(&observed.exe, &id.exe)
        && observed.argv == id.argv
        && validate_profile_arg(&observed.argv, id.user_data_dir.as_deref()).is_ok())
}

fn validate_profile_arg(argv: &[OsString], profile: Option<&Path>) -> Result<()> {
    let Some(profile) = profile else {
        return Ok(());
    };
    let needle = OsString::from(format!("--user-data-dir={}", profile.display()));
    let mut matches = 0;
    for arg in argv {
        let s = arg.to_string_lossy();
        if s == "--user-data-dir" || s.starts_with("--user-data-dir=") {
            if arg != &needle {
                bail!("process argv contains conflicting --user-data-dir");
            }
            matches += 1;
        }
    }
    if matches != 1 {
        bail!("process argv must contain exactly one matching --user-data-dir");
    }
    Ok(())
}

#[cfg(test)]
#[allow(dead_code)]
pub fn terminate(id: &ProcessIdentity) -> Result<()> {
    terminate_with(id, &RealOps, Duration::from_secs(5), Duration::from_secs(2))
}

pub fn terminate_until(id: &ProcessIdentity, deadline: Instant) -> Result<()> {
    signal_checked_with(id, libc::SIGTERM, &RealOps)?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    if wait_dead_with(id, remaining / 2, &RealOps)? {
        return Ok(());
    }
    if Instant::now() >= deadline {
        bail!("deadline elapsed while terminating process {}", id.pid);
    }
    signal_checked_with(id, libc::SIGKILL, &RealOps)?;
    if wait_dead_with(
        id,
        deadline.saturating_duration_since(Instant::now()),
        &RealOps,
    )? {
        Ok(())
    } else {
        bail!("process {} survived SIGKILL before deadline", id.pid)
    }
}

pub fn wait_for_exit(id: &ProcessIdentity, timeout: Duration) -> Result<bool> {
    wait_dead_with(id, timeout, &RealOps)
}

#[cfg(test)]
#[allow(dead_code)]
fn terminate_with(
    id: &ProcessIdentity,
    ops: &impl ProcessOps,
    term_timeout: Duration,
    kill_timeout: Duration,
) -> Result<()> {
    signal_checked_with(id, libc::SIGTERM, ops)?;
    if wait_dead_with(id, term_timeout, ops)? {
        return Ok(());
    }
    signal_checked_with(id, libc::SIGKILL, ops)?;
    if wait_dead_with(id, kill_timeout, ops)? {
        Ok(())
    } else {
        bail!("process {} survived SIGKILL", id.pid)
    }
}

fn wait_dead_with(id: &ProcessIdentity, timeout: Duration, ops: &impl ProcessOps) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        ops.reap_if_child(id.pid);
        if !matches_identity_with(id.pid, id, ops)? {
            return Ok(true);
        }
        std::thread::sleep(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100)),
        );
    }
    Ok(false)
}

fn signal_checked_with(
    id: &ProcessIdentity,
    sig: libc::c_int,
    ops: &impl ProcessOps,
) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        match ops.pidfd_open(id.pid) {
            Ok(pidfd) => {
                if !matches_identity_with(id.pid, id, ops)? {
                    bail!("refusing to signal non-matching process identity");
                }
                ops.pidfd_send_signal(&pidfd, sig)
                    .context("pidfd_send_signal failed")?;
                return Ok(());
            }
            Err(e) => {
                let raw = e.raw_os_error();
                if raw != Some(libc::ENOSYS) && raw != Some(libc::EINVAL) {
                    return Err(e).context("pidfd_open failed");
                }
            }
        }
    }
    let _ = sig;
    let _ = ops;
    bail!(
        "pidfd is unavailable; refusing unsafe numeric signaling of PID {}",
        id.pid
    )
}

fn matches_identity_with(pid: u32, id: &ProcessIdentity, ops: &impl ProcessOps) -> Result<bool> {
    let Some(observed) = ops.observe(pid)? else {
        return Ok(false);
    };
    Ok(observed.start_time == id.start_time
        && executable_matches(&observed.exe, &id.exe)
        && observed.argv == id.argv
        && validate_profile_arg(&observed.argv, id.user_data_dir.as_deref()).is_ok())
}

trait ProcessOps {
    fn observe(&self, pid: u32) -> Result<Option<Observed>>;
    fn reap_if_child(&self, pid: u32);
    #[cfg(target_os = "linux")]
    fn pidfd_open(&self, pid: u32) -> std::io::Result<PidFd>;
    #[cfg(target_os = "linux")]
    fn pidfd_send_signal(&self, fd: &PidFd, sig: libc::c_int) -> std::io::Result<()>;
}

struct RealOps;
impl ProcessOps for RealOps {
    fn observe(&self, pid: u32) -> Result<Option<Observed>> {
        observe(pid)
    }
    fn reap_if_child(&self, pid: u32) {
        reap_if_child(pid)
    }
    #[cfg(target_os = "linux")]
    fn pidfd_open(&self, pid: u32) -> std::io::Result<PidFd> {
        let fd =
            unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) as libc::c_int };
        if fd >= 0 {
            Ok(PidFd(fd))
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(target_os = "linux")]
    fn pidfd_send_signal(&self, fd: &PidFd, sig: libc::c_int) -> std::io::Result<()> {
        let rc = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.0,
                sig,
                std::ptr::null::<libc::c_void>(),
                0,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

#[cfg(target_os = "linux")]
struct PidFd(libc::c_int);
#[cfg(target_os = "linux")]
impl Drop for PidFd {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

fn reap_if_child(pid: u32) {
    unsafe {
        let mut st = 0;
        libc::waitpid(pid as libc::pid_t, &mut st, libc::WNOHANG);
    }
}

struct Observed {
    start_time: u64,
    exe: PathBuf,
    argv: Vec<OsString>,
}

#[cfg(target_os = "linux")]
fn observe(pid: u32) -> Result<Option<Observed>> {
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let after = stat.rsplit_once(") ").context("malformed proc stat")?.1;
    let start_time = after
        .split_whitespace()
        .nth(19)
        .context("missing proc starttime")?
        .parse()?;
    let exe = std::fs::read_link(format!("/proc/{pid}/exe"))?;
    let raw = std::fs::read(format!("/proc/{pid}/cmdline"))?;
    let argv = parse_nul_argv(&raw);
    Ok(Some(Observed {
        start_time,
        exe,
        argv,
    }))
}

#[cfg(target_os = "macos")]
fn observe(pid: u32) -> Result<Option<Observed>> {
    use std::{ffi::CStr, mem};
    unsafe {
        let mut info: libc::proc_bsdinfo = mem::zeroed();
        let rc = libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut _,
            mem::size_of::<libc::proc_bsdinfo>() as i32,
        );
        if rc <= 0 {
            return Ok(None);
        }
        let start_time = info.pbi_start_tvsec as u64 * 1_000_000 + info.pbi_start_tvusec as u64;
        let mut path = vec![0i8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        if libc::proc_pidpath(pid as i32, path.as_mut_ptr() as *mut _, path.len() as u32) <= 0 {
            bail!("proc_pidpath failed");
        }
        let exe = PathBuf::from(CStr::from_ptr(path.as_ptr()).to_string_lossy().into_owned());
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as i32];
        let mut argmax: libc::c_int = 0;
        let mut size = mem::size_of::<libc::c_int>();
        let mut argmax_mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        if libc::sysctl(
            argmax_mib.as_mut_ptr(),
            2,
            &mut argmax as *mut _ as *mut _,
            &mut size,
            std::ptr::null_mut(),
            0,
        ) != 0
            || size != mem::size_of::<libc::c_int>()
            || argmax <= 4
        {
            return Err(std::io::Error::last_os_error()).context("KERN_ARGMAX failed");
        }
        let mut size = argmax as usize;
        let mut buf = vec![0u8; size];
        if libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr() as *mut _,
            &mut size,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            bail!("KERN_PROCARGS2 failed");
        }
        buf.truncate(size);
        let argc_i = i32::from_ne_bytes(buf[0..4].try_into().unwrap());
        if argc_i < 0 {
            bail!("KERN_PROCARGS2 negative argc");
        }
        let argv = parse_macos_procargs2(&buf[4..], argc_i as usize)?;
        Ok(Some(Observed {
            start_time,
            exe,
            argv,
        }))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn observe(_pid: u32) -> Result<Option<Observed>> {
    bail!("process identity is unsupported on this platform")
}

fn normalize(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn executable_matches(observed: &Path, recorded: &Path) -> bool {
    if normalize(observed) == normalize(recorded) {
        return true;
    }
    #[cfg(target_os = "macos")]
    {
        // Chromium's hardened-runtime launcher can execute from a temporary
        // code_sign_clone path and later report the original app path through
        // proc_pidpath. PID start time, complete argv, and the private profile
        // argument remain exact; only accept this known path transition for an
        // identical executable basename.
        let is_clone = |path: &Path| path.to_string_lossy().contains("code_sign_clone");
        (is_clone(observed) || is_clone(recorded))
            && observed.file_name().is_some()
            && observed.file_name() == recorded.file_name()
    }
    #[cfg(not(target_os = "macos"))]
    false
}

#[cfg(target_os = "linux")]
fn parse_nul_argv(raw: &[u8]) -> Vec<OsString> {
    use std::os::unix::ffi::OsStringExt;
    let mut fields: Vec<&[u8]> = raw.split(|b| *b == 0).collect();
    if fields.last().is_some_and(|s| s.is_empty()) {
        fields.pop();
    }
    fields
        .into_iter()
        .map(|s| OsString::from_vec(s.to_vec()))
        .collect()
}

#[cfg(target_os = "macos")]
fn parse_macos_procargs2(raw: &[u8], argc: usize) -> Result<Vec<OsString>> {
    use std::os::unix::ffi::OsStringExt;
    let mut i = raw
        .iter()
        .position(|b| *b == 0)
        .context("missing exec path terminator")?
        + 1;
    while i < raw.len() && raw[i] == 0 {
        i += 1;
    }
    let mut argv = Vec::with_capacity(argc);
    for _ in 0..argc {
        let end = raw[i..]
            .iter()
            .position(|b| *b == 0)
            .context("truncated argv")?
            + i;
        argv.push(OsString::from_vec(raw[i..end].to_vec()));
        i = end + 1;
    }
    Ok(argv)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use std::{cell::RefCell, collections::VecDeque};

    #[cfg(target_os = "linux")]
    fn id() -> ProcessIdentity {
        ProcessIdentity {
            pid: 42,
            start_time: 9,
            exe: PathBuf::from("/bin/chrome"),
            user_data_dir: Some(PathBuf::from("/tmp/p")),
            argv: vec![
                OsString::from("chrome"),
                OsString::from("--user-data-dir=/tmp/p"),
            ],
        }
    }

    #[cfg(target_os = "linux")]
    #[derive(Default)]
    struct MockOps {
        events: RefCell<Vec<String>>,
        observes: RefCell<VecDeque<Option<Observed>>>,
        #[cfg(target_os = "linux")]
        pidfd_open: RefCell<Option<std::io::Error>>,
        #[cfg(target_os = "linux")]
        pidfd_send: RefCell<Option<std::io::Error>>,
    }
    #[cfg(target_os = "linux")]
    impl MockOps {
        #[cfg(target_os = "linux")]
        fn matching() -> Self {
            let m = Self::default();
            m.observes.borrow_mut().push_back(Some(Observed {
                start_time: 9,
                exe: PathBuf::from("/bin/chrome"),
                argv: id().argv,
            }));
            m
        }
        fn push(&self, o: Option<Observed>) {
            self.observes.borrow_mut().push_back(o);
        }
    }
    #[cfg(target_os = "linux")]
    impl ProcessOps for MockOps {
        fn observe(&self, _: u32) -> Result<Option<Observed>> {
            self.events.borrow_mut().push("observe".into());
            Ok(self.observes.borrow_mut().pop_front().flatten())
        }
        fn reap_if_child(&self, _: u32) {
            self.events.borrow_mut().push("reap".into());
        }
        #[cfg(target_os = "linux")]
        fn pidfd_open(&self, _: u32) -> std::io::Result<PidFd> {
            self.events.borrow_mut().push("pidfd_open".into());
            if let Some(e) = self.pidfd_open.borrow_mut().take() {
                Err(e)
            } else {
                Ok(PidFd(-1))
            }
        }
        #[cfg(target_os = "linux")]
        fn pidfd_send_signal(&self, _: &PidFd, sig: libc::c_int) -> std::io::Result<()> {
            self.events.borrow_mut().push(format!("pidfd_send:{sig}"));
            if let Some(e) = self.pidfd_send.borrow_mut().take() {
                Err(e)
            } else {
                Ok(())
            }
        }
    }
    #[test]
    fn legacy_pid_is_unverifiable() {
        assert_eq!(
            classify(Some(1), None, false),
            ProcessClass::LegacyUnverifiable
        );
    }
    #[test]
    fn attached_uses_reachability() {
        assert_eq!(classify(None, None, true), ProcessClass::AttachedReachable);
        assert_eq!(classify(None, None, false), ProcessClass::AttachedDead);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn signal_uses_pidfd_first_and_refuses_mismatch() {
        let ops = MockOps::matching();
        signal_checked_with(&id(), libc::SIGTERM, &ops).unwrap();
        assert_eq!(
            &*ops.events.borrow(),
            &["pidfd_open", "observe", "pidfd_send:15"]
        );
        let ops = MockOps::default();
        ops.push(Some(Observed {
            start_time: 10,
            exe: PathBuf::from("/bin/chrome"),
            argv: id().argv,
        }));
        assert!(signal_checked_with(&id(), libc::SIGTERM, &ops).is_err());
        assert_eq!(&*ops.events.borrow(), &["pidfd_open", "observe"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pidfd_unavailable_refuses_numeric_kill_and_propagates_other_errors() {
        let ops = MockOps::matching();
        *ops.pidfd_open.borrow_mut() = Some(std::io::Error::from_raw_os_error(libc::ENOSYS));
        let err = signal_checked_with(&id(), libc::SIGTERM, &ops).unwrap_err();
        assert!(
            err.to_string()
                .contains("refusing unsafe numeric signaling")
        );
        assert_eq!(&*ops.events.borrow(), &["pidfd_open"]);
        let ops = MockOps::matching();
        *ops.pidfd_open.borrow_mut() = Some(std::io::Error::from_raw_os_error(libc::EPERM));
        assert!(
            format!(
                "{:#}",
                signal_checked_with(&id(), libc::SIGTERM, &ops).unwrap_err()
            )
            .contains("pidfd_open failed")
        );
        let ops = MockOps::matching();
        *ops.pidfd_open.borrow_mut() = Some(std::io::Error::from_raw_os_error(libc::EINVAL));
        assert!(
            format!(
                "{:#}",
                signal_checked_with(&id(), libc::SIGTERM, &ops).unwrap_err()
            )
            .contains("refusing unsafe numeric signaling")
        );
        assert_eq!(&*ops.events.borrow(), &["pidfd_open"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn terminate_term_success_escalation_and_survivor() {
        let ops = MockOps::default();
        ops.push(Some(Observed {
            start_time: 9,
            exe: PathBuf::from("/bin/chrome"),
            argv: id().argv.clone(),
        }));
        ops.push(None);
        terminate_with(
            &id(),
            &ops,
            Duration::from_millis(1),
            Duration::from_millis(1),
        )
        .unwrap();
        assert!(ops.events.borrow().iter().any(|e| e.contains("15")));

        let ops = MockOps::default();
        for _ in 0..3 {
            ops.push(Some(Observed {
                start_time: 9,
                exe: PathBuf::from("/bin/chrome"),
                argv: id().argv.clone(),
            }));
        }
        ops.push(None);
        terminate_with(
            &id(),
            &ops,
            Duration::from_millis(1),
            Duration::from_millis(1),
        )
        .unwrap();
        assert!(ops.events.borrow().iter().any(|e| e.contains("9")));

        let ops = MockOps::default();
        for _ in 0..10 {
            ops.push(Some(Observed {
                start_time: 9,
                exe: PathBuf::from("/bin/chrome"),
                argv: id().argv.clone(),
            }));
        }
        assert!(
            format!(
                "{:#}",
                terminate_with(
                    &id(),
                    &ops,
                    Duration::from_millis(1),
                    Duration::from_millis(1)
                )
                .unwrap_err()
            )
            .contains("survived")
        );
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn detects_identity_mismatch_for_current_process() {
        let pid = std::process::id();
        let mut id = capture(pid, &std::env::current_exe().unwrap(), None).unwrap();
        id.start_time += 1;
        assert_eq!(
            classify(Some(pid), Some(&id), false),
            ProcessClass::PidReusedOrUnrelated
        );
    }

    #[test]
    fn validates_exact_profile_and_rejects_duplicates() {
        let profile = Path::new("/tmp/rdny-profile");
        let argv = vec![
            OsString::from("chrome"),
            OsString::from("--user-data-dir=/tmp/rdny-profile"),
        ];
        validate_profile_arg(&argv, Some(profile)).unwrap();
        let dup = vec![argv[0].clone(), argv[1].clone(), argv[1].clone()];
        assert!(validate_profile_arg(&dup, Some(profile)).is_err());
        let override_arg = vec![
            argv[0].clone(),
            OsString::from("--user-data-dir=/tmp/other"),
        ];
        assert!(validate_profile_arg(&override_arg, Some(profile)).is_err());
        let split = vec![
            argv[0].clone(),
            OsString::from("--user-data-dir"),
            OsString::from("/tmp/rdny-profile"),
        ];
        assert!(validate_profile_arg(&split, Some(profile)).is_err());
    }

    #[test]
    fn forged_state_disagreement_is_rejected() {
        let id = ProcessIdentity {
            pid: 7,
            start_time: 1,
            exe: PathBuf::from("/bin/echo"),
            user_data_dir: Some(PathBuf::from("/tmp/p")),
            argv: vec![
                OsString::from("echo"),
                OsString::from("--user-data-dir=/tmp/p"),
            ],
        };
        assert!(
            validate_persisted(
                Some(8),
                Some(Path::new("/bin/echo")),
                Some(Path::new("/tmp/p")),
                &id
            )
            .is_err()
        );
        validate_persisted(
            Some(7),
            Some(Path::new("/bin/sh")),
            Some(Path::new("/tmp/p")),
            &id,
        )
        .unwrap();
        assert!(
            validate_persisted(
                Some(7),
                Some(Path::new("/bin/echo")),
                Some(Path::new("/tmp/q")),
                &id
            )
            .is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn real_spawned_process_capture_and_terminate() {
        let executable = std::env::current_exe().unwrap();
        let mut child = test_helper_command(&executable).spawn().unwrap();
        let pid = child.id();
        let id = capture(pid, &executable, None).unwrap();
        assert!(matches_identity(pid, &id).unwrap());
        terminate(&id).unwrap();
        assert!(!matches_identity(pid, &id).unwrap());
        let _ = child.wait();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wrapper_script_exec_runtime_exe_capture_and_terminate() {
        let launcher = Path::new("sleep");
        let mut child = std::process::Command::new(launcher)
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let id = capture(pid, launcher, None).unwrap();
        let observed_executable = id.exe.clone();
        let identity_matches = matches_identity(pid, &id).unwrap();
        terminate(&id).unwrap();
        let identity_gone = !matches_identity(pid, &id).unwrap();
        let _ = child.wait();
        assert_eq!(observed_executable.file_name(), Some(OsStr::new("sleep")));
        assert_ne!(launcher, observed_executable);
        assert!(identity_matches);
        assert!(identity_gone);
    }

    #[cfg(target_os = "linux")]
    fn test_helper_command(executable: &Path) -> std::process::Command {
        let mut command = std::process::Command::new(executable);
        command
            .args([
                "--exact",
                "process_identity::tests::long_running_process_helper",
                "--nocapture",
            ])
            .env("RDNY_LONG_RUNNING_PROCESS_HELPER", "1");
        command
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn long_running_process_helper() {
        if std::env::var_os("RDNY_LONG_RUNNING_PROCESS_HELPER").is_none() {
            return;
        }
        loop {
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn forged_runtime_executable_mismatch_is_unrelated() {
        let pid = std::process::id();
        let mut id = capture(pid, Path::new("/definitely/not/current/launcher"), None).unwrap();
        id.exe = PathBuf::from("/bin/sh");
        assert!(!matches_identity(pid, &id).unwrap());
        assert_eq!(
            classify(Some(pid), Some(&id), false),
            ProcessClass::PidReusedOrUnrelated
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_nul_argv_preserves_empty_arguments() {
        assert_eq!(
            parse_nul_argv(b"a\0\0b\0"),
            vec![OsString::from("a"), OsString::from(""), OsString::from("b")]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_procargs_preserves_empty_and_stops_at_argc() {
        let raw = b"/bin/x\0\0arg0\0\0arg2\0ENV=x\0";
        assert_eq!(
            parse_macos_procargs2(raw, 3).unwrap(),
            vec![
                OsString::from("arg0"),
                OsString::from(""),
                OsString::from("arg2")
            ]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_code_sign_clone_matches_original_executable_only() {
        let clone = Path::new(
            "/private/var/folders/x/X/net.example.code_sign_clone/code_sign_clone.abc/Helium.app.bundle/Contents/MacOS/Helium",
        );
        let original = Path::new("/Applications/Helium.app/Contents/MacOS/Helium");
        assert!(executable_matches(clone, original));
        assert!(!executable_matches(
            clone,
            Path::new("/Applications/Other.app/Contents/MacOS/Other")
        ));
    }
}
