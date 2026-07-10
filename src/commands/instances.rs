//! Discover, list, and clean up rdny instance state files.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::browser;
use crate::cdp::http;
use crate::process_identity::{self, ProcessClass};
use crate::session::Deadline;
use crate::state::{self, Generation, Inspection, SessionState};

type CandidateDir = (PathBuf, Option<String>);

/// How an instance relates to a running browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// rdny launched the browser and its pid is alive.
    Alive,
    /// No pid (attach session) but the debug port answers.
    Attached,
    /// Neither a live pid nor a reachable debug port.
    Dead,
    Unverifiable,
    Unrelated,
}

/// Classify an instance given injectable probes, so tests need
/// neither real pids nor sockets.
#[cfg(test)]
fn classify(
    state: &SessionState,
    pid_alive: impl Fn(u32) -> bool,
    port_reachable: impl Fn(&str, u16) -> bool,
) -> Liveness {
    match state.pid {
        Some(pid) if state.process_identity.is_some() && pid_alive(pid) => Liveness::Alive,
        Some(_) if state.process_identity.is_none() => Liveness::Unverifiable,
        Some(_) => Liveness::Dead,
        None if port_reachable(&state.host, state.port) => Liveness::Attached,
        None => Liveness::Dead,
    }
}

fn probe_liveness_until(state: &SessionState, deadline: Deadline) -> Liveness {
    if state.endpoint.is_some() {
        return if crate::broker::ping(state, deadline).is_ok() {
            Liveness::Alive
        } else {
            Liveness::Dead
        };
    }
    if let Some(id) = &state.process_identity
        && process_identity::validate_persisted(
            state.pid,
            state.browser_path.as_deref(),
            state.user_data_dir.as_deref(),
            id,
        )
        .is_err()
    {
        return Liveness::Unrelated;
    }
    let reachable = http::version_until(&state.host, state.port, deadline.instant()).is_ok();
    match process_identity::classify(state.pid, state.process_identity.as_ref(), reachable) {
        ProcessClass::ManagedMatching => Liveness::Alive,
        ProcessClass::ManagedDead | ProcessClass::AttachedDead => Liveness::Dead,
        ProcessClass::PidReusedOrUnrelated => Liveness::Unrelated,
        ProcessClass::LegacyUnverifiable => Liveness::Unverifiable,
        ProcessClass::AttachedReachable => Liveness::Attached,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Instance {
    pub dir: PathBuf,
    pub state: SessionState,
    generation: Generation,
    registered: bool,
    registry_instance_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Discovery {
    pub instances: Vec<Instance>,
    pub diagnostics: Vec<String>,
    pub stale_registered: Vec<(PathBuf, String)>,
    pub malformed: Vec<(PathBuf, Option<String>)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InstanceLine {
    pub dir: PathBuf,
    pub pid: Option<u32>,
    pub liveness: Liveness,
    pub label: Option<String>,
}

#[allow(dead_code)]
pub fn list() -> Result<()> {
    list_format(false)
}

pub fn list_format(structured: bool) -> Result<()> {
    list_format_until(structured, Deadline::after(http::HTTP_TIMEOUT))
}

pub fn list_format_until(structured: bool, deadline: Deadline) -> Result<()> {
    let discovery = discover()?;
    for diagnostic in &discovery.diagnostics {
        eprintln!("warning: {diagnostic}");
    }
    let mut rows = Vec::new();
    for instance in discovery.instances {
        let liveness = probe_liveness_until(&instance.state, deadline);
        if structured {
            rows.push(serde_json::json!({"dir":instance.dir,"pid":instance.state.pid,"liveness":format!("{:?}", liveness).to_lowercase(),"label":instance.state.label,"instance":instance.state.instance_id,"target":instance.state.target_id,"host":instance.state.host,"port":instance.state.port}));
        } else {
            println!(
                "{}",
                format_line(&InstanceLine {
                    dir: instance.dir,
                    pid: instance.state.pid,
                    liveness,
                    label: instance.state.label
                })
            );
        }
    }
    if structured {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"schemaVersion":1,"kind":"list","instances":rows})
            )?
        );
    }
    Ok(())
}

pub fn cleanup_until(all: bool, deadline: Deadline) -> Result<()> {
    let discovery = discover()?;
    for diagnostic in &discovery.diagnostics {
        eprintln!("warning: {diagnostic}");
    }
    for (dir, instance_id) in &discovery.malformed {
        let lifecycle = state::lifecycle_lock_at(dir)
            .with_context(|| format!("locking lifecycle in {}", dir.display()))?;
        if let Some(path) = state::quarantine_malformed_locked(&lifecycle, instance_id.as_deref())?
        {
            eprintln!("warning: quarantined malformed state as {}", path.display());
        }
    }
    for (dir, instance_id) in &discovery.stale_registered {
        let lifecycle = state::lifecycle_lock_at(dir)
            .with_context(|| format!("locking lifecycle in {}", dir.display()))?;
        state::prune_stale_registry_locked(&lifecycle, instance_id)?;
    }
    let cleaned = cleanup_instances_with_deadline(
        discovery.instances,
        all,
        |state| probe_liveness_until(state, deadline),
        Some(deadline),
    )?;
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

pub fn discover() -> Result<Discovery> {
    let (dirs, diagnostics) = candidate_dirs()?;
    discover_from_with_diagnostics(dirs, diagnostics)
}

fn candidate_dirs() -> Result<(Vec<CandidateDir>, Vec<String>)> {
    let current = state::resolved_state_dir()?;
    let default = state::default_state_dir()?;
    let (registered, diagnostics) = state::registered_state_dirs()?;
    let mut dirs = vec![(current, None), (default, None)];
    dirs.extend(
        registered
            .into_iter()
            .map(|entry| (entry.dir, Some(entry.instance_id))),
    );
    Ok((dirs, diagnostics))
}

#[cfg(test)]
pub fn discover_from(candidate_dirs: Vec<PathBuf>) -> Result<Vec<Instance>> {
    Ok(discover_from_with_diagnostics(
        candidate_dirs.into_iter().map(|dir| (dir, None)).collect(),
        Vec::new(),
    )?
    .instances)
}

fn discover_from_with_diagnostics(
    candidate_dirs: Vec<CandidateDir>,
    mut diagnostics: Vec<String>,
) -> Result<Discovery> {
    let mut found: BTreeMap<PathBuf, Instance> = BTreeMap::new();
    let mut stale_registered = Vec::new();
    let mut malformed_found = Vec::new();
    for (dir, registry_instance_id) in candidate_dirs {
        let registered = registry_instance_id.is_some();
        let key = canonical_key(&dir);
        if let Some(existing) = found.get_mut(&key) {
            existing.registered |= registered;
            if let Some(observed_id) = registry_instance_id {
                if existing.state.instance_id.as_deref() == Some(&observed_id) {
                    if existing.registry_instance_id.is_none() {
                        existing.registry_instance_id = Some(observed_id);
                    }
                } else {
                    diagnostics.push(format!(
                        "registered state dir stale instance id: {}",
                        dir.display()
                    ));
                    stale_registered.push((dir, observed_id));
                }
            }
            continue;
        }
        let store = match state::open_store_at(&dir) {
            Ok(store) => store,
            Err(err) => {
                if registered {
                    diagnostics.push(format!(
                        "registered state dir unavailable: {} ({err})",
                        dir.display()
                    ));
                    if let Some(observed_id) = registry_instance_id {
                        stale_registered.push((dir, observed_id));
                    }
                }
                continue;
            }
        };
        let inspection = store.inspect::<SessionState>("state.json")?;
        let (state, generation) = match inspection {
            Inspection::Valid(state, generation) => (state, generation),
            Inspection::Missing => {
                if registered {
                    diagnostics.push(format!(
                        "registered state dir missing state.json: {}",
                        dir.display()
                    ));
                    if let Some(observed_id) = registry_instance_id {
                        stale_registered.push((dir, observed_id));
                    }
                }
                continue;
            }
            Inspection::Malformed(malformed) => {
                diagnostics.push(format!(
                    "malformed state in {}: {}",
                    dir.display(),
                    malformed.message()
                ));
                if let Some(observed_id) = registry_instance_id {
                    stale_registered.push((dir.clone(), observed_id.clone()));
                    malformed_found.push((dir, Some(observed_id)));
                } else {
                    malformed_found.push((dir, None));
                }
                continue;
            }
            Inspection::Incompatible(err, _) => {
                diagnostics.push(format!("incompatible state in {}: {err}", dir.display()));
                continue;
            }
        };
        if let Some(observed_id) = &registry_instance_id
            && state.instance_id.as_deref() != Some(observed_id)
        {
            diagnostics.push(format!(
                "registered state dir stale instance id: {}",
                dir.display()
            ));
            stale_registered.push((dir, observed_id.clone()));
            continue;
        }
        found.insert(
            key,
            Instance {
                dir,
                state,
                generation,
                registered,
                registry_instance_id,
            },
        );
    }
    Ok(Discovery {
        instances: found.into_values().collect(),
        diagnostics,
        stale_registered,
        malformed: malformed_found,
    })
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
            Liveness::Unverifiable => "legacy-unverifiable",
            Liveness::Unrelated => "pid-reused/unrelated",
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

#[cfg(test)]
pub fn cleanup_instances(
    instances: Vec<Instance>,
    all: bool,
    liveness: impl Fn(&SessionState) -> Liveness,
) -> Result<Vec<Instance>> {
    cleanup_instances_with_deadline(instances, all, liveness, None)
}

fn cleanup_instances_with_deadline(
    instances: Vec<Instance>,
    all: bool,
    liveness: impl Fn(&SessionState) -> Liveness,
    deadline: Option<Deadline>,
) -> Result<Vec<Instance>> {
    cleanup_instances_with_hook(instances, all, liveness, |_| {}, deadline)
}

fn cleanup_instances_with_hook(
    instances: Vec<Instance>,
    all: bool,
    liveness: impl Fn(&SessionState) -> Liveness,
    before_locked_action: impl Fn(&Instance),
    deadline: Option<Deadline>,
) -> Result<Vec<Instance>> {
    let mut cleaned = Vec::new();
    for instance in instances {
        let current_liveness = liveness(&instance.state);
        if current_liveness != Liveness::Dead && !all {
            continue;
        }
        before_locked_action(&instance);
        let _lifecycle = state::lifecycle_lock_at(&instance.dir)
            .with_context(|| format!("locking lifecycle in {}", instance.dir.display()))?;
        let store = state::open_store_at(&instance.dir)
            .with_context(|| format!("opening {}", instance.dir.display()))?;
        let Inspection::Valid(latest, latest_generation) =
            store.inspect::<SessionState>("state.json")?
        else {
            continue;
        };
        if latest_generation.bytes() != instance.generation.bytes()
            || instance
                .registry_instance_id
                .as_deref()
                .is_some_and(|id| latest.instance_id.as_deref() != Some(id))
        {
            continue;
        }
        let latest_liveness = liveness(&latest);
        if !all && latest_liveness != Liveness::Dead {
            continue;
        }
        if latest_liveness == Liveness::Dead && latest.endpoint.is_some() {
            crate::broker::remove_stale_socket(&latest).with_context(|| {
                format!(
                    "validating stale broker socket in {}",
                    instance.dir.display()
                )
            })?;
        }
        // Attached sessions are only detached. Managed live sessions selected
        // by --all are stopped while lifecycle ownership remains exclusive.
        if latest_liveness == Liveness::Alive {
            match deadline {
                Some(deadline) => browser::stop_until(&latest, deadline),
                None => browser::stop(&latest),
            }
            .with_context(|| format!("stopping instance in {}", instance.dir.display()))?;
        }
        let removed = state::clear_observed_lifecycle(
            &_lifecycle,
            latest.instance_id.as_deref(),
            &latest_generation,
        )?;
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
            instance_id: label.map(|l| format!("id-{l}")),
            endpoint: None,
            ws_url: "ws://x".into(),
            host: "127.0.0.1".into(),
            port: 1,
            pid,
            process_identity: None,
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
            instrumentation: None,
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
    fn discovery_dedupe_preserves_registered_provenance() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("state.json"),
            serde_json::to_vec(&state(Some(1), Some("a"))).unwrap(),
        )
        .unwrap();
        let discovery = discover_from_with_diagnostics(
            vec![
                (temp.path().to_path_buf(), None),
                (temp.path().to_path_buf(), Some("id-a".to_string())),
            ],
            Vec::new(),
        )
        .unwrap();
        assert_eq!(discovery.instances.len(), 1);
        assert!(discovery.instances[0].registered);
        assert_eq!(
            discovery.instances[0].registry_instance_id.as_deref(),
            Some("id-a")
        );
    }

    #[test]
    fn malformed_state_discovery_is_read_only() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("state.json"), "not json").unwrap();
        let discovery = discover_from_with_diagnostics(
            vec![(temp.path().to_path_buf(), Some("observed".to_string()))],
            Vec::new(),
        )
        .unwrap();
        assert!(discovery.instances.is_empty());
        assert_eq!(
            discovery.stale_registered,
            vec![(temp.path().to_path_buf(), "observed".to_string())]
        );
        assert!(
            discovery
                .diagnostics
                .iter()
                .any(|d| d.contains("malformed state"))
        );
        assert!(temp.path().join("state.json").exists());
        assert_eq!(discovery.malformed.len(), 1);
        assert!(
            !fs::read_dir(temp.path())
                .unwrap()
                .flatten()
                .any(|e| { e.file_name().to_string_lossy().contains("quarantine") })
        );
    }

    #[test]
    fn classifies_liveness() {
        assert_eq!(
            classify(&state(Some(1), None), |_| true, |_, _| false),
            Liveness::Unverifiable
        );
        assert_eq!(
            classify(&state(Some(1), None), |_| false, |_, _| true),
            Liveness::Unverifiable
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
    fn cleanup_preserves_legacy_unverifiable_without_all() {
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
        assert_eq!(cleaned.len(), 0);
        assert!(dead.join("state.json").exists());
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
                    None,
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

    #[test]
    fn cleanup_preserves_concurrent_replacement_and_registration() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("state.json"),
            serde_json::to_vec(&state(Some(1), Some("discovered"))).unwrap(),
        )
        .unwrap();
        let mut discovered = discover_from_with_diagnostics(
            vec![(temp.path().to_path_buf(), Some("id-discovered".to_string()))],
            Vec::new(),
        )
        .unwrap()
        .instances;
        assert_eq!(discovered.len(), 1);
        discovered[0].registered = true;
        let new_dir = temp.path().join("new");
        fs::create_dir_all(&new_dir).unwrap();
        let cleaned = cleanup_instances_with_hook(
            discovered,
            false,
            |_| Liveness::Dead,
            |_| {
                fs::write(
                    temp.path().join("state.json"),
                    serde_json::to_vec(&state(Some(2), Some("replacement"))).unwrap(),
                )
                .unwrap();
            },
            None,
        )
        .unwrap();
        assert!(cleaned.is_empty());
        assert!(temp.path().join("state.json").exists());
    }
}
