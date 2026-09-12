//! Read-only, evidence-linked request and usage ledger.

pub mod analysis;
pub mod explain;
pub mod parse;
pub mod source;
pub mod tracking;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cost::Usage;
use crate::providers::Agent;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SourceRef {
    pub source_id: String,
    pub version: String,
    pub path: PathBuf,
    pub line: u64,
    pub record_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub provider: Agent,
    pub project: String,
    pub parent_id: Option<String>,
    pub task_id: Option<String>,
    pub work_type: Option<String>,
    pub source: SourceRef,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UsageObservation {
    pub id: String,
    pub session_id: String,
    pub request_id: Option<String>,
    pub ts_ms: Option<i64>,
    pub model: String,
    pub basis: String,
    pub usage: Usage,
    pub reported_usd: Option<f64>,
    pub estimated_usd: Option<f64>,
    pub pricing_version: Option<String>,
    pub source: SourceRef,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LlmRequest {
    pub id: String,
    pub session_id: String,
    pub ts_ms: Option<i64>,
    pub model: String,
    pub status: String,
    pub source: SourceRef,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ToolExecution {
    pub id: String,
    pub session_id: String,
    pub request_id: Option<String>,
    pub batch_id: String,
    pub name: String,
    pub operation: String,
    pub object: Option<String>,
    pub args_fingerprint: String,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    pub status: String,
    pub result_bytes: u64,
    pub result_fingerprint: Option<String>,
    pub call: SourceRef,
    pub result: Option<SourceRef>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ContextEvent {
    pub session_id: String,
    pub kind: String,
    pub ts_ms: Option<i64>,
    pub source: SourceRef,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: String,
    pub message: String,
    pub source: Option<SourceRef>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Ledger {
    pub sessions: Vec<Session>,
    pub requests: Vec<LlmRequest>,
    pub observations: Vec<UsageObservation>,
    pub tools: Vec<ToolExecution>,
    pub contexts: Vec<ContextEvent>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Event-time bounds are inclusive at `since_ms` and exclusive at `until_ms`.
/// An empty `agents` list selects all supported ledger providers.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Query {
    pub root: Option<PathBuf>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub project: Option<String>,
    pub agents: Vec<Agent>,
}

/// Monetary provenance is disjoint: provider reported amounts are not estimates.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CostTotals {
    pub reported_usd: f64,
    pub estimated_usd: f64,
    pub unknown_observations: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub observations: usize,
    pub requests: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CostRow {
    pub project: String,
    pub session_id: Option<String>,
    pub work_type: Option<String>,
    pub totals: CostTotals,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CostReport {
    pub totals: CostTotals,
    pub rows: Vec<CostRow>,
    pub coverage: Vec<Diagnostic>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Candidate {
    pub id: String,
    pub kind: String,
    pub project: String,
    pub summary: String,
    pub observed: Vec<String>,
    pub hypothesis: String,
    pub related_cost: CostTotals,
    pub evidence: Vec<SourceRef>,
    pub limitations: Vec<String>,
}

impl CostTotals {
    /// Add one observation, preferring valid provider-reported money. Missing,
    /// negative, and non-finite amounts never silently become a zero estimate.
    pub fn add_observation(&mut self, observation: &UsageObservation) {
        self.observations += 1;
        let usage = &observation.usage;
        self.input_tokens += usage.input_tokens;
        self.output_tokens += usage.output_tokens;
        self.cache_read_tokens += usage.cache_read_tokens;
        // Legacy cache creation can duplicate the split buckets.
        self.cache_write_tokens +=
            if usage.cache_creation_5m_tokens == 0 && usage.cache_creation_1h_tokens == 0 {
                usage.cache_creation_tokens
            } else {
                usage.cache_creation_5m_tokens + usage.cache_creation_1h_tokens
            };
        if let Some(amount) = valid_amount(observation.reported_usd) {
            self.reported_usd += amount;
        } else if let Some(amount) = valid_amount(observation.estimated_usd) {
            self.estimated_usd += amount;
        } else {
            self.unknown_observations += 1;
        }
    }

    pub fn add(&mut self, other: &Self) {
        self.reported_usd += other.reported_usd;
        self.estimated_usd += other.estimated_usd;
        self.unknown_observations += other.unknown_observations;
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
        self.observations += other.observations;
        self.requests += other.requests;
    }
}

fn valid_amount(amount: Option<f64>) -> Option<f64> {
    amount.filter(|v| v.is_finite() && *v >= 0.0)
}

fn timestamp_in_window(timestamp: Option<i64>, query: &Query) -> bool {
    match timestamp {
        Some(ts) => {
            query.since_ms.map_or(true, |since| ts >= since)
                && query.until_ms.map_or(true, |until| ts < until)
        }
        None => query.since_ms.is_none() && query.until_ms.is_none(),
    }
}

/// Only event-time selection. Session/project/provider selection is performed by
/// `costs`, allowing callers to reuse this on an already-selected observation set.
pub fn observation_in_window(observation: &UsageObservation, query: &Query) -> bool {
    timestamp_in_window(observation.ts_ms, query)
}

/// Aggregate each selected usage observation once. Requests are counted
/// independently, including requests without usage, never inferred from tools.
pub fn costs(ledger: &Ledger, query: &Query) -> CostReport {
    let mut report = CostReport {
        since_ms: query.since_ms,
        until_ms: query.until_ms,
        coverage: ledger
            .diagnostics
            .iter()
            .map(|diagnostic| Diagnostic {
                code: diagnostic.code.clone(),
                message: redact(&diagnostic.message),
                source: diagnostic.source.clone(),
            })
            .collect(),
        ..CostReport::default()
    };
    if let (Some(since), Some(until)) = (query.since_ms, query.until_ms) {
        if since >= until {
            report.coverage.push(Diagnostic {
                code: "invalid_window".into(),
                message: "The inclusive start must precede the exclusive end.".into(),
                source: None,
            });
            return report;
        }
    }
    let sessions: HashMap<&str, &Session> = ledger
        .sessions
        .iter()
        .map(|session| (session.id.as_str(), session))
        .collect();
    let selected = |session: Option<&&Session>| match session {
        Some(session) => {
            query
                .project
                .as_ref()
                .map_or(true, |project| project == &session.project)
                && if query.agents.is_empty() {
                    matches!(session.provider, Agent::Claude | Agent::Codex | Agent::Omp)
                } else {
                    query.agents.contains(&session.provider)
                }
        }
        // Preserve orphaned amounts in unfiltered history, but do not assert
        // they belong to a selected provider or project.
        None => query.project.is_none() && query.agents.is_empty(),
    };
    let mut rows: BTreeMap<&str, CostRow> = BTreeMap::new();
    for observation in &ledger.observations {
        let session = sessions.get(observation.session_id.as_str());
        if !selected(session) {
            continue;
        }
        if observation.ts_ms.is_none() {
            report.coverage.push(missing_timestamp(
                "usage observation",
                &observation.source,
                query,
            ));
        }
        if !observation_in_window(observation, query) {
            continue;
        }
        if session.is_none() {
            report.coverage.push(Diagnostic {
                code: "unattributed_session".into(),
                message: "Usage lacks session metadata; project/provider attribution is unknown."
                    .into(),
                source: Some(observation.source.clone()),
            });
        }
        if observation
            .reported_usd
            .is_some_and(|v| !v.is_finite() || v < 0.0)
            || observation
                .estimated_usd
                .is_some_and(|v| !v.is_finite() || v < 0.0)
        {
            report.coverage.push(Diagnostic {
                code: "invalid_cost".into(),
                message: "Negative or non-finite monetary evidence was not aggregated.".into(),
                source: Some(observation.source.clone()),
            });
        }
        if valid_amount(observation.reported_usd).is_none()
            && valid_amount(observation.estimated_usd).is_none()
        {
            report.coverage.push(Diagnostic {
                code: "unknown_cost".into(),
                message: "Usage has neither valid reported cost nor a supported price estimate."
                    .into(),
                source: Some(observation.source.clone()),
            });
        }
        cost_row(&mut rows, &observation.session_id, session.copied())
            .totals
            .add_observation(observation);
        report.totals.add_observation(observation);
    }
    // Check linkage over the entire ledger, not just the event-time window:
    // usage may legitimately arrive after the request's timestamp.
    let observed_requests: HashSet<(&str, &str)> = ledger
        .observations
        .iter()
        .filter_map(|observation| {
            observation
                .request_id
                .as_deref()
                .map(|id| (observation.session_id.as_str(), id))
        })
        .collect();
    let mut requests_without_usage = 0usize;
    for request in &ledger.requests {
        let session = sessions.get(request.session_id.as_str());
        if !selected(session) {
            continue;
        }
        if request.ts_ms.is_none() {
            report
                .coverage
                .push(missing_timestamp("LLM request", &request.source, query));
        }
        if !timestamp_in_window(request.ts_ms, query) {
            continue;
        }
        cost_row(&mut rows, &request.session_id, session.copied())
            .totals
            .requests += 1;
        report.totals.requests += 1;
        if !observed_requests.contains(&(request.session_id.as_str(), request.id.as_str())) {
            requests_without_usage += 1;
        }
    }
    if requests_without_usage > 0 {
        report.coverage.push(Diagnostic {
            code: "requests_without_usage".into(),
            message: format!(
                "{requests_without_usage} selected LLM requests have no linked usage observation; their costs are unknown, not zero."
            ),
            source: None,
        });
    }
    report.rows = rows.into_values().collect();
    report
        .rows
        .sort_by(|a, b| (&a.project, &a.session_id).cmp(&(&b.project, &b.session_id)));
    report
}

fn cost_row<'a, 's>(
    rows: &'a mut BTreeMap<&'s str, CostRow>,
    session_id: &'s str,
    session: Option<&Session>,
) -> &'a mut CostRow {
    let project = session.map_or("(unknown)", |s| s.project.as_str());
    rows.entry(session_id).or_insert_with(|| CostRow {
        project: project.to_owned(),
        session_id: Some(session_id.to_owned()),
        work_type: session.and_then(|s| s.work_type.clone()),
        totals: CostTotals::default(),
    })
}

fn missing_timestamp(kind: &str, source: &SourceRef, query: &Query) -> Diagnostic {
    let disposition = if query.since_ms.is_some() || query.until_ms.is_some() {
        "excluded from bounded totals"
    } else {
        "included only because this is full-history aggregation"
    };
    Diagnostic {
        code: "missing_timestamp".into(),
        message: format!("A {kind} has no timestamp; {disposition}."),
        source: Some(source.clone()),
    }
}

pub fn parse_timestamp(value: &serde_json::Value) -> Option<i64> {
    value.as_i64().or_else(|| {
        chrono::DateTime::parse_from_rfc3339(value.as_str()?)
            .ok()
            .map(|datetime| datetime.timestamp_millis())
    })
}

/// SHA-256 of the original bytes, including whitespace and trailing newlines.
pub fn fingerprint(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let digest = Sha256::digest(bytes);
    let mut result = String::with_capacity(64);
    for byte in digest {
        write!(result, "{byte:02x}").expect("writing to a String cannot fail");
    }
    result
}

/// Conservative export scrubber. This is defense in depth, not permission to
/// export raw tool arguments or transcript bodies. Local SourceRef paths remain
/// intact internally for version-checked evidence lookup; exporters scrub them.
pub fn redact(text: &str) -> String {
    static RULES: LazyLock<Vec<(regex::Regex, &'static str)>> = LazyLock::new(|| {
        [
            (r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]+", "Bearer [REDACTED]"),
            (r"\b(?:sk-[A-Za-z0-9_-]{8,}|gh[pousr]_[A-Za-z0-9_]{8,}|github_pat_[A-Za-z0-9_]+|AKIA[A-Z0-9]{16})\b", "[REDACTED_CREDENTIAL]"),
            (r#"(?i)((?:api[_-]?key|access[_-]?token|refresh[_-]?token|authorization|password|passwd|secret|token)\s*["']?\s*[:=]\s*["']?)[^\s"',;&}\]]+"#, "${1}[REDACTED]"),
            (r"(?i)(https?://)[^/\s@]+@", "${1}[REDACTED]@"),
            (r"(?i)\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b", "[REDACTED_EMAIL]"),
            (r#"(^|[\s"'=:(])(?:/|~/|[A-Za-z]:\\|~\\)[^\s"'<>;,)\]}]+"#, "${1}[REDACTED_PATH]"),
        ].into_iter().map(|(pattern, replacement)| {
            (regex::Regex::new(pattern).expect("constant redaction regex"), replacement)
        }).collect()
    });
    let mut redacted = text.to_owned();
    for (pattern, replacement) in RULES.iter() {
        if let std::borrow::Cow::Owned(replacement) = pattern.replace_all(&redacted, *replacement) {
            redacted = replacement;
        }
    }
    redacted
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn observation(
        ts_ms: Option<i64>,
        reported: Option<f64>,
        estimated: Option<f64>,
    ) -> UsageObservation {
        UsageObservation {
            session_id: "s".into(),
            ts_ms,
            reported_usd: reported,
            estimated_usd: estimated,
            ..UsageObservation::default()
        }
    }

    fn session(id: &str, project: &str, provider: Agent) -> Session {
        Session {
            id: id.into(),
            project: project.into(),
            provider,
            parent_id: None,
            task_id: None,
            work_type: Some("implementation".into()),
            source: SourceRef::default(),
        }
    }

    #[test]
    fn mixed_provenance_does_not_double_count_or_convert_unknown_to_zero() {
        let mut totals = CostTotals::default();
        totals.add_observation(&observation(Some(1), Some(2.0), Some(7.0)));
        totals.add_observation(&observation(Some(1), None, Some(3.0)));
        totals.add_observation(&observation(Some(1), None, None));
        totals.add_observation(&observation(Some(1), Some(0.0), Some(9.0)));
        totals.add_observation(&observation(Some(1), Some(f64::NAN), Some(-1.0)));
        assert_eq!(totals.reported_usd, 2.0);
        assert_eq!(totals.estimated_usd, 3.0);
        assert_eq!(totals.unknown_observations, 2);
        assert_eq!(totals.observations, 5);
    }

    #[test]
    fn cache_tokens_choose_split_or_legacy_not_both() {
        let mut obs = observation(None, None, None);
        obs.usage = Usage {
            input_tokens: 11,
            output_tokens: 13,
            cache_read_tokens: 17,
            cache_creation_5m_tokens: 19,
            cache_creation_1h_tokens: 23,
            cache_creation_tokens: 42,
            ..Usage::default()
        };
        let mut totals = CostTotals::default();
        totals.add_observation(&obs);
        assert_eq!(
            (
                totals.input_tokens,
                totals.output_tokens,
                totals.cache_read_tokens
            ),
            (11, 13, 17)
        );
        assert_eq!(totals.cache_write_tokens, 42);
        obs.usage.cache_creation_5m_tokens = 0;
        obs.usage.cache_creation_1h_tokens = 0;
        totals.add_observation(&obs);
        assert_eq!(totals.cache_write_tokens, 84);
    }

    #[test]
    fn windows_use_event_time_and_count_requests_independently() {
        let ledger = Ledger {
            sessions: vec![session("s", "project", Agent::Claude)],
            observations: vec![
                observation(Some(9), Some(1.0), None),
                observation(Some(10), Some(2.0), None),
                observation(Some(19), None, Some(3.0)),
                observation(Some(20), Some(4.0), None),
                observation(None, Some(5.0), None),
            ],
            requests: vec![
                LlmRequest {
                    session_id: "s".into(),
                    ts_ms: Some(10),
                    ..LlmRequest::default()
                },
                LlmRequest {
                    session_id: "s".into(),
                    ts_ms: Some(15),
                    ..LlmRequest::default()
                },
                LlmRequest {
                    session_id: "s".into(),
                    ts_ms: Some(16),
                    ..LlmRequest::default()
                },
                LlmRequest {
                    session_id: "s".into(),
                    ts_ms: Some(20),
                    ..LlmRequest::default()
                },
                LlmRequest {
                    session_id: "s".into(),
                    ts_ms: None,
                    ..LlmRequest::default()
                },
            ],
            ..Ledger::default()
        };
        let query = Query {
            since_ms: Some(10),
            until_ms: Some(20),
            ..Query::default()
        };
        let report = costs(&ledger, &query);
        assert_eq!(
            (report.totals.reported_usd, report.totals.estimated_usd),
            (2.0, 3.0)
        );
        assert_eq!((report.totals.observations, report.totals.requests), (2, 3));
        assert_eq!(
            report
                .coverage
                .iter()
                .filter(|d| d.code == "missing_timestamp")
                .count(),
            2
        );
        assert_eq!(report.rows[0].totals.requests, 3);
        let all = costs(&ledger, &Query::default());
        assert_eq!(all.totals.reported_usd, 12.0);
        assert_eq!((all.totals.observations, all.totals.requests), (5, 5));
    }

    #[test]
    fn metadata_filters_apply_equally_to_requests_and_usage() {
        let mut other = observation(Some(1), Some(100.0), None);
        other.session_id = "other".into();
        let ledger = Ledger {
            sessions: vec![
                session("s", "project", Agent::Claude),
                session("other", "elsewhere", Agent::Codex),
            ],
            observations: vec![observation(Some(1), Some(2.0), None), other],
            requests: vec![
                LlmRequest {
                    session_id: "s".into(),
                    ts_ms: Some(1),
                    ..LlmRequest::default()
                },
                LlmRequest {
                    session_id: "other".into(),
                    ts_ms: Some(1),
                    ..LlmRequest::default()
                },
            ],
            ..Ledger::default()
        };
        for query in [
            Query {
                project: Some("project".into()),
                ..Query::default()
            },
            Query {
                agents: vec![Agent::Claude],
                ..Query::default()
            },
        ] {
            let report = costs(&ledger, &query);
            assert_eq!(report.totals.reported_usd, 2.0);
            assert_eq!(report.totals.requests, 1);
            assert_eq!(report.rows[0].work_type.as_deref(), Some("implementation"));
        }
    }

    #[test]
    fn evidence_primitives_keep_exact_versions_and_scrub_exported_secrets() {
        assert_eq!(
            fingerprint(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_ne!(fingerprint(b"abc"), fingerprint(b"abd"));
        assert_eq!(
            parse_timestamp(&json!("1970-01-01T01:00:00+01:00")),
            Some(0)
        );
        assert_eq!(parse_timestamp(&json!(1234)), Some(1234));
        assert_eq!(parse_timestamp(&json!(1.5)), None);
        assert_eq!(parse_timestamp(&json!("not a date")), None);
        let scrubbed = redact("Bearer abc.def password=hunter2 /Users/alice/private.rs alice@example.com sk-abcdefghijk");
        for secret in ["abc.def", "hunter2", "alice", "abcdefghijk"] {
            assert!(!scrubbed.contains(secret), "{scrubbed}");
        }
    }

    #[test]
    fn request_only_cost_is_unknown_and_usage_linkage_spans_event_windows() {
        let mut later_usage = observation(Some(20), Some(2.0), None);
        later_usage.request_id = Some("linked".into());
        let mut wrong_session_usage = observation(Some(15), Some(1.0), None);
        wrong_session_usage.session_id = "other".into();
        wrong_session_usage.request_id = Some("unlinked".into());
        let ledger = Ledger {
            sessions: vec![session("s", "project", Agent::Claude)],
            observations: vec![later_usage, wrong_session_usage],
            requests: ["linked", "unlinked", "missing"]
                .into_iter()
                .map(|id| LlmRequest {
                    id: id.into(),
                    session_id: "s".into(),
                    ts_ms: Some(10),
                    ..LlmRequest::default()
                })
                .collect(),
            ..Ledger::default()
        };
        let report = costs(
            &ledger,
            &Query {
                since_ms: Some(10),
                until_ms: Some(20),
                project: Some("project".into()),
                ..Query::default()
            },
        );
        assert_eq!(report.totals.requests, 3);
        assert_eq!(report.totals.observations, 0);
        let missing: Vec<_> = report
            .coverage
            .iter()
            .filter(|d| d.code == "requests_without_usage")
            .collect();
        assert_eq!(missing.len(), 1);
        assert!(missing[0].message.starts_with("2 "));
    }
}
