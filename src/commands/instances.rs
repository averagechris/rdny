//! Discover, list, and clean up rdny instance state files.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

#[cfg(test)]
use std::{fs, path::Path};

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

const CLEANUP_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CleanupProbe {
    Conclusive(Liveness),
    Inconclusive(CleanupProbeFailure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CleanupProbeFailure {
    Expired,
    Timeout,
    Unavailable,
}

impl CleanupProbeFailure {
    fn diagnostic(self) -> &'static str {
        match self {
            Self::Expired => "probe budget expired",
            Self::Timeout => "probe timed out",
            Self::Unavailable => "probe unavailable",
        }
    }
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

fn cleanup_probe_liveness_until(state: &SessionState, overall: Deadline) -> CleanupProbe {
    let Some(remaining) = overall.remaining() else {
        return CleanupProbe::Inconclusive(CleanupProbeFailure::Expired);
    };
    let probe_deadline = Deadline::at(
        std::time::Instant::now()
            .checked_add(remaining.min(CLEANUP_PROBE_TIMEOUT))
            .unwrap_or_else(|| overall.instant())
            .min(overall.instant()),
    );
    let liveness = probe_liveness_until(state, probe_deadline);
    if probe_deadline.expired() {
        CleanupProbe::Inconclusive(if overall.expired() {
            CleanupProbeFailure::Expired
        } else {
            CleanupProbeFailure::Timeout
        })
    } else if matches!(liveness, Liveness::Dead) && state.pid.is_none() {
        CleanupProbe::Inconclusive(CleanupProbeFailure::Unavailable)
    } else {
        CleanupProbe::Conclusive(liveness)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CleanupDecision {
    Cleaned,
    PreservedDeadRecheck,
    PreservedInconclusive(CleanupProbeFailure),
    PreservedLive(Liveness),
}

impl CleanupDecision {
    fn human_reason(self) -> &'static str {
        match self {
            Self::Cleaned => "cleaned",
            Self::PreservedDeadRecheck => "state changed before locked cleanup",
            Self::PreservedInconclusive(reason) => reason.diagnostic(),
            Self::PreservedLive(Liveness::Alive) => "browser is alive",
            Self::PreservedLive(Liveness::Attached) => "attached browser is reachable",
            Self::PreservedLive(Liveness::Dead) => "dead",
            Self::PreservedLive(Liveness::Unverifiable) => "legacy pid is unverifiable",
            Self::PreservedLive(Liveness::Unrelated) => "pid belongs to an unrelated process",
        }
    }

    fn structured_kind(self) -> &'static str {
        match self {
            Self::Cleaned => "cleaned",
            Self::PreservedDeadRecheck => "preserved_changed",
            Self::PreservedInconclusive(CleanupProbeFailure::Expired) => {
                "preserved_inconclusive_expired"
            }
            Self::PreservedInconclusive(CleanupProbeFailure::Timeout) => {
                "preserved_inconclusive_timeout"
            }
            Self::PreservedInconclusive(CleanupProbeFailure::Unavailable) => {
                "preserved_inconclusive_unavailable"
            }
            Self::PreservedLive(Liveness::Alive) => "preserved_live_alive",
            Self::PreservedLive(Liveness::Attached) => "preserved_live_attached",
            Self::PreservedLive(Liveness::Dead) => "preserved_dead",
            Self::PreservedLive(Liveness::Unverifiable) => "preserved_live_unverifiable",
            Self::PreservedLive(Liveness::Unrelated) => "preserved_live_unrelated",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct CleanupEntry {
    instance: Instance,
    decision: CleanupDecision,
}

#[derive(Debug, Default, Clone, PartialEq)]
struct CleanupReport {
    entries: Vec<CleanupEntry>,
}

impl CleanupReport {
    fn push(&mut self, instance: Instance, decision: CleanupDecision) {
        self.entries.push(CleanupEntry { instance, decision });
    }

    fn cleaned(&self) -> impl Iterator<Item = &CleanupEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.decision == CleanupDecision::Cleaned)
    }

    #[cfg(test)]
    fn cleaned_len(&self) -> usize {
        self.cleaned().count()
    }

    #[cfg(test)]
    fn cleaned_is_empty(&self) -> bool {
        self.cleaned_len() == 0
    }

    #[allow(dead_code)]
    fn structured_rows(&self) -> Vec<serde_json::Value> {
        self.entries
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "dir": entry.instance.dir,
                    "pid": entry.instance.state.pid,
                    "label": entry.instance.state.label,
                    "action": entry.decision.structured_kind(),
                    "reason": entry.decision.human_reason(),
                })
            })
            .collect()
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
    /// Stable value accepted by `rdny --instance`; absent for unregistered state.
    pub selector: Option<String>,
}

/// A registry-verified instance selected for command dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedInstance {
    pub dir: PathBuf,
    pub instance_id: String,
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
            let selector = instance
                .registered
                .then(|| instance.registry_instance_id.clone())
                .flatten();
            rows.push(serde_json::json!({"dir":instance.dir,"pid":instance.state.pid,"liveness":format!("{:?}", liveness).to_lowercase(),"label":instance.state.label,"instance":instance.state.instance_id,"selector":selector,"target":instance.state.target_id,"host":instance.state.host,"port":instance.state.port}));
        } else {
            println!(
                "{}",
                format_line(&InstanceLine {
                    dir: instance.dir,
                    pid: instance.state.pid,
                    liveness,
                    label: instance.state.label,
                    selector: instance.registered.then(|| {
                        instance
                            .registry_instance_id
                            .expect("registered discovery has a verified instance id")
                    }),
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
    let report = cleanup_instances_with_deadline(
        discovery.instances,
        all,
        |state| cleanup_probe_liveness_until(state, deadline),
        Some(deadline),
    )?;
    for entry in &report.entries {
        if entry.decision != CleanupDecision::Cleaned {
            eprintln!(
                "warning: preserved: {} (pid={} label={} reason={})",
                entry.instance.dir.display(),
                display_pid(entry.instance.state.pid),
                display_label(entry.instance.state.label.as_deref()),
                entry.decision.human_reason()
            );
        }
    }
    for entry in report.cleaned() {
        println!(
            "cleaned: {} (pid={} label={})",
            entry.instance.dir.display(),
            display_pid(entry.instance.state.pid),
            display_label(entry.instance.state.label.as_deref())
        );
    }
    Ok(())
}

pub fn discover() -> Result<Discovery> {
    let (dirs, diagnostics) = candidate_dirs()?;
    discover_from_with_diagnostics(dirs, diagnostics)
}

/// Resolve an exact registered instance id or a unique exact label.
///
/// This path is deliberately read-only. It uses only the secured global
/// registry, then verifies every referenced state directory before considering
/// it selectable. Current/default legacy state discovered by `rdny list` is not
/// implicitly promoted into the registry trust boundary.
pub fn resolve_registered(selector: &str) -> Result<ResolvedInstance> {
    let (registered, diagnostics) = state::registered_state_dirs_read_only()
        .context("reading the secured rdny instance registry without modifying it")?;
    let candidates = registered
        .into_iter()
        .map(|entry| (entry.dir, Some(entry.instance_id)))
        .collect();
    let discovery = discover_from_with_diagnostics_mode(candidates, diagnostics, true)?;
    resolve_discovery(selector, &discovery)
}

fn resolve_discovery(selector: &str, discovery: &Discovery) -> Result<ResolvedInstance> {
    let exact: Vec<_> = discovery
        .instances
        .iter()
        .filter(|instance| instance.registry_instance_id.as_deref() == Some(selector))
        .collect();
    let stale_exact: Vec<_> = discovery
        .stale_registered
        .iter()
        .filter(|(_, instance_id)| instance_id == selector)
        .collect();
    if exact.len() == 1 && stale_exact.is_empty() {
        return selectable(exact[0], selector);
    }
    if exact.len() > 1 || (!exact.is_empty() && !stale_exact.is_empty()) {
        let mut details = exact
            .iter()
            .map(|instance| format!("id={selector} dir={}", instance.dir.display()))
            .collect::<Vec<_>>();
        details.extend(
            stale_exact
                .iter()
                .map(|(dir, _)| format!("id={selector} dir={} (stale)", dir.display())),
        );
        anyhow::bail!(
            "ambiguous registered instance id `{selector}` has multiple registry entries: {}; no command was run. Run `rdny cleanup` after verifying each browser",
            details.join(", ")
        );
    }
    if !stale_exact.is_empty() {
        let dirs = stale_exact
            .iter()
            .map(|(dir, _)| dir.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let details = discovery_diagnostics(discovery);
        anyhow::bail!(
            "registered instance `{selector}` is stale or unavailable at {dirs}; no command was run.{details} Run `rdny cleanup` after verifying the browser"
        );
    }

    let labels: Vec<_> = discovery
        .instances
        .iter()
        .filter(|instance| instance.state.label.as_deref() == Some(selector))
        .collect();
    if labels.len() == 1 {
        if !discovery.stale_registered.is_empty() || !discovery.diagnostics.is_empty() {
            let instance_id = labels[0]
                .registry_instance_id
                .as_deref()
                .expect("registered label match has instance id");
            let details = discovery_diagnostics(discovery);
            anyhow::bail!(
                "cannot establish that registered label `{selector}` is unique while registry entries are stale or unavailable; no command was run.{details} Retry with exact id `--instance {instance_id}`, or run `rdny cleanup` after verifying the affected browsers"
            );
        }
        return selectable(labels[0], selector);
    }
    if labels.len() > 1 {
        return Err(ambiguous_error("label", selector, &labels));
    }

    let skipped = discovery_diagnostics(discovery);
    anyhow::bail!(
        "no registered rdny instance matches `{selector}`.{skipped} Run `rdny list` and copy a `selector=...` value, or start an instance with `rdny --state-dir PATH start --label LABEL`"
    )
}

fn discovery_diagnostics(discovery: &Discovery) -> String {
    if discovery.diagnostics.is_empty() {
        String::new()
    } else {
        format!(
            " Registry diagnostics: {}.",
            discovery.diagnostics.join("; ")
        )
    }
}

fn selectable(instance: &Instance, selector: &str) -> Result<ResolvedInstance> {
    let instance_id = instance
        .registry_instance_id
        .as_deref()
        .context("internal error: selected instance lacks registry identity")?;
    if instance.state.instance_id.as_deref() != Some(instance_id) {
        anyhow::bail!(
            "registered instance `{selector}` points to unrelated state in {}; no command was run. Run `rdny cleanup` after verifying the browser",
            instance.dir.display()
        );
    }
    if let Some(identity) = &instance.state.process_identity {
        process_identity::validate_persisted(
            instance.state.pid,
            instance.state.browser_path.as_deref(),
            instance.state.user_data_dir.as_deref(),
            identity,
        )
        .with_context(|| {
            format!(
                "registered instance `{selector}` has unrelated process state in {}; no command was run",
                instance.dir.display()
            )
        })?;
        if matches!(
            process_identity::classify(instance.state.pid, Some(identity), false),
            ProcessClass::PidReusedOrUnrelated
        ) {
            anyhow::bail!(
                "registered instance `{selector}` refers to a reused or unrelated process in {}; no command was run. Verify the browser, then use `rdny cleanup`",
                instance.dir.display()
            );
        }
    }
    Ok(ResolvedInstance {
        dir: instance.dir.clone(),
        instance_id: instance_id.to_string(),
    })
}

fn ambiguous_error(kind: &str, selector: &str, matches: &[&Instance]) -> anyhow::Error {
    let details = matches
        .iter()
        .map(|instance| {
            format!(
                "id={} dir={}",
                instance
                    .registry_instance_id
                    .as_deref()
                    .or(instance.state.instance_id.as_deref())
                    .unwrap_or("<missing>"),
                instance.dir.display()
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    anyhow::anyhow!(
        "ambiguous registered instance {kind} `{selector}` matches {details}; retry with `--instance` and one of the listed ids"
    )
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
    diagnostics: Vec<String>,
) -> Result<Discovery> {
    discover_from_with_diagnostics_mode(candidate_dirs, diagnostics, false)
}

fn discover_from_with_diagnostics_mode(
    candidate_dirs: Vec<CandidateDir>,
    mut diagnostics: Vec<String>,
    read_only: bool,
) -> Result<Discovery> {
    let mut found: BTreeMap<PathBuf, Instance> = BTreeMap::new();
    let mut stale_registered = Vec::new();
    let mut malformed_found = Vec::new();
    for (dir, registry_instance_id) in candidate_dirs {
        let registered = registry_instance_id.is_some();
        // Registry paths are normalized absolute paths. Do not canonicalize:
        // following a symlink here could deduplicate it before secure O_NOFOLLOW
        // validation sees and rejects the untrusted registry pathname.
        let key = dir.clone();
        if let Some(existing) = found.get_mut(&key) {
            if let Some(observed_id) = registry_instance_id {
                if existing.state.instance_id.as_deref() == Some(&observed_id) {
                    existing.registered = true;
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
        let store = match if read_only {
            state::open_store_at_read_only(&dir)
        } else {
            state::open_store_at(&dir)
        } {
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
        let inspection = match store.inspect::<SessionState>("state.json") {
            Ok(inspection) => inspection,
            Err(err) => {
                if registered {
                    diagnostics.push(format!(
                        "registered state dir inaccessible: {} ({err})",
                        dir.display()
                    ));
                    if let Some(observed_id) = registry_instance_id {
                        stale_registered.push((dir, observed_id));
                    }
                } else {
                    return Err(err)
                        .with_context(|| format!("inspecting state in {}", dir.display()));
                }
                continue;
            }
        };
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
                if let Some(observed_id) = registry_instance_id {
                    stale_registered.push((dir, observed_id));
                }
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

pub fn format_line(line: &InstanceLine) -> String {
    format!(
        "{}  pid={}  {}  label={}  selector={}",
        line.dir.display(),
        display_pid(line.pid),
        match line.liveness {
            Liveness::Alive => "alive",
            Liveness::Attached => "attached",
            Liveness::Dead => "dead",
            Liveness::Unverifiable => "legacy-unverifiable",
            Liveness::Unrelated => "pid-reused/unrelated",
        },
        display_label(line.label.as_deref()),
        line.selector.as_deref().unwrap_or("-")
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
    Ok(cleanup_instances_with_deadline(
        instances,
        all,
        |state| CleanupProbe::Conclusive(liveness(state)),
        None,
    )?
    .cleaned()
    .map(|entry| entry.instance.clone())
    .collect())
}

fn cleanup_instances_with_deadline(
    instances: Vec<Instance>,
    all: bool,
    liveness: impl FnMut(&SessionState) -> CleanupProbe,
    deadline: Option<Deadline>,
) -> Result<CleanupReport> {
    cleanup_instances_with_hook(instances, all, liveness, |_| {}, deadline)
}

fn cleanup_instances_with_hook(
    instances: Vec<Instance>,
    all: bool,
    mut liveness: impl FnMut(&SessionState) -> CleanupProbe,
    before_locked_action: impl Fn(&Instance),
    deadline: Option<Deadline>,
) -> Result<CleanupReport> {
    let mut report = CleanupReport::default();
    for instance in instances {
        let current_liveness = liveness(&instance.state);
        if !all && current_liveness != CleanupProbe::Conclusive(Liveness::Dead) {
            report.push(instance, cleanup_preserved_decision(current_liveness));
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
            report.push(instance, CleanupDecision::PreservedDeadRecheck);
            continue;
        }
        let latest_liveness = liveness(&latest);
        if !all && latest_liveness != CleanupProbe::Conclusive(Liveness::Dead) {
            report.push(instance, cleanup_preserved_decision(latest_liveness));
            continue;
        }
        let latest_liveness = match latest_liveness {
            CleanupProbe::Conclusive(liveness) => liveness,
            CleanupProbe::Inconclusive(_) => Liveness::Dead,
        };
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
            report.push(instance, CleanupDecision::Cleaned);
        }
    }
    Ok(report)
}

fn cleanup_preserved_decision(probe: CleanupProbe) -> CleanupDecision {
    match probe {
        CleanupProbe::Conclusive(liveness) => CleanupDecision::PreservedLive(liveness),
        CleanupProbe::Inconclusive(reason) => CleanupDecision::PreservedInconclusive(reason),
    }
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

    fn write_registered_state(dir: &Path, instance_id: &str, label: Option<&str>) {
        fs::create_dir_all(dir).unwrap();
        let mut value = state(None, label);
        value.instance_id = Some(instance_id.to_string());
        let path = dir.join("state.json");
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
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
    fn discovery_stale_dedupe_never_advertises_unverified_selector() {
        let temp = tempfile::tempdir().unwrap();
        write_registered_state(temp.path(), "replacement", Some("current"));
        let discovery = discover_from_with_diagnostics(
            vec![
                (temp.path().to_path_buf(), None),
                (temp.path().to_path_buf(), Some("stale-id".into())),
            ],
            Vec::new(),
        )
        .unwrap();

        assert_eq!(discovery.instances.len(), 1);
        assert!(!discovery.instances[0].registered);
        assert_eq!(discovery.instances[0].registry_instance_id, None);
        assert_eq!(
            discovery.stale_registered,
            vec![(temp.path().to_path_buf(), "stale-id".into())]
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
                label: Some("x".into()),
                selector: Some("abc123".into()),
            }),
            "/tmp/rdny-a  pid=7  alive  label=x  selector=abc123"
        );
        assert_eq!(
            format_line(&InstanceLine {
                dir: "/tmp/rdny-b".into(),
                pid: None,
                liveness: Liveness::Dead,
                label: None,
                selector: None,
            }),
            "/tmp/rdny-b  pid=-  dead  label=-  selector=-"
        );
        assert_eq!(
            format_line(&InstanceLine {
                dir: "/tmp/rdny-c".into(),
                pid: None,
                liveness: Liveness::Attached,
                label: Some("me".into()),
                selector: Some("def456".into()),
            }),
            "/tmp/rdny-c  pid=-  attached  label=me  selector=def456"
        );
    }

    #[test]
    fn selector_exact_id_wins_over_matching_label() {
        let temp = tempfile::tempdir().unwrap();
        let exact = temp.path().join("exact");
        let label = temp.path().join("label");
        write_registered_state(&exact, "id-exact", Some("other"));
        write_registered_state(&label, "id-label", Some("id-exact"));
        let discovery = discover_from_with_diagnostics(
            vec![
                (exact.clone(), Some("id-exact".into())),
                (label, Some("id-label".into())),
            ],
            Vec::new(),
        )
        .unwrap();

        assert_eq!(
            resolve_discovery("id-exact", &discovery).unwrap(),
            ResolvedInstance {
                dir: exact,
                instance_id: "id-exact".into(),
            }
        );
    }

    #[test]
    fn selector_accepts_unique_label_and_reports_ambiguous_ids_and_dirs() {
        let temp = tempfile::tempdir().unwrap();
        let one = temp.path().join("one");
        let two = temp.path().join("two");
        let unique = temp.path().join("unique");
        write_registered_state(&one, "id-one", Some("shared"));
        write_registered_state(&two, "id-two", Some("shared"));
        write_registered_state(&unique, "id-unique", Some("solo"));
        let discovery = discover_from_with_diagnostics(
            vec![
                (one.clone(), Some("id-one".into())),
                (two.clone(), Some("id-two".into())),
                (unique.clone(), Some("id-unique".into())),
            ],
            Vec::new(),
        )
        .unwrap();

        assert_eq!(
            resolve_discovery("solo", &discovery).unwrap(),
            ResolvedInstance {
                dir: unique,
                instance_id: "id-unique".into(),
            }
        );
        let error = resolve_discovery("shared", &discovery).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("ambiguous registered instance label `shared`"));
        assert!(message.contains("id=id-one"));
        assert!(message.contains(&one.display().to_string()));
        assert!(message.contains("id=id-two"));
        assert!(message.contains(&two.display().to_string()));
    }

    #[test]
    fn selector_rejects_stale_malformed_and_missing_registry_targets_read_only() {
        let temp = tempfile::tempdir().unwrap();
        let stale = temp.path().join("stale");
        let malformed = temp.path().join("malformed");
        let missing = temp.path().join("missing");
        let real = temp.path().join("real");
        let inaccessible = temp.path().join("inaccessible-link");
        write_registered_state(&stale, "replacement", Some("stale-label"));
        fs::create_dir_all(&malformed).unwrap();
        fs::write(malformed.join("state.json"), b"not json").unwrap();
        write_registered_state(&real, "inaccessible-id", Some("hidden"));
        std::os::unix::fs::symlink(&real, &inaccessible).unwrap();
        let discovery = discover_from_with_diagnostics(
            vec![
                (stale.clone(), Some("stale-id".into())),
                (malformed.clone(), Some("malformed-id".into())),
                (missing.clone(), Some("missing-id".into())),
                (inaccessible.clone(), Some("inaccessible-id".into())),
            ],
            vec!["registry test diagnostic".into()],
        )
        .unwrap();

        for selector in ["stale-id", "malformed-id", "missing-id", "inaccessible-id"] {
            let message = format!("{:#}", resolve_discovery(selector, &discovery).unwrap_err());
            assert!(message.contains("stale or unavailable"), "{message}");
            assert!(message.contains("no command was run"), "{message}");
        }
        let unknown = format!(
            "{:#}",
            resolve_discovery("unknown", &discovery).unwrap_err()
        );
        assert!(unknown.contains("Registry diagnostics"));
        assert!(malformed.join("state.json").exists());
        assert!(!missing.exists());
        assert!(
            inaccessible
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            !fs::read_dir(&malformed)
                .unwrap()
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().contains("quarantine"))
        );
    }

    #[test]
    fn selector_validates_each_registry_path_and_accounts_for_incompatible_ids() {
        let temp = tempfile::tempdir().unwrap();
        let valid = temp.path().join("valid");
        let alias = temp.path().join("alias");
        let incompatible = temp.path().join("incompatible");
        write_registered_state(&valid, "duplicate-id", Some("valid"));
        std::os::unix::fs::symlink(&valid, &alias).unwrap();
        fs::create_dir_all(&incompatible).unwrap();
        let incompatible_state = incompatible.join("state.json");
        fs::write(&incompatible_state, b"{}").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&incompatible, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(incompatible_state, fs::Permissions::from_mode(0o600)).unwrap();

        let discovery = discover_from_with_diagnostics_mode(
            vec![
                (valid.clone(), Some("duplicate-id".into())),
                (alias.clone(), Some("duplicate-id".into())),
                (incompatible.clone(), Some("duplicate-id".into())),
            ],
            Vec::new(),
            true,
        )
        .unwrap();
        let message = format!(
            "{:#}",
            resolve_discovery("duplicate-id", &discovery).unwrap_err()
        );
        assert!(
            message.contains("ambiguous registered instance id"),
            "{message}"
        );
        assert!(message.contains(&valid.display().to_string()), "{message}");
        assert!(message.contains(&alias.display().to_string()), "{message}");
        assert!(
            message.contains(&incompatible.display().to_string()),
            "{message}"
        );
    }

    #[test]
    fn inaccessible_state_file_does_not_block_unrelated_exact_id() {
        let temp = tempfile::tempdir().unwrap();
        let valid = temp.path().join("valid");
        let bad = temp.path().join("bad");
        write_registered_state(&valid, "wanted-id", Some("wanted"));
        fs::create_dir_all(&bad).unwrap();
        let target = temp.path().join("outside-state.json");
        fs::write(&target, b"{}").unwrap();
        std::os::unix::fs::symlink(&target, bad.join("state.json")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&bad, fs::Permissions::from_mode(0o700)).unwrap();

        let discovery = discover_from_with_diagnostics_mode(
            vec![
                (bad.clone(), Some("bad-id".into())),
                (valid.clone(), Some("wanted-id".into())),
            ],
            Vec::new(),
            true,
        )
        .unwrap();
        assert_eq!(
            resolve_discovery("wanted-id", &discovery).unwrap(),
            ResolvedInstance {
                dir: valid,
                instance_id: "wanted-id".into(),
            }
        );
        assert!(discovery.stale_registered.contains(&(bad, "bad-id".into())));
    }

    #[test]
    fn selector_rejects_structurally_unrelated_process_state() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("unrelated");
        fs::create_dir_all(&dir).unwrap();
        let mut value = state(Some(7), Some("bad-process"));
        value.instance_id = Some("id-unrelated".into());
        value.process_identity = Some(crate::process_identity::ProcessIdentity {
            pid: 8,
            start_time: 1,
            exe: "/browser".into(),
            user_data_dir: None,
            argv: Vec::new(),
        });
        fs::write(dir.join("state.json"), serde_json::to_vec(&value).unwrap()).unwrap();
        let discovery = discover_from_with_diagnostics(
            vec![(dir.clone(), Some("id-unrelated".into()))],
            Vec::new(),
        )
        .unwrap();

        let message = format!(
            "{:#}",
            resolve_discovery("id-unrelated", &discovery).unwrap_err()
        );
        assert!(message.contains("unrelated process state"), "{message}");
        assert!(message.contains(&dir.display().to_string()), "{message}");
    }

    #[test]
    fn selector_rejects_reused_or_unrelated_live_process() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("unrelated-live");
        fs::create_dir_all(&dir).unwrap();
        let pid = std::process::id();
        let mut value = state(Some(pid), Some("unrelated-live"));
        value.instance_id = Some("id-unrelated-live".into());
        value.process_identity = Some(crate::process_identity::ProcessIdentity {
            pid,
            start_time: 0,
            exe: "/definitely/not/this/test-binary".into(),
            user_data_dir: None,
            argv: Vec::new(),
        });
        fs::write(dir.join("state.json"), serde_json::to_vec(&value).unwrap()).unwrap();
        let discovery = discover_from_with_diagnostics(
            vec![(dir.clone(), Some("id-unrelated-live".into()))],
            Vec::new(),
        )
        .unwrap();

        let message = format!(
            "{:#}",
            resolve_discovery("id-unrelated-live", &discovery).unwrap_err()
        );
        assert!(message.contains("reused or unrelated process"), "{message}");
        assert!(message.contains(&dir.display().to_string()), "{message}");
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
                    |_| CleanupProbe::Conclusive(Liveness::Dead),
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
        assert!(cleanup_thread.join().unwrap().cleaned_is_empty());
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
        let report = cleanup_instances_with_hook(
            discovered,
            false,
            |_| CleanupProbe::Conclusive(Liveness::Dead),
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
        assert!(report.cleaned_is_empty());
        assert_eq!(
            report.entries[0].decision,
            CleanupDecision::PreservedDeadRecheck
        );
        assert!(temp.path().join("state.json").exists());
    }

    #[test]
    fn cleanup_preserves_expired_probe_without_all() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("state.json"),
            serde_json::to_vec(&state(None, Some("expired"))).unwrap(),
        )
        .unwrap();
        let instances = discover_from(vec![temp.path().to_path_buf()]).unwrap();
        let report = cleanup_instances_with_deadline(
            instances,
            false,
            |_| CleanupProbe::Inconclusive(CleanupProbeFailure::Expired),
            None,
        )
        .unwrap();
        assert!(report.cleaned_is_empty());
        assert_eq!(
            report.entries[0].decision,
            CleanupDecision::PreservedInconclusive(CleanupProbeFailure::Expired)
        );
        assert!(temp.path().join("state.json").exists());
    }

    #[test]
    fn real_expired_attached_probe_is_inconclusive_not_dead() {
        let probe = cleanup_probe_liveness_until(
            &state(None, Some("attached")),
            Deadline::at(std::time::Instant::now() - Duration::from_millis(1)),
        );
        assert_eq!(
            probe,
            CleanupProbe::Inconclusive(CleanupProbeFailure::Expired)
        );
    }

    #[test]
    fn cleanup_report_structured_rows_distinguish_dead_from_inconclusive() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("state.json"),
            serde_json::to_vec(&state(None, Some("distinguish"))).unwrap(),
        )
        .unwrap();
        let instances = discover_from(vec![temp.path().to_path_buf()]).unwrap();
        let report = cleanup_instances_with_deadline(
            instances,
            false,
            |_| CleanupProbe::Inconclusive(CleanupProbeFailure::Unavailable),
            None,
        )
        .unwrap();
        let rows = report.structured_rows();
        assert_eq!(rows[0]["action"], "preserved_inconclusive_unavailable");
        assert_ne!(rows[0]["action"], "preserved_dead");
        assert!(rows[0]["reason"].as_str().unwrap().contains("unavailable"));
    }

    #[test]
    fn cleanup_earlier_budget_consumption_is_inconclusive_not_dead() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("state.json"),
            serde_json::to_vec(&state(None, Some("budget"))).unwrap(),
        )
        .unwrap();
        let instances = discover_from(vec![temp.path().to_path_buf()]).unwrap();
        let mut probes = 0;
        let report = cleanup_instances_with_deadline(
            instances,
            false,
            |_| {
                probes += 1;
                CleanupProbe::Inconclusive(CleanupProbeFailure::Expired)
            },
            Some(Deadline::after(Duration::from_millis(1))),
        )
        .unwrap();
        assert_eq!(probes, 1);
        assert!(report.cleaned_is_empty());
        assert_eq!(
            report.entries[0].decision,
            CleanupDecision::PreservedInconclusive(CleanupProbeFailure::Expired)
        );
        assert!(temp.path().join("state.json").exists());
    }

    #[test]
    fn real_earlier_budget_consumption_preserves_later_attached_candidate() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(
            first.join("state.json"),
            serde_json::to_vec(&state(Some(1), Some("first"))).unwrap(),
        )
        .unwrap();
        fs::write(
            second.join("state.json"),
            serde_json::to_vec(&state(None, Some("second"))).unwrap(),
        )
        .unwrap();
        let instances = discover_from(vec![first, second.clone()]).unwrap();
        let deadline = Deadline::after(Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(2));
        let report = cleanup_instances_with_deadline(
            instances,
            false,
            |state| cleanup_probe_liveness_until(state, deadline),
            Some(deadline),
        )
        .unwrap();
        assert!(report.cleaned_is_empty());
        assert!(second.join("state.json").exists());
        assert!(report.entries.iter().any(|entry| matches!(
            entry.decision,
            CleanupDecision::PreservedInconclusive(CleanupProbeFailure::Expired)
        )));
    }

    #[test]
    fn cleanup_locked_timeout_recheck_cannot_turn_expiration_into_dead() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("state.json"),
            serde_json::to_vec(&state(None, Some("locked"))).unwrap(),
        )
        .unwrap();
        let instances = discover_from(vec![temp.path().to_path_buf()]).unwrap();
        let mut probes = 0;
        let report = cleanup_instances_with_deadline(
            instances,
            false,
            |_| {
                probes += 1;
                if probes == 1 {
                    CleanupProbe::Conclusive(Liveness::Dead)
                } else {
                    CleanupProbe::Inconclusive(CleanupProbeFailure::Timeout)
                }
            },
            None,
        )
        .unwrap();
        assert_eq!(probes, 2);
        assert!(report.cleaned_is_empty());
        assert_eq!(
            report.entries[0].decision,
            CleanupDecision::PreservedInconclusive(CleanupProbeFailure::Timeout)
        );
        assert!(temp.path().join("state.json").exists());
    }

    #[test]
    fn cleanup_preserves_live_without_all() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("state.json"),
            serde_json::to_vec(&state(Some(2), Some("live"))).unwrap(),
        )
        .unwrap();
        let instances = discover_from(vec![temp.path().to_path_buf()]).unwrap();
        let report = cleanup_instances_with_deadline(
            instances,
            false,
            |_| CleanupProbe::Conclusive(Liveness::Alive),
            None,
        )
        .unwrap();
        assert!(report.cleaned_is_empty());
        assert_eq!(
            report.entries[0].decision,
            CleanupDecision::PreservedLive(Liveness::Alive)
        );
        assert!(temp.path().join("state.json").exists());
    }

    #[test]
    fn cleanup_all_overrides_inconclusive_attached_probe() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("state.json"),
            serde_json::to_vec(&state(None, Some("override"))).unwrap(),
        )
        .unwrap();
        let instances = discover_from(vec![temp.path().to_path_buf()]).unwrap();
        let report = cleanup_instances_with_deadline(
            instances,
            true,
            |_| CleanupProbe::Inconclusive(CleanupProbeFailure::Unavailable),
            None,
        )
        .unwrap();
        assert_eq!(report.cleaned_len(), 1);
        assert!(!temp.path().join("state.json").exists());
    }
}
