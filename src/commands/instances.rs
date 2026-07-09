//! Discover, list, and clean up registered rdny instances.

use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::browser;
use crate::cdp::http;
use crate::state::{self, RegistryEntry, SessionState, StateFile, Transaction};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Alive,
    Attached,
    Dead,
}

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
        |pid| pid > 0 && pid <= libc::pid_t::MAX as u32 && browser::pid_exists(pid as libc::pid_t),
        |host, port| http::version(host, port).is_ok(),
    )
}

#[derive(Debug)]
pub enum Discovered {
    Instance {
        dir: PathBuf,
        state: Box<SessionState>,
    },
    Missing {
        dir: PathBuf,
    },
    Corrupt {
        dir: PathBuf,
        reason: String,
    },
    Unsafe {
        dir: PathBuf,
        reason: String,
    },
    RegistryProblem(String),
}

pub fn list() -> Result<()> {
    for item in discover()? {
        match item {
            Discovered::Instance { dir, state } => println!(
                "{}  pid={}  {}  label={}",
                dir.display(),
                state
                    .pid
                    .map(|pid| pid.to_string())
                    .unwrap_or_else(|| "-".into()),
                match probe_liveness(&state) {
                    Liveness::Alive => "alive",
                    Liveness::Attached => "attached",
                    Liveness::Dead => "dead",
                },
                state.label.as_deref().unwrap_or("-")
            ),
            Discovered::Missing { dir } => println!("{}  missing (prunable)", dir.display()),
            Discovered::Corrupt { dir, reason } => {
                println!("{}  corrupt: {reason} (run `rdny cleanup`)", dir.display())
            }
            Discovered::Unsafe { dir, reason } => {
                println!("{}  unsafe: {reason} (prunable)", dir.display())
            }
            Discovered::RegistryProblem(reason) => {
                println!("registry  malformed: {reason} (prunable)")
            }
        }
    }
    Ok(())
}

pub fn cleanup(all: bool) -> Result<()> {
    let items = discover()?;
    for item in items {
        match item {
            Discovered::Instance { dir, state: _ } => {
                let tx = Transaction::begin_in(&dir, Duration::from_secs(5))?;
                let Some(current) = tx.load()? else {
                    tx.unregister()?;
                    println!("pruned: {}", dir.display());
                    continue;
                };
                let liveness = probe_liveness(&current);
                if liveness != Liveness::Dead && !all {
                    println!("kept live: {}", dir.display());
                    continue;
                }
                if liveness == Liveness::Alive {
                    browser::stop(&current)
                        .with_context(|| format!("stopping instance in {}", dir.display()))?;
                }
                tx.clear_state_file()?;
                tx.unregister()?;
                println!("cleaned: {}", dir.display());
            }
            Discovered::Corrupt { dir, .. } => {
                let tx = Transaction::begin_in(&dir, Duration::from_secs(5))?;
                if let Some(path) = tx.quarantine_corrupt()? {
                    println!("quarantined: {}", path.display());
                }
                tx.unregister()?;
            }
            Discovered::Missing { dir } | Discovered::Unsafe { dir, .. } => {
                state::unregister_dir_for_cleanup(&dir)?;
                println!("pruned: {}", dir.display());
            }
            Discovered::RegistryProblem(reason) => {
                println!("pruned malformed registry entry: {reason}")
            }
        }
    }
    Ok(())
}

pub fn discover() -> Result<Vec<Discovered>> {
    let mut entries = state::registry_entries()?;
    entries.push(RegistryEntry::Path(state::resolved_state_dir()?));
    let default = state::default_state_dir()?;
    if default.exists() {
        entries.push(RegistryEntry::Path(default));
    }
    discover_from(entries)
}

pub fn discover_from(entries: Vec<RegistryEntry>) -> Result<Vec<Discovered>> {
    let mut seen = BTreeSet::new();
    let mut found = Vec::new();
    for entry in entries {
        let dir = match entry {
            RegistryEntry::Malformed(reason) => {
                found.push(Discovered::RegistryProblem(reason));
                continue;
            }
            RegistryEntry::Path(dir) => dir,
        };
        if !seen.insert(dir.clone()) {
            continue;
        }
        match std::fs::symlink_metadata(&dir) {
            Ok(_) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {
                found.push(Discovered::Missing { dir });
                continue;
            }
            Err(err) => {
                found.push(Discovered::Unsafe {
                    dir,
                    reason: format!("inspecting registered state directory: {err}"),
                });
                continue;
            }
        }
        let tx = match Transaction::begin_in(&dir, Duration::from_secs(5)) {
            Ok(tx) => tx,
            Err(err) => {
                found.push(Discovered::Unsafe {
                    dir,
                    reason: format!("{err:#}"),
                });
                continue;
            }
        };
        match tx.inspect() {
            Ok(StateFile::Valid(state)) => found.push(Discovered::Instance { dir, state }),
            Ok(StateFile::Missing) => found.push(Discovered::Missing { dir }),
            Ok(StateFile::Corrupt(reason)) => found.push(Discovered::Corrupt { dir, reason }),
            Err(err) => found.push(Discovered::Unsafe {
                dir,
                reason: format!("{err:#}"),
            }),
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(pid: Option<u32>) -> SessionState {
        SessionState {
            ws_url: "ws://x".into(),
            host: "127.0.0.1".into(),
            port: 1,
            pid,
            process_identity: None,
            user_data_dir: None,
            browser_path: None,
            target_id: None,
            label: None,
            viewport: None,
            recording: false,
        }
    }

    #[test]
    fn classifies_liveness() {
        assert_eq!(
            classify(&state(Some(1)), |_| true, |_, _| false),
            Liveness::Alive
        );
        assert_eq!(
            classify(&state(Some(1)), |_| false, |_, _| true),
            Liveness::Dead
        );
        assert_eq!(
            classify(&state(None), |_| true, |_, _| true),
            Liveness::Attached
        );
    }

    #[test]
    fn discovery_surfaces_missing_corrupt_and_malformed_registry_entries() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        let corrupt = temp.path().join("corrupt");
        state::secure_dir(&corrupt).unwrap();
        std::fs::write(corrupt.join("state.json"), "{").unwrap();
        std::fs::set_permissions(
            corrupt.join("state.json"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let found = discover_from(vec![
            RegistryEntry::Path(missing.clone()),
            RegistryEntry::Path(corrupt),
            RegistryEntry::Malformed("bad item".into()),
        ])
        .unwrap();
        assert!(
            found
                .iter()
                .any(|item| matches!(item, Discovered::Missing { .. }))
        );
        assert!(
            !missing.exists(),
            "discovery must not recreate missing entries"
        );
        assert!(
            found
                .iter()
                .any(|item| matches!(item, Discovered::Corrupt { .. }))
        );
        assert!(
            found
                .iter()
                .any(|item| matches!(item, Discovered::RegistryProblem(_)))
        );
    }

    use std::os::unix::fs::PermissionsExt;
}
