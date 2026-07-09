//! Browser discovery, launch, and lifecycle (start/connect/stop/status).

use std::ffi::OsStr;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::cdp::http;
use crate::config;
use crate::hint::hint_error;
use crate::state::{ProcessIdentity, SessionState};

const DEVTOOLS_TIMEOUT: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Options for `rdny start`.
#[derive(Debug, Clone, Default)]
pub struct LaunchOpts {
    /// Run with a visible window instead of headless.
    pub show: bool,
    /// Ignore TLS certificate errors.
    pub insecure: bool,
    /// Extra Chrome args (from RDNY_CHROME_ARGS), already split.
    pub extra_args: Vec<String>,
    /// Human-readable label to persist for this instance.
    pub label: Option<String>,
}

/// Discover a browser binary: RDNY_CHROME env var, config file, then
/// well-known locations, else an actionable hint error.
pub fn discover() -> Result<PathBuf> {
    let env_chrome = std::env::var_os("RDNY_CHROME");
    let config_path = config::config_path()?;
    let config = config::load_from_path(config_path.clone())?;
    discover_from(
        env_chrome.as_deref(),
        config
            .binaries
            .as_ref()
            .and_then(|binaries| binaries.chrome.as_deref()),
        Some(config_path.as_path()),
        &well_known_candidates(),
    )
}

/// Launch the browser with a remote debugging port and verify it is
/// reachable (probe /json/version). `data_root` is the rdny state dir
/// (from `state::state_dir()`); the profile dir and chrome.log live
/// under it. Returns the session to persist.
pub struct LaunchGuard {
    state: SessionState,
    child: Option<Child>,
}

impl LaunchGuard {
    pub fn state(&self) -> &SessionState {
        &self.state
    }

    /// Disarm rollback only after durable state and registry persistence.
    pub fn commit(mut self) -> SessionState {
        self.child.take();
        self.state.clone()
    }

    pub fn rollback(mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            kill_child_result(&mut child)
        } else {
            Ok(())
        }
    }
}

impl Drop for LaunchGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            kill_child(child);
        }
    }
}

pub fn launch(opts: &LaunchOpts, data_root: &Path) -> Result<LaunchGuard> {
    let binary = discover()?;
    crate::state::secure_dir(data_root)?;
    let profile_dir = data_root.join("chrome-profile");
    crate::state::secure_dir(&profile_dir)?;
    let active_port_path = profile_dir.join("DevToolsActivePort");
    let _ = fs::remove_file(&active_port_path);

    let mut user_args = opts.extra_args.clone();
    // Split RDNY_CHROME_ARGS on ASCII whitespace. Shell-style quoting is not supported.
    if let Ok(raw) = std::env::var("RDNY_CHROME_ARGS") {
        user_args.extend(raw.split_ascii_whitespace().map(String::from));
    }
    let args = build_args(opts, &profile_dir, &user_args, cfg!(target_os = "macos"));

    let log = crate::state::secure_output(&data_root.join("chrome.log"), true)
        .with_context(|| format!("opening {}/chrome.log", data_root.display()))?;
    let log_err = log.try_clone().context("cloning chrome log handle")?;
    let mut command = Command::new(&binary);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    // SAFETY: umask is async-signal-safe and this closure performs no allocation.
    unsafe {
        command.pre_exec(|| {
            libc::umask(0o077);
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("launching {}", binary.display()))?;

    let port = match wait_for_devtools_port(&active_port_path) {
        Ok(port) => port,
        Err(_) => {
            kill_child(&mut child);
            return Err(launch_probe_error(data_root));
        }
    };
    let version = match wait_for_version("127.0.0.1", port) {
        Ok(version) => version,
        Err(_) => {
            kill_child(&mut child);
            return Err(launch_probe_error(data_root));
        }
    };
    let target_id = first_page_target("127.0.0.1", port);

    let identity = match process_identity(child.id(), &binary, &profile_dir).context(
        "recording browser process identity; refusing to persist a session that cannot be safely stopped",
    ) {
        Ok(identity) => identity,
        Err(identity_err) => {
            return match kill_child_result(&mut child) {
                Ok(()) => Err(identity_err),
                Err(cleanup_err) => Err(anyhow::anyhow!(
                    "{identity_err:#}; additionally failed to kill/reap the untracked browser: {cleanup_err:#}"
                )),
            };
        }
    };
    Ok(LaunchGuard {
        state: SessionState {
            ws_url: version.ws_url,
            host: "127.0.0.1".into(),
            port,
            pid: Some(child.id()),
            process_identity: Some(identity),
            user_data_dir: Some(profile_dir),
            browser_path: Some(binary),
            target_id,
            label: opts.label.clone(),
            viewport: None,
            recording: false,
        },
        child: Some(child),
    })
}

/// Attach to an already-running browser at host:port.
pub fn connect(host: &str, port: u16) -> Result<SessionState> {
    let version = http::version(host, port).map_err(|_| {
        let relaunch = relaunch_example(discover().ok().as_deref(), port, cfg!(target_os = "macos"));
        hint_error(
            format!("could not reach Chrome DevTools at {host}:{port} (the debug port only exists when the browser was launched with it)"),
            format!("{relaunch}; if it is still unreachable add a non-default --user-data-dir (Chromium 136+ silently ignores the debug port on the default profile)"),
            Some("chromium-remote-debugging"),
        )
    })?;
    Ok(SessionState {
        ws_url: version.ws_url,
        host: host.into(),
        port,
        pid: None,
        process_identity: None,
        user_data_dir: None,
        browser_path: None,
        target_id: first_page_target(host, port),
        label: None,
        viewport: None,
        recording: false,
    })
}

/// How a `stop` request was satisfied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// rdny launched this browser and shut it down.
    Stopped,
    /// The session was attached to a browser rdny did not launch;
    /// the browser is left running and only rdny detaches.
    Detached,
}

/// Stop the session's browser and report whether it was running.
/// Attached sessions (no pid) are never killed: the caller should
/// clear the session state, leaving the browser running.
pub fn stop(state: &SessionState) -> Result<StopOutcome> {
    let Some(pid) = state.pid else {
        return Ok(StopOutcome::Detached);
    };
    if pid == 0 || pid > libc::pid_t::MAX as u32 {
        bail!("refusing to signal invalid recorded browser pid {pid}; state was preserved");
    }
    let pid = pid as libc::pid_t;
    reap_if_child(pid);
    if !pid_is_alive(pid)? {
        return Ok(StopOutcome::Stopped);
    }
    let recorded = state.process_identity.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "live browser pid {pid} has legacy state without process identity; refusing to signal it and preserving state (stop it manually, then run `rdny cleanup`)"
        )
    })?;
    validate_process_identity(pid as u32, recorded)?;
    send_signal(pid, libc::SIGTERM)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        reap_if_child(pid);
        if !pid_is_alive(pid)? {
            return Ok(StopOutcome::Stopped);
        }
        thread::sleep(POLL_INTERVAL);
    }
    // Validate again immediately before escalating, guarding against PID reuse.
    validate_process_identity(pid as u32, recorded)?;
    send_signal(pid, libc::SIGKILL)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        reap_if_child(pid);
        if !pid_is_alive(pid)? {
            return Ok(StopOutcome::Stopped);
        }
        thread::sleep(POLL_INTERVAL);
    }
    bail!(
        "browser pid {pid} survived SIGTERM and SIGKILL; shutdown is unconfirmed and state was preserved"
    )
}

fn send_signal(pid: libc::pid_t, signal: libc::c_int) -> Result<()> {
    if unsafe { libc::kill(pid, signal) } == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    Err(err).with_context(|| {
        format!("sending signal {signal} to browser pid {pid}; state was preserved")
    })
}

fn process_identity(pid: u32, executable: &Path, profile: &Path) -> Result<ProcessIdentity> {
    let snapshot = process_snapshot(pid)?;
    if !snapshot.matches(executable, profile) {
        bail!("launched process command does not correlate with browser executable and profile")
    }
    Ok(ProcessIdentity {
        birth_token: snapshot.birth_token,
        executable: executable.to_path_buf(),
        profile: profile.to_path_buf(),
    })
}

fn validate_process_identity(pid: u32, expected: &ProcessIdentity) -> Result<()> {
    let snapshot = process_snapshot(pid).with_context(|| {
        format!("validating browser pid {pid}; refusing to signal and preserving state")
    })?;
    if snapshot.birth_token != expected.birth_token {
        bail!(
            "browser pid {pid} birth token does not match recorded identity; refusing to signal and preserving state"
        );
    }
    if !snapshot.matches(&expected.executable, &expected.profile) {
        bail!(
            "browser pid {pid} command does not match recorded executable/profile; refusing to signal and preserving state"
        );
    }
    Ok(())
}

struct ProcessSnapshot {
    birth_token: String,
    executable: Option<PathBuf>,
    argv: Vec<String>,
}

impl ProcessSnapshot {
    fn matches(&self, executable: &Path, profile: &Path) -> bool {
        let executable_matches = self
            .executable
            .as_deref()
            .is_some_and(|actual| actual == executable)
            || self
                .argv
                .first()
                .is_some_and(|arg0| Path::new(arg0) == executable);
        let profile_arg = format!("--user-data-dir={}", profile.display());
        executable_matches && self.argv.iter().any(|arg| arg == &profile_arg)
    }
}

fn process_snapshot(pid: u32) -> Result<ProcessSnapshot> {
    process_snapshot_platform(pid)
}

#[cfg(target_os = "linux")]
fn process_snapshot_platform(pid: u32) -> Result<ProcessSnapshot> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let end = stat.rfind(')').context("malformed /proc process stat")?;
    let fields: Vec<_> = stat[end + 1..].split_whitespace().collect();
    let birth_token = fields
        .get(19)
        .context("missing process start token")?
        .to_string();
    let argv = read_nul_argv(&fs::read(format!("/proc/{pid}/cmdline"))?);
    let executable = fs::read_link(format!("/proc/{pid}/exe")).ok();
    Ok(ProcessSnapshot {
        birth_token,
        executable,
        argv,
    })
}

#[cfg(target_os = "macos")]
fn process_snapshot_platform(pid: u32) -> Result<ProcessSnapshot> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "start=", "-o", "command="])
        .output()
        .context("running ps for process identity")?;
    if !output.status.success() {
        bail!("ps could not inspect pid {pid}");
    }
    let text = String::from_utf8(output.stdout).context("ps returned non-UTF-8 process data")?;
    let mut fields = text.split_whitespace();
    let birth_token = fields
        .next()
        .context("ps omitted process start token")?
        .to_string();
    let argv = fields.map(str::to_string).collect();
    Ok(ProcessSnapshot {
        birth_token,
        executable: None,
        argv,
    })
}

#[cfg(target_os = "linux")]
fn read_nul_argv(raw: &[u8]) -> Vec<String> {
    raw.split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect()
}

/// Reap the process if it is a zombie child of this process (e.g. when
/// stop() runs in the same process that launched the browser, as in
/// tests or launch-failure cleanup). Harmless ECHILD otherwise.
fn reap_if_child(pid: libc::pid_t) {
    let mut status: libc::c_int = 0;
    unsafe {
        libc::waitpid(pid, &mut status, libc::WNOHANG);
    }
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
pub fn status(state: &SessionState) -> Result<BrowserStatus> {
    match http::version(&state.host, state.port) {
        Ok(v) => Ok(BrowserStatus::Running { browser: v.browser }),
        Err(_) => Ok(BrowserStatus::Stale),
    }
}

fn discover_from(
    env_chrome: Option<&OsStr>,
    config_chrome: Option<&Path>,
    config_path: Option<&Path>,
    candidates: &[PathBuf],
) -> Result<PathBuf> {
    if let Some(path) = env_chrome {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(hint_error(
            format!(
                "RDNY_CHROME is set to {}, which does not exist",
                path.display()
            ),
            "point RDNY_CHROME at a Chrome/Chromium binary",
            Some("chromium-remote-debugging"),
        ));
    }
    if let Some(path) = config_chrome {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        let config_source = config_path
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "the rdny config file".to_string());
        return Err(hint_error(
            format!(
                "binaries.chrome in {config_source} is set to {}, which does not exist",
                path.display()
            ),
            format!(
                "point binaries.chrome in {config_source} at a Chrome/Chromium binary, or set RDNY_CHROME"
            ),
            Some("chromium-remote-debugging"),
        ));
    }
    candidates
        .iter()
        .find(|p| p.is_file())
        .cloned()
        .ok_or_else(|| {
            hint_error(
                "no Chrome or Chromium browser found",
                "install Google Chrome, set RDNY_CHROME, or set binaries.chrome in the rdny config file to a Chromium-based binary",
                Some("chromium-remote-debugging"),
            )
        })
}

/// Build a copy-pasteable command showing how to relaunch the browser
/// with a debug port. Uses the browser rdny itself would pick
/// (RDNY_CHROME > config > well-known locations) so the example names
/// the binary the user actually has; falls back to a platform-typical
/// command when discovery finds nothing.
fn relaunch_example(binary: Option<&Path>, port: u16, macos: bool) -> String {
    let command = match binary {
        Some(path) => {
            let app_name = macos
                .then(|| {
                    path.components().rev().find_map(|c| {
                        c.as_os_str()
                            .to_str()
                            .and_then(|s| s.strip_suffix(".app"))
                            .map(str::to_string)
                    })
                })
                .flatten();
            match app_name {
                Some(name) if name.contains(' ') => {
                    format!("open -na \"{name}\" --args --remote-debugging-port={port}")
                }
                Some(name) => format!("open -na {name} --args --remote-debugging-port={port}"),
                None => {
                    let path = path.display();
                    if path.to_string().contains(' ') {
                        format!("\"{path}\" --remote-debugging-port={port}")
                    } else {
                        format!("{path} --remote-debugging-port={port}")
                    }
                }
            }
        }
        None if macos => {
            format!("open -na \"Google Chrome\" --args --remote-debugging-port={port}")
        }
        None => format!("chromium --remote-debugging-port={port}"),
    };
    format!("relaunch it like `{command}`")
}

fn well_known_candidates() -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        vec![
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into(),
            "/Applications/Chromium.app/Contents/MacOS/Chromium".into(),
            "/Applications/Helium.app/Contents/MacOS/Helium".into(),
        ]
    } else if cfg!(target_os = "linux") {
        path_candidates(&[
            "chromium",
            "chromium-browser",
            "google-chrome",
            "google-chrome-stable",
        ])
    } else {
        Vec::new()
    }
}

fn path_candidates(names: &[&str]) -> Vec<PathBuf> {
    let dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    names
        .iter()
        .filter_map(|name| dirs.iter().map(|dir| dir.join(name)).find(|p| p.is_file()))
        .collect()
}

fn build_args(
    opts: &LaunchOpts,
    user_data_dir: &Path,
    user_args: &[String],
    macos: bool,
) -> Vec<String> {
    let mut args = vec![
        "--remote-debugging-port=0".to_string(),
        format!("--user-data-dir={}", user_data_dir.display()),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
    ];
    if !opts.show {
        args.push("--headless=new".to_string());
    }
    if opts.insecure {
        args.push("--ignore-certificate-errors".to_string());
    }
    args.extend(user_args.iter().filter_map(|arg| {
        if macos && (arg == "--single-process" || arg.starts_with("--single-process=")) {
            eprintln!("ignoring --single-process on macOS: it crashes recent Chromium");
            None
        } else {
            Some(arg.clone())
        }
    }));
    args
}

fn parse_devtools_active_port(contents: &str) -> Result<u16> {
    let first = contents
        .lines()
        .next()
        .context("DevToolsActivePort is empty")?;
    let port: u16 = first
        .trim()
        .parse()
        .context("parsing DevToolsActivePort port")?;
    if port == 0 {
        bail!("DevToolsActivePort contained port 0");
    }
    Ok(port)
}

fn wait_for_devtools_port(path: &Path) -> Result<u16> {
    let deadline = Instant::now() + DEVTOOLS_TIMEOUT;
    let mut last_err = None;
    while Instant::now() < deadline {
        if let Ok(mut file) = crate::state::secure_input(path) {
            let mut contents = String::new();
            use std::io::Read;
            if file.read_to_string(&mut contents).is_err() {
                thread::sleep(POLL_INTERVAL);
                continue;
            }
            match parse_devtools_active_port(&contents) {
                Ok(port) => return Ok(port),
                Err(err) => last_err = Some(err),
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
    if let Some(err) = last_err {
        return Err(err);
    }
    bail!("DevToolsActivePort never appeared")
}

fn wait_for_version(host: &str, port: u16) -> Result<http::VersionInfo> {
    let deadline = Instant::now() + DEVTOOLS_TIMEOUT;
    let mut last_err = None;
    while Instant::now() < deadline {
        match http::version(host, port) {
            Ok(version) => return Ok(version),
            Err(err) => last_err = Some(err),
        }
        thread::sleep(POLL_INTERVAL);
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("DevTools port never became reachable")))
}

fn launch_probe_error(data_root: &Path) -> anyhow::Error {
    hint_error(
        "Chrome started but its DevTools port never became reachable (Chromium 136+ silently ignores --remote-debugging-port on the default user data dir)",
        format!(
            "inspect {}/chrome.log for launch errors",
            data_root.display()
        ),
        Some("chromium-remote-debugging"),
    )
}

fn first_page_target(host: &str, port: u16) -> Option<String> {
    http::list_targets(host, port).ok().and_then(|targets| {
        targets
            .into_iter()
            .find(|target| target.target_type == "page")
            .map(|target| target.id)
    })
}

pub fn pid_exists(pid: libc::pid_t) -> bool {
    pid_is_alive(pid).unwrap_or(false)
}

pub fn pid_is_alive(pid: libc::pid_t) -> Result<bool> {
    if unsafe { libc::kill(pid, 0) } == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        Ok(false)
    } else {
        Err(err).with_context(|| format!("probing browser pid {pid}"))
    }
}

fn kill_child(child: &mut std::process::Child) {
    let _ = kill_child_result(child);
}

fn kill_child_result(child: &mut Child) -> Result<()> {
    let pid = child.id() as libc::pid_t;
    if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ESRCH) {
            return Err(err).with_context(|| format!("killing launched browser pid {pid}"));
        }
    }
    child
        .wait()
        .with_context(|| format!("reaping launched browser pid {pid}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_pid(pid: u32, identity: Option<ProcessIdentity>) -> SessionState {
        SessionState {
            ws_url: "ws://unused".into(),
            host: "127.0.0.1".into(),
            port: 1,
            pid: Some(pid),
            process_identity: identity,
            user_data_dir: None,
            browser_path: None,
            target_id: None,
            label: None,
            viewport: None,
            recording: false,
        }
    }

    #[test]
    fn relaunch_example_names_the_discovered_mac_app() {
        assert_eq!(
            relaunch_example(
                Some(Path::new("/Applications/Helium.app/Contents/MacOS/Helium")),
                9333,
                true,
            ),
            "relaunch it like `open -na Helium --args --remote-debugging-port=9333`"
        );
        assert_eq!(
            relaunch_example(
                Some(Path::new(
                    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
                )),
                9222,
                true,
            ),
            "relaunch it like `open -na \"Google Chrome\" --args --remote-debugging-port=9222`"
        );
    }

    #[test]
    fn relaunch_example_uses_binary_path_on_linux_and_raw_binaries() {
        assert_eq!(
            relaunch_example(Some(Path::new("/usr/bin/chromium")), 9333, false),
            "relaunch it like `/usr/bin/chromium --remote-debugging-port=9333`"
        );
        // RDNY_CHROME pointing at a raw binary on macOS (not an .app bundle)
        assert_eq!(
            relaunch_example(Some(Path::new("/opt/thorium/thorium")), 9333, true),
            "relaunch it like `/opt/thorium/thorium --remote-debugging-port=9333`"
        );
    }

    #[test]
    fn relaunch_example_falls_back_per_platform() {
        assert_eq!(
            relaunch_example(None, 9333, true),
            "relaunch it like `open -na \"Google Chrome\" --args --remote-debugging-port=9333`"
        );
        assert_eq!(
            relaunch_example(None, 9333, false),
            "relaunch it like `chromium --remote-debugging-port=9333`"
        );
    }

    #[test]
    fn discover_env_override_wins() {
        let dir = tempfile::tempdir().unwrap();
        let env = dir.path().join("chrome");
        let candidate = dir.path().join("candidate");
        fs::write(&env, "").unwrap();
        fs::write(&candidate, "").unwrap();
        assert_eq!(
            discover_from(Some(env.as_os_str()), None, None, &[candidate]).unwrap(),
            env
        );
    }

    #[test]
    fn discover_config_beats_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config-chrome");
        let candidate = dir.path().join("candidate");
        fs::write(&config, "").unwrap();
        fs::write(&candidate, "").unwrap();
        assert_eq!(
            discover_from(
                None,
                Some(&config),
                Some(Path::new("/cfg.toml")),
                &[candidate]
            )
            .unwrap(),
            config
        );
    }

    #[test]
    fn discover_env_missing_errors() {
        let err = discover_from(
            Some(OsStr::new("/definitely/missing/chrome")),
            None,
            None,
            &[],
        )
        .unwrap_err();
        assert!(format!("{err}").contains("RDNY_CHROME"));
    }

    #[test]
    fn discover_env_beats_config() {
        let dir = tempfile::tempdir().unwrap();
        let env = dir.path().join("env");
        let config = dir.path().join("config");
        fs::write(&env, "").unwrap();
        fs::write(&config, "").unwrap();
        assert_eq!(
            discover_from(
                Some(env.as_os_str()),
                Some(&config),
                Some(Path::new("/cfg.toml")),
                &[]
            )
            .unwrap(),
            env
        );
    }

    #[test]
    fn discover_config_missing_errors() {
        let err = discover_from(
            None,
            Some(Path::new("/definitely/missing/config-chrome")),
            Some(Path::new("/tmp/rdny-config.toml")),
            &[],
        )
        .unwrap_err();
        let text = format!("{err}");
        assert!(text.contains("binaries.chrome"));
        assert!(text.contains("/tmp/rdny-config.toml"));
    }

    #[test]
    fn discover_candidates_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let second = dir.path().join("second");
        fs::write(&second, "").unwrap();
        assert_eq!(
            discover_from(None, None, None, &[missing, second.clone()]).unwrap(),
            second
        );
    }

    #[test]
    fn discover_empty_has_hint_and_docs() {
        let err = discover_from(None, None, None, &[]).unwrap_err();
        let text = format!("{err}");
        assert!(text.contains("RDNY_CHROME"));
        assert!(text.contains("binaries.chrome"));
        assert!(text.contains("hint:"));
        assert!(text.contains("docs:"));
    }

    #[test]
    fn build_args_defaults_and_options() {
        let opts = LaunchOpts::default();
        let args = build_args(&opts, Path::new("/tmp/profile"), &[], false);
        assert!(args.contains(&"--remote-debugging-port=0".to_string()));
        assert!(args.contains(&"--user-data-dir=/tmp/profile".to_string()));
        assert!(args.contains(&"--headless=new".to_string()));
        assert!(!args.iter().any(|a| a.contains("single-process")));

        let opts = LaunchOpts {
            show: true,
            insecure: true,
            extra_args: Vec::new(),
            label: None,
        };
        let args = build_args(&opts, Path::new("/tmp/profile"), &[], false);
        assert!(!args.contains(&"--headless=new".to_string()));
        assert!(args.contains(&"--ignore-certificate-errors".to_string()));
    }

    #[test]
    fn build_args_single_process_filter_is_macos_only_and_preserves_order() {
        let user = vec!["--foo".into(), "--single-process".into(), "--bar=1".into()];
        let mac = build_args(&LaunchOpts::default(), Path::new("/p"), &user, true);
        assert!(mac.ends_with(&["--foo".into(), "--bar=1".into()]));
        assert!(!mac.contains(&"--single-process".to_string()));

        let linux = build_args(&LaunchOpts::default(), Path::new("/p"), &user, false);
        assert!(linux.ends_with(&["--foo".into(), "--single-process".into(), "--bar=1".into()]));
    }

    #[test]
    fn parses_devtools_active_port() {
        assert_eq!(
            parse_devtools_active_port("12345\n/devtools/browser/abc\n").unwrap(),
            12345
        );
        assert!(parse_devtools_active_port("nope\n/devtools/browser/abc\n").is_err());
        assert!(parse_devtools_active_port("").is_err());
    }

    #[test]
    fn stop_rejects_invalid_and_legacy_live_pids() {
        let invalid = state_with_pid(0, None);
        assert!(format!("{:#}", stop(&invalid).unwrap_err()).contains("invalid"));

        let legacy = state_with_pid(std::process::id(), None);
        let err = stop(&legacy).unwrap_err();
        assert!(format!("{err:#}").contains("legacy state"));
    }

    #[test]
    fn identity_mismatch_refuses_to_signal() {
        let snapshot = process_snapshot(std::process::id()).unwrap();
        let identity = ProcessIdentity {
            birth_token: format!("{}-wrong", snapshot.birth_token),
            executable: "/definitely/not-this-process".into(),
            profile: "/also/not-a-profile".into(),
        };
        let err = stop(&state_with_pid(std::process::id(), Some(identity))).unwrap_err();
        assert!(format!("{err:#}").contains("birth token does not match"));
    }

    #[test]
    fn identity_matching_is_exact_for_argv_and_profile() {
        let snapshot = ProcessSnapshot {
            birth_token: "1".into(),
            executable: Some("/bin/chrome".into()),
            argv: vec![
                "/bin/chrome".into(),
                "--user-data-dir=/tmp/rdny-profile".into(),
            ],
        };
        assert!(snapshot.matches(Path::new("/bin/chrome"), Path::new("/tmp/rdny-profile")));
        assert!(!snapshot.matches(
            Path::new("/bin/chrome-other"),
            Path::new("/tmp/rdny-profile")
        ));
        assert!(!snapshot.matches(Path::new("/bin/chrome"), Path::new("/tmp/rdny")));
    }

    #[test]
    fn launch_guard_drop_kills_and_reaps_child() {
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let guard = LaunchGuard {
            state: state_with_pid(pid, None),
            child: Some(child),
        };
        drop(guard);
        assert!(!pid_exists(pid as libc::pid_t));
    }

    #[test]
    fn launch_guard_commit_disarms_rollback() {
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let guard = LaunchGuard {
            state: state_with_pid(pid, None),
            child: Some(child),
        };

        let state = guard.commit();
        assert_eq!(state.pid, Some(pid));
        assert!(pid_exists(pid as libc::pid_t));

        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) }, 0);
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) },
            pid as libc::pid_t
        );
        assert!(!pid_exists(pid as libc::pid_t));
    }

    #[test]
    fn launch_guard_explicit_rollback_kills_and_reaps_child() {
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let guard = LaunchGuard {
            state: state_with_pid(pid, None),
            child: Some(child),
        };

        guard.rollback().unwrap();
        assert!(!pid_exists(pid as libc::pid_t));
    }

    #[test]
    #[ignore]
    fn real_launch_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let launch = launch(&LaunchOpts::default(), dir.path()).unwrap_or_else(|err| {
            let log = fs::read_to_string(dir.path().join("chrome.log")).unwrap_or_default();
            panic!("{err}\nchrome.log:\n{log}");
        });
        let state = launch.commit();
        match status(&state).unwrap() {
            BrowserStatus::Running { browser } => assert!(!browser.is_empty()),
            BrowserStatus::Stale => panic!("launched browser is stale"),
        }
        stop(&state).unwrap();
        if let Some(pid) = state.pid {
            assert!(!pid_exists(pid as libc::pid_t));
        }
    }
}
