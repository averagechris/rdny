//! Session-owned ingestion of CDP screencast events.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use base64::Engine;
use serde_json::Value;

use crate::state;

const DEFAULT_MAX_FRAME_BYTES: u64 = 8 * 1024 * 1024;
const DEFAULT_MAX_RECORDING_BYTES: u64 = 512 * 1024 * 1024;
const DEFAULT_MAX_RECORDING_FRAMES: u64 = 18_000;
const DEFAULT_MAX_RECORDING_SECONDS: f64 = 30.0 * 60.0;
const DEFAULT_MIN_FREE_DISK_BYTES: u64 = 256 * 1024 * 1024;

pub(crate) fn ingest_screencast_frame(
    params: &Value,
    frames_dir: &state::SecureDir,
) -> Result<Option<String>> {
    let ack = params
        .get("sessionId")
        .and_then(Value::as_i64)
        .map(|id| id.to_string());
    let Some(data) = params.get("data").and_then(Value::as_str) else {
        return Ok(ack);
    };
    let Some(timestamp) = params
        .get("metadata")
        .and_then(|metadata| metadata.get("timestamp"))
        .and_then(Value::as_f64)
    else {
        return Ok(ack);
    };
    let Some(_lease) = state::active_recording_writer_lease(frames_dir.path())? else {
        // A cached page session can receive a frame after stop-video has
        // deactivated this recording. Acknowledge it without repopulating the
        // directory being assembled or cleaned up.
        return Ok(ack);
    };
    if estimated_decoded_len(data)
        > env_u64("RDNY_MAX_SCREENCAST_FRAME_BYTES", DEFAULT_MAX_FRAME_BYTES)
    {
        stop_for_quota(frames_dir, "screencast frame exceeds max decoded size")?;
        return Ok(ack);
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .context("invalid base64 from browser")?;
    if bytes.len() as u64 > env_u64("RDNY_MAX_SCREENCAST_FRAME_BYTES", DEFAULT_MAX_FRAME_BYTES) {
        stop_for_quota(frames_dir, "screencast frame exceeds max decoded size")?;
        return Ok(ack);
    }
    if enforce_quota(frames_dir, timestamp, bytes.len() as u64)? {
        return Ok(ack);
    }
    frames_dir
        .write_file(&format!("{timestamp:.6}.jpg"), &bytes)
        .context("writing screencast frame")?;
    Ok(ack)
}

fn enforce_quota(frames_dir: &state::SecureDir, timestamp: f64, incoming: u64) -> Result<bool> {
    let mut timestamps = fs::read_dir(frames_dir.path())
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            entry
                .ok()?
                .path()
                .file_stem()?
                .to_str()?
                .parse::<f64>()
                .ok()
        })
        .collect::<Vec<_>>();
    timestamps.sort_by(f64::total_cmp);
    let frame_count = timestamps.len() as u64;
    let bytes = dir_bytes(frames_dir.path()).saturating_add(incoming);
    let duration = timestamps
        .first()
        .map(|first| (timestamp - first).max(0.0))
        .unwrap_or(0.0);
    let reason =
        if frame_count + 1 > env_u64("RDNY_MAX_RECORDING_FRAMES", DEFAULT_MAX_RECORDING_FRAMES) {
            Some("recording frame quota exceeded")
        } else if bytes > env_u64("RDNY_MAX_RECORDING_BYTES", DEFAULT_MAX_RECORDING_BYTES) {
            Some("recording byte quota exceeded")
        } else if duration > env_f64("RDNY_MAX_RECORDING_SECONDS", DEFAULT_MAX_RECORDING_SECONDS) {
            Some("recording duration quota exceeded")
        } else if free_bytes(frames_dir.path())
            < env_u64("RDNY_MIN_FREE_DISK_BYTES", DEFAULT_MIN_FREE_DISK_BYTES)
        {
            Some("recording stopped to preserve free disk")
        } else {
            None
        };
    if let Some(reason) = reason {
        stop_for_quota(frames_dir, reason)?;
        return Ok(true);
    }
    Ok(false)
}

fn stop_for_quota(frames_dir: &state::SecureDir, reason: &str) -> Result<()> {
    let frames_path = frames_dir.path().to_path_buf();
    state::update_if_present(|session| {
        if session.recording_frames_dir.as_deref() == Some(frames_path.as_path()) {
            let id = session
                .recording_id
                .clone()
                .unwrap_or_else(|| "quota-stopped".into());
            session.recording = false;
            session.recording_id = None;
            session.recording_frames_dir = None;
            let recording = state::RecoverableRecording {
                id: id.clone(),
                frames_dir: frames_path.clone(),
                status: state::RecordingStatus::Recoverable,
            };
            if let Some(existing) = session
                .recoverable_recordings
                .iter_mut()
                .find(|existing| existing.id == id)
            {
                *existing = recording;
            } else {
                session.recoverable_recordings.push(recording);
            }
        }
        Ok(())
    })?;
    eprintln!("warning: {reason}; recording marked recoverable");
    Ok(())
}

fn estimated_decoded_len(encoded: &str) -> u64 {
    (encoded.len() as u64 / 4).saturating_mul(3)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn dir_bytes(path: &Path) -> u64 {
    fs::read_dir(path)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok()?.metadata().ok().map(|metadata| metadata.len()))
        .sum()
}

fn free_bytes(path: &Path) -> u64 {
    let Some(path) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok() else {
        return u64::MAX;
    };
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    unsafe {
        if libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) == 0 {
            let stat = stat.assume_init();
            #[cfg(target_os = "macos")]
            let available = u64::from(stat.f_bavail);
            #[cfg(not(target_os = "macos"))]
            let available = stat.f_bavail;
            available.saturating_mul(stat.f_frsize)
        } else {
            u64::MAX
        }
    }
}
