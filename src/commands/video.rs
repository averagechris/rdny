//! Video recording via CDP screencast frames.

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::commands::decode_base64;
use crate::config;
use crate::hint::hint_error;
use crate::{session::PageSession, state};

const LAST_FRAME_DURATION: f64 = 0.1;
static RECORDING_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn start() -> Result<()> {
    let mut allocated: Option<(String, state::SecureDir)> = None;
    let result = state::update(|state| {
        if state.recording {
            return Err(hint_error(
                "video recording already active",
                "run `rdny stop-video` before starting another recording",
                None,
            ));
        }
        let (id, frames_dir) = allocate_recording_frames_dir()?;
        frames_dir.validate_external_path()?;
        state.recording = true;
        state.recording_id = Some(id.clone());
        state.recording_frames_dir = Some(frames_dir.path().to_path_buf());
        allocated = Some((id, frames_dir));
        Ok(())
    });
    if result.is_err()
        && let Some((id, _)) = allocated
    {
        let _ = state::remove_recording_dir(&id);
    }
    result
}

pub fn stop(session: Option<&mut PageSession>, output: Option<&Path>, force: bool) -> Result<()> {
    if let Some(session) = session {
        let _ = session.call("Page.stopScreencast", serde_json::json!({}));
        session.drain_events(std::time::Duration::from_millis(300))?;
    }

    let (recording_id, frames_path, lease) = deactivate_or_recover_recording()?;
    let mut guard = AssemblingGuard::new(recording_id.clone(), lease);
    let frames_dir = state::open_frames_dir_from_path(&frames_path)?;
    frames_dir.validate_external_path()?;
    let frames = read_frames(&frames_dir)?;
    if frames.is_empty() {
        return Err(hint_error(
            "no video frames were captured",
            format!(
                "recording {recording_id} is recoverable at {}; start a new recording or leave the directory for inspection",
                frames_dir.path().display()
            ),
            None,
        ));
    }

    let output = output.unwrap_or_else(|| Path::new("recording.mp4"));
    let list = concat_list(&frames);
    frames_dir
        .write_file("frames.txt", list.as_bytes())
        .context("writing ffmpeg concat list")?;
    let list_path = frames_dir.path().join("frames.txt");

    let config = config::load()?;
    let ffmpeg = config::resolve_ffmpeg(std::env::var_os("RDNY_FFMPEG"), &config);
    assemble_video(&ffmpeg, &frames_dir, &list_path, output, force)?;
    clear_recoverable_recording(&recording_id)?;
    guard.disarm();
    if let Err(err) = remove_recording_frames(&recording_id, &frames_dir) {
        eprintln!(
            "warning: assembled recording {recording_id}, but could not remove frames at {}: {err}; remove them manually after verifying the output",
            frames_dir.path().display()
        );
    }
    println!("{}", output.display());
    Ok(())
}

fn assemble_video(
    ffmpeg: &Path,
    frames_dir: &state::SecureDir,
    list_path: &Path,
    output: &Path,
    force: bool,
) -> Result<()> {
    let reservation = crate::commands::artifacts::ReservedArtifact::reserve(output, force)?;
    let mut command = Command::new(ffmpeg);
    command
        .args([OsStr::new("-loglevel"), OsStr::new("error")])
        .args([OsStr::new("-y"), OsStr::new("-f"), OsStr::new("concat")])
        .args([OsStr::new("-safe"), OsStr::new("0"), OsStr::new("-i")])
        .arg(list_path)
        .args([
            OsStr::new("-vf"),
            OsStr::new("pad=ceil(iw/2)*2:ceil(ih/2)*2"),
        ])
        .args([OsStr::new("-pix_fmt"), OsStr::new("yuv420p")]);
    if let Some(format) = muxer_for_output(output) {
        command.args([OsStr::new("-f"), OsStr::new(format)]);
    }
    let status = command.arg(reservation.tmp_path()).status();

    match status {
        Ok(status) if status.success() => {
            reservation.finalize(force)?;
            Ok(())
        }
        Ok(status) => Err(hint_error(
            format!("ffmpeg failed with status {status}"),
            format!(
                "install/fix ffmpeg and retry; frames are preserved at {}",
                frames_dir.path().display()
            ),
            None,
        )),
        Err(err) => Err(hint_error(
            format!("could not run {}: {err}", ffmpeg.display()),
            format!(
                "install ffmpeg, set RDNY_FFMPEG, or set binaries.ffmpeg in the rdny config file; frames are preserved at {}",
                frames_dir.path().display()
            ),
            None,
        )),
    }
}

fn muxer_for_output(output: &Path) -> Option<&'static str> {
    match output
        .extension()
        .and_then(OsStr::to_str)?
        .to_ascii_lowercase()
        .as_str()
    {
        "mp4" | "m4v" => Some("mp4"),
        "mov" => Some("mov"),
        "mkv" => Some("matroska"),
        "webm" => Some("webm"),
        _ => None,
    }
}

fn allocate_recording_frames_dir() -> Result<(String, state::SecureDir)> {
    for _ in 0..16 {
        let id = new_recording_id();
        if let Some(frames) = state::create_recording_frames_dir(&id)? {
            return Ok((id, frames));
        }
    }
    bail!("could not allocate a unique recording directory");
}

struct AssemblingGuard {
    recording_id: String,
    _lease: Option<state::RecordingLease>,
    armed: bool,
}

impl AssemblingGuard {
    fn new(recording_id: String, lease: state::RecordingLease) -> Self {
        Self {
            recording_id,
            _lease: Some(lease),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for AssemblingGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = mark_recoverable(&self.recording_id);
        }
    }
}

fn deactivate_or_recover_recording() -> Result<(String, PathBuf, state::RecordingLease)> {
    let mut selected: Option<(String, PathBuf)> = None;
    let mut selected_lease: Option<state::RecordingLease> = None;
    let legacy_frames = state::frames_dir()?.path().to_path_buf();
    state::update(|state| {
        migrate_singular_recovery(state);
        if state.recording {
            let id = state
                .recording_id
                .clone()
                .unwrap_or_else(|| "legacy".to_string());
            let frames = state
                .recording_frames_dir
                .clone()
                .unwrap_or_else(|| legacy_frames.clone());
            let Some(lease) = state::try_recording_lease(&frames)? else {
                return Err(hint_error(
                    "video recording is already being assembled",
                    "wait for the running `rdny stop-video` to finish, then retry if needed",
                    None,
                ));
            };
            state.recording = false;
            state.recording_id = None;
            state.recording_frames_dir = None;
            upsert_recovery(
                state,
                state::RecoverableRecording {
                    id: id.clone(),
                    frames_dir: frames.clone(),
                    status: state::RecordingStatus::Assembling,
                },
            );
            selected = Some((id, frames));
            selected_lease = Some(lease);
            Ok(())
        } else {
            for index in 0..state.recoverable_recordings.len() {
                let recoverable = state.recoverable_recordings[index].clone();
                let Some(lease) = state::try_recording_lease(&recoverable.frames_dir)? else {
                    continue;
                };
                state.recoverable_recordings[index].status = state::RecordingStatus::Assembling;
                selected = Some((recoverable.id, recoverable.frames_dir));
                selected_lease = Some(lease);
                return Ok(());
            }
            let has_live_assembly = state
                .recoverable_recordings
                .iter()
                .any(|recording| recording.status == state::RecordingStatus::Assembling);
            Err(hint_error(
                "no active or recoverable video recording",
                if has_live_assembly {
                    "a recording is currently being assembled by another process; wait for it to finish or crash before retrying"
                } else {
                    "run `rdny start-video`, then interact with the page, then `rdny stop-video`; retries use the oldest recoverable recording first"
                },
                None,
            ))
        }
    })?;
    let (id, frames) = selected.expect("state update selected recording");
    let lease = selected_lease.expect("state update selected recording lease");
    Ok((id, frames, lease))
}

fn clear_recoverable_recording(recording_id: &str) -> Result<()> {
    state::update_if_present(|state| {
        migrate_singular_recovery(state);
        state
            .recoverable_recordings
            .retain(|recording| recording.id != recording_id);
        Ok(())
    })?;
    Ok(())
}

fn mark_recoverable(recording_id: &str) -> Result<()> {
    state::update_if_present(|state| {
        migrate_singular_recovery(state);
        if let Some(recording) = state
            .recoverable_recordings
            .iter_mut()
            .find(|recording| recording.id == recording_id)
        {
            recording.status = state::RecordingStatus::Recoverable;
        }
        Ok(())
    })?;
    Ok(())
}

fn migrate_singular_recovery(state: &mut state::SessionState) {
    if let Some(mut recording) = state.recoverable_recording.take()
        && !state
            .recoverable_recordings
            .iter()
            .any(|existing| existing.id == recording.id)
    {
        recording.status = state::RecordingStatus::Recoverable;
        state.recoverable_recordings.push(recording);
    }
}

fn upsert_recovery(state: &mut state::SessionState, recording: state::RecoverableRecording) {
    if let Some(existing) = state
        .recoverable_recordings
        .iter_mut()
        .find(|existing| existing.id == recording.id)
    {
        *existing = recording;
    } else {
        state.recoverable_recordings.push(recording);
    }
}

fn remove_recording_frames(recording_id: &str, _frames_dir: &state::SecureDir) -> Result<()> {
    if recording_id == "legacy" {
        state::remove_frames_dir()
    } else {
        state::remove_recording_dir(recording_id)
    }
}

fn new_recording_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let random = random_u64();
    let counter = RECORDING_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
    format!(
        "{millis}-{}-{random:016x}-{counter:016x}",
        std::process::id()
    )
}

fn random_u64() -> u64 {
    let mut bytes = [0_u8; 8];
    if let Ok(mut file) = fs::File::open("/dev/urandom") {
        use std::io::Read;
        if file.read_exact(&mut bytes).is_ok() {
            return u64::from_ne_bytes(bytes);
        }
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

pub(crate) fn handle_screencast_frame(
    params: &Value,
    frames_dir: &state::SecureDir,
) -> Result<Option<String>> {
    let Some(data) = params.get("data").and_then(Value::as_str) else {
        return Ok(None);
    };
    let Some(timestamp) = params
        .get("metadata")
        .and_then(|m| m.get("timestamp"))
        .and_then(Value::as_f64)
    else {
        return Ok(None);
    };
    let bytes = decode_base64(data)?;
    frames_dir
        .write_file(&format!("{timestamp:.6}.jpg"), &bytes)
        .context("writing screencast frame")?;
    Ok(params
        .get("sessionId")
        .and_then(Value::as_i64)
        .map(|id| id.to_string()))
}

pub fn concat_list(frames: &[(f64, PathBuf)]) -> String {
    let mut frames = frames.to_vec();
    frames.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
    let mut out = String::new();
    for (idx, (timestamp, path)) in frames.iter().enumerate() {
        out.push_str(&format!("file '{}'\n", escape_path(path)));
        let duration = frames
            .get(idx + 1)
            .map(|(next, _)| (next - timestamp).max(0.001))
            .unwrap_or(LAST_FRAME_DURATION);
        out.push_str(&format!("duration {duration:.6}\n"));
    }
    if let Some((_, path)) = frames.last() {
        out.push_str(&format!("file '{}'\n", escape_path(path)));
    }
    out
}

fn escape_path(path: &Path) -> String {
    path.to_string_lossy().replace('\'', "'\\''")
}

fn read_frames(frames_dir: &state::SecureDir) -> Result<Vec<(f64, PathBuf)>> {
    let mut frames = Vec::new();
    for path in frames_dir.regular_paths_with_suffix(".jpg")? {
        let Some(stem) = path.file_stem().and_then(OsStr::to_str) else {
            continue;
        };
        let timestamp = stem
            .parse::<f64>()
            .with_context(|| format!("parsing video frame timestamp from {}", path.display()))?;
        // The path is beneath a descriptor-validated, cross-UID-safe ancestor
        // chain. Same-UID lifecycle orchestration remains #130/#131 work.
        frames.push((timestamp, path));
    }
    if frames.iter().any(|(timestamp, _)| !timestamp.is_finite()) {
        bail!("video frame timestamp is not finite");
    }
    frames.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::{env, panic};

    struct EnvGuard {
        state_dir: Option<std::ffi::OsString>,
        ffmpeg: Option<std::ffi::OsString>,
        config: Option<std::ffi::OsString>,
        cwd: PathBuf,
    }

    impl EnvGuard {
        fn new() -> Self {
            Self {
                state_dir: env::var_os("RDNY_STATE_DIR"),
                ffmpeg: env::var_os("RDNY_FFMPEG"),
                config: env::var_os("RDNY_CONFIG"),
                cwd: env::current_dir().unwrap(),
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.state_dir {
                unsafe { env::set_var("RDNY_STATE_DIR", value) };
            } else {
                unsafe { env::remove_var("RDNY_STATE_DIR") };
            }
            if let Some(value) = &self.ffmpeg {
                unsafe { env::set_var("RDNY_FFMPEG", value) };
            } else {
                unsafe { env::remove_var("RDNY_FFMPEG") };
            }
            if let Some(value) = &self.config {
                unsafe { env::set_var("RDNY_CONFIG", value) };
            } else {
                unsafe { env::remove_var("RDNY_CONFIG") };
            }
            env::set_current_dir(&self.cwd).unwrap();
        }
    }
    fn sample_state() -> state::SessionState {
        state::SessionState {
            ws_url: "ws://127.0.0.1:9222/devtools/browser/abc".to_string(),
            host: "127.0.0.1".to_string(),
            port: 9222,
            pid: None,
            user_data_dir: None,
            browser_path: None,
            target_id: Some("target-1".to_string()),
            label: None,
            viewport: None,
            recording: false,
            recording_id: None,
            recording_frames_dir: None,
            recoverable_recording: None,
            recoverable_recordings: Vec::new(),
        }
    }

    fn with_state_dir<T>(f: impl FnOnce(&Path) -> T) -> T {
        let _guard = state::ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env = EnvGuard::new();
        let temp = tempfile::tempdir().unwrap();
        unsafe { env::set_var("RDNY_STATE_DIR", temp.path()) };
        f(temp.path())
    }

    fn assert_recoverable(id: &str) {
        let st = state::load().unwrap().unwrap();
        let recording = st
            .recoverable_recordings
            .iter()
            .find(|recording| recording.id == id)
            .unwrap_or_else(|| panic!("missing recoverable recording {id}"));
        assert_eq!(recording.status, state::RecordingStatus::Recoverable);
    }

    fn start_with_frame() -> (String, PathBuf) {
        start().unwrap();
        let st = state::load().unwrap().unwrap();
        let id = st.recording_id.clone().unwrap();
        let frames = st.recording_frames_dir.clone().unwrap();
        state::open_frames_dir_from_path(&frames)
            .unwrap()
            .write_file("1.000000.jpg", &[0xff, 0xd8, 0xff, 0xd9])
            .unwrap();
        (id, frames)
    }

    #[test]
    fn video_process_helper() {
        let Some(root) = env::var_os("RDNY_VIDEO_PROCESS_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let role = env::var("RDNY_VIDEO_PROCESS_ROLE").unwrap();
        fs::write(root.join(format!("ready-{role}")), b"ready").unwrap();
        while !root.join("go").exists() {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let result = match role.as_str() {
            "start-a" | "start-b" => start().map(|()| "ok".to_string()).or_else(|err| {
                if err.to_string().contains("already active") {
                    Ok("active".to_string())
                } else {
                    Err(err)
                }
            }),
            "stop" => stop(None, Some(&root.join("out.mp4")), false)
                .map(|()| "stopped".to_string())
                .or_else(|err| {
                    if err.to_string().contains("ffmpeg failed") {
                        Ok("failed".to_string())
                    } else {
                        Err(err)
                    }
                }),
            "intervene" => state::update(|state| {
                state.label = Some("intervened".to_string());
                Ok("intervened".to_string())
            }),
            "claim" => {
                let (_id, _frames, _lease) = deactivate_or_recover_recording().unwrap();
                fs::write(root.join("claimed"), b"claimed").unwrap();
                while !root.join("release-claim").exists() {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Ok("claimed".to_string())
            }
            _ => unreachable!(),
        }
        .unwrap();
        fs::write(root.join(format!("done-{role}")), result).unwrap();
    }

    fn spawn_video_helper(root: &Path, role: &str) -> std::process::Child {
        std::process::Command::new(std::env::current_exe().unwrap())
            .arg("commands::video::tests::video_process_helper")
            .arg("--exact")
            .env("RDNY_STATE_DIR", root)
            .env("RDNY_VIDEO_PROCESS_ROOT", root)
            .env("RDNY_VIDEO_PROCESS_ROLE", role)
            .spawn()
            .unwrap()
    }

    fn wait_ready(root: &Path, roles: &[&str]) {
        while roles
            .iter()
            .any(|role| !root.join(format!("ready-{role}")).exists())
        {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    fn handles_screencast_frame() {
        let temp = tempfile::tempdir().unwrap();
        let params = serde_json::json!({
            "data": "/9j/2Q==",
            "metadata": {"timestamp": 123.4567894},
            "sessionId": 7
        });
        let frames = crate::state::open_store_at(temp.path())
            .unwrap()
            .subdir("frames")
            .unwrap();
        let ack = handle_screencast_frame(&params, &frames).unwrap();
        assert_eq!(ack.as_deref(), Some("7"));
        assert_eq!(
            std::fs::read(frames.path().join("123.456789.jpg")).unwrap(),
            vec![0xff, 0xd8, 0xff, 0xd9]
        );
    }

    #[test]
    fn ffmpeg_frame_paths_are_stable_real_paths_under_private_root() {
        let temp = tempfile::tempdir().unwrap();
        let frames = crate::state::open_store_at(temp.path())
            .unwrap()
            .subdir("frames")
            .unwrap();
        frames.write_file("1.000000.jpg", b"jpeg").unwrap();
        let found = read_frames(&frames).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, frames.path().join("1.000000.jpg"));
        assert!(!found[0].1.to_string_lossy().contains("/dev/fd/"));
    }

    #[test]
    fn concat_list_sorts_and_uses_durations() {
        let frames = vec![(2.5, PathBuf::from("b.jpg")), (1.0, PathBuf::from("a.jpg"))];
        assert_eq!(
            concat_list(&frames),
            "file 'a.jpg'\nduration 1.500000\nfile 'b.jpg'\nduration 0.100000\nfile 'b.jpg'\n"
        );
    }

    #[test]
    fn concat_list_single_frame_repeats_last() {
        assert_eq!(
            concat_list(&[(1.0, PathBuf::from("one.jpg"))]),
            "file 'one.jpg'\nduration 0.100000\nfile 'one.jpg'\n"
        );
    }

    #[test]
    fn stop_video_passes_mp4_temp_output_and_publishes_atomically() {
        let bin_dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let output = work.path().join("out.mp4");
        let ffmpeg = bin_dir.path().join("ffmpeg");
        fs::write(
            &ffmpeg,
            "#!/bin/sh\nfor out do :; done\ncase \"$out\" in *.mp4) printf video > \"$out\" ;; *) exit 44 ;; esac\n",
        )
        .unwrap();
        fs::set_permissions(&ffmpeg, fs::Permissions::from_mode(0o700)).unwrap();
        let frames = crate::state::open_store_at(work.path())
            .unwrap()
            .subdir("frames")
            .unwrap();
        frames.write_file("1.000000.jpg", b"jpg").unwrap();
        let list_path = frames.path().join("frames.txt");
        fs::write(&list_path, "file '1.000000.jpg'\n").unwrap();
        assemble_video(&ffmpeg, &frames, &list_path, &output, false).unwrap();
        assert_eq!(fs::read(&output).unwrap(), b"video");
        assert!(frames.path().exists());
    }

    #[test]
    fn failed_ffmpeg_removes_reserved_temp_and_preserves_output() {
        let bin_dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let output = work.path().join("out.mp4");
        fs::write(&output, b"old").unwrap();
        let ffmpeg = bin_dir.path().join("ffmpeg");
        fs::write(&ffmpeg, "#!/bin/sh\nexit 44\n").unwrap();
        fs::set_permissions(&ffmpeg, fs::Permissions::from_mode(0o700)).unwrap();
        let frames = crate::state::open_store_at(work.path())
            .unwrap()
            .subdir("frames")
            .unwrap();
        let list_path = frames.path().join("frames.txt");
        fs::write(&list_path, "file '1.000000.jpg'\n").unwrap();
        assert!(assemble_video(&ffmpeg, &frames, &list_path, &output, true).is_err());
        assert_eq!(fs::read(&output).unwrap(), b"old");
        assert!(
            !fs::read_dir(work.path())
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().contains("rdny-"))
        );
    }

    #[test]
    fn concat_list_escapes_spaces_apostrophes_and_absolute_paths() {
        let path = PathBuf::from("/tmp/rdny state/it ain't/1.jpg");
        assert_eq!(
            concat_list(&[(1.0, path)]),
            "file '/tmp/rdny state/it ain'\\''t/1.jpg'\nduration 0.100000\nfile '/tmp/rdny state/it ain'\\''t/1.jpg'\n"
        );
    }

    #[test]
    fn env_guard_restores_after_panic() {
        let _lock = state::ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let original = env::var_os("RDNY_STATE_DIR");
        let result = panic::catch_unwind(|| {
            let _guard = EnvGuard::new();
            unsafe { env::set_var("RDNY_STATE_DIR", "panic-state") };
            panic!("boom");
        });
        assert!(result.is_err());
        assert_eq!(env::var_os("RDNY_STATE_DIR"), original);
    }

    #[test]
    fn start_rejects_active_and_uses_dedicated_dirs() {
        with_state_dir(|_state_dir| {
            state::replace(&sample_state()).unwrap();
            start().unwrap();
            let first = state::load().unwrap().unwrap();
            assert!(first.recording);
            assert!(first.recording_id.is_some());
            assert!(first.recording_frames_dir.as_ref().unwrap().is_absolute());
            assert!(start().unwrap_err().to_string().contains("already active"));
            assert!(first.recording_frames_dir.unwrap().is_dir());
        });
    }

    #[test]
    fn multiprocess_simultaneous_start_allows_one_without_orphan_dir() {
        let _lock = state::ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env = EnvGuard::new();
        let temp = tempfile::tempdir_in(".").unwrap();
        unsafe { env::set_var("RDNY_STATE_DIR", temp.path()) };
        state::replace(&sample_state()).unwrap();
        let mut a = spawn_video_helper(temp.path(), "start-a");
        let mut b = spawn_video_helper(temp.path(), "start-b");
        wait_ready(temp.path(), &["start-a", "start-b"]);
        fs::write(temp.path().join("go"), b"go").unwrap();
        assert!(a.wait().unwrap().success());
        assert!(b.wait().unwrap().success());
        let results = ["start-a", "start-b"]
            .map(|role| fs::read_to_string(temp.path().join(format!("done-{role}"))).unwrap());
        assert_eq!(
            results
                .iter()
                .filter(|result| result.as_str() == "ok")
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| result.as_str() == "active")
                .count(),
            1
        );
        let state = state::load().unwrap().unwrap();
        let id = state.recording_id.unwrap();
        let recordings = fs::read_dir(temp.path().join("recordings"))
            .unwrap()
            .count();
        assert_eq!(recordings, 1);
        assert!(
            temp.path()
                .join("recordings")
                .join(id)
                .join("frames")
                .is_dir()
        );
    }

    #[test]
    fn relative_state_dir_survives_changed_cwd_retry() {
        let _lock = state::ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env = EnvGuard::new();
        let root = tempfile::tempdir().unwrap();
        env::set_current_dir(root.path()).unwrap();
        unsafe { env::set_var("RDNY_STATE_DIR", "relative state") };
        state::replace(&sample_state()).unwrap();
        start().unwrap();
        let st = state::load().unwrap().unwrap();
        let frames = st.recording_frames_dir.as_ref().unwrap();
        assert!(frames.is_absolute());
        fs::write(frames.join("1.000000.jpg"), [0xff, 0xd8, 0xff, 0xd9]).unwrap();
        let other = root.path().join("other cwd");
        fs::create_dir(&other).unwrap();
        env::set_current_dir(&other).unwrap();
        let ffmpeg = root.path().join("ffmpeg");
        fs::write(
            &ffmpeg,
            "#!/bin/sh\nfor last do :; done\ntouch \"$last\"\nexit 0\n",
        )
        .unwrap();
        fs::set_permissions(&ffmpeg, fs::Permissions::from_mode(0o755)).unwrap();
        unsafe { env::set_var("RDNY_FFMPEG", &ffmpeg) };
        stop(None, Some(&root.path().join("out.mp4")), false).unwrap();
        assert!(root.path().join("out.mp4").exists());
    }

    #[test]
    fn legacy_active_recording_uses_legacy_frames_dir_and_migrates_to_recovery() {
        with_state_dir(|state_dir| {
            let mut st = sample_state();
            st.recording = true;
            state::replace(&st).unwrap();
            fs::create_dir_all(state_dir.join("frames")).unwrap();
            let err = stop(None, None, false).unwrap_err();
            assert!(err.to_string().contains("no video frames"));
            let st = state::load().unwrap().unwrap();
            assert!(!st.recording);
            let recovery = st.recoverable_recordings.first().unwrap();
            assert_eq!(recovery.id, "legacy");
            assert_eq!(
                recovery.frames_dir,
                fs::canonicalize(state_dir.join("frames")).unwrap()
            );
        });
    }

    #[test]
    fn recording_ids_are_unique_and_directories_are_exclusive() {
        with_state_dir(|state_dir| {
            let mut ids = std::collections::HashSet::new();
            for _ in 0..64 {
                let (id, frames) = allocate_recording_frames_dir().unwrap();
                assert!(ids.insert(id));
                assert!(frames.path().is_dir());
            }
            fs::create_dir_all(state_dir.join("recordings").join("stale").join("frames")).unwrap();
            let (_id, frames) = allocate_recording_frames_dir().unwrap();
            assert_ne!(
                frames.path(),
                state_dir.join("recordings").join("stale").join("frames")
            );
        });
    }

    #[test]
    fn stop_empty_capture_deactivates_and_preserves_recovery() {
        with_state_dir(|_| {
            state::replace(&sample_state()).unwrap();
            start().unwrap();
            let id = state::load().unwrap().unwrap().recording_id.unwrap();
            let err = stop(None, None, false).unwrap_err();
            assert!(err.to_string().contains("no video frames"));
            let state = state::load().unwrap().unwrap();
            assert!(!state.recording);
            assert_eq!(state.recoverable_recordings.len(), 1);
            assert_recoverable(&id);
        });
    }

    #[test]
    fn assembling_guard_restores_open_path_failure() {
        with_state_dir(|state_dir| {
            let mut st = sample_state();
            st.recoverable_recordings.push(state::RecoverableRecording {
                id: "bad-path".into(),
                frames_dir: state_dir.join("outside").join("frames"),
                status: state::RecordingStatus::Recoverable,
            });
            state::replace(&st).unwrap();
            assert!(stop(None, None, false).is_err());
            assert_recoverable("bad-path");
        });
    }

    #[test]
    fn assembling_guard_restores_frame_read_failure() {
        with_state_dir(|_| {
            state::replace(&sample_state()).unwrap();
            let (id, frames) = start_with_frame();
            state::open_frames_dir_from_path(&frames)
                .unwrap()
                .write_file("NaN.jpg", b"bad")
                .unwrap();
            assert!(stop(None, None, false).is_err());
            assert_recoverable(&id);
        });
    }

    #[test]
    fn assembling_guard_restores_concat_write_failure() {
        with_state_dir(|_| {
            state::replace(&sample_state()).unwrap();
            let (id, frames) = start_with_frame();
            state::open_frames_dir_from_path(&frames)
                .unwrap()
                .subdir("frames.txt")
                .unwrap();
            assert!(stop(None, None, false).is_err());
            assert_recoverable(&id);
        });
    }

    #[test]
    fn assembling_guard_restores_config_load_failure() {
        with_state_dir(|state_dir| {
            state::replace(&sample_state()).unwrap();
            let (id, _) = start_with_frame();
            let cfg = state_dir.join("bad.toml");
            fs::write(&cfg, "not = [toml").unwrap();
            unsafe { env::set_var("RDNY_CONFIG", cfg) };
            assert!(stop(None, None, false).is_err());
            assert_recoverable(&id);
        });
    }

    #[test]
    fn assembling_guard_restores_artifact_reservation_failure() {
        with_state_dir(|state_dir| {
            state::replace(&sample_state()).unwrap();
            let (id, _) = start_with_frame();
            let output = state_dir.join("exists.mp4");
            fs::write(&output, b"old").unwrap();
            assert!(stop(None, Some(&output), false).is_err());
            assert_recoverable(&id);
        });
    }

    #[test]
    fn assembling_guard_restores_ffmpeg_spawn_failure() {
        with_state_dir(|state_dir| {
            state::replace(&sample_state()).unwrap();
            let (id, _) = start_with_frame();
            unsafe { env::set_var("RDNY_FFMPEG", state_dir.join("missing-ffmpeg")) };
            assert!(stop(None, Some(&state_dir.join("out.mp4")), false).is_err());
            assert_recoverable(&id);
        });
    }

    #[test]
    fn assembling_guard_restores_ffmpeg_status_failure() {
        with_state_dir(|state_dir| {
            state::replace(&sample_state()).unwrap();
            let (id, _) = start_with_frame();
            let ffmpeg = state_dir.join("ffmpeg");
            fs::write(&ffmpeg, "#!/bin/sh\nexit 44\n").unwrap();
            fs::set_permissions(&ffmpeg, fs::Permissions::from_mode(0o755)).unwrap();
            unsafe { env::set_var("RDNY_FFMPEG", &ffmpeg) };
            assert!(stop(None, Some(&state_dir.join("out.mp4")), false).is_err());
            assert_recoverable(&id);
        });
    }

    #[test]
    fn ffmpeg_failure_is_retryable_and_success_clears_recovery() {
        with_state_dir(|state_dir| {
            state::replace(&sample_state()).unwrap();
            start().unwrap();
            let st = state::load().unwrap().unwrap();
            let frames = st.recording_frames_dir.as_ref().unwrap();
            fs::write(frames.join("1.000000.jpg"), [0xff, 0xd8, 0xff, 0xd9]).unwrap();
            let ffmpeg = state_dir.join("ffmpeg");
            fs::write(&ffmpeg, "#!/bin/sh\nexit 2\n").unwrap();
            fs::set_permissions(&ffmpeg, fs::Permissions::from_mode(0o755)).unwrap();
            unsafe { std::env::set_var("RDNY_FFMPEG", &ffmpeg) };
            assert!(stop(None, Some(&state_dir.join("out one.mp4")), false).is_err());
            assert!(!state::load().unwrap().unwrap().recording);
            fs::write(
                &ffmpeg,
                "#!/bin/sh\nfor last do :; done\ntouch \"$last\"\nexit 0\n",
            )
            .unwrap();
            stop(None, Some(&state_dir.join("out two.mp4")), false).unwrap();
            assert!(
                state::load()
                    .unwrap()
                    .unwrap()
                    .recoverable_recordings
                    .is_empty()
            );
            unsafe { std::env::remove_var("RDNY_FFMPEG") };
        });
    }

    #[test]
    fn success_does_not_clear_newer_active_recording() {
        with_state_dir(|state_dir| {
            state::replace(&sample_state()).unwrap();
            start().unwrap();
            let a = state::load().unwrap().unwrap();
            let a_id = a.recording_id.clone().unwrap();
            let a_frames = a.recording_frames_dir.clone().unwrap();
            fs::write(a_frames.join("1.000000.jpg"), [0xff, 0xd8, 0xff, 0xd9]).unwrap();

            let ffmpeg = state_dir.join("ffmpeg");
            let state_file = state_dir.join("state.json");
            let b_frames = state_dir.join("recordings").join("b").join("frames");
            fs::create_dir_all(&b_frames).unwrap();
            fs::write(
                &ffmpeg,
                format!(
                    "#!/bin/sh\ncat > '{}' <<'JSON'\n{{\n  \"ws_url\":\"ws://127.0.0.1:9222/devtools/browser/abc\",\n  \"host\":\"127.0.0.1\",\n  \"port\":9222,\n  \"pid\":null,\n  \"user_data_dir\":null,\n  \"browser_path\":null,\n  \"target_id\":\"target-1\",\n  \"recording\":true,\n  \"recording_id\":\"b\",\n  \"recording_frames_dir\":\"{}\",\n  \"recoverable_recording\":{{\"id\":\"{}\",\"frames_dir\":\"{}\"}}\n}}\nJSON\nfor last do :; done\ntouch \"$last\"\nexit 0\n",
                    state_file.display(),
                    b_frames.display(),
                    a_id,
                    a_frames.display()
                ),
            )
            .unwrap();
            fs::set_permissions(&ffmpeg, fs::Permissions::from_mode(0o755)).unwrap();
            unsafe { env::set_var("RDNY_FFMPEG", &ffmpeg) };

            stop(None, Some(&state_dir.join("out.mp4")), false).unwrap();
            let st = state::load().unwrap().unwrap();
            assert!(st.recording);
            assert_eq!(st.recording_id.as_deref(), Some("b"));
            assert!(st.recoverable_recordings.is_empty());
        });
    }

    #[test]
    fn multiprocess_stop_cleanup_preserves_intervening_update() {
        let _lock = state::ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env = EnvGuard::new();
        let temp = tempfile::tempdir_in(".").unwrap();
        unsafe { env::set_var("RDNY_STATE_DIR", temp.path()) };
        state::replace(&sample_state()).unwrap();
        start().unwrap();
        let st = state::load().unwrap().unwrap();
        state::open_frames_dir_from_path(st.recording_frames_dir.as_ref().unwrap())
            .unwrap()
            .write_file("1.000000.jpg", &[0xff, 0xd8, 0xff, 0xd9])
            .unwrap();
        let ffmpeg = temp.path().join("ffmpeg");
        fs::write(
            &ffmpeg,
            format!(
                "#!/bin/sh\ntouch '{}'\nwhile [ ! -f '{}' ]; do sleep 0.01; done\nfor last do :; done\ntouch \"$last\"\nexit 0\n",
                temp.path().join("ffmpeg-ready").display(),
                temp.path().join("ffmpeg-go").display()
            ),
        )
        .unwrap();
        fs::set_permissions(&ffmpeg, fs::Permissions::from_mode(0o755)).unwrap();
        unsafe { env::set_var("RDNY_FFMPEG", &ffmpeg) };
        let mut stop_child = spawn_video_helper(temp.path(), "stop");
        wait_ready(temp.path(), &["stop"]);
        fs::write(temp.path().join("go"), b"go").unwrap();
        while !temp.path().join("ffmpeg-ready").exists() {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        state::update(|state| {
            state.label = Some("intervened".to_string());
            Ok(())
        })
        .unwrap();
        fs::write(temp.path().join("ffmpeg-go"), b"go").unwrap();
        assert!(stop_child.wait().unwrap().success());
        let st = state::load().unwrap().unwrap();
        assert_eq!(st.label.as_deref(), Some("intervened"));
        assert!(!st.recording);
        assert!(st.recoverable_recordings.is_empty());
    }

    #[test]
    fn multiprocess_stop_identity_preserves_newer_recording() {
        let _lock = state::ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env = EnvGuard::new();
        let temp = tempfile::tempdir_in(".").unwrap();
        unsafe { env::set_var("RDNY_STATE_DIR", temp.path()) };
        state::replace(&sample_state()).unwrap();
        start().unwrap();
        let a = state::load().unwrap().unwrap();
        let a_id = a.recording_id.clone().unwrap();
        state::open_frames_dir_from_path(a.recording_frames_dir.as_ref().unwrap())
            .unwrap()
            .write_file("1.000000.jpg", &[0xff, 0xd8, 0xff, 0xd9])
            .unwrap();
        let ffmpeg = temp.path().join("ffmpeg");
        fs::write(
            &ffmpeg,
            format!(
                "#!/bin/sh\ntouch '{}'\nwhile [ ! -f '{}' ]; do sleep 0.01; done\nfor last do :; done\ntouch \"$last\"\nexit 0\n",
                temp.path().join("ffmpeg-ready").display(),
                temp.path().join("ffmpeg-go").display()
            ),
        )
        .unwrap();
        fs::set_permissions(&ffmpeg, fs::Permissions::from_mode(0o755)).unwrap();
        unsafe { env::set_var("RDNY_FFMPEG", &ffmpeg) };
        let mut stop_child = spawn_video_helper(temp.path(), "stop");
        wait_ready(temp.path(), &["stop"]);
        fs::write(temp.path().join("go"), b"go").unwrap();
        while !temp.path().join("ffmpeg-ready").exists() {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        state::update(|state| {
            let b_frames = temp.path().join("recordings").join("b").join("frames");
            fs::create_dir_all(&b_frames).unwrap();
            state.recording = true;
            state.recording_id = Some("b".to_string());
            state.recording_frames_dir = Some(b_frames);
            assert!(state.recoverable_recordings.iter().any(|r| r.id == a_id));
            Ok(())
        })
        .unwrap();
        fs::write(temp.path().join("ffmpeg-go"), b"go").unwrap();
        assert!(stop_child.wait().unwrap().success());
        let st = state::load().unwrap().unwrap();
        assert!(st.recording);
        assert_eq!(st.recording_id.as_deref(), Some("b"));
        assert!(st.recoverable_recordings.is_empty());
    }

    #[test]
    fn overlapping_assembly_and_new_recording_keep_both_recoverable_and_retryable() {
        let _lock = state::ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env = EnvGuard::new();
        let temp = tempfile::tempdir_in(".").unwrap();
        unsafe { env::set_var("RDNY_STATE_DIR", temp.path()) };
        state::replace(&sample_state()).unwrap();

        start().unwrap();
        let a = state::load().unwrap().unwrap();
        let a_id = a.recording_id.clone().unwrap();
        state::open_frames_dir_from_path(a.recording_frames_dir.as_ref().unwrap())
            .unwrap()
            .write_file("1.000000.jpg", &[0xff, 0xd8, 0xff, 0xd9])
            .unwrap();

        let blocking_fail = temp.path().join("ffmpeg-blocking-fail");
        fs::write(
            &blocking_fail,
            format!(
                "#!/bin/sh\ntouch '{}'\nwhile [ ! -f '{}' ]; do sleep 0.01; done\nexit 44\n",
                temp.path().join("a-ffmpeg-ready").display(),
                temp.path().join("a-ffmpeg-go").display()
            ),
        )
        .unwrap();
        fs::set_permissions(&blocking_fail, fs::Permissions::from_mode(0o755)).unwrap();
        unsafe { env::set_var("RDNY_FFMPEG", &blocking_fail) };
        let mut a_stop = spawn_video_helper(temp.path(), "stop");
        wait_ready(temp.path(), &["stop"]);
        fs::write(temp.path().join("go"), b"go").unwrap();
        while !temp.path().join("a-ffmpeg-ready").exists() {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        start().unwrap();
        let b = state::load().unwrap().unwrap();
        let b_id = b.recording_id.clone().unwrap();
        assert_ne!(a_id, b_id);
        state::open_frames_dir_from_path(b.recording_frames_dir.as_ref().unwrap())
            .unwrap()
            .write_file("2.000000.jpg", &[0xff, 0xd8, 0xff, 0xd9])
            .unwrap();

        let fail_fast = temp.path().join("ffmpeg-fail-fast");
        fs::write(&fail_fast, "#!/bin/sh\nexit 45\n").unwrap();
        fs::set_permissions(&fail_fast, fs::Permissions::from_mode(0o755)).unwrap();
        unsafe { env::set_var("RDNY_FFMPEG", &fail_fast) };
        assert!(stop(None, Some(&temp.path().join("b-fail.mp4")), false).is_err());

        fs::write(temp.path().join("a-ffmpeg-go"), b"go").unwrap();
        assert!(a_stop.wait().unwrap().success());
        let st = state::load().unwrap().unwrap();
        let recoverable_ids: Vec<_> = st
            .recoverable_recordings
            .iter()
            .map(|recording| (recording.id.as_str(), recording.status))
            .collect();
        assert_eq!(
            recoverable_ids,
            vec![
                (a_id.as_str(), state::RecordingStatus::Recoverable),
                (b_id.as_str(), state::RecordingStatus::Recoverable),
            ]
        );

        let success = temp.path().join("ffmpeg-success");
        fs::write(
            &success,
            "#!/bin/sh\nfor last do :; done\ntouch \"$last\"\nexit 0\n",
        )
        .unwrap();
        fs::set_permissions(&success, fs::Permissions::from_mode(0o755)).unwrap();
        unsafe { env::set_var("RDNY_FFMPEG", &success) };
        stop(None, Some(&temp.path().join("retry-a.mp4")), false).unwrap();
        let st = state::load().unwrap().unwrap();
        assert_eq!(st.recoverable_recordings.len(), 1);
        assert_eq!(st.recoverable_recordings[0].id, b_id);
        stop(None, Some(&temp.path().join("retry-b.mp4")), false).unwrap();
        assert!(
            state::load()
                .unwrap()
                .unwrap()
                .recoverable_recordings
                .is_empty()
        );
    }

    #[test]
    fn crashed_assembling_claim_is_reclaimed_and_retried() {
        let _lock = state::ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env = EnvGuard::new();
        let temp = tempfile::tempdir_in(".").unwrap();
        unsafe { env::set_var("RDNY_STATE_DIR", temp.path()) };
        state::replace(&sample_state()).unwrap();
        let (id, _) = start_with_frame();
        let mut child = spawn_video_helper(temp.path(), "claim");
        wait_ready(temp.path(), &["claim"]);
        fs::write(temp.path().join("go"), b"go").unwrap();
        while !temp.path().join("claimed").exists() {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        unsafe { libc::kill(child.id() as i32, libc::SIGKILL) };
        let _ = child.wait().unwrap();
        let st = state::load().unwrap().unwrap();
        assert_eq!(st.recoverable_recordings[0].id, id);
        assert_eq!(
            st.recoverable_recordings[0].status,
            state::RecordingStatus::Assembling
        );
        let ffmpeg = temp.path().join("ffmpeg");
        fs::write(
            &ffmpeg,
            "#!/bin/sh\nfor last do :; done\ntouch \"$last\"\nexit 0\n",
        )
        .unwrap();
        fs::set_permissions(&ffmpeg, fs::Permissions::from_mode(0o755)).unwrap();
        unsafe { env::set_var("RDNY_FFMPEG", &ffmpeg) };
        stop(None, Some(&temp.path().join("retry.mp4")), false).unwrap();
        assert!(
            state::load()
                .unwrap()
                .unwrap()
                .recoverable_recordings
                .is_empty()
        );
    }

    #[test]
    fn live_assembling_claim_is_not_stolen() {
        let _lock = state::ENV_LOCK
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let _env = EnvGuard::new();
        let temp = tempfile::tempdir_in(".").unwrap();
        unsafe { env::set_var("RDNY_STATE_DIR", temp.path()) };
        state::replace(&sample_state()).unwrap();
        let (id, _) = start_with_frame();
        let mut child = spawn_video_helper(temp.path(), "claim");
        wait_ready(temp.path(), &["claim"]);
        fs::write(temp.path().join("go"), b"go").unwrap();
        while !temp.path().join("claimed").exists() {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let err = stop(None, Some(&temp.path().join("stolen.mp4")), false).unwrap_err();
        assert!(err.to_string().contains("no active or recoverable"));
        let st = state::load().unwrap().unwrap();
        assert_eq!(st.recoverable_recordings[0].id, id);
        assert_eq!(
            st.recoverable_recordings[0].status,
            state::RecordingStatus::Assembling
        );
        fs::write(temp.path().join("release-claim"), b"go").unwrap();
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn mp4_output_selects_mp4_muxer_for_temp_path_compatibility() {
        assert_eq!(muxer_for_output(Path::new("video.mp4")), Some("mp4"));
        assert_eq!(muxer_for_output(Path::new("VIDEO.M4V")), Some("mp4"));
        assert_eq!(muxer_for_output(Path::new("recording")), None);
    }

    #[test]
    fn stop_passes_explicit_muxer_for_mp4_outputs() {
        with_state_dir(|state_dir| {
            state::replace(&sample_state()).unwrap();
            start().unwrap();
            let st = state::load().unwrap().unwrap();
            let frames = st.recording_frames_dir.as_ref().unwrap();
            fs::write(frames.join("1.000000.jpg"), [0xff, 0xd8, 0xff, 0xd9]).unwrap();
            let ffmpeg = state_dir.join("ffmpeg");
            let args = state_dir.join("args.txt");
            fs::write(
                &ffmpeg,
                format!(
                    "#!/bin/sh\nprintf '%s\n' \"$@\" > '{}'\nfor last do :; done\ntouch \"$last\"\nexit 0\n",
                    args.display()
                ),
            )
            .unwrap();
            fs::set_permissions(&ffmpeg, fs::Permissions::from_mode(0o755)).unwrap();
            unsafe { env::set_var("RDNY_FFMPEG", &ffmpeg) };
            stop(None, Some(&state_dir.join("reserved-temp-name.mp4")), false).unwrap();
            let args = fs::read_to_string(args).unwrap();
            assert!(args.contains("-f\nmp4\n"), "{args}");
        });
    }
}
