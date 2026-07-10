//! Browser discovery, launch, and lifecycle (start/connect/stop/status).

use std::ffi::OsStr;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::cdp::client::CdpClient;
use crate::cdp::http;
use crate::config;
use crate::hint::hint_error;
use crate::process_identity::{self, ProcessClass};
use crate::session::Deadline;
use crate::state::{BrowserStorage, SessionState};

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
/// reachable (probe /json/version). Profile and log creation is performed
/// relative to validated, open state-directory descriptors. Chrome requires a
/// real directory path for descendant creation, so the profile argument is the
/// normalized absolute path after validation. Every ancestor is descriptor-
/// checked as root/current-user owned and non-writable by other users, except
/// root-owned sticky directories; the state root/profile are current-user 0700.
/// This prevents cross-UID replacement. Same-UID lifecycle orchestration is
/// intentionally deferred to #130/#131.
#[allow(dead_code)]
pub fn launch(opts: &LaunchOpts, storage: BrowserStorage) -> Result<SessionState> {
    let mut launched = launch_armed_until(opts, storage, Deadline::after(DEVTOOLS_TIMEOUT))?;
    launched.commit();
    Ok(launched.state)
}

pub(crate) struct LaunchedBrowser {
    pub(crate) state: SessionState,
    guard: ChildLaunchGuard,
}

impl LaunchedBrowser {
    pub(crate) fn commit(&mut self) {
        self.guard.commit();
    }
}

pub(crate) fn launch_armed_until(
    opts: &LaunchOpts,
    storage: BrowserStorage,
    deadline: Deadline,
) -> Result<LaunchedBrowser> {
    let binary = discover()?;
    let profile_dir = storage.profile.path().to_path_buf();
    let data_root = profile_dir.parent().context("profile has no state root")?;
    storage.profile.remove_file("DevToolsActivePort")?;

    let mut user_args = opts.extra_args.clone();
    // Split RDNY_CHROME_ARGS on ASCII whitespace. Shell-style quoting is not supported.
    if let Ok(raw) = std::env::var("RDNY_CHROME_ARGS") {
        user_args.extend(raw.split_ascii_whitespace().map(String::from));
    }
    storage.profile.validate_external_path()?;
    let args = build_args(opts, &profile_dir, &user_args, cfg!(target_os = "macos"));

    let log = storage.log;
    let log_err = log.try_clone().context("cloning chrome log handle")?;
    let child = Command::new(&binary)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .with_context(|| format!("launching {}", binary.display()))?;
    let launch_guard = ChildLaunchGuard::armed(child);

    let port = match wait_for_devtools_port(&storage.profile, deadline) {
        Ok(port) => port,
        Err(_) => return Err(launch_probe_error(data_root)),
    };
    let version = match wait_for_version("127.0.0.1", port, deadline) {
        Ok(version) => version,
        Err(_) => return Err(launch_probe_error(data_root)),
    };
    crate::cdp::client::validate_debugger_url(&version.ws_url, "127.0.0.1", port)?;
    let target_id = first_page_target("127.0.0.1", port, deadline)?;

    let child_id = launch_guard.id();
    let process_identity = Some(
        process_identity::capture(child_id, &binary, Some(&profile_dir))
            .context("capturing launched browser process identity")?,
    );
    let state = SessionState {
        instance_id: None,
        endpoint: None,
        ws_url: version.ws_url,
        host: "127.0.0.1".into(),
        port,
        pid: Some(child_id),
        process_identity,
        user_data_dir: Some(profile_dir),
        browser_path: Some(binary),
        target_id,
        label: opts.label.clone(),
        viewport: None,
        recording: false,
        recording_id: None,
        recording_frames_dir: None,
        recoverable_recording: None,
        recoverable_recordings: Vec::new(),
        instrumentation: None,
    };
    Ok(LaunchedBrowser {
        state,
        guard: launch_guard,
    })
}

/// Attach with an explicit remote policy. Direct remote CDP remains rejected:
/// it has no authenticated HTTP discovery transport. `allow_remote` exists as
/// a deadline-ready policy hook and produces guidance instead of silently
/// downgrading security.
#[cfg(test)]
pub fn connect_with_policy(host: &str, port: u16, allow_remote: bool) -> Result<SessionState> {
    connect_with_policy_until(
        host,
        port,
        allow_remote,
        Deadline::after(http::HTTP_TIMEOUT),
    )
}

pub fn connect_with_policy_until(
    host: &str,
    port: u16,
    allow_remote: bool,
    deadline: Deadline,
) -> Result<SessionState> {
    // Do not bless arbitrary DNS names merely because one lookup returned a
    // loopback address: the HTTP and WebSocket lookups could be rebound.
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host.ends_with(".localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    if !loopback {
        let opt_in = if allow_remote {
            "direct remote CDP is still refused because its HTTP/WS transport is unauthenticated and unencrypted"
        } else {
            "non-loopback CDP requires explicit --allow-remote, but direct insecure transport is not supported"
        };
        bail!(
            "refusing Chrome DevTools endpoint {host}:{port}: {opt_in}; create a verified SSH tunnel (for example `ssh -N -L 9222:127.0.0.1:{port} HOST`) and connect to 127.0.0.1:9222"
        );
    }
    let version = http::version_until(host, port, deadline.instant()).map_err(|_| {
        let relaunch = relaunch_example(discover().ok().as_deref(), port, cfg!(target_os = "macos"));
        hint_error(
            format!("could not reach Chrome DevTools at {host}:{port} (the debug port only exists when the browser was launched with it)"),
            format!("{relaunch}; if it is still unreachable add a non-default --user-data-dir (Chromium 136+ silently ignores the debug port on the default profile)"),
            Some("chromium-remote-debugging"),
        )
    })?;
    crate::cdp::client::validate_debugger_url(&version.ws_url, host, port)?;
    Ok(SessionState {
        instance_id: None,
        endpoint: None,
        ws_url: version.ws_url,
        host: host.into(),
        port,
        pid: None,
        process_identity: None,
        user_data_dir: None,
        browser_path: None,
        target_id: first_page_target(host, port, deadline)?,
        label: None,
        viewport: None,
        recording: false,
        recording_id: None,
        recording_frames_dir: None,
        recoverable_recording: None,
        recoverable_recordings: Vec::new(),
        instrumentation: None,
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
    stop_until(state, Deadline::after(Duration::from_secs(7)))
}

pub fn stop_until(state: &SessionState, deadline: Deadline) -> Result<StopOutcome> {
    if state.endpoint.is_some() {
        crate::broker::stop(state, deadline).context("requesting authenticated broker stop")?;
        return Ok(StopOutcome::Stopped);
    }
    if let Some(id) = &state.process_identity {
        process_identity::validate_persisted(
            state.pid,
            state.browser_path.as_deref(),
            state.user_data_dir.as_deref(),
            id,
        )?;
    }
    match process_identity::classify(state.pid, state.process_identity.as_ref(), false) {
        ProcessClass::AttachedReachable | ProcessClass::AttachedDead => Ok(StopOutcome::Detached),
        ProcessClass::ManagedDead => Ok(StopOutcome::Stopped),
        ProcessClass::ManagedMatching => {
            let identity = state.process_identity.as_ref().expect("classified managed");
            let close_result = close_browser(&state.ws_url, deadline);
            let graceful_wait = deadline.remaining().unwrap_or_default() / 2;
            if process_identity::wait_for_exit(identity, graceful_wait)? {
                return Ok(StopOutcome::Stopped);
            }

            // Linux can safely escalate only through a pidfd, which binds the
            // signal to the already-open process identity. Other platforms and
            // kernels without pidfd must never validate and then numeric-kill.
            if let Err(signal_error) =
                process_identity::terminate_until(identity, deadline.instant())
            {
                let close_detail = close_result
                    .err()
                    .map(|err| format!("Browser.close failed: {err:#}; "))
                    .unwrap_or_default();
                bail!(
                    "{close_detail}managed browser PID {} is still running; {signal_error:#}. Close it manually, verify the profile {}, then retry `rdny stop`; session state was preserved",
                    identity.pid,
                    state
                        .user_data_dir
                        .as_deref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "(unknown)".to_string())
                );
            }
            Ok(StopOutcome::Stopped)
        }
        ProcessClass::LegacyUnverifiable => bail!(
            "refusing to signal legacy PID-only state; run cleanup or remove state after verifying the process manually"
        ),
        ProcessClass::PidReusedOrUnrelated => {
            bail!("refusing to signal PID that does not match rdny's managed process identity")
        }
    }
}

fn close_browser(ws_url: &str, deadline: Deadline) -> Result<()> {
    let mut client = CdpClient::connect_until(ws_url, deadline)?;
    client
        .call_until(None, "Browser.close", json!({}), deadline)
        .context("sending Browser.close")?;
    Ok(())
}

/// Health of the recorded session.
#[derive(Debug, Clone)]
pub enum BrowserStatus {
    /// Browser identity matches and answers /json/version.
    Running { browser: String },
    /// State file exists but the browser is gone.
    Stale,
}

/// Probe the session's browser.
#[cfg(test)]
pub fn status(state: &SessionState) -> Result<BrowserStatus> {
    status_until(state, Deadline::after(http::HTTP_TIMEOUT))
}

pub fn status_until(state: &SessionState, deadline: Deadline) -> Result<BrowserStatus> {
    if state.endpoint.is_some() {
        return match crate::broker::ping(state, deadline) {
            Ok(()) => Ok(BrowserStatus::Running {
                browser: state
                    .browser_path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "Chrome".into()),
            }),
            Err(_) => Ok(BrowserStatus::Stale),
        };
    }
    if let Some(id) = &state.process_identity {
        process_identity::validate_persisted(
            state.pid,
            state.browser_path.as_deref(),
            state.user_data_dir.as_deref(),
            id,
        )?;
    }
    let reachable = http::version_until(&state.host, state.port, deadline.instant()).ok();
    match process_identity::classify(
        state.pid,
        state.process_identity.as_ref(),
        reachable.is_some(),
    ) {
        ProcessClass::ManagedMatching | ProcessClass::AttachedReachable => reachable
            .map(|v| Ok(BrowserStatus::Running { browser: v.browser }))
            .unwrap_or(Ok(BrowserStatus::Stale)),
        _ => Ok(BrowserStatus::Stale),
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
        if arg == "--user-data-dir" || arg.starts_with("--user-data-dir=") {
            eprintln!("ignoring user-supplied --user-data-dir: rdny owns the managed profile");
            None
        } else if macos && (arg == "--single-process" || arg.starts_with("--single-process=")) {
            eprintln!("ignoring --single-process on macOS: it crashes recent Chromium");
            None
        } else {
            Some(arg.clone())
        }
    }));
    args
}

pub(crate) fn build_managed_pipe_args(
    opts: &LaunchOpts,
    user_data_dir: &Path,
    user_args: &[String],
    macos: bool,
) -> Result<Vec<String>> {
    for arg in user_args {
        if arg == "--remote-debugging-port"
            || arg.starts_with("--remote-debugging-port=")
            || arg == "--remote-debugging-pipe"
            || arg.starts_with("--remote-debugging-pipe=")
        {
            bail!(
                "managed sessions reject user-supplied `{arg}`: rdny exclusively owns --remote-debugging-pipe; remove every remote-debugging flag from RDNY_CHROME_ARGS/configuration"
            );
        }
    }
    let mut args = build_args(opts, user_data_dir, user_args, macos);
    args[0] = "--remote-debugging-pipe".to_string();
    debug_assert_eq!(
        args.iter()
            .filter(|arg| arg.as_str() == "--remote-debugging-pipe")
            .count(),
        1
    );
    Ok(args)
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

fn wait_for_devtools_port(profile: &crate::state::SecureDir, deadline: Deadline) -> Result<u16> {
    let mut last_err = None;
    while !deadline.expired() {
        if let Ok(contents) = profile.read_string("DevToolsActivePort") {
            match parse_devtools_active_port(&contents) {
                Ok(port) => return Ok(port),
                Err(err) => last_err = Some(err),
            }
        }
        deadline.sleep(POLL_INTERVAL);
    }
    if let Some(err) = last_err {
        return Err(err);
    }
    bail!("DevToolsActivePort never appeared")
}

fn wait_for_version(host: &str, port: u16, deadline: Deadline) -> Result<http::VersionInfo> {
    let mut last_err = None;
    while !deadline.expired() {
        match http::version_until(host, port, deadline.instant()) {
            Ok(version) => return Ok(version),
            Err(err) => last_err = Some(err),
        }
        deadline.sleep(POLL_INTERVAL);
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

fn first_page_target(host: &str, port: u16, deadline: Deadline) -> Result<Option<String>> {
    Ok(http::list_targets_until(host, port, deadline.instant())?
        .into_iter()
        .find(|target| target.target_type == "page")
        .map(|target| target.id))
}

fn kill_child(child: &mut std::process::Child) {
    // This is the owned launch handle, not a persisted numeric PID.
    let _ = child.kill();
    let _ = child.wait();
}

struct ChildLaunchGuard {
    child: Option<std::process::Child>,
}

impl ChildLaunchGuard {
    fn armed(child: std::process::Child) -> Self {
        Self { child: Some(child) }
    }

    fn id(&self) -> u32 {
        self.child.as_ref().expect("guard is armed").id()
    }

    fn commit(&mut self) {
        self.child = None;
    }
}

impl Drop for ChildLaunchGuard {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            kill_child(child);
        }
    }
}

#[cfg(test)]
fn pid_exists(pid: libc::pid_t) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::net::TcpListener;
    use tungstenite::{Message, accept};

    fn fake_close_server(response: &'static str) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = accept(stream).unwrap();
            let Message::Text(raw) = socket.read().unwrap() else {
                panic!("expected text request")
            };
            let request: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(request["method"], "Browser.close");
            assert_eq!(request["params"], json!({}));
            socket.send(Message::Text(response.into())).unwrap();
        });
        (url, handle)
    }

    #[test]
    fn graceful_close_uses_browser_level_cdp() {
        let (url, handle) = fake_close_server(r#"{"id":1,"result":{}}"#);
        close_browser(&url, Deadline::after(Duration::from_secs(1))).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn graceful_close_surfaces_cdp_rejection() {
        let (url, handle) =
            fake_close_server(r#"{"id":1,"error":{"code":-32000,"message":"close denied"}}"#);
        let err = close_browser(&url, Deadline::after(Duration::from_secs(1)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Browser.close"), "{err}");
        handle.join().unwrap();
    }

    #[test]
    fn remote_connect_is_rejected_with_or_without_opt_in() {
        let error = connect_with_policy("192.0.2.1", 9222, false).unwrap_err();
        assert!(format!("{error}").contains("--allow-remote"));
        let error = connect_with_policy("192.0.2.1", 9222, true).unwrap_err();
        assert!(format!("{error}").contains("SSH tunnel"));
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
    fn build_args_rejects_user_data_dir_override() {
        let user = vec![
            "--foo".into(),
            "--user-data-dir=/tmp/evil".into(),
            "--user-data-dir".into(),
            "--bar".into(),
        ];
        let args = build_args(
            &LaunchOpts::default(),
            Path::new("/tmp/profile"),
            &user,
            false,
        );
        assert_eq!(
            args.iter()
                .filter(|a| a.starts_with("--user-data-dir"))
                .count(),
            1
        );
        assert!(args.contains(&"--user-data-dir=/tmp/profile".to_string()));
        assert!(args.ends_with(&["--foo".into(), "--bar".into()]));
    }

    #[test]
    fn managed_args_reject_all_remote_debugging_overrides() {
        for hostile in [
            vec!["--remote-debugging-port=9222"],
            vec!["--remote-debugging-port", "9222"],
            vec!["--remote-debugging-pipe"],
            vec!["--remote-debugging-pipe=true"],
        ] {
            let args: Vec<String> = hostile.into_iter().map(str::to_string).collect();
            let error = build_managed_pipe_args(
                &LaunchOpts::default(),
                Path::new("/tmp/profile"),
                &args,
                false,
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains("exclusively owns"));
        }

        let args = build_managed_pipe_args(
            &LaunchOpts::default(),
            Path::new("/tmp/profile"),
            &["--disable-gpu".into()],
            false,
        )
        .unwrap();
        assert_eq!(
            args.iter()
                .filter(|arg| arg.as_str() == "--remote-debugging-pipe")
                .count(),
            1
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg.starts_with("--remote-debugging-port"))
        );
    }

    #[test]
    fn post_spawn_identity_failure_cleanup_kills_and_reaps_child() {
        let child = Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 30")
            .spawn()
            .unwrap();
        let pid = child.id() as libc::pid_t;
        // This is the exact cleanup path launch uses after a post-spawn
        // identity-capture error.
        drop(ChildLaunchGuard::armed(child));
        assert!(!pid_exists(pid));
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
    #[ignore]
    fn real_launch_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let state = launch(
            &LaunchOpts::default(),
            crate::state::browser_storage_at(dir.path()).unwrap(),
        )
        .unwrap_or_else(|err| {
            let log = fs::read_to_string(dir.path().join("chrome.log")).unwrap_or_default();
            panic!("{err}\nchrome.log:\n{log}");
        });
        match status(&state).unwrap() {
            BrowserStatus::Running { browser } => assert!(!browser.is_empty()),
            BrowserStatus::Stale => panic!("launched browser is stale"),
        }
        stop(&state).unwrap();
        if let Some(pid) = state.pid {
            assert_ne!(
                process_identity::classify(Some(pid), state.process_identity.as_ref(), false),
                ProcessClass::ManagedMatching
            );
        }
    }

    #[test]
    #[ignore]
    fn real_default_state_start_connect_stop_lifecycle() {
        let _guard = crate::state::ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let previous_state = std::env::var_os("RDNY_STATE_DIR");
        let previous_xdg = std::env::var_os("XDG_STATE_HOME");
        let temp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::remove_var("RDNY_STATE_DIR");
            std::env::set_var("XDG_STATE_HOME", temp.path());
        }

        let managed = launch(
            &LaunchOpts::default(),
            crate::state::browser_storage().unwrap(),
        )
        .unwrap_or_else(|err| {
            let log = fs::read_to_string(
                crate::state::default_state_dir()
                    .unwrap()
                    .join("chrome.log"),
            )
            .unwrap_or_default();
            panic!("{err}\nchrome.log:\n{log}");
        });
        crate::state::replace(&managed).unwrap();
        assert!(crate::state::load().unwrap().unwrap().instance_id.is_some());
        let attached = connect_with_policy(&managed.host, managed.port, false).unwrap();
        assert_eq!(attached.ws_url, managed.ws_url);
        assert!(attached.pid.is_none());
        assert_eq!(stop(&managed).unwrap(), StopOutcome::Stopped);
        crate::state::clear().unwrap();

        if let Some(value) = previous_state {
            unsafe { std::env::set_var("RDNY_STATE_DIR", value) };
        } else {
            unsafe { std::env::remove_var("RDNY_STATE_DIR") };
        }
        if let Some(value) = previous_xdg {
            unsafe { std::env::set_var("XDG_STATE_HOME", value) };
        } else {
            unsafe { std::env::remove_var("XDG_STATE_HOME") };
        }
    }
}
