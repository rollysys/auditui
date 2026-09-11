// Harness debt detectors: run on `(SessionMeta, ToolTimeline)` pairs to find
// token cost traps and extractable workflows across agent sessions.

use crate::providers::Agent;
use crate::session::SessionMeta;
use crate::tools::{flag, ToolEvent, ToolTimeline};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fmt;

// ── Thresholds (public for tests) ──────────────────────────────────────────

/// A user turn with ≥ this many tool calls triggers `HeavyTurn`.
pub const HEAVY_TURN_CALLS: u32 = 25;
/// A retry chain of ≥ this length triggers `RetryChain`.
pub const RETRY_CHAIN_MIN: u32 = 3;
/// A single result ≥ this many bytes triggers `PayloadBomb` (single).
pub const PAYLOAD_SINGLE_BYTES: u32 = 20_000;
/// Cumulative result bytes per (session, tool) ≥ this triggers `PayloadBomb` (cumulative).
pub const PAYLOAD_CUMUL_BYTES: u64 = 200_000;
/// Compactions ≥ this triggers `Compaction`.
pub const COMPACTION_MIN: u32 = 3;
/// SSH calls ≥ this triggers `SshByHand`.
pub const SSH_CALL_MIN: u32 = 10;
/// A `shape_fp` appearing in ≥ this many sessions triggers `RecurringShape`.
pub const RECURRING_SESSION_MIN: usize = 4;
/// A HELP shape in ≥ this many sessions triggers `HelpLookup`.
pub const HELP_SESSION_MIN: usize = 3;
/// INLINE_SCRIPT in ≥ this many sessions AND total ≥ this triggers `InlineScript`.
pub const INLINE_SCRIPT_SESSION_MIN: usize = 3;
pub const INLINE_SCRIPT_TOTAL_MIN: usize = 10;
/// A read path in ≥ this many sessions triggers `ReadOnlyHotFile`.
pub const READ_HOT_SESSION_MIN: usize = 4;

/// Tools whose args_fp duplicates are natural (polling / coordination).
const DEDUP_WHITELIST: &[&str] = &[
    "todo", "hub", "job", "jobs", "TodoRead", "TodoWrite",
    "list_todos", "get_todos",
];

// ── Output types ───────────────────────────────────────────────────────────

#[derive(Serialize, Clone, Debug)]
pub struct AuditReport {
    pub window: String,
    pub sessions: usize,
    pub tool_calls: usize,
    pub errors: usize,
    pub findings: Vec<Finding>,
    pub by_tool: Vec<ToolRow>,
}

#[derive(Serialize, Clone, Debug)]
pub struct ToolRow {
    pub name: String,
    pub calls: usize,
    pub errors: usize,
    pub result_bytes: u64,
}

#[derive(Serialize, Clone, Debug)]
pub struct Finding {
    pub kind: FindingKind,
    pub severity: Severity,
    pub metric: u64,
    pub suggestion: String,
    pub evidence: Vec<Evidence>,
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FindingKind {
    HeavyTurn,
    RetryChain,
    PayloadBomb,
    DuplicateCall,
    Compaction,
    SshByHand,
    RecurringShape,
    HelpLookup,
    InlineScript,
    ReadOnlyHotFile,
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Serialize, Clone, Debug)]
pub struct Evidence {
    pub session_id: String,
    pub agent: String,
    pub line: u32,
    pub preview: String,
}

impl fmt::Display for FindingKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FindingKind::HeavyTurn => write!(f, "HeavyTurn"),
            FindingKind::RetryChain => write!(f, "RetryChain"),
            FindingKind::PayloadBomb => write!(f, "PayloadBomb"),
            FindingKind::DuplicateCall => write!(f, "DuplicateCall"),
            FindingKind::Compaction => write!(f, "Compaction"),
            FindingKind::SshByHand => write!(f, "SshByHand"),
            FindingKind::RecurringShape => write!(f, "RecurringShape"),
            FindingKind::HelpLookup => write!(f, "HelpLookup"),
            FindingKind::InlineScript => write!(f, "InlineScript"),
            FindingKind::ReadOnlyHotFile => write!(f, "ReadOnlyHotFile"),
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Severity::Low => write!(f, "LOW"),
            Severity::Medium => write!(f, "MEDIUM"),
            Severity::High => write!(f, "HIGH"),
            Severity::Critical => write!(f, "CRITICAL"),
        }
    }
}

// ── Evidence helper ────────────────────────────────────────────────────────

fn ev(meta: &SessionMeta, e: &ToolEvent) -> Evidence {
    Evidence {
        session_id: meta.id.clone(),
        agent: meta.agent.short().to_string(),
        line: e.line,
        preview: if e.preview.is_empty() {
            e.name.clone()
        } else {
            format!("{}: {}", e.name, e.preview)
        },
    }
}

fn ev_bare(meta: &SessionMeta, line: u32, preview: String) -> Evidence {
    Evidence {
        session_id: meta.id.clone(),
        agent: meta.agent.short().to_string(),
        line,
        preview,
    }
}

const MAX_EVIDENCE: usize = 5;

fn push_ev(vec: &mut Vec<Evidence>, e: Evidence) {
    if vec.len() < MAX_EVIDENCE {
        vec.push(e);
    }
}

// ── Detectors ──────────────────────────────────────────────────────────────

pub fn run(sessions: &[(SessionMeta, ToolTimeline)]) -> AuditReport {
    let mut findings: Vec<Finding> = Vec::new();
    let mut tool_calls = 0usize;
    let mut errors = 0usize;
    let mut tool_agg: HashMap<String, (usize, usize, u64)> = HashMap::new();

    // Per-session detectors
    for (meta, tl) in sessions {
        tool_calls += tl.events.len();
        for e in &tl.events {
            if e.is_error {
                errors += 1;
            }
            let entry = tool_agg.entry(e.name.clone()).or_insert((0, 0, 0));
            entry.0 += 1;
            if e.is_error {
                entry.1 += 1;
            }
            entry.2 += e.result_bytes as u64;
        }

        detect_heavy_turn(meta, tl, &mut findings);
        detect_retry_chain(meta, tl, &mut findings);
        detect_payload_bomb(meta, tl, &mut findings);
        detect_duplicate_call(meta, tl, &mut findings);
        detect_compaction(meta, tl, &mut findings);
        detect_ssh_by_hand(meta, tl, &mut findings);
    }

    // Cross-session detectors
    detect_recurring_shape(sessions, &mut findings);
    detect_help_lookup(sessions, &mut findings);
    detect_inline_script(sessions, &mut findings);
    detect_read_hot_file(sessions, &mut findings);

    // Sort: severity DESC, metric DESC
    findings.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then(b.metric.cmp(&a.metric))
    });

    let mut by_tool: Vec<ToolRow> = tool_agg
        .into_iter()
        .map(|(name, (calls, errs, rb))| ToolRow {
            name,
            calls,
            errors: errs,
            result_bytes: rb,
        })
        .collect();
    by_tool.sort_by(|a, b| b.calls.cmp(&a.calls));

    AuditReport {
        window: String::new(), // filled by caller
        sessions: sessions.len(),
        tool_calls,
        errors,
        findings,
        by_tool,
    }
}

// ── 1. HeavyTurn ──────────────────────────────────────────────────────────

fn detect_heavy_turn(meta: &SessionMeta, tl: &ToolTimeline, out: &mut Vec<Finding>) {
    // Count calls per turn
    let mut turn_counts: HashMap<u32, (u32, Vec<&ToolEvent>)> = HashMap::new();
    for e in &tl.events {
        let entry = turn_counts.entry(e.turn).or_insert((0, Vec::new()));
        entry.0 += 1;
        entry.1.push(e);
    }
    for (_, (count, evts)) in &turn_counts {
        if *count >= HEAVY_TURN_CALLS {
            let mut evidence = Vec::new();
            for e in evts.iter().take(MAX_EVIDENCE) {
                push_ev(&mut evidence, ev(meta, e));
            }
            out.push(Finding {
                kind: FindingKind::HeavyTurn,
                severity: severity_by_count(*count as u64, 25, 50, 100),
                metric: *count as u64,
                suggestion: "Break this turn into a skill or Makefile target \
                             so the agent doesn't re-discover the workflow each time."
                    .to_string(),
                evidence,
            });
        }
    }
}

// ── 2. RetryChain ─────────────────────────────────────────────────────────

fn detect_retry_chain(meta: &SessionMeta, tl: &ToolTimeline, out: &mut Vec<Finding>) {
    if tl.events.is_empty() {
        return;
    }
    let mut chain_start = 0usize;
    let mut chain_len = 0u32;
    let mut chain_name = "";

    let flush =
        |name: &str, len: u32, start: usize, events: &[ToolEvent], meta: &SessionMeta, out: &mut Vec<Finding>| {
            if len >= RETRY_CHAIN_MIN {
                let mut evidence = Vec::new();
                for e in events[start..].iter().take(len as usize).take(MAX_EVIDENCE) {
                    push_ev(&mut evidence, ev(meta, e));
                }
                out.push(Finding {
                    kind: FindingKind::RetryChain,
                    severity: severity_by_count(len as u64, 3, 5, 10),
                    metric: len as u64,
                    suggestion: format!(
                        "`{}` retried {} times — add pre-flight validation or a CLAUDE.md guardrail.",
                        name, len
                    ),
                    evidence,
                });
            }
        };

    for (i, e) in tl.events.iter().enumerate() {
        if e.name == chain_name && e.is_error {
            chain_len += 1;
        } else {
            flush(chain_name, chain_len, chain_start, &tl.events, meta, out);
            chain_start = i;
            chain_len = if e.is_error { 1 } else { 0 };
            chain_name = &e.name;
        }
    }
    flush(chain_name, chain_len, chain_start, &tl.events, meta, out);
}

// ── 3. PayloadBomb ────────────────────────────────────────────────────────

fn detect_payload_bomb(meta: &SessionMeta, tl: &ToolTimeline, out: &mut Vec<Finding>) {
    // Single-result bombs
    for e in &tl.events {
        if e.result_bytes >= PAYLOAD_SINGLE_BYTES {
            let mut evidence = Vec::new();
            push_ev(&mut evidence, ev(meta, e));
            out.push(Finding {
                kind: FindingKind::PayloadBomb,
                severity: severity_by_count(
                    e.result_bytes as u64,
                    PAYLOAD_SINGLE_BYTES as u64,
                    100_000,
                    500_000,
                ),
                metric: e.result_bytes as u64,
                suggestion: format!(
                    "`{}` returned {}KB in one call — pipe through jq/head or write to a file.",
                    e.name,
                    e.result_bytes / 1024
                ),
                evidence,
            });
        }
    }
    // Cumulative per (session, tool)
    let mut cumul: HashMap<&str, (u64, Vec<&ToolEvent>)> = HashMap::new();
    for e in &tl.events {
        let entry = cumul.entry(&e.name).or_insert((0, Vec::new()));
        entry.0 += e.result_bytes as u64;
        entry.1.push(e);
    }
    for (name, (total, evts)) in &cumul {
        if *total >= PAYLOAD_CUMUL_BYTES {
            let mut evidence = Vec::new();
            // Pick the largest result_bytes events as evidence
            let mut sorted: Vec<&&ToolEvent> = evts.iter().collect();
            sorted.sort_by(|a, b| b.result_bytes.cmp(&a.result_bytes));
            for e in sorted.into_iter().take(MAX_EVIDENCE) {
                push_ev(&mut evidence, ev(meta, e));
            }
            out.push(Finding {
                kind: FindingKind::PayloadBomb,
                severity: severity_by_count(*total, PAYLOAD_CUMUL_BYTES, 500_000, 2_000_000),
                metric: *total,
                suggestion: format!(
                    "`{}` dumped {}KB total in this session — extract a smaller tool or add result truncation.",
                    name,
                    total / 1024
                ),
                evidence,
            });
        }
    }
}

// ── 4. DuplicateCall ──────────────────────────────────────────────────────

fn detect_duplicate_call(meta: &SessionMeta, tl: &ToolTimeline, out: &mut Vec<Finding>) {
    // Group by args_fp, skipping whitelisted tools
    let mut by_fp: HashMap<u64, Vec<&ToolEvent>> = HashMap::new();
    for e in &tl.events {
        if DEDUP_WHITELIST.iter().any(|w| *w == e.name) {
            continue;
        }
        by_fp.entry(e.args_fp).or_default().push(e);
    }
    for (_, evts) in &by_fp {
        if evts.len() >= 2 {
            let count = evts.len() as u64;
            let mut evidence = Vec::new();
            for e in evts.iter().take(MAX_EVIDENCE) {
                push_ev(&mut evidence, ev(meta, e));
            }
            out.push(Finding {
                kind: FindingKind::DuplicateCall,
                severity: severity_by_count(count, 2, 4, 8),
                metric: count,
                suggestion: format!(
                    "`{}` called with identical args {} times — cache the result or refactor the workflow.",
                    evts[0].name, count
                ),
                evidence,
            });
        }
    }
}

// ── 5. Compaction ─────────────────────────────────────────────────────────

fn detect_compaction(meta: &SessionMeta, tl: &ToolTimeline, out: &mut Vec<Finding>) {
    if tl.compactions >= COMPACTION_MIN {
        let first_line = tl.events.first().map(|e| e.line).unwrap_or(0);
        let mut evidence = Vec::new();
        push_ev(
            &mut evidence,
            ev_bare(
                meta,
                first_line,
                format!("{} compactions in session", tl.compactions),
            ),
        );
        out.push(Finding {
            kind: FindingKind::Compaction,
            severity: severity_by_count(tl.compactions as u64, 3, 5, 10),
            metric: tl.compactions as u64,
            suggestion:
                "Session hit context limit repeatedly — split into smaller, focused tasks or use sub-agents."
                    .to_string(),
            evidence,
        });
    }
}

// ── 6. SshByHand ──────────────────────────────────────────────────────────

fn detect_ssh_by_hand(meta: &SessionMeta, tl: &ToolTimeline, out: &mut Vec<Finding>) {
    let mut ssh_count = 0u32;
    let mut quote_hell = 0u32;
    let mut timeout = 0u32;
    let mut evidence = Vec::new();
    for e in &tl.events {
        if e.flags & flag::SSH != 0 {
            ssh_count += 1;
            if e.flags & flag::QUOTE_HELL != 0 {
                quote_hell += 1;
            }
            if e.flags & flag::TIMEOUT != 0 {
                timeout += 1;
            }
            push_ev(&mut evidence, ev(meta, e));
        }
    }
    if ssh_count >= SSH_CALL_MIN {
        out.push(Finding {
            kind: FindingKind::SshByHand,
            severity: severity_by_count(ssh_count as u64, 10, 25, 50),
            metric: ssh_count as u64,
            suggestion: format!(
                "{} SSH calls ({} quote-hell, {} timeouts) — deploy a remote agentd or write a deploy script.",
                ssh_count, quote_hell, timeout
            ),
            evidence,
        });
    }
}

// ── 7. RecurringShape ─────────────────────────────────────────────────────

fn detect_recurring_shape(sessions: &[(SessionMeta, ToolTimeline)], out: &mut Vec<Finding>) {
    // shape_fp → { sessions: set, total calls, errors, one Evidence per session }
    struct ShapeInfo {
        session_ids: HashSet<String>,
        total: usize,
        errors: usize,
        per_session: Vec<Evidence>,
        shape_text: String,
        tool_name: String,
    }
    let mut map: HashMap<u64, ShapeInfo> = HashMap::new();
    for (meta, tl) in sessions {
        // Track which shape_fps we already recorded evidence for in this session
        let mut seen_in_session: HashSet<u64> = HashSet::new();
        for e in &tl.events {
            let info = map.entry(e.shape_fp).or_insert_with(|| ShapeInfo {
                session_ids: HashSet::new(),
                total: 0,
                errors: 0,
                per_session: Vec::new(),
                shape_text: e.shape.clone(),
                tool_name: e.name.clone(),
            });
            info.total += 1;
            if e.is_error {
                info.errors += 1;
            }
            if seen_in_session.insert(e.shape_fp) {
                info.session_ids.insert(meta.id.clone());
                if info.per_session.len() < MAX_EVIDENCE {
                    info.per_session.push(ev(meta, e));
                }
            }
        }
    }
    for (_, info) in map {
        if info.session_ids.len() >= RECURRING_SESSION_MIN {
            let sess_count = info.session_ids.len() as u64;
            out.push(Finding {
                kind: FindingKind::RecurringShape,
                severity: severity_by_count(sess_count, 4, 8, 16),
                metric: sess_count,
                suggestion: format!(
                    "`{}: {}` seen in {} sessions ({} calls, {} errors) — extract as a skill or Makefile target.",
                    info.tool_name,
                    if info.shape_text.is_empty() { "(no shape)" } else { &info.shape_text },
                    sess_count,
                    info.total,
                    info.errors
                ),
                evidence: info.per_session,
            });
        }
    }
}

// ── 8. HelpLookup ─────────────────────────────────────────────────────────

fn detect_help_lookup(sessions: &[(SessionMeta, ToolTimeline)], out: &mut Vec<Finding>) {
    // shape_fp (with HELP flag) → set of session ids
    struct HelpInfo {
        session_ids: HashSet<String>,
        shape_text: String,
        tool_name: String,
        evidence: Vec<Evidence>,
    }
    let mut map: HashMap<u64, HelpInfo> = HashMap::new();
    for (meta, tl) in sessions {
        let mut seen: HashSet<u64> = HashSet::new();
        for e in &tl.events {
            if e.flags & flag::HELP == 0 {
                continue;
            }
            let info = map.entry(e.shape_fp).or_insert_with(|| HelpInfo {
                session_ids: HashSet::new(),
                shape_text: e.shape.clone(),
                tool_name: e.name.clone(),
                evidence: Vec::new(),
            });
            if seen.insert(e.shape_fp) {
                info.session_ids.insert(meta.id.clone());
                if info.evidence.len() < MAX_EVIDENCE {
                    info.evidence.push(ev(meta, e));
                }
            }
        }
    }
    for (_, info) in map {
        if info.session_ids.len() >= HELP_SESSION_MIN {
            let sess_count = info.session_ids.len() as u64;
            out.push(Finding {
                kind: FindingKind::HelpLookup,
                severity: Severity::Medium,
                metric: sess_count,
                suggestion: format!(
                    "`{}` help looked up in {} sessions — add CLI usage to CLAUDE.md or a skill.",
                    info.shape_text, sess_count
                ),
                evidence: info.evidence,
            });
        }
    }
}

// ── 9. InlineScript ───────────────────────────────────────────────────────

fn detect_inline_script(sessions: &[(SessionMeta, ToolTimeline)], out: &mut Vec<Finding>) {
    struct ScriptInfo {
        session_ids: HashSet<String>,
        total: usize,
        shape_text: String,
        tool_name: String,
        evidence: Vec<Evidence>,
    }
    let mut map: HashMap<u64, ScriptInfo> = HashMap::new();
    for (meta, tl) in sessions {
        let mut seen: HashSet<u64> = HashSet::new();
        for e in &tl.events {
            if e.flags & flag::INLINE_SCRIPT == 0 {
                continue;
            }
            let info = map.entry(e.shape_fp).or_insert_with(|| ScriptInfo {
                session_ids: HashSet::new(),
                total: 0,
                shape_text: e.shape.clone(),
                tool_name: e.name.clone(),
                evidence: Vec::new(),
            });
            info.total += 1;
            if seen.insert(e.shape_fp) {
                info.session_ids.insert(meta.id.clone());
                if info.evidence.len() < MAX_EVIDENCE {
                    info.evidence.push(ev(meta, e));
                }
            }
        }
    }
    for (_, info) in map {
        if info.session_ids.len() >= INLINE_SCRIPT_SESSION_MIN
            && info.total >= INLINE_SCRIPT_TOTAL_MIN
        {
            out.push(Finding {
                kind: FindingKind::InlineScript,
                severity: severity_by_count(info.total as u64, 10, 20, 50),
                metric: info.total as u64,
                suggestion: format!(
                    "`{}` inline script in {} sessions ({} calls) — harden into a standalone script or tool.",
                    info.shape_text,
                    info.session_ids.len(),
                    info.total
                ),
                evidence: info.evidence,
            });
        }
    }
}

// ── 10. ReadOnlyHotFile ───────────────────────────────────────────────────

fn detect_read_hot_file(sessions: &[(SessionMeta, ToolTimeline)], out: &mut Vec<Finding>) {
    // path → set of session ids
    struct ReadInfo {
        session_ids: HashSet<String>,
        evidence: Vec<Evidence>,
    }
    let mut map: HashMap<String, ReadInfo> = HashMap::new();
    for (meta, tl) in sessions {
        let mut seen: HashSet<String> = HashSet::new();
        for e in &tl.events {
            // Match read/Read tools by name
            if e.name != "read" && e.name != "Read" && e.name != "read_file" {
                continue;
            }
            let path = if e.preview.is_empty() {
                continue;
            } else {
                // preview for read is "read: <path>" via ev(), but here we want raw preview
                e.preview.clone()
            };
            let info = map.entry(path.clone()).or_insert_with(|| ReadInfo {
                session_ids: HashSet::new(),
                evidence: Vec::new(),
            });
            if seen.insert(path) {
                info.session_ids.insert(meta.id.clone());
                if info.evidence.len() < MAX_EVIDENCE {
                    info.evidence.push(ev(meta, e));
                }
            }
        }
    }
    for (path, info) in map {
        if info.session_ids.len() >= READ_HOT_SESSION_MIN {
            let sess_count = info.session_ids.len() as u64;
            out.push(Finding {
                kind: FindingKind::ReadOnlyHotFile,
                severity: severity_by_count(sess_count, 4, 8, 16),
                metric: sess_count,
                suggestion: format!(
                    "`{}` read in {} sessions — add a project summary or onboarding doc so agents don't re-read.",
                    path, sess_count
                ),
                evidence: info.evidence,
            });
        }
    }
}

// ── Severity helper ────────────────────────────────────────────────────────

fn severity_by_count(val: u64, low: u64, med: u64, high: u64) -> Severity {
    if val >= high {
        Severity::Critical
    } else if val >= med {
        Severity::High
    } else if val >= low {
        Severity::Medium
    } else {
        Severity::Low
    }
}

// ── Human-readable rendering ───────────────────────────────────────────────

pub fn render_markdown(report: &AuditReport) -> String {
    let mut out = String::with_capacity(4096);
    out.push_str(&format!(
        "# Harness Audit Report\n\n\
         **Window:** {}  \n\
         **Sessions:** {}  |  **Tool calls:** {}  |  **Errors:** {}\n\n",
        report.window, report.sessions, report.tool_calls, report.errors
    ));

    if report.findings.is_empty() {
        out.push_str("_No findings (provider extractors may still be stubs)._\n\n");
    } else {
        out.push_str(&format!("## Findings ({})\n\n", report.findings.len()));
        for (i, f) in report.findings.iter().enumerate() {
            out.push_str(&format!(
                "### {}. {} [{}] metric={}\n\n",
                i + 1,
                f.kind,
                f.severity,
                f.metric
            ));
            out.push_str(&format!("{}\n\n", f.suggestion));
            if !f.evidence.is_empty() {
                out.push_str("Evidence:\n");
                for e in &f.evidence {
                    out.push_str(&format!(
                        "  {}:{}:{}  {}\n",
                        e.agent, e.session_id, e.line, e.preview
                    ));
                }
                out.push('\n');
            }
        }
    }

    if !report.by_tool.is_empty() {
        out.push_str("## Tool Summary\n\n");
        out.push_str("| Tool | Calls | Errors | Result KB |\n");
        out.push_str("|------|------:|-------:|----------:|\n");
        for t in report.by_tool.iter().take(30) {
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                t.name,
                t.calls,
                t.errors,
                t.result_bytes / 1024
            ));
        }
        out.push('\n');
    }

    out
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolCollector;
    use std::path::PathBuf;

    fn make_meta(id: &str, agent: Agent) -> SessionMeta {
        SessionMeta {
            agent,
            id: id.to_string(),
            path: PathBuf::from("/dev/null"),
            cwd: Some("/tmp/test".to_string()),
            model: None,
            prompt: None,
            turns: 0,
            last_active_ts: 0,
            started_at_ts: 0,
            is_scripted: false,
            parent_id: None,
            child_count: 0,
        }
    }

    fn bash_call(c: &mut ToolCollector, id: &str, cmd: &str, ts: u64, line: u32) {
        c.call(
            id,
            "bash",
            &serde_json::json!({"command": cmd}),
            ts,
            line,
        );
    }

    fn read_call(c: &mut ToolCollector, id: &str, path: &str, ts: u64, line: u32) {
        c.call(
            id,
            "read",
            &serde_json::json!({"path": path}),
            ts,
            line,
        );
    }

    #[test]
    fn heavy_turn_fires_at_threshold() {
        let meta = make_meta("s1", Agent::Claude);
        let mut c = ToolCollector::new();
        c.user_turn();
        for i in 0..30 {
            let id = format!("t{}", i);
            bash_call(&mut c, &id, &format!("cmd{}", i), 100 + i as u64, 10 + i as u32);
            c.result(&id, "ok", false, 101 + i as u64, 11 + i as u32);
        }
        let tl = c.finish(0);
        let sessions = vec![(meta, tl)];
        let report = run(&sessions);
        let ht: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.kind == FindingKind::HeavyTurn)
            .collect();
        assert_eq!(ht.len(), 1);
        assert_eq!(ht[0].metric, 30);
        assert!(ht[0].evidence.len() <= MAX_EVIDENCE);
    }

    #[test]
    fn heavy_turn_no_fire_below_threshold() {
        let meta = make_meta("s1", Agent::Claude);
        let mut c = ToolCollector::new();
        c.user_turn();
        for i in 0..24 {
            let id = format!("t{}", i);
            bash_call(&mut c, &id, &format!("cmd{}", i), 100, 10);
            c.result(&id, "ok", false, 101, 11);
        }
        let tl = c.finish(0);
        let report = run(&[(meta, tl)]);
        assert!(report
            .findings
            .iter()
            .all(|f| f.kind != FindingKind::HeavyTurn));
    }

    #[test]
    fn retry_chain_detected() {
        let meta = make_meta("s1", Agent::Codex);
        let mut c = ToolCollector::new();
        c.user_turn();
        // 4 consecutive errors on same tool
        for i in 0..4 {
            let id = format!("r{}", i);
            bash_call(&mut c, &id, "make build", 100 + i, 10 + i as u32);
            c.result(&id, "Error: compilation failed", true, 101 + i, 11 + i as u32);
        }
        // Then a success
        bash_call(&mut c, "r4", "make build", 200, 20);
        c.result("r4", "ok", false, 201, 21);
        let tl = c.finish(0);
        let report = run(&[(meta, tl)]);
        let rc: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.kind == FindingKind::RetryChain)
            .collect();
        assert_eq!(rc.len(), 1);
        assert_eq!(rc[0].metric, 4);
    }

    #[test]
    fn duplicate_call_whitelist_excludes_hub() {
        let meta = make_meta("s1", Agent::Omp);
        let mut c = ToolCollector::new();
        c.user_turn();
        // Duplicate "hub" calls — should be whitelisted
        for i in 0..5 {
            let id = format!("h{}", i);
            c.call(&id, "hub", &serde_json::json!({"op": "jobs"}), 100, 10);
            c.result(&id, "ok", false, 101, 11);
        }
        // Duplicate "grep" calls — should NOT be whitelisted
        for i in 0..3 {
            let id = format!("g{}", i);
            c.call(
                &id,
                "grep",
                &serde_json::json!({"pattern": "TODO", "path": "src/"}),
                200,
                20,
            );
            c.result(&id, "match", false, 201, 21);
        }
        let tl = c.finish(0);
        let report = run(&[(meta, tl)]);
        let dc: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.kind == FindingKind::DuplicateCall)
            .collect();
        // Only grep duplicates, not hub
        assert_eq!(dc.len(), 1);
        assert_eq!(dc[0].metric, 3);
        assert!(dc[0].evidence[0].preview.contains("grep"));
    }

    #[test]
    fn recurring_shape_threshold_boundary() {
        // 3 sessions with same shape → should NOT fire
        let mut sessions_3: Vec<(SessionMeta, ToolTimeline)> = Vec::new();
        for i in 0..3 {
            let meta = make_meta(&format!("s{}", i), Agent::Claude);
            let mut c = ToolCollector::new();
            c.user_turn();
            bash_call(&mut c, "t1", "cargo build --release", 100, 10);
            c.result("t1", "ok", false, 101, 11);
            sessions_3.push((meta, c.finish(0)));
        }
        let report = run(&sessions_3);
        assert!(
            report
                .findings
                .iter()
                .all(|f| f.kind != FindingKind::RecurringShape),
            "3 sessions should not trigger RecurringShape (threshold=4)"
        );

        // 4 sessions → should fire
        let mut sessions_4: Vec<(SessionMeta, ToolTimeline)> = Vec::new();
        for i in 0..4 {
            let meta = make_meta(&format!("s{}", i), Agent::Claude);
            let mut c = ToolCollector::new();
            c.user_turn();
            bash_call(&mut c, "t1", "cargo build --release", 100, 10);
            c.result("t1", "ok", false, 101, 11);
            sessions_4.push((meta, c.finish(0)));
        }
        let report = run(&sessions_4);
        let rs: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.kind == FindingKind::RecurringShape)
            .collect();
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].metric, 4);
    }

    #[test]
    fn compaction_fires() {
        let meta = make_meta("s1", Agent::Claude);
        let mut c = ToolCollector::new();
        c.user_turn();
        for _ in 0..5 {
            c.compaction();
        }
        bash_call(&mut c, "t1", "echo hi", 100, 10);
        c.result("t1", "hi", false, 101, 11);
        let tl = c.finish(0);
        let report = run(&[(meta, tl)]);
        let cf: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.kind == FindingKind::Compaction)
            .collect();
        assert_eq!(cf.len(), 1);
        assert_eq!(cf[0].metric, 5);
    }

    #[test]
    fn payload_bomb_single() {
        let meta = make_meta("s1", Agent::Claude);
        let mut c = ToolCollector::new();
        c.user_turn();
        bash_call(&mut c, "t1", "cat bigfile", 100, 10);
        let big = "x".repeat(25_000);
        c.result("t1", &big, false, 101, 11);
        let tl = c.finish(0);
        let report = run(&[(meta, tl)]);
        let pb: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.kind == FindingKind::PayloadBomb)
            .collect();
        // At least the single-result bomb
        assert!(!pb.is_empty());
        assert!(pb.iter().any(|f| f.metric >= 25_000));
    }

    #[test]
    fn findings_sorted_severity_desc_then_metric_desc() {
        let meta = make_meta("s1", Agent::Claude);
        let mut c = ToolCollector::new();
        c.user_turn();
        // Create a heavy turn (medium severity) and compaction (medium severity, lower metric)
        for i in 0..30 {
            let id = format!("t{}", i);
            bash_call(&mut c, &id, &format!("cmd{}", i), 100, 10);
            c.result(&id, "ok", false, 101, 11);
        }
        for _ in 0..3 {
            c.compaction();
        }
        let tl = c.finish(0);
        let report = run(&[(meta, tl)]);
        // Verify sorted: severity DESC then metric DESC
        for pair in report.findings.windows(2) {
            let cmp = pair[1]
                .severity
                .cmp(&pair[0].severity)
                .then(pair[1].metric.cmp(&pair[0].metric));
            assert!(
                cmp != std::cmp::Ordering::Greater,
                "findings not sorted: {:?} metric={} before {:?} metric={}",
                pair[0].kind,
                pair[0].metric,
                pair[1].kind,
                pair[1].metric
            );
        }
    }
}
