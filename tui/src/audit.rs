//! Offline audit commands and a separate terminal surface for the request ledger.
use anyhow::{bail, Context, Result};
use auditui_core::ledger::{
    self, analysis, explain, source, tracking, Candidate, CostReport, CostTotals, Ledger, Query,
    SourceRef,
};
use auditui_core::providers::Agent;
use chrono::{DateTime, Utc};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs, Wrap};
use ratatui::Terminal;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};

const HELP: &str = "\
auditui audit <COMMAND> [OPTIONS]

COMMANDS:
  costs       Request/usage cost ledger, with coverage diagnostics
  candidates  Evidence-backed recovery, context-growth, workflow candidates
  explain     Sanitized explanation preview; network ONLY with --execute
  compare     Compare recorded before/after tasks for --intervention ID
  record      Import one Intervention JSON with --file PATH
  outcome     Import one TaskOutcome JSON with --file PATH
  evidence    Resolve one SourceRef JSON with --file PATH; refuse stale versions
  browse      Interactive Costs / Candidates / Effects terminal screen

DATA FILTERS (costs, candidates, explain, browse):
  --root PATH          Read a local source file/tree or root/manifest.json sources
  --since WINDOW       7d, 30d (default), all, or RFC3339; inclusive
  --until RFC3339      Exclusive upper observation-time boundary
  --project PROJECT   Project filter (same semantics as core source loader)
  --agent LIST        Comma-separated claude,codex,omp (default: all three)
  Compare accepts --root, --project, --agent, but always uses complete tasks.

OUTPUT / TRACKING:
  --json               Machine-readable output (not browse)
  --include-paths      Local path export for costs/candidates/compare only;
                       sensitive! Bodies and credentials remain redacted
  --state-dir PATH     Explain/compare/record/outcome/browse durable state;
                       default ~/.claude-audit/ledger-state, not the TUI cache
  --config PATH        ExplainConfig JSON (explain only)
  --execute            Explicit explanation execution; requires --config
  --intervention ID    Required for compare
  --file PATH          Required for record/outcome/evidence JSON imports
  --raw                Evidence only: explicitly display local raw source record
  -h, --help           Show this help

No transcript writes. Full-content scan; SHA256-validated parsed cache, not incremental.
Reported actual, estimated, and unknown costs are separate, not an invoice.
Candidate-related cost is NOT avoidable cost or predicted savings.
Browse: Tab/Left/Right tabs; Up/Down or j/k select; Enter/e evidence;
[/] evidence reference; R raw evidence opt-in; Esc back; PgUp/PgDn scroll;
r full rescan; q/Ctrl-C quit. No model calls or update checks from audit.
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Costs,
    Candidates,
    Explain,
    Compare,
    Record,
    Outcome,
    Evidence,
    Browse,
}

#[derive(Debug)]
struct Options {
    command: Command,
    query: Query,
    json: bool,
    include_paths: bool,
    state_dir: Option<PathBuf>,
    config: Option<PathBuf>,
    execute: bool,
    intervention: Option<String>,
    file: Option<PathBuf>,
    raw: bool,
    help: bool,
}

impl Options {
    fn parse(args: &[String], now_ms: i64) -> Result<Self> {
        let first = args
            .first()
            .map(String::as_str)
            .context("missing audit command; run `auditui audit --help`")?;
        let command = match first {
            "costs" | "--help" | "-h" => Command::Costs,
            "candidates" => Command::Candidates,
            "explain" => Command::Explain,
            "compare" => Command::Compare,
            "record" => Command::Record,
            "outcome" => Command::Outcome,
            "evidence" => Command::Evidence,
            "browse" => Command::Browse,
            other => bail!("unknown audit command: {other}; run `auditui audit --help`"),
        };
        if matches!(first, "--help" | "-h") && args.len() != 1 {
            bail!("audit --help does not accept additional arguments");
        }
        let mut options = Self {
            command,
            query: Query {
                since_ms: if command == Command::Compare {
                    None
                } else {
                    Some(now_ms.saturating_sub(30 * 86_400_000))
                },
                ..Query::default()
            },
            json: false,
            include_paths: false,
            state_dir: None,
            config: None,
            execute: false,
            intervention: None,
            file: None,
            raw: false,
            help: matches!(first, "--help" | "-h"),
        };
        let dataset = matches!(
            command,
            Command::Costs
                | Command::Candidates
                | Command::Explain
                | Command::Compare
                | Command::Browse
        );
        let mut seen = HashSet::new();
        let mut index = 1;
        while index < args.len() {
            let flag = args[index].as_str();
            let canonical = if flag == "-h" { "--help" } else { flag };
            if !seen.insert(canonical) {
                bail!("duplicate flag: {flag}");
            }
            let allowed = match canonical {
                "--root" | "--project" | "--agent" => dataset,
                "--since" | "--until" => dataset && command != Command::Compare,
                "--json" => command != Command::Browse,
                "--include-paths" => matches!(
                    command,
                    Command::Costs | Command::Candidates | Command::Compare
                ),
                "--state-dir" => matches!(
                    command,
                    Command::Explain
                        | Command::Compare
                        | Command::Record
                        | Command::Outcome
                        | Command::Browse
                ),
                "--config" | "--execute" => command == Command::Explain,
                "--intervention" => command == Command::Compare,
                "--file" => matches!(
                    command,
                    Command::Record | Command::Outcome | Command::Evidence
                ),
                "--raw" => command == Command::Evidence,
                "--help" => true,
                _ => bail!("unknown audit flag or positional argument: {flag}"),
            };
            if !allowed {
                bail!("{flag} is not supported by audit {first}");
            }
            match canonical {
                "--help" => options.help = true,
                "--json" => options.json = true,
                "--include-paths" => options.include_paths = true,
                "--execute" => options.execute = true,
                "--raw" => options.raw = true,
                _ => {
                    index += 1;
                    let value = args
                        .get(index)
                        .filter(|value| !value.is_empty() && !value.starts_with('-'))
                        .with_context(|| format!("missing value for {flag}"))?;
                    match canonical {
                        "--root" => options.query.root = Some(PathBuf::from(value)),
                        "--since" => {
                            options.query.since_ms = match value.as_str() {
                                "all" => None,
                                "7d" => Some(now_ms.saturating_sub(7 * 86_400_000)),
                                "30d" => Some(now_ms.saturating_sub(30 * 86_400_000)),
                                _ => Some(timestamp(value, flag)?),
                            }
                        }
                        "--until" => options.query.until_ms = Some(timestamp(value, flag)?),
                        "--project" => options.query.project = Some(value.clone()),
                        "--agent" => {
                            for item in value.split(',') {
                                let agent = match item {
                                    "claude" => Agent::Claude,
                                    "codex" => Agent::Codex,
                                    "omp" => Agent::Omp,
                                    _ => bail!("unknown --agent entry: {item:?}; expected claude,codex,omp"),
                                };
                                if options.query.agents.contains(&agent) {
                                    bail!("duplicate --agent entry: {item}");
                                }
                                options.query.agents.push(agent);
                            }
                        }
                        "--state-dir" => options.state_dir = Some(PathBuf::from(value)),
                        "--config" => options.config = Some(PathBuf::from(value)),
                        "--intervention" => options.intervention = Some(value.clone()),
                        "--file" => options.file = Some(PathBuf::from(value)),
                        _ => unreachable!("all valued flags are handled"),
                    }
                }
            }
            index += 1;
        }
        if let (Some(since), Some(until)) = (options.query.since_ms, options.query.until_ms) {
            if since >= until {
                bail!("--since must precede --until (until is exclusive)");
            }
        }
        if !options.help {
            if matches!(
                command,
                Command::Record | Command::Outcome | Command::Evidence
            ) && options.file.is_none()
            {
                bail!("audit {first} requires --file PATH");
            }
            if command == Command::Compare && options.intervention.is_none() {
                bail!("audit compare requires --intervention ID");
            }
            if options.execute && options.config.is_none() {
                bail!("--execute requires --config PATH; without --execute explain is an offline preview");
            }
        }
        Ok(options)
    }

    fn state_dir(&self) -> Result<PathBuf> {
        let path = self
            .state_dir
            .clone()
            .or_else(|| dirs::home_dir().map(|home| home.join(".claude-audit/ledger-state")))
            .context("cannot locate home directory; provide --state-dir")?;
        let resolved = prospective_path(&path)?;
        if let Some(root) = &self.query.root {
            let root = fs::canonicalize(root)
                .context("cannot resolve source root for state-directory safety")?;
            let boundary = if root.is_file() {
                root.parent().context("source file has no parent")?
            } else {
                root.as_path()
            };
            if path_within(&resolved, boundary) {
                bail!("--state-dir must be outside the source root (or source file's parent directory)");
            }
        }
        if let Some(home) = dirs::home_dir() {
            for agent in [".claude", ".codex", ".omp", ".qwen"] {
                if path_within(&resolved, &prospective_path(&home.join(agent))?) {
                    bail!("--state-dir must be outside agent data directories");
                }
            }
        }
        Ok(resolved)
    }
}

/// Resolve existing ancestors before comparing a not-yet-created state path.
/// Reject parent traversal rather than treating `missing/../source` as harmless.
fn prospective_path(path: &Path) -> Result<PathBuf> {
    if path
        .components()
        .any(|part| part == std::path::Component::ParentDir)
    {
        bail!("state-directory paths must not contain '..'");
    }
    let mut ancestor = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut missing = Vec::new();
    loop {
        match fs::canonicalize(&ancestor) {
            Ok(mut resolved) => {
                for component in missing.into_iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(
                    ancestor
                        .file_name()
                        .context("cannot resolve state-directory ancestor")?
                        .to_os_string(),
                );
                if !ancestor.pop() {
                    bail!("cannot resolve state-directory ancestor");
                }
            }
            Err(error) => return Err(error).context("cannot resolve state-directory ancestor"),
        }
    }
}

fn path_within(path: &Path, parent: &Path) -> bool {
    #[cfg(windows)]
    {
        // Windows filesystem identity is case-insensitive; keep component boundaries.
        let path = PathBuf::from(path.to_string_lossy().to_lowercase());
        let parent = PathBuf::from(parent.to_string_lossy().to_lowercase());
        path.starts_with(parent)
    }
    #[cfg(not(windows))]
    {
        path.starts_with(parent)
    }
}

fn timestamp(value: &str, flag: &str) -> Result<i64> {
    Ok(DateTime::parse_from_rfc3339(value)
        .with_context(|| {
            format!(
                "invalid {flag} timestamp; use RFC3339 with a timezone (or 7d/30d/all for --since)"
            )
        })?
        .timestamp_millis())
}

fn import_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let file = fs::File::open(path).context("cannot open JSON input file")?;
    serde_json::from_reader(file).context("invalid JSON input or incompatible schema")
}

pub fn run(args: &[String]) -> Result<()> {
    let options = Options::parse(args, Utc::now().timestamp_millis())?;
    if options.help {
        print!("{HELP}");
        return Ok(());
    }
    if options.command == Command::Browse
        && (!io::stdin().is_terminal() || !io::stdout().is_terminal())
    {
        bail!("audit browse requires an interactive terminal; use audit costs/candidates --json for pipes");
    }
    match options.command {
        Command::Record => {
            let record: tracking::Intervention =
                import_json(options.file.as_deref().expect("validated file"))?;
            tracking::save_intervention(&options.state_dir()?, &record)?;
            emit(
                &json!({"saved": "intervention", "id": record.id, "status": record.status}),
                &options,
            )
        }
        Command::Outcome => {
            let outcome: tracking::TaskOutcome =
                import_json(options.file.as_deref().expect("validated file"))?;
            tracking::save_outcome(&options.state_dir()?, &outcome)?;
            emit(
                &json!({"saved": "outcome", "task_id": outcome.task_id, "passed": outcome.passed, "cohort": outcome.cohort}),
                &options,
            )
        }
        Command::Evidence => {
            let reference: SourceRef =
                import_json(options.file.as_deref().expect("validated file"))?;
            let content = evidence_text(&reference, options.raw)?;
            if options.json {
                let value = json!({"source_id": reference.source_id, "version": reference.version, "line": reference.line, "raw": options.raw, "content": content});
                if options.raw {
                    // --raw is explicit local output. Do not redact its payload again.
                    println!("{}", serde_json::to_string_pretty(&value)?);
                } else {
                    emit(&value, &options)?;
                }
            } else {
                println!("{}", terminal_text(&content));
            }
            Ok(())
        }
        _ => {
            eprintln!("audit: read-only full-content scan (SHA256-validated parsed cache; no update check)");
            let ledger = source::load(&options.query)?;
            if options.command == Command::Compare {
                return emit(
                    &tracking::compare(
                        &ledger,
                        &options.state_dir()?,
                        options
                            .intervention
                            .as_deref()
                            .expect("validated intervention"),
                    )?,
                    &options,
                );
            }
            if options.command == Command::Browse {
                return Browser::new(ledger, &options)?.run();
            }
            let report = ledger::costs(&ledger, &options.query);
            match options.command {
                Command::Costs if !options.json => {
                    print_costs(&report, options.include_paths);
                    Ok(())
                }
                Command::Costs => emit(&report, &options),
                Command::Candidates => {
                    let candidates = analysis::candidates(&ledger, &options.query);
                    if options.json {
                        emit(
                            &json!({"cost_basis": "reported_actual_and_estimated_separate; related_cost_is_not_savings", "coverage": report.coverage, "candidates": candidates}),
                            &options,
                        )
                    } else {
                        print_candidates(&candidates, &report, options.include_paths);
                        Ok(())
                    }
                }
                Command::Explain => {
                    let candidates = analysis::candidates(&ledger, &options.query);
                    let config: Option<explain::ExplainConfig> =
                        options.config.as_deref().map(import_json).transpose()?;
                    if options.execute {
                        let explanations = explain::explain(
                            &candidates,
                            config.as_ref().expect("validated config"),
                            &options.state_dir()?,
                        )?;
                        emit(
                            &json!({"mode": "executed", "interpretation_status": "unverified model hypotheses and recommendations; only schema and evidence citations are checked", "analysis_spend": "recorded separately in state-dir; never added to work cost", "coverage": report.coverage, "explanations": explanations}),
                            &options,
                        )
                    } else {
                        let preview = explain::preview(&candidates, config.as_ref())?;
                        emit(
                            &json!({"mode": "preview", "network": false, "coverage": report.coverage, "preview": preview}),
                            &options,
                        )
                    }
                }
                _ => unreachable!("imports handled above"),
            }
        }
    }
}

fn evidence_text(reference: &SourceRef, raw: bool) -> Result<String> {
    if raw {
        source::read_evidence_raw(reference)
    } else {
        source::read_evidence(reference)
    }
}

/// Exports never contain raw args or source bodies; path opt-in does not bypass credential scrubbing.
fn export_value<T: Serialize>(value: &T, include_paths: bool) -> Result<Value> {
    fn scrub(value: &mut Value, include_paths: bool) {
        match value {
            Value::String(text) => *text = ledger::redact(text),
            Value::Array(values) => values
                .iter_mut()
                .for_each(|value| scrub(value, include_paths)),
            Value::Object(fields) => {
                for (key, value) in fields {
                    if key == "path" {
                        if !include_paths {
                            *value =
                                Value::String("[local path omitted; use --include-paths]".into());
                        }
                    } else {
                        scrub(value, include_paths);
                    }
                }
            }
            _ => {}
        }
    }
    let mut value = serde_json::to_value(value)?;
    scrub(&mut value, include_paths);
    Ok(value)
}

fn emit<T: Serialize>(value: &T, options: &Options) -> Result<()> {
    let value = export_value(value, options.include_paths)?;
    if options.json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("{}", terminal_text(&serde_json::to_string_pretty(&value)?));
    }
    Ok(())
}

fn terminal_text(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control() || *ch == '\n')
        .collect()
}

fn totals_text(totals: &CostTotals) -> String {
    format!("Reported actual ${:.6} | Estimated ${:.6} | Unknown {} observations\n{} requests | {} usage observations | input {} / output {} / cache-read {} / cache-write {} tokens",
        totals.reported_usd, totals.estimated_usd, totals.unknown_observations,
        totals.requests, totals.observations, totals.input_tokens, totals.output_tokens,
        totals.cache_read_tokens, totals.cache_write_tokens)
}

fn print_costs(report: &CostReport, include_paths: bool) {
    println!("AUDIT COST LEDGER\n{}\nProvider-reported actual is not an invoice; estimates are a disjoint bucket. Unknown is not zero.\n", totals_text(&report.totals));
    println!(
        "Window: {} <= observation time < {}",
        time_label(report.since_ms, "unbounded"),
        time_label(report.until_ms, "unbounded")
    );
    for row in &report.rows {
        println!(
            "\n{} | session {} | work {}\n{}",
            ledger::redact(&row.project),
            row.session_id.as_deref().unwrap_or("all"),
            ledger::redact(row.work_type.as_deref().unwrap_or("unknown")),
            totals_text(&row.totals)
        );
    }
    print_coverage(report, include_paths);
}

fn print_coverage(report: &CostReport, include_paths: bool) {
    println!("\nCOVERAGE: {} diagnostics. No diagnostics means no detected gap, not proof of complete billing.", report.coverage.len());
    for diagnostic in &report.coverage {
        println!(
            "- {}: {}",
            terminal_text(&diagnostic.code),
            terminal_text(&ledger::redact(&diagnostic.message))
        );
        if include_paths {
            if let Some(reference) = &diagnostic.source {
                println!(
                    "  {}:{} @ {}",
                    terminal_text(&reference.path.to_string_lossy()),
                    reference.line,
                    reference.version
                );
            }
        }
    }
}

fn print_candidates(candidates: &[Candidate], report: &CostReport, include_paths: bool) {
    println!("AUDIT CANDIDATES: {}\nObserved facts and hypotheses are separate. Related request cost is NOT avoidable cost or savings.", candidates.len());
    for candidate in candidates {
        println!(
            "\n{} [{}] {}\n{}",
            candidate.id,
            candidate.kind,
            ledger::redact(&candidate.project),
            terminal_text(&ledger::redact(&candidate.summary))
        );
        for fact in &candidate.observed {
            println!("  Observed: {}", terminal_text(&ledger::redact(fact)));
        }
        println!(
            "  Hypothesis: {}\n  Related cost: {}",
            terminal_text(&ledger::redact(&candidate.hypothesis)),
            totals_text(&candidate.related_cost)
        );
        for limitation in &candidate.limitations {
            println!(
                "  Limitation: {}",
                terminal_text(&ledger::redact(limitation))
            );
        }
        for reference in &candidate.evidence {
            println!(
                "  Evidence: {}:{} @ {}",
                reference.source_id, reference.line, reference.version
            );
            if include_paths {
                println!("    {}", terminal_text(&reference.path.to_string_lossy()));
            }
        }
    }
    print_coverage(report, include_paths);
}

fn time_label(ms: Option<i64>, absent: &str) -> String {
    ms.and_then(DateTime::<Utc>::from_timestamp_millis)
        .map(|time| time.to_rfc3339())
        .unwrap_or_else(|| absent.into())
}

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
    }
}

struct Browser {
    ledger: Ledger,
    query: Query,
    state_dir: PathBuf,
    report: CostReport,
    candidates: Vec<Candidate>,
    interventions: Vec<tracking::Intervention>,
    tab: usize,
    selected: [usize; 3],
    list: ListState,
    detail: String,
    references: Vec<SourceRef>,
    evidence_index: usize,
    evidence_open: bool,
    evidence_raw: bool,
    evidence_content: String,
    scroll: u16,
    status: String,
}

impl Browser {
    fn new(ledger: Ledger, options: &Options) -> Result<Self> {
        let state_dir = options.state_dir()?;
        let report = ledger::costs(&ledger, &options.query);
        let candidates = analysis::candidates(&ledger, &options.query);
        let interventions = tracking::list_interventions(&state_dir)?
            .into_iter()
            .filter(|record| {
                options
                    .query
                    .project
                    .as_ref()
                    .map_or(true, |project| &record.project == project)
            })
            .collect();
        let mut browser = Self {
            ledger, query: options.query.clone(), state_dir, report, candidates, interventions,
            tab: 0, selected: [0; 3], list: ListState::default(), detail: String::new(),
            references: Vec::new(), evidence_index: 0, evidence_open: false,
            evidence_raw: false, evidence_content: String::new(), scroll: 0,
            status: "Full scan; sources immutable. No network. Effects use complete selected-project tasks, not the cost window.".into(),
        };
        browser.prepare_detail()?;
        Ok(browser)
    }

    fn row_count(&self) -> usize {
        match self.tab {
            0 => self.report.rows.len(),
            1 => self.candidates.len(),
            _ => self.interventions.len(),
        }
    }

    fn prepare_detail(&mut self) -> Result<()> {
        self.evidence_open = false;
        self.evidence_raw = false;
        self.evidence_index = 0;
        self.scroll = 0;
        self.references.clear();
        let count = self.row_count();
        self.selected[self.tab] = self.selected[self.tab].min(count.saturating_sub(1));
        self.list.select(if count == 0 {
            None
        } else {
            Some(self.selected[self.tab])
        });
        let index = self.selected[self.tab];
        let value = match self.tab {
            0 => {
                if let Some(row) = self.report.rows.get(index) {
                    let sessions: HashSet<&str> =
                        self.ledger
                            .sessions
                            .iter()
                            .filter(|session| {
                                session.project == row.project
                                    && row.session_id.as_ref().map_or(true, |id| session.id == *id)
                                    && row.work_type.as_ref().map_or(true, |work| {
                                        session.work_type.as_ref() == Some(work)
                                    })
                            })
                            .map(|session| session.id.as_str())
                            .collect();
                    self.references.extend(
                        self.ledger
                            .observations
                            .iter()
                            .filter(|observation| {
                                sessions.contains(observation.session_id.as_str())
                                    && ledger::observation_in_window(observation, &self.query)
                            })
                            .map(|observation| observation.source.clone()),
                    );
                    self.references.extend(
                        self.ledger
                            .requests
                            .iter()
                            .filter(|request| {
                                sessions.contains(request.session_id.as_str())
                                    && in_window(request.ts_ms, &self.query)
                            })
                            .map(|request| request.source.clone()),
                    );
                    json!({"cost": row, "coverage": self.report.coverage, "note": "Reported actual and estimates are separate. Unknown is not zero. Enter/e opens version-checked local evidence."})
                } else {
                    json!({"message": "No cost rows match this window.", "coverage": self.report.coverage})
                }
            }
            1 => {
                if let Some(candidate) = self.candidates.get(index) {
                    self.references = candidate.evidence.clone();
                    json!({"candidate": candidate, "note": "Observed facts are not hypotheses; related_cost is not avoidable cost or savings.", "coverage": self.report.coverage})
                } else {
                    json!({"message": "No evidence-backed candidates in the selected scope.", "coverage": self.report.coverage})
                }
            }
            _ => {
                if let Some(intervention) = self.interventions.get(index) {
                    if let Some(candidate) = self
                        .candidates
                        .iter()
                        .find(|candidate| candidate.id == intervention.candidate_id)
                    {
                        self.references = candidate.evidence.clone();
                    }
                    let comparison = match tracking::compare(
                        &self.ledger,
                        &self.state_dir,
                        &intervention.id,
                    ) {
                        Ok(value) => value,
                        Err(error) => {
                            json!({"error": ledger::redact(&error.to_string()), "status": "comparison_unavailable"})
                        }
                    };
                    collect_references(&comparison, &mut self.references);
                    json!({"intervention": intervention, "comparison": comparison, "scope": "Complete tasks in selected root/project/agent, independent of Costs/Candidates time window", "coverage": self.report.coverage})
                } else {
                    json!({"message": "No interventions recorded. Import with audit record --file intervention.json and audit outcome --file outcome.json.", "scope": "Effects require explicit comparable task outcomes; final assistant prose is not quality evidence."})
                }
            }
        };
        let mut identities = HashSet::new();
        self.references.retain(|reference| {
            identities.insert((
                reference.source_id.clone(),
                reference.version.clone(),
                reference.line,
            ))
        });
        self.detail = terminal_text(&serde_json::to_string_pretty(&export_value(
            &value, false,
        )?)?);
        Ok(())
    }

    fn open_evidence(&mut self, raw: bool) {
        if let Some(reference) = self.references.get(self.evidence_index) {
            self.evidence_open = true;
            self.evidence_raw = raw;
            self.scroll = 0;
            self.evidence_content = match evidence_text(reference, raw) {
                Ok(content) => terminal_text(&format!("{}:{}\nVersion {}\n{}\n\n{}", reference.source_id, reference.line, reference.version, if raw { "RAW LOCAL RECORD: sensitive; never sent to a model" } else { "Sanitized record. Press R for explicit local raw display." }, content)),
                Err(error) => terminal_text(&format!("EVIDENCE REFUSED\n{}\n\nSource versions must match; press Esc then r to rescan.", ledger::redact(&error.to_string()))),
            };
        } else {
            self.status =
                "No source references for this row; no evidence has been invented.".into();
        }
    }

    fn refresh(&mut self) -> Result<()> {
        let ledger = source::load(&self.query)?;
        let interventions = tracking::list_interventions(&self.state_dir)?
            .into_iter()
            .filter(|record| {
                self.query
                    .project
                    .as_ref()
                    .map_or(true, |project| &record.project == project)
            })
            .collect();
        self.report = ledger::costs(&ledger, &self.query);
        self.candidates = analysis::candidates(&ledger, &self.query);
        self.ledger = ledger;
        self.interventions = interventions;
        self.status = "Full-content scan complete; evidence versions and parsed cache checked. No model call.".into();
        self.prepare_detail()
    }

    fn run(mut self) -> Result<()> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            bail!("audit browse requires an interactive terminal; use audit costs/candidates --json for pipes");
        }
        enable_raw_mode()?;
        let _guard = TerminalGuard;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        loop {
            terminal.draw(|frame| self.draw(frame))?;
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if key.code == KeyCode::Char('q')
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL))
                {
                    break;
                }
                match key.code {
                    KeyCode::Esc => {
                        self.evidence_open = false;
                        self.evidence_raw = false;
                        self.scroll = 0;
                    }
                    KeyCode::Tab | KeyCode::Right if !self.evidence_open => {
                        self.tab = (self.tab + 1) % 3;
                        self.prepare_detail()?;
                    }
                    KeyCode::BackTab | KeyCode::Left if !self.evidence_open => {
                        self.tab = (self.tab + 2) % 3;
                        self.prepare_detail()?;
                    }
                    KeyCode::Up | KeyCode::Char('k') if !self.evidence_open => {
                        self.selected[self.tab] = self.selected[self.tab].saturating_sub(1);
                        self.prepare_detail()?;
                    }
                    KeyCode::Down | KeyCode::Char('j') if !self.evidence_open => {
                        self.selected[self.tab] =
                            (self.selected[self.tab] + 1).min(self.row_count().saturating_sub(1));
                        self.prepare_detail()?;
                    }
                    KeyCode::Home if !self.evidence_open => {
                        self.selected[self.tab] = 0;
                        self.prepare_detail()?;
                    }
                    KeyCode::End if !self.evidence_open => {
                        self.selected[self.tab] = self.row_count().saturating_sub(1);
                        self.prepare_detail()?;
                    }
                    KeyCode::PageDown => self.scroll = self.scroll.saturating_add(10),
                    KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.scroll = self.scroll.saturating_add(1)
                    }
                    KeyCode::Up | KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
                    KeyCode::Enter | KeyCode::Char('e') => self.open_evidence(false),
                    KeyCode::Char('R') if self.evidence_open => self.open_evidence(true),
                    KeyCode::Char('[') if self.evidence_open => {
                        self.evidence_index = self.evidence_index.saturating_sub(1);
                        self.open_evidence(false);
                    }
                    KeyCode::Char(']') if self.evidence_open => {
                        self.evidence_index =
                            (self.evidence_index + 1).min(self.references.len().saturating_sub(1));
                        self.open_evidence(false);
                    }
                    KeyCode::Char('r') => {
                        if let Err(error) = self.refresh() {
                            self.status =
                                format!("Rescan failed: {}", ledger::redact(&error.to_string()));
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut ratatui::Frame<'_>) {
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(4),
                Constraint::Min(4),
                Constraint::Length(3),
            ])
            .split(frame.area());
        frame.render_widget(
            Tabs::new(vec!["Costs", "Candidates", "Effects"])
                .select(self.tab)
                .highlight_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Audit ledger | offline / read-only / full scan"),
                ),
            layout[0],
        );
        let summary = format!(
            "{}\nCoverage: {} diagnostics | [{} .. {}) | related cost != savings",
            totals_text(&self.report.totals),
            self.report.coverage.len(),
            time_label(self.query.since_ms, "all"),
            time_label(self.query.until_ms, "unbounded")
        );
        frame.render_widget(
            Paragraph::new(summary).style(Style::default().fg(Color::Yellow)),
            layout[1],
        );
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(34), Constraint::Percentage(66)])
            .split(layout[2]);
        let items: Vec<ListItem<'_>> = match self.tab {
            0 => self
                .report
                .rows
                .iter()
                .map(|row| {
                    ListItem::new(format!(
                        "{}\nactual ${:.4} est ${:.4} unknown {}",
                        terminal_text(&ledger::redact(&row.project)),
                        row.totals.reported_usd,
                        row.totals.estimated_usd,
                        row.totals.unknown_observations
                    ))
                })
                .collect(),
            1 => self
                .candidates
                .iter()
                .map(|candidate| {
                    ListItem::new(format!(
                        "{} | {}\n{}",
                        candidate.kind,
                        terminal_text(&ledger::redact(&candidate.project)),
                        terminal_text(&ledger::redact(&candidate.summary))
                    ))
                })
                .collect(),
            _ => self
                .interventions
                .iter()
                .map(|intervention| {
                    ListItem::new(format!(
                        "{} [{}]\n{}",
                        terminal_text(&intervention.id),
                        terminal_text(&intervention.status),
                        terminal_text(&ledger::redact(&intervention.description))
                    ))
                })
                .collect(),
        };
        frame.render_stateful_widget(
            List::new(items)
                .highlight_style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("> ")
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!("{} rows | Up/Down select", self.row_count())),
                ),
            columns[0],
            &mut self.list,
        );
        let title = if self.evidence_open {
            format!(
                "Evidence {}/{} | {} | [ ] / R / Esc",
                self.evidence_index + 1,
                self.references.len(),
                if self.evidence_raw {
                    "RAW LOCAL"
                } else {
                    "sanitized"
                }
            )
        } else {
            format!(
                "Details | {} evidence refs | Enter/e",
                self.references.len()
            )
        };
        frame.render_widget(
            Paragraph::new(if self.evidence_open {
                self.evidence_content.as_str()
            } else {
                self.detail.as_str()
            })
            .wrap(Wrap { trim: false })
            .scroll((self.scroll, 0))
            .block(Block::default().borders(Borders::ALL).title(title)),
            columns[1],
        );
        frame.render_widget(Paragraph::new(format!("Tab/Left/Right tabs | Up/Down j/k select | Enter/e evidence | PgUp/PgDn scroll | r rescan | q quit\n{}", terminal_text(&self.status))), layout[3]);
    }
}

fn in_window(ts_ms: Option<i64>, query: &Query) -> bool {
    match ts_ms {
        Some(ts) => {
            query.since_ms.map_or(true, |since| ts >= since)
                && query.until_ms.map_or(true, |until| ts < until)
        }
        None => query.since_ms.is_none() && query.until_ms.is_none(),
    }
}

fn collect_references(value: &Value, references: &mut Vec<SourceRef>) {
    match value {
        Value::Object(fields) => {
            if fields.contains_key("source_id")
                && fields.contains_key("version")
                && fields.contains_key("path")
                && fields.contains_key("line")
            {
                if let Ok(reference) = serde_json::from_value::<SourceRef>(value.clone()) {
                    references.push(reference);
                }
            } else {
                fields
                    .values()
                    .for_each(|value| collect_references(value, references));
            }
        }
        Value::Array(values) => values
            .iter()
            .for_each(|value| collect_references(value, references)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Options> {
        Options::parse(
            &args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
            1_800_000_000_000,
        )
    }

    #[test]
    fn missing_value_does_not_consume_the_next_flag() {
        assert!(parse(&["costs", "--root", "--json"]).is_err());
        assert!(parse(&["explain", "--config"]).is_err());
    }

    #[test]
    fn invalid_scope_cannot_silently_broaden_a_scan() {
        assert!(parse(&["costs", "--agent", "claude,unknown"]).is_err());
        assert!(parse(&["costs", "--agent", "claude,"]).is_err());
        assert!(parse(&["costs", "--project", "one", "--project", "two"]).is_err());
        assert!(parse(&["costs", "--cwd", "/project"]).is_err());
    }

    #[test]
    fn date_boundaries_are_explicit_and_exclusive() {
        let options = parse(&[
            "costs",
            "--since",
            "2026-09-01T00:00:00Z",
            "--until",
            "2026-09-02T00:00:00Z",
        ])
        .unwrap();
        assert!(in_window(options.query.since_ms, &options.query));
        assert!(!in_window(options.query.until_ms, &options.query));
        assert!(!in_window(None, &options.query));
        assert!(parse(&[
            "costs",
            "--since",
            "2026-09-02T00:00:00Z",
            "--until",
            "2026-09-02T00:00:00Z"
        ])
        .is_err());
        assert!(parse(&["costs", "--until", "2026-09-02"]).is_err());
        let all = parse(&["costs", "--since", "all"]).unwrap();
        assert!(in_window(None, &all.query));
    }

    #[test]
    fn durable_state_cannot_be_created_in_a_source_root() {
        let root = std::env::temp_dir();
        let mut options = parse(&["explain"]).unwrap();
        options.query.root = Some(root.clone());
        options.state_dir = Some(root.clone());
        assert!(options.state_dir().is_err());
        options.state_dir = Some(
            root.join("auditui-no-create-state-boundary")
                .join("new-state"),
        );
        assert!(options.state_dir().is_err());
        options.state_dir = Some(root.join("missing").join("..").join("state"));
        assert!(options.state_dir().is_err());
    }

    #[test]
    fn side_effects_require_explicit_command_specific_inputs() {
        assert!(parse(&["explain", "--execute"]).is_err());
        assert!(parse(&["explain", "--raw"]).is_err());
        assert!(parse(&["record"]).is_err());
        assert!(parse(&["outcome", "--file", "outcome.json", "--root", "/tmp"]).is_err());
        assert!(parse(&["compare", "--intervention", "fix", "--since", "7d"]).is_err());
        assert!(parse(&["browse", "--json"]).is_err());
    }
}
