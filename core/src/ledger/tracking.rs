//! Durable, explicitly mapped PDCA records. Transcript data is never modified.
use super::{CostTotals, Ledger, Session};
use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Intervention {
    pub id: String,
    pub candidate_id: String,
    pub project: String,
    pub work_type: Option<String>,
    pub description: String,
    pub artifact: String,
    pub effective_ms: i64,
    pub status: String,
    pub quality_criteria: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskOutcome {
    pub task_id: String,
    pub session_ids: Vec<String>,
    pub passed: bool,
    pub quality_notes: String,
    pub model: String,
    pub harness_version: String,
    pub project_version: String,
    pub cohort: String,
}

fn safe_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id.as_bytes()[0].is_ascii_alphanumeric()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        bail!("record ID must be 1..128 ASCII letters, digits, dots, underscores or hyphens, starting with a letter or digit");
    }
    Ok(())
}

fn nonempty(value: &str, field: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{field} must not be empty");
    }
    Ok(())
}

/// Refuse symlink components rather than letting state writes follow them into sources.
pub(crate) fn state_subdir(root: &Path, name: &str) -> Result<PathBuf> {
    let absolute = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()?.join(root)
    };
    let mut checked = PathBuf::new();
    for part in absolute.components() {
        if matches!(part, std::path::Component::ParentDir) {
            bail!("state directory must not contain parent components");
        }
        checked.push(part);
        match fs::symlink_metadata(&checked) {
            Ok(meta) if meta.file_type().is_symlink() => {
                bail!("state directory must not traverse symlinks")
            }
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    if let Some(home) = dirs::home_dir() {
        for source in [
            ".claude/projects",
            ".codex/sessions",
            ".omp/agent/sessions",
            ".qwen/tmp",
        ] {
            if absolute.starts_with(home.join(source)) {
                bail!("state directory cannot be inside a transcript source directory");
            }
        }
    }
    let directory = absolute.join(name);
    if fs::symlink_metadata(&directory).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("state subdirectory must not be a symlink");
    }
    fs::create_dir_all(&directory).context("create audit state directory")?;
    Ok(directory)
}

static TEMP_ID: AtomicU64 = AtomicU64::new(0);

pub(crate) fn unique_suffix() -> String {
    format!(
        "{}-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
        TEMP_ID.fetch_add(1, Ordering::Relaxed)
    )
}

pub(crate) fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    if bytes.len() > 4 * 1024 * 1024 {
        bail!("audit state record exceeds the 4 MiB bound");
    }
    let parent = path
        .parent()
        .context("state file needs a parent directory")?;
    if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("state file must not be a symlink");
    }
    let temporary = parent.join(format!(".write-{}.tmp", unique_suffix()));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .context("create atomic state file")?;
        file.write_all(&bytes).context("write atomic state file")?;
        file.sync_all().context("sync atomic state file")?;
        drop(file); // Windows cannot replace a file while this write handle is open.
        fs::rename(&temporary, path).context("replace atomic state file")?;
        #[cfg(unix)]
        File::open(parent)?
            .sync_all()
            .context("sync state directory")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

struct StateLock(PathBuf);

impl StateLock {
    fn acquire(directory: &Path) -> Result<Self> {
        let path = directory.join(".writer.lock");
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .context("state is locked; an interrupted writer requires explicit lock recovery")?;
        let lock = Self(path);
        writeln!(file, "{}", std::process::id())?;
        Ok(lock)
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn read_record<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > 4 * 1024 * 1024 {
        bail!("state record must be a regular JSON file of at most 4 MiB");
    }
    serde_json::from_reader(File::open(path)?).context("invalid audit state JSON")
}

fn list_records<T: DeserializeOwned>(directory: &Path) -> Result<Vec<T>> {
    if !directory.exists() {
        return Ok(Vec::new());
    }
    if fs::symlink_metadata(directory)?.file_type().is_symlink() {
        bail!("state directory must not be a symlink");
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "json") {
            paths.push(path);
        }
    }
    paths.sort();
    paths.iter().map(|path| read_record(path)).collect()
}

pub fn list_interventions(state_dir: &Path) -> Result<Vec<Intervention>> {
    let records: Vec<Intervention> = list_records(&state_dir.join("interventions"))?;
    let mut ids = BTreeSet::new();
    for record in &records {
        validate_intervention(record)?;
        if !ids.insert(&record.id) {
            bail!("duplicate intervention IDs in state");
        }
    }
    Ok(records)
}

fn validate_intervention(record: &Intervention) -> Result<()> {
    safe_id(&record.id)?;
    for (value, field) in [
        (&record.candidate_id, "candidate_id"),
        (&record.project, "project"),
        (&record.description, "description"),
        (&record.quality_criteria, "quality_criteria"),
    ] {
        nonempty(value, field)?;
    }
    if let Some(work_type) = &record.work_type {
        nonempty(work_type, "work_type")?;
    }
    if !matches!(
        record.status.as_str(),
        "candidate"
            | "confirmed"
            | "implemented"
            | "pending_validation"
            | "effective"
            | "ineffective"
            | "insufficient_evidence"
    ) {
        bail!("unknown intervention status");
    }
    if !matches!(record.status.as_str(), "candidate" | "confirmed") {
        nonempty(&record.artifact, "implementation artifact")?;
        if record.effective_ms <= 0 {
            bail!("implemented interventions require a positive effective_ms");
        }
    }
    Ok(())
}

fn transition_allowed(old: &str, new: &str) -> bool {
    old == new
        || matches!(
            (old, new),
            ("candidate", "confirmed")
                | ("confirmed", "implemented")
                | ("implemented", "pending_validation")
                | (
                    "pending_validation",
                    "effective" | "ineffective" | "insufficient_evidence"
                )
                | (
                    "effective" | "ineffective" | "insufficient_evidence",
                    "pending_validation"
                )
        )
}

pub fn save_intervention(state_dir: &Path, record: &Intervention) -> Result<()> {
    validate_intervention(record)?;
    let directory = state_subdir(state_dir, "interventions")?;
    let _lock = StateLock::acquire(&directory)?;
    let path = directory.join(format!("{}.json", record.id));
    if path.exists() {
        let previous: Intervention = read_record(&path)?;
        validate_intervention(&previous)?;
        if previous.id != record.id
            || previous.candidate_id != record.candidate_id
            || previous.project != record.project
            || previous.work_type != record.work_type
        {
            bail!("intervention identity and scope are immutable; create a new intervention");
        }
        if !transition_allowed(&previous.status, &record.status) {
            bail!(
                "invalid intervention transition: {} -> {}",
                previous.status,
                record.status
            );
        }
        if !matches!(previous.status.as_str(), "candidate" | "confirmed")
            && (previous.effective_ms != record.effective_ms
                || previous.quality_criteria != record.quality_criteria
                || previous.artifact != record.artifact)
        {
            bail!("implemented intervention timing, artifact and quality criteria are immutable");
        }
    } else if record.status != "candidate" {
        bail!("new intervention must start in candidate status");
    }
    atomic_json(&path, record)
}

fn validate_outcome(record: &TaskOutcome) -> Result<()> {
    safe_id(&record.task_id)?;
    if record.session_ids.is_empty() || record.session_ids.iter().any(|id| id.trim().is_empty()) {
        bail!("task outcome requires explicit nonempty session_ids");
    }
    if record.session_ids.iter().collect::<BTreeSet<_>>().len() != record.session_ids.len() {
        bail!("task outcome contains duplicate session IDs");
    }
    if !matches!(record.cohort.as_str(), "before" | "after") {
        bail!("cohort must be before or after");
    }
    for (value, field) in [
        (&record.quality_notes, "quality_notes"),
        (&record.model, "model"),
        (&record.harness_version, "harness_version"),
        (&record.project_version, "project_version"),
    ] {
        nonempty(value, field)?;
    }
    Ok(())
}

pub fn save_outcome(state_dir: &Path, record: &TaskOutcome) -> Result<()> {
    validate_outcome(record)?;
    let directory = state_subdir(state_dir, "outcomes")?;
    let _lock = StateLock::acquire(&directory)?;
    let previous: Vec<TaskOutcome> = list_records(&directory)?;
    for other in &previous {
        validate_outcome(other)?;
        if other.task_id == record.task_id {
            if other.cohort != record.cohort
                || other
                    .session_ids
                    .iter()
                    .any(|id| !record.session_ids.contains(id))
            {
                bail!("an outcome cannot change cohort or remove previously mapped attempts");
            }
        } else if other
            .session_ids
            .iter()
            .any(|id| record.session_ids.contains(id))
        {
            bail!("a session cannot be explicitly mapped to more than one task");
        }
    }
    atomic_json(&directory.join(format!("{}.json", record.task_id)), record)
}

#[derive(Serialize)]
struct TaskSample {
    task_id: String,
    cohort: String,
    passed: bool,
    quality_notes: String,
    model: String,
    harness_version: String,
    project_version: String,
    work_types: Vec<String>,
    observed_models: Vec<String>,
    session_ids: Vec<String>,
    failed_requests: usize,
    failed_tools: usize,
    costs: CostTotals,
    known_cost_usd: Option<f64>,
    exclusions: Vec<String>,
    matched: bool,
    #[serde(skip)]
    matching_key: String,
}

fn expand_sessions(outcome: &TaskOutcome, sessions: &[Session]) -> BTreeSet<String> {
    let mut ids: BTreeSet<String> = outcome.session_ids.iter().cloned().collect();
    ids.extend(
        sessions
            .iter()
            .filter(|s| s.task_id.as_deref() == Some(outcome.task_id.as_str()))
            .map(|s| s.id.clone()),
    );
    loop {
        let before = ids.len();
        for session in sessions {
            if session
                .parent_id
                .as_ref()
                .is_some_and(|id| ids.contains(id))
            {
                ids.insert(session.id.clone());
            }
        }
        if ids.len() == before {
            return ids;
        }
    }
}

fn distribution(values: impl Iterator<Item = f64>) -> Value {
    let mut values: Vec<f64> = values.collect();
    values.sort_by(f64::total_cmp);
    if values.is_empty() {
        return json!({"samples": [], "count": 0, "min": null, "median": null, "p90": null, "max": null});
    }
    let count = values.len();
    let median = if count % 2 == 0 {
        (values[count / 2 - 1] + values[count / 2]) / 2.0
    } else {
        values[count / 2]
    };
    let p90 = values[(count * 9).div_ceil(10).saturating_sub(1)];
    json!({"count": count, "min": values[0], "median": median, "p90": p90, "max": values[count - 1], "samples": values})
}

fn cohort_summary(samples: &[&TaskSample]) -> Value {
    let mut totals = CostTotals::default();
    for sample in samples {
        totals.add(&sample.costs);
    }
    let completed = samples.iter().filter(|task| task.passed).count();
    let complete_cost =
        !samples.is_empty() && samples.iter().all(|task| task.known_cost_usd.is_some());
    let per_completed = if complete_cost && completed > 0 {
        Some((totals.reported_usd + totals.estimated_usd) / completed as f64)
    } else {
        None
    };
    json!({
        "tasks": samples.len(), "accepted_quality_tasks": completed,
        "failed_quality_tasks": samples.len() - completed,
        "sessions_including_children": samples.iter().map(|task| task.session_ids.len()).sum::<usize>(),
        "failed_requests": samples.iter().map(|task| task.failed_requests).sum::<usize>(),
        "failed_tools": samples.iter().map(|task| task.failed_tools).sum::<usize>(),
        "costs": totals, "cost_coverage_complete": complete_cost,
        "all_attempt_cost_per_accepted_task_usd": per_completed,
        "per_task_known_mixed_usd": distribution(samples.iter().filter_map(|task| task.known_cost_usd)),
        "unknown_cost_tasks": samples.iter().filter(|task| task.known_cost_usd.is_none()).count(),
        "models": samples.iter().map(|task| &task.model).collect::<BTreeSet<_>>(),
        "harness_versions": samples.iter().map(|task| &task.harness_version).collect::<BTreeSet<_>>(),
        "project_versions": samples.iter().map(|task| &task.project_version).collect::<BTreeSet<_>>(),
    })
}

fn coverage_failure(code: &str) -> bool {
    !matches!(
        code,
        "source_coverage"
            | "duplicate_source_content"
            | "inherited_project_missing_cwd"
            | "invalid_parsed_cache"
            | "parsed_cache_write_failed"
            | "cumulative_reset"
            | "missing_request_identity"
            | "missing_project"
            | "invalid_tool_call"
            | "missing_tool_arguments"
            | "missing_tool_identity"
            | "conflicting_tool_identity"
            | "orphan_tool_result"
            | "conflicting_tool_result"
            | "non_monotonic_tool_time"
    )
}

/// Descriptive cohorts, never a causal savings estimate or automatic PDCA transition.
pub fn compare(ledger: &Ledger, state_dir: &Path, intervention_id: &str) -> Result<Value> {
    safe_id(intervention_id)?;
    let intervention: Intervention = read_record(
        &state_dir
            .join("interventions")
            .join(format!("{intervention_id}.json")),
    )?;
    validate_intervention(&intervention)?;
    if intervention.id != intervention_id {
        bail!("intervention file identity mismatch");
    }
    let outcomes: Vec<TaskOutcome> = list_records(&state_dir.join("outcomes"))?;
    let mut task_ids = BTreeSet::new();
    for outcome in &outcomes {
        validate_outcome(outcome)?;
        if !task_ids.insert(&outcome.task_id) {
            bail!("duplicate task outcomes in state");
        }
    }
    let sessions: BTreeMap<_, _> = ledger
        .sessions
        .iter()
        .map(|session| (session.id.as_str(), session))
        .collect();
    let expanded: Vec<_> = outcomes
        .iter()
        .map(|outcome| expand_sessions(outcome, &ledger.sessions))
        .collect();
    let mut owners: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (outcome, ids) in outcomes.iter().zip(&expanded) {
        for id in ids {
            owners.entry(id).or_default().insert(&outcome.task_id);
        }
    }
    let global_coverage_blockers: Vec<_> = ledger
        .diagnostics
        .iter()
        .filter(|diagnostic| {
            matches!(
                diagnostic.code.as_str(),
                "source_traversal_error"
                    | "missing_home"
                    | "source_read_error"
                    | "source_parse_error"
                    | "unsupported_schema"
                    | "provider_mismatch"
                    | "source_symlink_skipped"
                    | "duplicate_source_metadata_conflict"
            )
        })
        .map(|diagnostic| diagnostic.code.as_str())
        .collect();
    let mut samples = Vec::new();
    let mut confounders = BTreeSet::from([
        "Observational comparison only: task difficulty, selection and concurrent changes are not controlled.".to_string(),
        "Quality acceptance is explicitly imported, not inferred from assistant prose; criteria are not independently verified.".to_string(),
        "Reported and estimated work costs remain separate; mixed amounts are not an invoice or causal savings.".to_string(),
    ]);
    if matches!(intervention.status.as_str(), "candidate" | "confirmed")
        || intervention.effective_ms <= 0
    {
        confounders.insert("Intervention has not established an implementation boundary.".into());
    }
    for (outcome, ids) in outcomes.iter().zip(&expanded) {
        let mapped: Vec<_> = ids
            .iter()
            .filter_map(|id| sessions.get(id.as_str()).copied())
            .collect();
        let roots: Vec<_> = mapped
            .iter()
            .copied()
            .filter(|session| {
                !session
                    .parent_id
                    .as_ref()
                    .is_some_and(|parent| ids.contains(parent))
            })
            .collect();
        let mut exclusions = Vec::new();
        if mapped.len() != ids.len() {
            exclusions.push("missing_mapped_sessions".to_string());
        }
        if roots.is_empty() {
            exclusions.push("missing_root_or_parent_cycle".to_string());
        }
        if roots
            .iter()
            .any(|session| session.project != intervention.project)
        {
            exclusions.push("project_mismatch".to_string());
        }
        if let Some(work_type) = &intervention.work_type {
            if roots
                .iter()
                .any(|session| session.work_type.as_ref() != Some(work_type))
            {
                exclusions.push("work_type_mismatch_or_unknown".to_string());
            }
        }
        if ids
            .iter()
            .any(|id| owners.get(id.as_str()).is_some_and(|set| set.len() > 1))
        {
            exclusions.push("overlapping_task_session_ownership".to_string());
        }
        if mapped.iter().any(|session| {
            session
                .task_id
                .as_ref()
                .is_some_and(|task| task != &outcome.task_id)
        }) {
            exclusions.push("conflicting_manifest_task_id".to_string());
        }
        let work_types: Vec<_> = roots
            .iter()
            .map(|session| session.work_type.clone().unwrap_or_default())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if work_types.iter().any(String::is_empty) {
            exclusions.push("unknown_work_type".to_string());
        }
        let mut costs = CostTotals::default();
        let mut models = BTreeSet::new();
        let mut observed_sessions = BTreeSet::new();
        let mut time_unknown = false;
        let mut time_mismatch = false;
        for observation in &ledger.observations {
            if !ids.contains(&observation.session_id) {
                continue;
            }
            observed_sessions.insert(observation.session_id.as_str());
            costs.add_observation(observation);
            models.insert(observation.model.clone());
            match observation.ts_ms {
                None => time_unknown = true,
                Some(ts)
                    if (outcome.cohort == "before" && ts >= intervention.effective_ms)
                        || (outcome.cohort == "after" && ts < intervention.effective_ms) =>
                {
                    time_mismatch = true
                }
                Some(_) => (),
            }
        }
        let requests: Vec<_> = ledger
            .requests
            .iter()
            .filter(|request| ids.contains(&request.session_id))
            .collect();
        costs.requests = requests.len();
        let unobserved_request = requests.iter().any(|request| {
            !ledger.observations.iter().any(|observation| {
                observation.session_id == request.session_id
                    && (observation.request_id.as_deref() == Some(request.id.as_str())
                        || (observation.basis == "cumulative_delta"
                            && observation.request_id.is_none()))
            })
        });
        let source_incomplete = ledger.diagnostics.iter().any(|diagnostic| {
            coverage_failure(&diagnostic.code)
                && diagnostic.source.as_ref().is_some_and(|source| {
                    mapped
                        .iter()
                        .any(|session| session.source.source_id == source.source_id)
                })
        });
        let missing_costs = costs.observations == 0
            || observed_sessions.len() != mapped.len()
            || unobserved_request
            || costs.unknown_observations > 0
            || source_incomplete
            || !global_coverage_blockers.is_empty();
        if time_unknown {
            exclusions.push("unknown_observation_time".to_string());
        }
        if time_mismatch {
            exclusions.push("cohort_crosses_effective_boundary".to_string());
        }
        if missing_costs {
            exclusions.push("incomplete_cost_coverage".to_string());
        }
        if source_incomplete {
            exclusions.push("source_coverage_diagnostics".to_string());
        }
        let known_cost_usd = if missing_costs {
            None
        } else {
            Some(costs.reported_usd + costs.estimated_usd)
        };
        let observed_models: Vec<_> = models.into_iter().collect();
        // Harness version is disclosed separately because it may be the intervention.
        let matching_key = serde_json::to_string(&(
            &outcome.model,
            &outcome.project_version,
            &work_types,
            &observed_models,
        ))?;
        samples.push(TaskSample {
            task_id: outcome.task_id.clone(),
            cohort: outcome.cohort.clone(),
            passed: outcome.passed,
            quality_notes: outcome.quality_notes.clone(),
            model: outcome.model.clone(),
            harness_version: outcome.harness_version.clone(),
            project_version: outcome.project_version.clone(),
            work_types,
            observed_models,
            session_ids: ids.iter().cloned().collect(),
            failed_requests: requests
                .iter()
                .filter(|request| matches!(request.status.as_str(), "failure" | "failed" | "error"))
                .count(),
            failed_tools: ledger
                .tools
                .iter()
                .filter(|tool| ids.contains(&tool.session_id) && tool.status == "failure")
                .count(),
            costs,
            known_cost_usd,
            exclusions,
            matched: false,
            matching_key,
        });
    }
    let mut strata: BTreeMap<String, (bool, bool)> = BTreeMap::new();
    for sample in &samples {
        if !sample.exclusions.is_empty() {
            continue;
        }
        let entry = strata.entry(sample.matching_key.clone()).or_default();
        if sample.cohort == "before" {
            entry.0 = true;
        } else {
            entry.1 = true;
        }
    }
    for sample in &mut samples {
        sample.matched =
            sample.exclusions.is_empty() && strata.get(&sample.matching_key) == Some(&(true, true));
        if sample.exclusions.is_empty() && !sample.matched {
            sample
                .exclusions
                .push("no_comparable_opposite_cohort".to_string());
        }
    }
    let mut comparisons = Vec::new();
    let mut enough = false;
    for (key, (before_exists, after_exists)) in &strata {
        if !before_exists || !after_exists {
            continue;
        }
        let before: Vec<_> = samples
            .iter()
            .filter(|sample| {
                sample.matched && sample.cohort == "before" && sample.matching_key == *key
            })
            .collect();
        let after: Vec<_> = samples
            .iter()
            .filter(|sample| {
                sample.matched && sample.cohort == "after" && sample.matching_key == *key
            })
            .collect();
        let before_report = cohort_summary(&before);
        let after_report = cohort_summary(&after);
        let sufficient = before.iter().filter(|sample| sample.passed).count() >= 3
            && after.iter().filter(|sample| sample.passed).count() >= 3;
        let difference = before_report["all_attempt_cost_per_accepted_task_usd"]
            .as_f64()
            .zip(after_report["all_attempt_cost_per_accepted_task_usd"].as_f64())
            .map(|(before, after)| after - before);
        enough |= sufficient && difference.is_some();
        let before_harness: BTreeSet<_> = before
            .iter()
            .map(|sample| &sample.harness_version)
            .collect();
        let after_harness: BTreeSet<_> =
            after.iter().map(|sample| &sample.harness_version).collect();
        if before_harness != after_harness {
            confounders.insert("Harness versions differ across cohorts; this may be the intervention or a concurrent change, not isolated causality.".into());
        }
        comparisons.push(json!({"matching_dimensions": serde_json::from_str::<Value>(key)?, "before": before_report, "after": after_report, "status": if sufficient { "descriptive_comparison" } else { "insufficient_evidence" }, "observed_after_minus_before_usd_per_accepted_task": difference}));
    }
    if samples.iter().any(|sample| !sample.exclusions.is_empty()) {
        confounders.insert("Some tasks are excluded from matched comparisons; their failures and known costs remain visible in samples and scoped cohorts.".into());
    }
    if strata.len() > 1 {
        confounders.insert("Model, observed model mix, work type or project version varies; compare within strata, not an unadjusted pooled savings figure.".into());
    }
    if !ledger.diagnostics.is_empty() {
        confounders.insert("The source ledger contains coverage diagnostics; inspect coverage before interpreting totals.".into());
    }
    let in_scope = |sample: &&TaskSample| {
        !sample.exclusions.iter().any(|reason| {
            reason == "project_mismatch"
                || reason == "work_type_mismatch_or_unknown"
                || reason == "overlapping_task_session_ownership"
        })
    };
    let before: Vec<_> = samples
        .iter()
        .filter(in_scope)
        .filter(|sample| sample.cohort == "before")
        .collect();
    let after: Vec<_> = samples
        .iter()
        .filter(in_scope)
        .filter(|sample| sample.cohort == "after")
        .collect();
    let unmapped: BTreeSet<_> = ledger
        .sessions
        .iter()
        .filter(|session| {
            session.project == intervention.project
                && !owners.contains_key(session.id.as_str())
                && intervention.work_type.as_ref().map_or(true, |work_type| {
                    session.work_type.as_ref() == Some(work_type)
                })
        })
        .map(|session| session.id.as_str())
        .collect();
    let mut unmapped_costs = CostTotals::default();
    for observation in &ledger.observations {
        if unmapped.contains(observation.session_id.as_str()) {
            unmapped_costs.add_observation(observation);
        }
    }
    unmapped_costs.requests = ledger
        .requests
        .iter()
        .filter(|request| unmapped.contains(request.session_id.as_str()))
        .count();
    if !unmapped.is_empty() {
        confounders.insert("Unmapped sessions exist in the target scope; they are not silently assigned to tasks or cohorts.".into());
    }
    let status = if enough
        && !matches!(intervention.status.as_str(), "candidate" | "confirmed")
        && intervention.effective_ms > 0
    {
        "descriptive_comparison"
    } else {
        "insufficient_evidence"
    };
    Ok(json!({
        "intervention": intervention, "status": status, "causal_savings_usd": null,
        "minimum_accepted_tasks_per_cohort_per_stratum": 3,
        "quality_policy": "passed=true and explicit quality_notes against the intervention criteria; failed tasks remain in the cost numerator",
        "cost_policy": "All mapped sessions, manifest-linked attempts and recursive children; unknown costs are never zero. Bounded source loading is not suitable for cohort comparison.",
        "matching_policy": "Exact declared model, observed model mix, project version and work type within the intervention project; harness changes are disclosed separately.",
        "scoped_cohorts": {"before": cohort_summary(&before), "after": cohort_summary(&after)},
        "matched_strata": comparisons, "samples": samples, "confounders": confounders,
        "unmapped_scope": {"sessions": unmapped.len(), "known_costs": unmapped_costs},
        "coverage_blockers": global_coverage_blockers,
        "coverage": ledger.diagnostics,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::Usage;
    use crate::ledger::{SourceRef, UsageObservation};
    use crate::providers::Agent;

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            Self(
                std::env::temp_dir()
                    .canonicalize()
                    .unwrap()
                    .join(format!("auditui-tracking-{}", unique_suffix())),
            )
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn intervention() -> Intervention {
        Intervention {
            id: "change-a".into(),
            candidate_id: "candidate-a".into(),
            project: "project".into(),
            work_type: Some("review".into()),
            description: "Use the reviewed workflow".into(),
            artifact: "workflow-v1".into(),
            effective_ms: 1000,
            status: "candidate".into(),
            quality_criteria: "Independent review accepted".into(),
        }
    }
    fn outcome(id: &str, cohort: &str, passed: bool) -> TaskOutcome {
        TaskOutcome {
            task_id: id.into(),
            session_ids: vec![id.into()],
            passed,
            quality_notes: "Reviewer decision recorded".into(),
            model: "model".into(),
            harness_version: "h1".into(),
            project_version: "p1".into(),
            cohort: cohort.into(),
        }
    }
    fn append_session(
        ledger: &mut Ledger,
        id: &str,
        parent: Option<&str>,
        cohort: &str,
        amount: Option<f64>,
    ) {
        let source = SourceRef {
            source_id: id.into(),
            version: "v".into(),
            path: PathBuf::from("fixture.jsonl"),
            line: 1,
            record_id: None,
        };
        ledger.sessions.push(Session {
            id: id.into(),
            provider: Agent::Claude,
            project: "project".into(),
            parent_id: parent.map(str::to_string),
            task_id: None,
            work_type: Some("review".into()),
            source: source.clone(),
        });
        ledger.observations.push(UsageObservation {
            id: id.into(),
            session_id: id.into(),
            request_id: None,
            ts_ms: Some(if cohort == "before" { 900 } else { 1100 }),
            model: "model".into(),
            basis: "unattributed".into(),
            usage: Usage::default(),
            cache_counters_complete: false,
            reported_usd: amount,
            estimated_usd: None,
            pricing_version: None,
            source,
        });
    }
    #[test]
    fn transitions_and_path_escape_are_refused_without_replacing_record() {
        let state = Temp::new();
        let mut record = intervention();
        record.id = "../escape".into();
        assert!(save_intervention(&state.0, &record).is_err());
        record = intervention();
        save_intervention(&state.0, &record).unwrap();
        record.status = "effective".into();
        assert!(save_intervention(&state.0, &record).is_err());
        assert_eq!(list_interventions(&state.0).unwrap()[0].status, "candidate");
        for status in [
            "confirmed",
            "implemented",
            "pending_validation",
            "insufficient_evidence",
            "pending_validation",
        ] {
            record.status = status.into();
            save_intervention(&state.0, &record).unwrap();
        }
        record.effective_ms += 1;
        assert!(save_intervention(&state.0, &record).is_err());
    }
    #[test]
    fn failed_attempt_mapping_cannot_be_removed_or_double_assigned() {
        let state = Temp::new();
        let mut task = outcome("task", "before", false);
        save_outcome(&state.0, &task).unwrap();
        task.session_ids = vec!["retry".into()];
        task.passed = true;
        assert!(save_outcome(&state.0, &task).is_err());
        task.session_ids.push("task".into());
        save_outcome(&state.0, &task).unwrap();
        let mut another = outcome("other", "before", false);
        another.session_ids = vec!["retry".into()];
        assert!(save_outcome(&state.0, &another).is_err());
    }
    #[test]
    fn cohorts_include_failed_tasks_and_recursive_children_without_claiming_savings() {
        let state = Temp::new();
        save_intervention(&state.0, &intervention()).unwrap();
        let mut ledger = Ledger::default();
        for (id, passed, amount) in [("ok", true, 2.0), ("failed", false, 3.0)] {
            append_session(&mut ledger, id, None, "before", Some(amount));
            save_outcome(&state.0, &outcome(id, "before", passed)).unwrap();
        }
        append_session(&mut ledger, "child", Some("failed"), "before", Some(4.0));
        append_session(&mut ledger, "after", None, "after", Some(1.0));
        save_outcome(&state.0, &outcome("after", "after", true)).unwrap();
        let report = compare(&ledger, &state.0, "change-a").unwrap();
        assert_eq!(
            report["scoped_cohorts"]["before"]["all_attempt_cost_per_accepted_task_usd"],
            9.0
        );
        assert_eq!(
            report["scoped_cohorts"]["before"]["failed_quality_tasks"],
            1
        );
        assert_eq!(report["status"], "insufficient_evidence");
        assert!(report["causal_savings_usd"].is_null());
    }
    #[test]
    fn unknown_cost_and_wrong_work_type_do_not_become_comparable_savings() {
        let state = Temp::new();
        save_intervention(&state.0, &intervention()).unwrap();
        let mut ledger = Ledger::default();
        append_session(&mut ledger, "before", None, "before", None);
        append_session(&mut ledger, "after", None, "after", Some(1.0));
        ledger.sessions[1].work_type = Some("unrelated".into());
        save_outcome(&state.0, &outcome("before", "before", true)).unwrap();
        save_outcome(&state.0, &outcome("after", "after", true)).unwrap();
        let report = compare(&ledger, &state.0, "change-a").unwrap();
        assert!(
            report["scoped_cohorts"]["before"]["all_attempt_cost_per_accepted_task_usd"].is_null()
        );
        assert_eq!(report["scoped_cohorts"]["after"]["tasks"], 0);
        assert_eq!(report["matched_strata"], json!([]));
    }
}
