//! Video recording via CDP screencast frames.

use std::cmp::Ordering;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::commands::decode_base64;
use crate::config;
use crate::hint::hint_error;
use crate::{session::PageSession, state};

const LAST_FRAME_DURATION: f64 = 0.1;

pub fn start() -> Result<()> {
    let _frames = state::frames_dir()?;
    state::update(|state| {
        state.recording = true;
        Ok(())
    })
}

pub fn stop(session: Option<&mut PageSession>, output: Option<&Path>) -> Result<()> {
    if let Some(session) = session {
        let _ = session.call("Page.stopScreencast", serde_json::json!({}));
        session.drain_events(std::time::Duration::from_millis(300))?;
    }

    let frames_dir = state::frames_dir()?;
    frames_dir.validate_external_path()?;
    let frames = read_frames(&frames_dir)?;
    if frames.is_empty() {
        return Err(hint_error(
            "no video frames were captured",
            "run commands that interact with the page after `rdny start-video`; frames are only captured while rdny is connected",
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
    let status = Command::new(&ffmpeg)
        .args([OsStr::new("-loglevel"), OsStr::new("error")])
        .args([OsStr::new("-y"), OsStr::new("-f"), OsStr::new("concat")])
        .args([OsStr::new("-safe"), OsStr::new("0"), OsStr::new("-i")])
        .arg(&list_path)
        .args([
            OsStr::new("-vf"),
            OsStr::new("pad=ceil(iw/2)*2:ceil(ih/2)*2"),
        ])
        .args([OsStr::new("-pix_fmt"), OsStr::new("yuv420p")])
        .arg(output)
        .status();

    match status {
        Ok(status) if status.success() => {
            state::remove_frames_dir().context("removing video frames directory")?;
            state::update_if_present(|state| {
                state.recording = false;
                Ok(())
            })?;
            println!("{}", output.display());
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
}
