//! Per-instance authenticated broker for managed Chrome remote-debugging pipes.

pub(crate) mod mux;
pub(crate) mod protocol;
pub(crate) mod security;

use std::{
    fs,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{
            net::{UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    browser::LaunchOpts,
    cdp::client::MAX_PIPE_MESSAGE_BYTES,
    process_identity::{self, ProcessIdentity},
    session::Deadline,
    state::{BrokerEndpoint, BrowserStorage, SessionState},
};
use mux::Multiplexer;
use protocol::{BrokerMessage, Token};
use security::{CredentialProvider, OsCredentialProvider};

const STARTUP_FD: RawFd = 5;
const CLIENT_QUEUE: usize = 1024;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Serialize, Deserialize)]
struct Startup {
    instance_id: String,
    token: Vec<u8>,
    state_root: PathBuf,
    binary: PathBuf,
    profile: PathBuf,
    args: Vec<String>,
    expected_uid: u32,
}

#[derive(Debug, Serialize, Deserialize)]
enum StartupReply {
    Ready {
        browser_pid: u32,
        browser_identity: ProcessIdentity,
        browser_name: String,
        target_id: Option<String>,
        socket: PathBuf,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
enum StartupControl {
    Commit,
}

#[derive(Debug, Serialize, Deserialize)]
enum StartupAck {
    Committed,
}

/// A provisional broker. Dropping it before commit closes the anonymous startup
/// channel; the broker then kills/reaps Chrome and removes its socket.
pub(crate) struct ProvisionalBroker {
    startup: UnixStream,
    child: Child,
    pub(crate) state: SessionState,
    committed: bool,
}

impl ProvisionalBroker {
    pub(crate) fn commit(&mut self) -> Result<()> {
        protocol::write_frame(&mut self.startup, &StartupControl::Commit)?;
        match protocol::read_frame_until(&mut self.startup, Deadline::after(STARTUP_TIMEOUT))? {
            StartupAck::Committed => {}
        }
        self.committed = true;
        Ok(())
    }
}

impl Drop for ProvisionalBroker {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.startup.shutdown(std::net::Shutdown::Both);
            let _ = self.child.kill();
            let _ = self.child.wait();
            if let Some(endpoint) = &self.state.endpoint
                && security::validate_socket_path(&endpoint.socket, &OsCredentialProvider).is_ok()
            {
                let _ = fs::remove_file(&endpoint.socket);
            }
        }
    }
}

pub(crate) fn launch_armed(
    opts: &LaunchOpts,
    storage: BrowserStorage,
    instance_id: String,
    deadline: Deadline,
) -> Result<ProvisionalBroker> {
    let binary = crate::browser::discover()?;
    let profile = storage.profile.path().to_path_buf();
    let state_root = profile
        .parent()
        .context("profile has no state root")?
        .to_path_buf();
    storage.profile.validate_external_path()?;
    let socket = security::socket_path(&storage.broker, &instance_id)?;
    if fs::symlink_metadata(&socket).is_ok() {
        bail!(
            "refusing to replace stale broker socket {}; run `rdny cleanup` after validating the old instance",
            socket.display()
        );
    }
    let mut user_args = opts.extra_args.clone();
    if let Ok(raw) = std::env::var("RDNY_CHROME_ARGS") {
        user_args.extend(raw.split_ascii_whitespace().map(String::from));
    }
    let args = crate::browser::build_managed_pipe_args(
        opts,
        &profile,
        &user_args,
        cfg!(target_os = "macos"),
    )?;
    let token = Token::generate()?;
    let startup = Startup {
        instance_id: instance_id.clone(),
        token: token.0.to_vec(),
        state_root: state_root.clone(),
        binary: binary.clone(),
        profile: profile.clone(),
        args,
        expected_uid: unsafe { libc::geteuid() },
    };
    let (mut parent, child_socket) = UnixStream::pair()?;
    set_cloexec(parent.as_raw_fd(), true)?;
    set_cloexec(child_socket.as_raw_fd(), true)?;
    let child_fd = child_socket.as_raw_fd();
    let exe = std::env::current_exe().context("resolving exact current rdny executable")?;
    let log = storage.log;
    let log_err = log.try_clone()?;
    let mut command = Command::new(&exe);
    command
        .arg("__broker")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(child_fd, STARTUP_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            set_cloexec_raw(STARTUP_FD, false)?;
            if child_fd != STARTUP_FD {
                libc::close(child_fd);
            }
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().context("spawning internal broker")?;
    drop(child_socket);
    protocol::write_frame_until(&mut parent, &startup, deadline)?;
    let reply: StartupReply = match protocol::read_frame_until(&mut parent, deadline) {
        Ok(reply) => reply,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error).context("waiting for managed broker startup");
        }
    };
    let (browser_pid, browser_identity, target_id, socket) = match reply {
        StartupReply::Ready {
            browser_pid,
            browser_identity,
            browser_name: _,
            target_id,
            socket,
        } => (browser_pid, browser_identity, target_id, socket),
        StartupReply::Error { message } => {
            let _ = child.wait();
            bail!("managed broker startup failed: {message}");
        }
    };
    let broker_pid = child.id();
    let broker_identity = match process_identity::capture(broker_pid, &exe, None) {
        Ok(identity) => identity,
        Err(error) => {
            let _ = parent.shutdown(std::net::Shutdown::Both);
            let _ = child.kill();
            let _ = child.wait();
            if security::validate_socket_path(&socket, &OsCredentialProvider).is_ok() {
                let _ = fs::remove_file(&socket);
            }
            return Err(error).context("capturing broker identity");
        }
    };
    let endpoint = BrokerEndpoint {
        socket,
        token: token.0.to_vec(),
        version: protocol::VERSION,
        broker_pid,
        broker_identity,
    };
    let state = SessionState {
        instance_id: Some(instance_id),
        endpoint: Some(endpoint),
        ws_url: String::new(),
        host: String::new(),
        port: 0,
        pid: Some(browser_pid),
        process_identity: Some(browser_identity),
        user_data_dir: Some(profile),
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
    Ok(ProvisionalBroker {
        startup: parent,
        child,
        state,
        committed: false,
    })
}

pub(crate) fn connect(state: &SessionState, deadline: Deadline) -> Result<UnixStream> {
    let endpoint = state
        .endpoint
        .as_ref()
        .context("state has no managed broker endpoint")?;
    if endpoint.version != protocol::VERSION {
        bail!("unsupported broker protocol version {}", endpoint.version);
    }
    if !process_identity::matches_identity(endpoint.broker_pid, &endpoint.broker_identity)? {
        bail!("recorded broker process identity is stale or mismatched");
    }
    let creds = OsCredentialProvider;
    let mut stream = security::connect(&endpoint.socket, &creds)?;
    security::verify_peer(&stream, creds.current_uid(), Some(endpoint.broker_pid))?;
    let bytes: [u8; protocol::TOKEN_LEN] = endpoint
        .token
        .clone()
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid broker token length in state"))?;
    protocol::client_handshake_until(
        &mut stream,
        state
            .instance_id
            .as_deref()
            .context("managed broker state missing instance id")?,
        &Token::from_bytes(bytes),
        deadline,
    )?;
    Ok(stream)
}

pub(crate) fn ping(state: &SessionState, deadline: Deadline) -> Result<()> {
    let mut stream = connect(state, deadline)?;
    protocol::write_frame_until(
        &mut stream,
        &BrokerMessage::Ping {
            nonce: Some("health".into()),
        },
        deadline,
    )?;
    match protocol::read_frame_until(&mut stream, deadline)? {
        BrokerMessage::Pong { .. } => Ok(()),
        other => bail!("unexpected broker ping response: {other:?}"),
    }
}

pub(crate) fn stop(state: &SessionState, deadline: Deadline) -> Result<()> {
    let mut stream = connect(state, deadline)?;
    protocol::write_frame_until(
        &mut stream,
        &BrokerMessage::Stop {
            reason: Some("rdny stop".into()),
        },
        deadline,
    )?;
    match protocol::read_frame_until(&mut stream, deadline)? {
        BrokerMessage::Pong { .. } => {}
        other => bail!("unexpected broker stop response: {other:?}"),
    }
    let endpoint = state.endpoint.as_ref().expect("checked by connect");
    while !deadline.expired() {
        if !process_identity::matches_identity(endpoint.broker_pid, &endpoint.broker_identity)
            .unwrap_or(false)
            && fs::symlink_metadata(&endpoint.socket).is_err()
        {
            return Ok(());
        }
        deadline.sleep(Duration::from_millis(25));
    }
    bail!("broker did not exit and clean its socket before stop deadline")
}

pub(crate) fn remove_stale_socket(state: &SessionState) -> Result<()> {
    let endpoint = state
        .endpoint
        .as_ref()
        .context("state has no broker endpoint")?;
    if process_identity::matches_identity(endpoint.broker_pid, &endpoint.broker_identity)? {
        bail!("refusing to remove socket for a live matching broker");
    }
    match fs::symlink_metadata(&endpoint.socket) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => {
            security::validate_socket_path(&endpoint.socket, &OsCredentialProvider)?;
            fs::remove_file(&endpoint.socket)
                .context("removing lifecycle-validated stale broker socket")
        }
    }
}

pub(crate) fn run_hidden() -> Result<()> {
    let mut startup_stream = unsafe { UnixStream::from_raw_fd(STARTUP_FD) };
    set_cloexec(startup_stream.as_raw_fd(), true)?;
    let startup: Startup =
        protocol::read_frame_until(&mut startup_stream, Deadline::after(STARTUP_TIMEOUT))?;
    let result = run_broker(&mut startup_stream, startup);
    if let Err(error) = &result {
        let _ = protocol::write_frame(
            &mut startup_stream,
            &StartupReply::Error {
                message: format!("{error:#}"),
            },
        );
    }
    result
}

fn run_broker(startup_stream: &mut UnixStream, startup: Startup) -> Result<()> {
    if unsafe { libc::geteuid() } != startup.expected_uid {
        bail!("broker UID changed across spawn");
    }
    let token_bytes: [u8; protocol::TOKEN_LEN] = startup
        .token
        .clone()
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid startup token"))?;
    let token = Token::from_bytes(token_bytes);
    let storage = crate::state::browser_storage_at(&startup.state_root)?;
    if storage.profile.path() != startup.profile {
        bail!("startup profile does not match secured state root");
    }
    let creds = OsCredentialProvider;
    let (listener, socket) = security::bind(&storage.broker, &startup.instance_id, &creds)?;
    let mut chrome = match spawn_chrome_pipe(&startup.binary, &startup.args, storage.log) {
        Ok(value) => value,
        Err(error) => {
            let _ = fs::remove_file(&socket);
            return Err(error);
        }
    };
    let result = broker_after_chrome(
        startup_stream,
        &startup,
        &token,
        &listener,
        &socket,
        &mut chrome,
    );
    shutdown_chrome(&mut chrome);
    let _ = fs::remove_file(&socket);
    result
}

struct ChromePipe {
    child: Child,
    read: Option<std::fs::File>,
    write: Arc<Mutex<std::fs::File>>,
}

fn pipe_pair() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    #[cfg(target_os = "linux")]
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    #[cfg(not(target_os = "linux"))]
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let pair = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    #[cfg(not(target_os = "linux"))]
    {
        set_cloexec(pair.0.as_raw_fd(), true)?;
        set_cloexec(pair.1.as_raw_fd(), true)?;
    }
    Ok(pair)
}

fn set_cloexec(fd: RawFd, enabled: bool) -> Result<()> {
    set_cloexec_raw(fd, enabled).map_err(Into::into)
}

fn set_cloexec_raw(fd: RawFd, enabled: bool) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let next = if enabled {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, next) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn duplicate_cloexec(fd: RawFd) -> Result<OwnedFd> {
    let duplicated = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) };
    if duplicated < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}

fn spawn_chrome_pipe(binary: &Path, args: &[String], log: std::fs::File) -> Result<ChromePipe> {
    spawn_chrome_pipe_with_env(binary, args, log, None)
}

fn spawn_chrome_pipe_with_env(
    binary: &Path,
    args: &[String],
    log: std::fs::File,
    test_env: Option<(&str, &str)>,
) -> Result<ChromePipe> {
    let (from_chrome_read, from_chrome_write) = pipe_pair()?;
    let (to_chrome_read, to_chrome_write) = pipe_pair()?;
    // Chromium reads commands from fd3 and writes NUL-delimited responses to
    // fd4 (the parent-side directions are therefore the inverse).
    // Stable high-numbered copies avoid fd3/fd4 source/target collisions.
    let chrome_input = duplicate_cloexec(to_chrome_read.as_raw_fd())?;
    let chrome_output = duplicate_cloexec(from_chrome_write.as_raw_fd())?;
    let fd3 = chrome_input.as_raw_fd();
    let fd4 = chrome_output.as_raw_fd();
    let log_err = log.try_clone()?;
    let mut command = Command::new(binary);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    if let Some((name, value)) = test_env {
        command.env(name, value);
    }
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(fd3, 3) < 0 || libc::dup2(fd4, 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            set_cloexec_raw(3, false)?;
            set_cloexec_raw(4, false)?;
            // Mark every other descriptor close-on-exec. Marking rather than
            // closing preserves std::process's private exec-error pipe until
            // exec succeeds, while the child executable still receives only
            // stdio and fd3/fd4.
            let max = libc::sysconf(libc::_SC_OPEN_MAX).clamp(5, 65_536);
            for fd in 5..max as RawFd {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags >= 0 {
                    libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                }
            }
            Ok(())
        });
    }
    let child = command
        .spawn()
        .with_context(|| format!("launching {} with remote-debugging pipe", binary.display()))?;
    drop(from_chrome_write);
    drop(to_chrome_read);
    drop(chrome_input);
    drop(chrome_output);
    Ok(ChromePipe {
        child,
        read: Some(std::fs::File::from(from_chrome_read)),
        write: Arc::new(Mutex::new(std::fs::File::from(to_chrome_write))),
    })
}

fn raw_send(write: &Arc<Mutex<std::fs::File>>, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(0);
    let mut guard = write
        .lock()
        .map_err(|_| anyhow::anyhow!("Chrome pipe writer poisoned"))?;
    guard.write_all(&bytes)?;
    guard.flush()?;
    Ok(())
}

fn raw_read(read: &mut std::fs::File, buffer: &mut Vec<u8>) -> Result<Value> {
    loop {
        if let Some(pos) = buffer.iter().position(|b| *b == 0) {
            let frame: Vec<_> = buffer.drain(..=pos).take(pos).collect();
            return serde_json::from_slice(&frame).context("parsing Chrome pipe message");
        }
        if buffer.len() >= MAX_PIPE_MESSAGE_BYTES {
            bail!("Chrome pipe message exceeds limit");
        }
        let mut chunk = [0; 8192];
        let n = read.read(&mut chunk)?;
        if n == 0 {
            bail!("Chrome debugging pipe closed");
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
}

fn startup_call(chrome: &mut ChromePipe, id: u64, method: &str) -> Result<Value> {
    raw_send(&chrome.write, &json!({"id":id,"method":method,"params":{}}))?;
    let read = chrome.read.as_mut().context("Chrome pipe reader missing")?;
    let mut buf = Vec::new();
    loop {
        let value = raw_read(read, &mut buf)?;
        if value["id"] == id {
            if value.get("error").is_some() {
                bail!("{method} failed: {}", value["error"]);
            }
            return Ok(value["result"].clone());
        }
    }
}

fn broker_after_chrome(
    startup_stream: &mut UnixStream,
    startup: &Startup,
    token: &Token,
    listener: &UnixListener,
    socket: &Path,
    chrome: &mut ChromePipe,
) -> Result<()> {
    let version = startup_call(chrome, 1, "Browser.getVersion")?;
    let targets = startup_call(chrome, 2, "Target.getTargets")?;
    let target_id = targets["targetInfos"]
        .as_array()
        .and_then(|items| items.iter().find(|v| v["type"] == "page"))
        .and_then(|v| v["targetId"].as_str())
        .map(str::to_string);
    let browser_identity =
        process_identity::capture(chrome.child.id(), &startup.binary, Some(&startup.profile))?;
    protocol::write_frame(
        startup_stream,
        &StartupReply::Ready {
            browser_pid: chrome.child.id(),
            browser_identity,
            browser_name: version["product"].as_str().unwrap_or("Chrome").to_string(),
            target_id,
            socket: socket.to_path_buf(),
        },
    )?;
    let control: StartupControl =
        protocol::read_frame_until(startup_stream, Deadline::after(STARTUP_TIMEOUT))
            .context("parent exited before broker startup commit")?;
    if !matches!(control, StartupControl::Commit) {
        bail!("invalid startup commit");
    }
    protocol::write_frame(startup_stream, &StartupAck::Committed)
        .context("acknowledging broker startup commit")?;
    serve(listener, startup, token, chrome)
}

enum Input {
    Client(u64, BrokerMessage),
    Gone(u64),
    Chrome(Value),
    ChromeGone(String),
}

fn serve(
    listener: &UnixListener,
    startup: &Startup,
    token: &Token,
    chrome: &mut ChromePipe,
) -> Result<()> {
    listener.set_nonblocking(true)?;
    let (input_tx, input_rx) = mpsc::channel();
    let mut read = chrome.read.take().context("Chrome reader already taken")?;
    let chrome_tx = input_tx.clone();
    thread::spawn(move || {
        let mut buf = Vec::new();
        loop {
            match raw_read(&mut read, &mut buf) {
                Ok(v) => {
                    if chrome_tx.send(Input::Chrome(v)).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = chrome_tx.send(Input::ChromeGone(format!("{e:#}")));
                    break;
                }
            }
        }
    });
    let mut mux = Multiplexer::new();
    let mut writers = std::collections::HashMap::new();
    let mut next_client = 1u64;
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                if security::verify_peer(&stream, startup.expected_uid, None).is_err() {
                    continue;
                }
                if protocol::server_handshake(&mut stream, &startup.instance_id, token).is_err() {
                    continue;
                }
                let id = next_client;
                next_client += 1;
                mux.attach_client(id);
                let (out_tx, out_rx) = mpsc::sync_channel::<BrokerMessage>(CLIENT_QUEUE);
                writers.insert(id, out_tx);
                let mut writer = stream.try_clone()?;
                let writer_tx = input_tx.clone();
                thread::spawn(move || {
                    while let Ok(msg) = out_rx.recv() {
                        if protocol::write_frame(&mut writer, &msg).is_err() {
                            let _ = writer_tx.send(Input::Gone(id));
                            break;
                        }
                    }
                });
                let tx = input_tx.clone();
                thread::spawn(move || {
                    loop {
                        match protocol::read_frame_until::<BrokerMessage>(
                            &mut stream,
                            Deadline::after(Duration::from_secs(24 * 60 * 60)),
                        ) {
                            Ok(msg) => {
                                if tx.send(Input::Client(id, msg)).is_err() {
                                    break;
                                }
                            }
                            Err(_) => {
                                let _ = tx.send(Input::Gone(id));
                                break;
                            }
                        }
                    }
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e).context("accepting broker client"),
        }
        match input_rx.recv_timeout(Duration::from_millis(10)) {
            Ok(Input::Client(id, BrokerMessage::Ping { nonce })) => {
                send_to_client(
                    &mut mux,
                    &mut writers,
                    id,
                    BrokerMessage::Pong { nonce },
                    &chrome.write,
                );
            }
            Ok(Input::Client(id, BrokerMessage::Stop { .. })) => {
                let _ = raw_send(
                    &chrome.write,
                    &json!({"id":u64::MAX,"method":"Browser.close","params":{}}),
                );
                send_to_client(
                    &mut mux,
                    &mut writers,
                    id,
                    BrokerMessage::Pong {
                        nonce: Some("stopping".into()),
                    },
                    &chrome.write,
                );
                return Ok(());
            }
            Ok(Input::Client(id, BrokerMessage::Cdp { message })) => {
                match mux.client_request(id, message) {
                    Ok(up) => raw_send(&chrome.write, &up)?,
                    Err(e) => {
                        send_to_client(
                            &mut mux,
                            &mut writers,
                            id,
                            BrokerMessage::Error {
                                message: format!("{e:#}"),
                            },
                            &chrome.write,
                        );
                    }
                }
            }
            Ok(Input::Client(_, _)) => {}
            Ok(Input::Gone(id)) => {
                disconnect_client(&mut mux, &mut writers, id, &chrome.write);
            }
            Ok(Input::Chrome(value)) => {
                if let Some(session_id) = mux.take_late_attached_session(&value) {
                    send_detach(&chrome.write, &session_id);
                    continue;
                }
                if let Some(session_id) = mux.take_unowned_attached_session(&value) {
                    send_detach(&chrome.write, &session_id);
                    continue;
                }
                if let Some(out) = mux.upstream_message(value)? {
                    send_to_client(
                        &mut mux,
                        &mut writers,
                        out.client_id,
                        BrokerMessage::Cdp { message: out.value },
                        &chrome.write,
                    );
                }
            }
            Ok(Input::ChromeGone(error)) => bail!("browser pipe failed closed: {error}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if chrome.child.try_wait()?.is_some() {
                    bail!("browser exited");
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!("broker event loop disconnected"),
        }
    }
}

fn send_detach(write: &Arc<Mutex<std::fs::File>>, session_id: &str) {
    static NEXT_DETACH: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(u64::MAX / 2);
    let id = NEXT_DETACH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let _ = raw_send(
        write,
        &json!({"id":id,"method":"Target.detachFromTarget","params":{"sessionId":session_id}}),
    );
}

fn disconnect_client(
    mux: &mut Multiplexer,
    writers: &mut std::collections::HashMap<u64, mpsc::SyncSender<BrokerMessage>>,
    client_id: u64,
    chrome_write: &Arc<Mutex<std::fs::File>>,
) {
    writers.remove(&client_id);
    for session_id in mux.disconnect_client(client_id) {
        send_detach(chrome_write, &session_id);
    }
}

fn send_to_client(
    mux: &mut Multiplexer,
    writers: &mut std::collections::HashMap<u64, mpsc::SyncSender<BrokerMessage>>,
    client_id: u64,
    message: BrokerMessage,
    chrome_write: &Arc<Mutex<std::fs::File>>,
) {
    let failed = writers
        .get(&client_id)
        .is_none_or(|writer| writer.try_send(message).is_err());
    if failed {
        disconnect_client(mux, writers, client_id, chrome_write);
    }
}

fn shutdown_chrome(chrome: &mut ChromePipe) {
    if chrome.child.try_wait().ok().flatten().is_none() {
        let _ = raw_send(
            &chrome.write,
            &json!({"id":u64::MAX-1,"method":"Browser.close","params":{}}),
        );
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until {
            if chrome.child.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = chrome.child.kill();
    }
    let _ = chrome.child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::IntoRawFd;

    fn empty_state() -> SessionState {
        SessionState {
            instance_id: Some("test".into()),
            endpoint: None,
            ws_url: String::new(),
            host: String::new(),
            port: 0,
            pid: None,
            process_identity: None,
            user_data_dir: None,
            browser_path: None,
            target_id: None,
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
    fn missing_commit_ack_keeps_guard_armed_and_kills_child() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let reader = thread::spawn(move || {
            let _: StartupControl = protocol::read_frame(&mut server).unwrap();
            // Simulate broker death after receiving commit but before ACK.
        });
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "broker::tests::long_running_process_helper",
                "--nocapture",
            ])
            .env("RDNY_BROKER_LONG_RUNNING_HELPER", "1")
            .spawn()
            .unwrap();
        let pid = child.id();
        let mut provisional = ProvisionalBroker {
            startup: client,
            child,
            state: empty_state(),
            committed: false,
        };
        assert!(provisional.commit().is_err());
        assert!(!provisional.committed);
        drop(provisional);
        reader.join().unwrap();
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    }

    #[test]
    fn long_running_process_helper() {
        if std::env::var_os("RDNY_BROKER_LONG_RUNNING_HELPER").is_none() {
            return;
        }
        loop {
            thread::sleep(Duration::from_secs(60));
        }
    }

    #[test]
    fn fd_report_helper() {
        if std::env::var_os("RDNY_FD_REPORT_HELPER").is_none() {
            return;
        }
        let open: Vec<_> = (0..64)
            .filter(|fd| unsafe { libc::fcntl(*fd, libc::F_GETFD) } >= 0)
            .collect();
        let mut bytes = serde_json::to_vec(&json!({"open":open})).unwrap();
        bytes.push(0);
        let written = unsafe { libc::write(4, bytes.as_ptr().cast(), bytes.len()) };
        assert_eq!(written as usize, bytes.len());
    }

    #[test]
    fn chrome_child_inherits_only_stdio_and_pipe_fds() {
        run_fd_report_child();
    }

    fn run_fd_report_child() {
        let temp = tempfile::tempdir().unwrap();
        let log = fs::File::create(temp.path().join("child.log")).unwrap();
        let exe = std::env::current_exe().unwrap();
        let args = vec![
            "broker::tests::fd_report_helper".to_string(),
            "--exact".to_string(),
        ];
        let mut child =
            spawn_chrome_pipe_with_env(&exe, &args, log, Some(("RDNY_FD_REPORT_HELPER", "1")))
                .unwrap();
        let mut buffer = Vec::new();
        let report = raw_read(child.read.as_mut().unwrap(), &mut buffer).unwrap();
        assert_eq!(report["open"], json!([0, 1, 2, 3, 4]));
        assert!(child.child.wait().unwrap().success());
    }

    #[test]
    fn fd_collision_process_helper() {
        if std::env::var_os("RDNY_FD_COLLISION_HELPER").is_none() {
            return;
        }
        unsafe {
            libc::close(3);
            libc::close(4);
        }
        // pipe_pair now necessarily reuses fd3/fd4 for source descriptors;
        // collision-safe high duplicates must still produce correct fd3/fd4.
        run_fd_report_child();
    }

    #[test]
    fn chrome_pipe_setup_handles_fd3_fd4_source_collisions() {
        let status = Command::new(std::env::current_exe().unwrap())
            .arg("broker::tests::fd_collision_process_helper")
            .arg("--exact")
            .env("RDNY_FD_COLLISION_HELPER", "1")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn full_production_writer_queue_disconnects_and_detaches() {
        let mut mux = Multiplexer::new();
        mux.attach_client(7);
        let request = mux
            .client_request(7, json!({"id":1,"method":"Target.attachToTarget"}))
            .unwrap();
        mux.upstream_message(json!({"id":request["id"],"result":{"sessionId":"owned"}}))
            .unwrap();

        let (tx, rx) = mpsc::sync_channel(1);
        tx.try_send(BrokerMessage::Pong { nonce: None }).unwrap();
        let mut writers = std::collections::HashMap::from([(7, tx)]);
        let (read_fd, write_fd) = pipe_pair().unwrap();
        let write = Arc::new(Mutex::new(std::fs::File::from(write_fd)));

        // This is the production dispatch path with a genuinely full bounded
        // SyncSender and no receiver progress (a blocked writer).
        send_to_client(
            &mut mux,
            &mut writers,
            7,
            BrokerMessage::Pong { nonce: None },
            &write,
        );
        assert!(!writers.contains_key(&7));
        drop(rx);
        let mut read = std::fs::File::from(read_fd);
        let detach = raw_read(&mut read, &mut Vec::new()).unwrap();
        assert_eq!(detach["method"], "Target.detachFromTarget");
        assert_eq!(detach["params"]["sessionId"], "owned");
    }

    #[test]
    fn pipe_descriptors_are_cloexec_by_default() {
        let (read, write) = pipe_pair().unwrap();
        for fd in [read.into_raw_fd(), write.into_raw_fd()] {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
            unsafe { libc::close(fd) };
        }
    }

    #[test]
    #[ignore = "real managed broker test: set RDNY_BROWSER_TESTS=1 and RDNY_BIN"]
    fn real_broker_kill_closes_owned_chrome_pipe() {
        if std::env::var_os("RDNY_BROWSER_TESTS").is_none() {
            return;
        }
        let bin = std::env::var_os("RDNY_BIN").expect("RDNY_BIN is required");
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        assert!(
            Command::new(&bin)
                .arg("start")
                .env("RDNY_STATE_DIR", &state_dir)
                .status()
                .unwrap()
                .success()
        );
        let state: SessionState =
            serde_json::from_slice(&fs::read(state_dir.join("state.json")).unwrap()).unwrap();
        let browser_identity = state.process_identity.as_ref().unwrap();
        assert_eq!(
            browser_identity
                .argv
                .iter()
                .filter(|arg| arg.to_string_lossy() == "--remote-debugging-pipe")
                .count(),
            1
        );
        assert!(
            !browser_identity
                .argv
                .iter()
                .any(|arg| arg.to_string_lossy().starts_with("--remote-debugging-port"))
        );
        #[cfg(target_os = "macos")]
        {
            let output = Command::new("/usr/sbin/lsof")
                .args(["-Pan", "-p", &state.pid.unwrap().to_string(), "-iTCP"])
                .output()
                .unwrap();
            assert!(
                !String::from_utf8_lossy(&output.stdout).contains("LISTEN"),
                "managed Chrome unexpectedly has a TCP listener"
            );
        }
        let endpoint = state.endpoint.as_ref().unwrap();
        assert_eq!(
            unsafe { libc::kill(endpoint.broker_pid as i32, libc::SIGKILL) },
            0
        );
        let browser_pid = state.pid.unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline && unsafe { libc::kill(browser_pid as i32, 0) } == 0 {
            thread::sleep(Duration::from_millis(50));
        }
        assert_ne!(
            unsafe { libc::kill(browser_pid as i32, 0) },
            0,
            "Chrome survived broker pipe EOF"
        );
        assert!(
            !state
                .user_data_dir
                .as_ref()
                .unwrap()
                .join("DevToolsActivePort")
                .exists()
        );
        let _ = Command::new(&bin)
            .arg("cleanup")
            .env("RDNY_STATE_DIR", &state_dir)
            .status();
    }

    #[test]
    #[ignore = "real managed startup crash test: set RDNY_BROWSER_TESTS=1 and RDNY_BIN"]
    fn real_cli_crash_before_commit_leaves_no_untracked_process() {
        if std::env::var_os("RDNY_BROWSER_TESTS").is_none() {
            return;
        }
        let bin = std::env::var_os("RDNY_BIN").expect("RDNY_BIN is required");
        let temp = tempfile::tempdir().unwrap();
        let state_dir = temp.path().join("state");
        let marker = temp.path().join("pids.json");
        let status = Command::new(&bin)
            .arg("start")
            .env("RDNY_STATE_DIR", &state_dir)
            .env("XDG_STATE_HOME", temp.path().join("xdg"))
            .env("RDNY_TEST_CRASH_BEFORE_BROKER_COMMIT", &marker)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(86));
        let evidence: Value = serde_json::from_slice(&fs::read(&marker).unwrap()).unwrap();
        let broker = evidence["broker"].as_u64().unwrap() as i32;
        let browser = evidence["browser"].as_u64().unwrap() as i32;
        let socket = PathBuf::from(evidence["socket"].as_str().unwrap());
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline
            && (unsafe { libc::kill(broker, 0) } == 0
                || unsafe { libc::kill(browser, 0) } == 0
                || socket.exists())
        {
            thread::sleep(Duration::from_millis(50));
        }
        assert_ne!(unsafe { libc::kill(broker, 0) }, 0);
        assert_ne!(unsafe { libc::kill(browser, 0) }, 0);
        assert!(!socket.exists());
        assert!(!state_dir.join("state.json").exists());
        assert!(!temp.path().join("xdg/rdny/instances.json").exists());
    }
}
