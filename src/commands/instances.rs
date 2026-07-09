//! Discover, list, and clean up rdny instance state files.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::browser;
use crate::cdp::http;
use crate::state::{self, Generation, Inspection, SessionState};

/// How an instance relates to a running browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// rdny launched the browser and its pid is alive.
    Alive,
    /// No pid (attach session) but the debug port answers.
    Attached,
    /// Neither a live pid nor a reachable debug port.
    Dead,
}

/// Classify an instance given injectable probes, so tests need
/// neither real pids nor sockets.
pub fn classify(
    state: &SessionState,
    pid_alive: impl Fn(u32) -> bool,
    port_reachable: impl Fn(&str, u16) -> bool,
) -> Liveness {
    match state.pid {
        Some(pid) if pid_alive(pid) => Liveness::Alive,
        Some(_) => Liveness::Dead,
        None if port_reachable(&state.host, state.port) => Liveness::Attached,
        None => Liveness::Dead,
    }
}

fn probe_liveness(state: &SessionState) -> Liveness {
    classify(
        state,
        |pid| browser::pid_exists(pid as libc::pid_t),
        |host, port| http::version(host, port).is_ok(),
    )
}

#[derive(Debug, Clone, PartialEq)]
pub struct Instance {
    pub dir: PathBuf,
    pub state: SessionState,
    generation: Generation,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InstanceLine {
    pub dir: PathBuf,
    pub pid: Option<u32>,
    pub liveness: Liveness,
    pub label: Option<String>,
}

pub fn list() -> Result<()> {
    for instance in discover()? {
        let liveness = probe_liveness(&instance.state);
        println!(
            "{}",
            format_line(&InstanceLine {
                dir: instance.dir,
                pid: instance.state.pid,
                liveness,
                label: instance.state.label,
            })
        );
    }
    Ok(())
}

pub fn cleanup(all: bool) -> Result<()> {
    let cleaned = cleanup_instances(discover()?, all, probe_liveness)?;
    for instance in cleaned {
        println!(
            "cleaned: {} (pid={} label={})",
            instance.dir.display(),
            display_pid(instance.state.pid),
            display_label(instance.state.label.as_deref())
        );
    }
    Ok(())
}

pub fn discover() -> Result<Vec<Instance>> {
    discover_from(candidate_dirs()?)
}

fn candidate_dirs() -> Result<Vec<PathBuf>> {
    let mut dirs = vec![state::state_dir()?, state::default_state_dir()?];
    let tmp = std::env::temp_dir();
    scan_rdny_tmp(&tmp, &mut dirs);
    scan_rdny_tmp(Path::new("/tmp"), &mut dirs);
    Ok(dirs)
}

fn scan_rdny_tmp(root: &Path, dirs: &mut Vec<PathBuf>) {
    if let Ok(entries) = fs::read_dir(root) {
        dirs.extend(entries.flatten().filter_map(|entry| {
            let name = entry.file_name();
            name.to_str()
                .is_some_and(|s| s.starts_with("rdny-"))
                .then(|| entry.path())
        }));
    }
}

pub fn discover_from(candidate_dirs: Vec<PathBuf>) -> Result<Vec<Instance>> {
    let mut found = BTreeMap::new();
    for dir in candidate_dirs {
        let key = canonical_key(&dir);
        if found.contains_key(&key) {
            continue;
        }
        let Ok(store) = state::open_store_at(&dir) else {
            continue;
        };
        let Ok(Inspection::Valid(state, generation)) = store.inspect::<SessionState>("state.json")
        else {
            continue;
        };
        found.insert(
            key,
            Instance {
                dir,
                state,
                generation,
            },
        );
    }
    Ok(found.into_values().collect())
}

fn canonical_key(dir: &Path) -> PathBuf {
    fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
}

pub fn format_line(line: &InstanceLine) -> String {
    format!(
        "{}  pid={}  {}  label={}",
        line.dir.display(),
        display_pid(line.pid),
        match line.liveness {
            Liveness::Alive => "alive",
            Liveness::Attached => "attached",
            Liveness::Dead => "dead",
        },
        display_label(line.label.as_deref())
    )
}

fn display_pid(pid: Option<u32>) -> String {
    pid.map(|pid| pid.to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn display_label(label: Option<&str>) -> String {
    label.unwrap_or("-").to_string()
}

pub fn cleanup_instances(
    instances: Vec<Instance>,
    all: bool,
    liveness: impl Fn(&SessionState) -> Liveness,
) -> Result<Vec<Instance>> {
    cleanup_instances_with_hook(instances, all, liveness, |_| {})
}

fn cleanup_instances_with_hook(
    instances: Vec<Instance>,
    all: bool,
    liveness: impl Fn(&SessionState) -> Liveness,
    before_locked_action: impl Fn(&Instance),
) -> Result<Vec<Instance>> {
    let mut cleaned = Vec::new();
    for instance in instances {
        let current_liveness = liveness(&instance.state);
        if current_liveness != Liveness::Dead && !all {
            continue;
        }
        before_locked_action(&instance);
        // Attached sessions are only detached: removing state.json
        // never touches the externally-owned browser.
        let removed = state::open_store_at(&instance.dir)
            .with_context(|| format!("opening {}", instance.dir.display()))?
            .remove_if_generation::<SessionState, _>("state.json", &instance.generation, |latest| {
                let latest_liveness = liveness(latest);
                if latest_liveness == Liveness::Alive {
                    browser::stop(latest).with_context(|| {
                        format!("stopping instance in {}", instance.dir.display())
                    })?;
                }
                Ok(all || latest_liveness == Liveness::Dead)
            })
            .with_context(|| format!("removing state in {}", instance.dir.display()))?;
        if removed {
            cleaned.push(instance);
        }
    }
    Ok(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn state(pid: Option<u32>, label: Option<&str>) -> SessionState {
        SessionState {
            ws_url: "ws://x".into(),
            host: "127.0.0.1".into(),
            port: 1,
            pid,
            user_data_dir: None,
            browser_path: None,
            target_id: None,
            label: label.map(str::to_string),
            viewport: None,
            recording: false,
            recording_id: None,
            recording_frames_dir: None,
            recoverable_recording: None,
            recoverable_recordings: Vec::new(),
        }
    }

    #[test]
    fn cleanup_replacement_helper() {
        let Some(dir) = std::env::var_os("RDNY_CLEANUP_REPLACE_DIR") else {
            return;
        };
        let go = Path::new(&dir).join("go");
        while !go.exists() {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        fs::write(
            Path::new(&dir).join("state.json"),
            serde_json::to_vec(&state(Some(99), Some("replacement"))).unwrap(),
        )
        .unwrap();
        fs::write(Path::new(&dir).join("done"), b"done").unwrap();
    }

    #[test]
    fn discovery_loads_valid_state_and_dedupes() {
        let temp = tempfile::tempdir().unwrap();
        let one = temp.path().join("rdny-one");
        let bad = temp.path().join("rdny-bad");
        fs::create_dir_all(&one).unwrap();
        fs::create_dir_all(&bad).unwrap();
        fs::write(
            one.join("state.json"),
            serde_json::to_string(&state(Some(1), Some("a"))).unwrap(),
        )
        .unwrap();
        fs::write(bad.join("state.json"), "not json").unwrap();

        let found = discover_from(vec![one.clone(), bad, one.clone()]).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].dir, one);
        assert_eq!(found[0].state.label.as_deref(), Some("a"));
    }

    #[test]
    fn classifies_liveness() {
        assert_eq!(
            classify(&state(Some(1), None), |_| true, |_, _| false),
            Liveness::Alive
        );
        assert_eq!(
            classify(&state(Some(1), None), |_| false, |_, _| true),
            Liveness::Dead
        );
        assert_eq!(
            classify(&state(None, None), |_| true, |_, _| true),
            Liveness::Attached
        );
        assert_eq!(
            classify(&state(None, None), |_| true, |_, _| false),
            Liveness::Dead
        );
    }

    #[test]
    fn formats_list_lines() {
        assert_eq!(
            format_line(&InstanceLine {
                dir: "/tmp/rdny-a".into(),
                pid: Some(7),
                liveness: Liveness::Alive,
                label: Some("x".into())
            }),
            "/tmp/rdny-a  pid=7  alive  label=x"
        );
        assert_eq!(
            format_line(&InstanceLine {
                dir: "/tmp/rdny-b".into(),
                pid: None,
                liveness: Liveness::Dead,
                label: None
            }),
            "/tmp/rdny-b  pid=-  dead  label=-"
        );
        assert_eq!(
            format_line(&InstanceLine {
                dir: "/tmp/rdny-c".into(),
                pid: None,
                liveness: Liveness::Attached,
                label: Some("me".into())
            }),
            "/tmp/rdny-c  pid=-  attached  label=me"
        );
    }

    #[test]
    fn cleanup_removes_dead_but_leaves_live_and_attached_without_all() {
        let temp = tempfile::tempdir().unwrap();
        let dead = temp.path().join("dead");
        let live = temp.path().join("live");
        let attached = temp.path().join("attached");
        fs::create_dir_all(&dead).unwrap();
        fs::create_dir_all(&live).unwrap();
        fs::create_dir_all(&attached).unwrap();
        fs::write(
            dead.join("state.json"),
            serde_json::to_vec(&state(Some(1), None)).unwrap(),
        )
        .unwrap();
        fs::write(
            live.join("state.json"),
            serde_json::to_vec(&state(Some(2), None)).unwrap(),
        )
        .unwrap();
        fs::write(
            attached.join("state.json"),
            serde_json::to_vec(&state(None, None)).unwrap(),
        )
        .unwrap();
        let instances = discover_from(vec![dead.clone(), live.clone(), attached.clone()]).unwrap();
        let cleaned = cleanup_instances(instances, false, |st| {
            classify(st, |pid| pid == 2, |_, _| true)
        })
        .unwrap();
        assert_eq!(cleaned.len(), 1);
        assert!(!dead.join("state.json").exists());
        assert!(live.join("state.json").exists());
        assert!(attached.join("state.json").exists());
    }

    #[test]
    fn cleanup_never_deletes_a_replacement_generation() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("state.json"),
            serde_json::to_vec(&state(Some(1), Some("discovered"))).unwrap(),
        )
        .unwrap();
        let discovered = discover_from(vec![temp.path().to_path_buf()]).unwrap();
        let paused = std::sync::Arc::new(std::sync::Barrier::new(2));
        let resume = std::sync::Arc::new(std::sync::Barrier::new(2));
        let cleanup_thread = {
            let paused = std::sync::Arc::clone(&paused);
            let resume = std::sync::Arc::clone(&resume);
            std::thread::spawn(move || {
                cleanup_instances_with_hook(
                    discovered,
                    false,
                    |_| Liveness::Dead,
                    |_| {
                        paused.wait();
                        resume.wait();
                    },
                )
                .unwrap()
            })
        };
        // Discovery is complete and cleanup is paused immediately before the
        // secure store is reopened and locked.
        paused.wait();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("commands::instances::tests::cleanup_replacement_helper")
            .arg("--exact")
            .env("RDNY_CLEANUP_REPLACE_DIR", temp.path())
            .spawn()
            .unwrap();
        fs::write(temp.path().join("go"), b"go").unwrap();
        while !temp.path().join("done").exists() {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(child.wait().unwrap().success());
        resume.wait();
        assert!(cleanup_thread.join().unwrap().is_empty());
        let replacement: SessionState =
            serde_json::from_slice(&fs::read(temp.path().join("state.json")).unwrap()).unwrap();
        assert_eq!(replacement.label.as_deref(), Some("replacement"));
    }
}
