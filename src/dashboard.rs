use crate::cache::CacheStore;
use crate::cost::{compute_cost, Usage};
use crate::providers::Agent;
use crate::session::{self, SessionMeta};
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

// (start_ts, cost_usd) one entry per time bucket
pub type TimeSeries = Vec<(u64, f64)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    H1,
    H4,
    D1,
    D7,
    D30,
    All,
}

impl Range {
    pub const ALL: [Range; 6] = [Range::H1, Range::H4, Range::D1, Range::D7, Range::D30, Range::All];
    pub fn label(self) -> &'static str {
        match self {
            Range::H1 => "1h",
            Range::H4 => "4h",
            Range::D1 => "1d",
            Range::D7 => "7d",
            Range::D30 => "30d",
            Range::All => "all",
        }
    }
    pub fn seconds(self) -> u64 {
        match self {
            Range::H1 => 3_600,
            Range::H4 => 14_400,
            Range::D1 => 86_400,
            Range::D7 => 604_800,
            Range::D30 => 2_592_000,
            Range::All => 0,
        }
    }
    pub fn cutoff_ts(self, now: u64) -> u64 {
        let s = self.seconds();
        if s == 0 {
            0
        } else {
            now.saturating_sub(s)
        }
    }
    pub fn window_hours(self, now: u64, sessions: &[SessionMeta]) -> f64 {
        let s = self.seconds();
        if s > 0 {
            return s as f64 / 3600.0;
        }
        // All — span from earliest session ts to now
        let earliest = sessions
            .iter()
            .map(|m| m.started_at_ts.min(m.last_active_ts))
            .filter(|t| *t > 0)
            .min()
            .unwrap_or(now);
        let span = now.saturating_sub(earliest) as f64;
        (span / 3600.0).max(1.0)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ModelRow {
    pub model: String,
    pub sessions: usize,
    pub turns: usize,
    pub cost: f64,
    pub calls: u64,
    pub usage: Usage,
}

#[derive(Debug, Clone, Default)]
pub struct AgentRow {
    pub sessions: usize,
    pub turns: usize,
    pub cost: f64,
    pub calls: u64,
}

#[derive(Debug, Clone, Default)]
pub struct GroupRow {
    pub agent: &'static str,
    pub cwd: String,
    pub n_sessions: usize,
    pub prompt: String, // last member's prompt
    pub cost: f64,
    pub calls: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub total_sessions: usize, // raw sessions in window
    pub total_groups: usize,   // merged groups in window
    pub total_turns: usize,
    pub total_cost: f64,
    pub total_calls: u64,
    pub window_hours: f64,
    pub by_agent: BTreeMap<&'static str, AgentRow>,
    pub by_model: Vec<ModelRow>,
    pub by_group: Vec<GroupRow>,
    pub elapsed_ms: u128,
    pub range: String,
    pub by_time: TimeSeries,
    pub by_time_calls: Vec<(u64, u64)>,
    /// Per-agent per-bucket cost (USD). Indices align with `by_time`'s timestamps.
    /// Only agents with at least one non-zero bucket are present.
    pub by_time_agent: BTreeMap<Agent, Vec<f64>>,
    /// Per-agent per-bucket LLM-call counts. Indices align with `by_time_calls`.
    pub by_time_agent_calls: BTreeMap<Agent, Vec<u64>>,
    pub bucket_seconds: u64,
}

fn bucket_spec(range: Range, now: u64, sessions: &[SessionMeta]) -> (u64, u64, usize) {
    // returns (start_ts, bucket_seconds, n_buckets)
    match range {
        Range::H1 => (now.saturating_sub(3_600), 300, 12),
        Range::H4 => (now.saturating_sub(14_400), 900, 16),
        Range::D1 => (now.saturating_sub(86_400), 3_600, 24),
        Range::D7 => (now.saturating_sub(604_800), 21_600, 28),
        Range::D30 => (now.saturating_sub(2_592_000), 86_400, 30),
        Range::All => {
            let earliest = sessions
                .iter()
                .map(|s| s.started_at_ts.min(s.last_active_ts))
                .filter(|t| *t > 0)
                .min()
                .unwrap_or(now.saturating_sub(2_592_000));
            let span = now.saturating_sub(earliest).max(1);
            let n_buckets: usize = 30;
            let bucket_secs = (span / n_buckets as u64).max(3_600);
            (earliest, bucket_secs, n_buckets)
        }
    }
}

pub fn compute(sessions: &[SessionMeta], store: &CacheStore, range: Range) -> Stats {
    let start = std::time::Instant::now();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cutoff = range.cutoff_ts(now);
    let (ts_start, bucket_secs, n_buckets) = bucket_spec(range, now, sessions);

    // Parallel scan of every session (cached). Each session returns its
    // window aggregate + per-bucket cost vector.
    struct PerSession<'a> {
        meta: &'a SessionMeta,
        model: String,
        turns: usize,
        usage: Usage,
        cost: f64,
        calls: u64,
        buckets: Vec<f64>,
        call_buckets: Vec<u64>,
    }
    let per_session: Vec<PerSession> = sessions
        .par_iter()
        .map(|s| {
            let timeline = session::get_timeline(s, store);
            let mut usage = Usage::default();
            let mut turns = 0usize;
            let mut calls = 0u64;
            let mut last_model = String::new();
            let mut buckets = vec![0f64; n_buckets];
            let mut call_buckets = vec![0u64; n_buckets];
            for ev in &timeline.events {
                if ev.ts < cutoff {
                    continue;
                }
                if ev.is_user_turn {
                    turns += 1;
                    continue;
                }
                usage.add(&ev.usage);
                if !ev.model.is_empty() {
                    last_model = ev.model.clone();
                }
                let is_llm_call = ev.usage.input_tokens > 0
                    || ev.usage.output_tokens > 0
                    || ev.usage.cache_read_tokens > 0;
                if is_llm_call {
                    calls += 1;
                }
                let model_ev = if ev.model.is_empty() {
                    s.model.as_deref().unwrap_or("")
                } else {
                    ev.model.as_str()
                };
                let ev_cost = compute_cost(model_ev, &ev.usage);
                if n_buckets > 0 && ev.ts >= ts_start {
                    let idx = ((ev.ts - ts_start) / bucket_secs) as usize;
                    if idx < n_buckets {
                        buckets[idx] += ev_cost;
                        if is_llm_call {
                            call_buckets[idx] += 1;
                        }
                    }
                }
            }
            let model = if last_model.is_empty() {
                s.model.clone().unwrap_or_default()
            } else {
                last_model
            };
            let cost = compute_cost(&model, &usage);
            PerSession {
                meta: s,
                model,
                turns,
                usage,
                cost,
                calls,
                buckets,
                call_buckets,
            }
        })
        .filter(|ps| {
            ps.turns > 0
                || ps.usage.input_tokens > 0
                || ps.usage.output_tokens > 0
                || ps.usage.cache_read_tokens > 0
                || ps.usage.web_search_calls > 0
        })
        .collect();

    let mut stats = Stats::default();
    stats.range = range.label().to_string();
    stats.bucket_seconds = bucket_secs;
    stats.window_hours = range.window_hours(now, sessions);

    let mut totals = vec![0f64; n_buckets];
    let mut totals_calls = vec![0u64; n_buckets];
    let mut model_map: BTreeMap<String, ModelRow> = BTreeMap::new();
    let mut per_agent_cost: BTreeMap<Agent, Vec<f64>> = BTreeMap::new();
    let mut per_agent_calls: BTreeMap<Agent, Vec<u64>> = BTreeMap::new();
    for ps in &per_session {
        stats.total_sessions += 1;
        stats.total_turns += ps.turns;
        stats.total_cost += ps.cost;
        stats.total_calls += ps.calls;

        let agent_entry = stats.by_agent.entry(agent_key(ps.meta.agent)).or_default();
        agent_entry.sessions += 1;
        agent_entry.turns += ps.turns;
        agent_entry.cost += ps.cost;
        agent_entry.calls += ps.calls;

        let m = if ps.model.is_empty() { "-" } else { ps.model.as_str() };
        let row = model_map.entry(m.to_string()).or_insert_with(|| ModelRow {
            model: m.to_string(),
            ..Default::default()
        });
        row.sessions += 1;
        row.turns += ps.turns;
        row.cost += ps.cost;
        row.calls += ps.calls;
        row.usage.add(&ps.usage);

        let cost_vec = per_agent_cost
            .entry(ps.meta.agent)
            .or_insert_with(|| vec![0f64; n_buckets]);
        let calls_vec = per_agent_calls
            .entry(ps.meta.agent)
            .or_insert_with(|| vec![0u64; n_buckets]);
        for (i, v) in ps.buckets.iter().enumerate() {
            totals[i] += v;
            cost_vec[i] += v;
        }
        for (i, v) in ps.call_buckets.iter().enumerate() {
            totals_calls[i] += v;
            calls_vec[i] += v;
        }
    }

    stats.by_time = (0..n_buckets)
        .map(|i| (ts_start + i as u64 * bucket_secs, totals[i]))
        .collect();
    stats.by_time_calls = (0..n_buckets)
        .map(|i| (ts_start + i as u64 * bucket_secs, totals_calls[i]))
        .collect();
    // Drop agents whose entire series is zero (e.g. a filter excluded them but they
    // still appeared as 0-valued PerSession entries earlier).
    stats.by_time_agent = per_agent_cost
        .into_iter()
        .filter(|(_, v)| v.iter().any(|c| *c > 0.0))
        .collect();
    stats.by_time_agent_calls = per_agent_calls
        .into_iter()
        .filter(|(_, v)| v.iter().any(|c| *c > 0))
        .collect();

    stats.by_model = model_map.into_values().collect();
    stats.by_model.sort_by(|a, b| b.cost.partial_cmp(&a.cost).unwrap_or(std::cmp::Ordering::Equal));

    // Aggregate sessions into groups (same agent + cwd + gap < 24h) and
    // build by_group rows from per_session (which already respect the cutoff).
    let metas_in_window: Vec<SessionMeta> = per_session.iter().map(|ps| ps.meta.clone()).collect();
    let groups = session::group_sessions(&metas_in_window, session::DEFAULT_GROUP_GAP_SECS);
    let cost_by_sid: std::collections::HashMap<&str, (f64, u64, &str, &str)> = per_session
        .iter()
        .map(|ps| (
            ps.meta.id.as_str(),
            (ps.cost, ps.calls, agent_key(ps.meta.agent), ps.meta.prompt.as_deref().unwrap_or("")),
        ))
        .collect();
    let mut by_group: Vec<GroupRow> = groups
        .iter()
        .map(|g| {
            let mut cost = 0f64;
            let mut calls = 0u64;
            let mut agent_tag: &'static str = "";
            let mut last_prompt: &str = "";
            for m in &g.members {
                if let Some((c, k, at, pr)) = cost_by_sid.get(m.id.as_str()) {
                    cost += *c;
                    calls += *k;
                    agent_tag = *at;
                    last_prompt = *pr;
                }
            }
            GroupRow {
                agent: agent_tag,
                cwd: g.cwd.clone().unwrap_or_default(),
                n_sessions: g.members.len(),
                prompt: last_prompt.to_string(),
                cost,
                calls,
            }
        })
        .collect();
    by_group.sort_by(|a, b| b.cost.partial_cmp(&a.cost).unwrap_or(std::cmp::Ordering::Equal));
    stats.total_groups = by_group.len();
    stats.by_group = by_group;

    stats.elapsed_ms = start.elapsed().as_millis();
    stats
}

fn agent_key(a: Agent) -> &'static str {
    match a {
        Agent::Claude => "claude",
        Agent::Codex => "codex",
        Agent::Qwen => "qwen",
    }
}
