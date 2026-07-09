//! Browser discovery, launch, and lifecycle (start/connect/stop/status).

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::cdp::http;
use crate::config;
use crate::hint::hint_error;
use crate::state::SessionState;

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
pub fn launch(opts: &LaunchOpts, data_root: &Path) -> Result<SessionState> {
    let binary = discover()?;
    fs::create_dir_all(data_root).with_context(|| format!("creating {}", data_root.display()))?;
    let profile_dir = data_root.join("chrome-profile");
    fs::create_dir_all(&profile_dir)
        .with_context(|| format!("creating {}", profile_dir.display()))?;
    let active_port_path = profile_dir.join("DevToolsActivePort");
    let _ = fs::remove_file(&active_port_path);

    let mut user_args = opts.extra_args.clone();
    // Split RDNY_CHROME_ARGS on ASCII whitespace. Shell-style quoting is not supported.
    if let Ok(raw) = std::env::var("RDNY_CHROME_ARGS") {
        user_args.extend(raw.split_ascii_whitespace().map(String::from));
    }
    let args = build_args(opts, &profile_dir, &user_args, cfg!(target_os = "macos"));

    let log = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(data_root.join("chrome.log"))
        .with_context(|| format!("opening {}/chrome.log", data_root.display()))?;
    let log_err = log.try_clone().context("cloning chrome log handle")?;
    let mut child = Command::new(&binary)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
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

    Ok(SessionState {
        ws_url: version.ws_url,
        host: "127.0.0.1".into(),
        port,
        pid: Some(child.id()),
        user_data_dir: Some(profile_dir),
        browser_path: Some(binary),
        target_id,
        label: opts.label.clone(),
        viewport: None,
        recording: false,
    })
}

/// Attach to an already-running browser at host:port.
pub fn connect(host: &str, port: u16) -> Result<SessionState> {
    let version = http::version(host, port).map_err(|_| {
        let relaunch = if cfg!(target_os = "macos") {
            format!("relaunch it like `open -na Helium --args --remote-debugging-port={port}`")
        } else {
            format!("relaunch it like `chromium --remote-debugging-port={port}`")
        };
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
    let pid = pid as libc::pid_t;
    reap_if_child(pid);
    if !pid_exists(pid) {
        return Ok(StopOutcome::Stopped);
    }
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        reap_if_child(pid);
        if !pid_exists(pid) {
            return Ok(StopOutcome::Stopped);
        }
        thread::sleep(POLL_INTERVAL);
    }
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        reap_if_child(pid);
        if !pid_exists(pid) {
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }
    Ok(StopOutcome::Stopped)
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
        if let Ok(contents) = fs::read_to_string(path) {
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
    unsafe { libc::kill(pid, 0) == 0 }
}

fn kill_child(child: &mut std::process::Child) {
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGKILL);
    }
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

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
    #[ignore]
    fn real_launch_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let state = launch(&LaunchOpts::default(), dir.path()).unwrap_or_else(|err| {
            let log = fs::read_to_string(dir.path().join("chrome.log")).unwrap_or_default();
            panic!("{err}\nchrome.log:\n{log}");
        });
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
