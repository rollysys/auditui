use crate::cache::{self, CacheStore, SessionTimeline, TokenEvent};
use crate::cost::Usage;
use crate::providers::{self, Agent};
use anyhow::Result;
use chrono::DateTime;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub agent: Agent,
    pub id: String,
    pub path: PathBuf,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub prompt: Option<String>,
    pub turns: usize,
    pub last_active_ts: u64,
    pub started_at_ts: u64,
    pub is_scripted: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum TranscriptKind {
    User,
    Assistant,
    Thinking,
    ToolUse,
    ToolResult,
    System,
}

impl TranscriptKind {
    pub fn label(self) -> &'static str {
        match self {
            TranscriptKind::User => "USER",
            TranscriptKind::Assistant => "ASSIS",
            TranscriptKind::Thinking => "THINK",
            TranscriptKind::ToolUse => "TOOL→",
            TranscriptKind::ToolResult => "TOOL←",
            TranscriptKind::System => "SYS  ",
        }
    }
}

#[derive(Debug, Clone)]
pub struct TranscriptEvent {
    pub ts: String,
    pub kind: TranscriptKind,
    pub body: String,
}

#[derive(Debug, Clone, Default)]
pub struct WindowScan {
    pub usage: Usage,
    pub model: String,
    pub turns: usize,
}

pub fn parse_ts_secs(s: &str) -> Option<u64> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp().max(0) as u64)
}

pub fn index_all() -> Vec<SessionMeta> {
    let mut all = Vec::new();
    all.extend(providers::claude::list_sessions());
    all.extend(providers::codex::list_sessions());
    all.extend(providers::hermes::list_sessions());
    all.extend(providers::qwen::list_sessions());
    all.sort_by(|a, b| b.last_active_ts.cmp(&a.last_active_ts));
    all
}

// Default gap threshold for session grouping: 24h.
pub const DEFAULT_GROUP_GAP_SECS: u64 = 86_400;

#[derive(Debug, Clone)]
pub struct SessionGroup {
    pub key: String,              // "{agent}|{cwd}"
    pub agent: Agent,
    pub cwd: Option<String>,
    pub members: Vec<SessionMeta>, // sorted by last_active_ts DESC
    pub latest_active_ts: u64,
}

impl SessionGroup {
    pub fn len(&self) -> usize { self.members.len() }
}

// Group sessions by same agent + same cwd, splitting when the gap between
// consecutive members (next.started_at_ts − prev.last_active_ts) exceeds
// `gap_secs`. Returns groups sorted by latest_active_ts DESC; members within
// each group sorted by last_active_ts DESC.
pub fn group_sessions(sessions: &[SessionMeta], gap_secs: u64) -> Vec<SessionGroup> {
    use std::collections::BTreeMap;
    // bucket by (agent_tag, cwd); remember agent from first entry
    let mut buckets: BTreeMap<(&'static str, String), (Agent, Vec<SessionMeta>)> = BTreeMap::new();
    for s in sessions {
        let cwd = s.cwd.clone().unwrap_or_default();
        let tag = agent_tag(s.agent);
        buckets
            .entry((tag, cwd))
            .or_insert_with(|| (s.agent, Vec::new()))
            .1
            .push(s.clone());
    }
    let mut out: Vec<SessionGroup> = Vec::new();
    for ((_tag, cwd), (agent, mut metas)) in buckets {
        metas.sort_by_key(|m| m.started_at_ts);
        let mut cur: Vec<SessionMeta> = Vec::new();
        let mut prev_end: u64 = 0;
        for m in metas {
            let start = m.started_at_ts;
            let split = !cur.is_empty()
                && start > prev_end
                && start.saturating_sub(prev_end) > gap_secs;
            if split {
                out.push(finalize_group(&agent, &cwd, std::mem::take(&mut cur)));
            }
            prev_end = prev_end.max(m.last_active_ts);
            cur.push(m);
        }
        if !cur.is_empty() {
            out.push(finalize_group(&agent, &cwd, cur));
        }
    }
    out.sort_by(|a, b| b.latest_active_ts.cmp(&a.latest_active_ts));
    out
}

fn finalize_group(agent: &Agent, cwd: &str, mut members: Vec<SessionMeta>) -> SessionGroup {
    // Sort within group by last activity time descending so the TUI list
    // shows the most recently active session first.
    members.sort_by(|a, b| b.last_active_ts.cmp(&a.last_active_ts));
    let latest = members[0].last_active_ts;
    SessionGroup {
        key: format!("{}|{}", agent_tag(*agent), cwd),
        agent: *agent,
        cwd: if cwd.is_empty() { None } else { Some(cwd.to_string()) },
        members,
        latest_active_ts: latest,
    }
}

fn agent_tag(a: Agent) -> &'static str {
    match a {
        Agent::Claude => "claude",
        Agent::Codex => "codex",
        Agent::Hermes => "hermes",
        Agent::Qwen => "qwen",
    }
}

pub fn read_transcript(meta: &SessionMeta) -> Result<Vec<TranscriptEvent>> {
    match meta.agent {
        Agent::Claude => providers::claude::read_transcript(&meta.path),
        Agent::Codex => providers::codex::read_transcript(&meta.path),
        Agent::Hermes => providers::hermes::read_transcript(&meta.path),
        Agent::Qwen => providers::qwen::read_transcript(&meta.path),
    }
}

fn file_size(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn extract_events(meta: &SessionMeta) -> Vec<TokenEvent> {
    match meta.agent {
        Agent::Claude => providers::claude::extract_events(&meta.path),
        Agent::Codex => providers::codex::extract_events(&meta.path),
        Agent::Hermes => providers::hermes::extract_events(&meta.path),
        Agent::Qwen => providers::qwen::extract_events(&meta.path),
    }
}

// Return timeline from cache, falling back to disk, falling back to re-parse+persist.
pub fn get_timeline(meta: &SessionMeta, store: &CacheStore) -> Arc<SessionTimeline> {
    let size = file_size(&meta.path);
    if let Some(hit) = store.get_mem(&meta.id) {
        if hit.file_size == size {
            return hit;
        }
    }
    if let Some(disk) = cache::disk_path(meta.agent, &meta.id) {
        if let Some(loaded) = cache::load_from_disk(&disk, size) {
            let arc = Arc::new(loaded);
            store.put(meta.id.clone(), arc.clone());
            return arc;
        }
    }
    let events = extract_events(meta);
    let timeline = cache::new_timeline(size, events);
    if let Some(disk) = cache::disk_path(meta.agent, &meta.id) {
        let _ = cache::save_to_disk(&disk, &timeline);
    }
    let arc = Arc::new(timeline);
    store.put(meta.id.clone(), arc.clone());
    arc
}

// Aggregate all events in [cutoff_ts, ∞). cutoff_ts=0 means whole session.
pub fn aggregate_window(timeline: &SessionTimeline, cutoff_ts: u64) -> WindowScan {
    let mut usage = Usage::default();
    let mut turns = 0usize;
    let mut last_model = String::new();
    for ev in &timeline.events {
        if ev.ts < cutoff_ts {
            continue;
        }
        if ev.is_user_turn {
            turns += 1;
        } else {
            usage.add(&ev.usage);
            if !ev.model.is_empty() {
                last_model = ev.model.clone();
            }
        }
    }
    WindowScan {
        usage,
        model: last_model,
        turns,
    }
}

pub fn scan_window(meta: &SessionMeta, store: &CacheStore, cutoff_ts: u64) -> WindowScan {
    let timeline = get_timeline(meta, store);
    aggregate_window(&timeline, cutoff_ts)
}
