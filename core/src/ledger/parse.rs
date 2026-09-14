//! Provider records normalized without inferring requests from cumulative counters.
//!
//! Evidence is tied to the exact original bytes. Transcript entry IDs identify
//! batches, not API requests; in particular an omp entry ID is not a response ID.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::cost::{estimate_cost_strict, Usage, PRICING_VERSION};
use crate::providers::Agent;

use super::{
    fingerprint, parse_timestamp, ContextEvent, Diagnostic, Ledger, LlmRequest, Session, SourceRef,
    ToolExecution, UsageObservation,
};

/// Parse one immutable snapshot of a transcript. No caches or source writes.
pub fn parse_file(
    path: &Path,
    provider: Agent,
    project_override: Option<&str>,
    parent_id: Option<&str>,
) -> Result<Ledger> {
    let tag = match provider {
        Agent::Claude => "claude",
        Agent::Codex => "codex",
        Agent::Omp => "omp",
        _ => bail!("unsupported ledger provider"),
    };
    let path = path
        .canonicalize()
        .context("cannot resolve transcript source")?;
    let bytes = std::fs::read(&path).context("cannot read transcript source")?;
    let source = SourceRef {
        source_id: format!("{tag}:{}", fingerprint(path.as_os_str().as_encoded_bytes())),
        version: fingerprint(&bytes),
        path,
        line: 1,
        record_id: None,
    };
    let mut parser = Parser::new(source, provider, project_override, parent_id);
    for (index, line) in bytes.split_inclusive(|b| *b == b'\n').enumerate() {
        let line_no = index as u64 + 1;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let source = parser.source_at(line_no, None);
        match serde_json::from_slice::<Value>(line) {
            Ok(record) if record.is_object() => parser.record(&record, line_no),
            Ok(_) => parser.diagnostic("invalid_record", "JSON record is not an object", &source),
            Err(error) => {
                let partial = !line.ends_with(b"\n") && error.is_eof();
                parser.diagnostic(
                    if partial {
                        "partial_record"
                    } else {
                        "malformed_record"
                    },
                    if partial {
                        "Incomplete final JSON record was excluded"
                    } else {
                        "Malformed JSON record was excluded"
                    },
                    &source,
                );
            }
        }
    }
    Ok(parser.finish())
}

struct Snapshot {
    observation: usize,
    request: Option<usize>,
    raw_usage: Option<Value>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Counters {
    input: u64,
    cached: u64,
    output: u64,
}

struct Parser {
    ledger: Ledger,
    source: SourceRef,
    provider: Agent,
    project_override: bool,
    cwd: Option<String>,
    model: String,
    snapshots: HashMap<String, Snapshot>,
    request_aliases: HashMap<String, String>,
    tools: HashMap<String, usize>,
    missing_times: HashSet<u64>,
    cumulative: Option<Counters>,
    codex_batch: Option<String>,
    compact_boundary: bool,
    saw_record: bool,
    saw_metadata: bool,
}

impl Parser {
    fn new(
        source: SourceRef,
        provider: Agent,
        project: Option<&str>,
        parent: Option<&str>,
    ) -> Self {
        let project = project.filter(|p| !p.is_empty());
        let mut ledger = Ledger::default();
        ledger.sessions.push(Session {
            id: source.source_id.clone(),
            provider,
            project: project.unwrap_or("").to_owned(),
            parent_id: parent.map(str::to_owned),
            task_id: None,
            work_type: None,
            source: source.clone(),
        });
        Self {
            ledger,
            source,
            provider,
            project_override: project.is_some(),
            cwd: project.and_then(|p| resolve_path(p, None)),
            model: String::new(),
            snapshots: HashMap::new(),
            request_aliases: HashMap::new(),
            tools: HashMap::new(),
            missing_times: HashSet::new(),
            cumulative: None,
            codex_batch: None,
            compact_boundary: false,
            saw_record: false,
            saw_metadata: false,
        }
    }

    fn source_at(&self, line: u64, record_id: Option<&str>) -> SourceRef {
        SourceRef {
            line,
            record_id: record_id.map(str::to_owned),
            ..self.source.clone()
        }
    }

    fn diagnostic(&mut self, code: &str, message: &str, source: &SourceRef) {
        self.ledger.diagnostics.push(Diagnostic {
            code: code.to_owned(),
            message: message.to_owned(),
            source: Some(source.clone()),
        });
    }

    fn timestamp(&mut self, record: &Value, source: &SourceRef) -> Option<i64> {
        let timestamp = if self.provider == Agent::Omp {
            record
                .pointer("/message/timestamp")
                .and_then(parse_timestamp)
                .or_else(|| record.get("timestamp").and_then(parse_timestamp))
        } else {
            record.get("timestamp").and_then(parse_timestamp)
        };
        if timestamp.is_none() && self.missing_times.insert(source.line) {
            self.diagnostic(
                "missing_timestamp",
                "Event has no valid timestamp; no file time was substituted",
                source,
            );
        }
        timestamp
    }

    fn context(&mut self, kind: &str, record: &Value, source: &SourceRef) {
        let ts_ms = self.timestamp(record, source);
        self.ledger.contexts.push(ContextEvent {
            session_id: self.source.source_id.clone(),
            kind: kind.to_owned(),
            ts_ms,
            source: source.clone(),
        });
    }

    fn record(&mut self, record: &Value, line: u64) {
        let id = string(record, "uuid").or_else(|| string(record, "id"));
        let source = self.source_at(line, id);
        if !self.saw_record {
            self.ledger.sessions[0].source = source.clone();
            self.saw_record = true;
        }
        let Some(kind) = string(record, "type") else {
            self.diagnostic(
                "invalid_record",
                "Record has no type discriminator",
                &source,
            );
            return;
        };
        let metadata = match self.provider {
            Agent::Codex if kind == "session_meta" => record.get("payload"),
            Agent::Omp if kind == "session" => Some(record),
            Agent::Claude => Some(record),
            _ => None,
        };
        if let Some(meta) = metadata {
            let cwd = string(meta, "cwd");
            let session_id = if self.provider == Agent::Claude {
                string(meta, "sessionId")
            } else {
                string(meta, "id")
            };
            if !self.saw_metadata && (cwd.is_some() || session_id.is_some()) {
                self.ledger.sessions[0].source = self.source_at(line, session_id.or(id));
                self.saw_metadata = true;
            }
            if let Some(cwd) = cwd {
                self.set_cwd(cwd);
            }
            if let Some(model) = string(meta, "model") {
                self.model = model.to_owned();
            }
        }
        match self.provider {
            Agent::Claude => self.claude(record, kind, &source),
            Agent::Omp => self.omp(record, kind, &source),
            Agent::Codex => self.codex(record, kind, &source),
            _ => unreachable!(),
        }
    }

    fn set_cwd(&mut self, cwd: &str) {
        // A transcript's cwd must not be interpreted relative to the audit process.
        self.cwd = resolve_path(cwd, self.cwd.as_deref());
        if !self.project_override && self.ledger.sessions[0].project.is_empty() {
            self.ledger.sessions[0].project = self.cwd.clone().unwrap_or_else(|| cwd.to_owned());
        }
    }

    fn claude(&mut self, record: &Value, kind: &str, source: &SourceRef) {
        if record.get("isMeta").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let boundary = string(record, "subtype") == Some("compact_boundary")
            || record.get("compactMetadata").is_some_and(|v| !v.is_null());
        let summary = kind == "summary"
            || record.get("isCompactSummary").and_then(Value::as_bool) == Some(true);
        if boundary {
            self.context("compaction", record, source);
            self.compact_boundary = true;
            return;
        }
        if summary {
            if !self.compact_boundary {
                self.context("compaction", record, source);
            }
            self.compact_boundary = false;
            return;
        }
        self.compact_boundary = false;
        match kind {
            "assistant" => {
                let Some(message) = record.get("message").filter(|v| v.is_object()) else {
                    self.diagnostic(
                        "invalid_record",
                        "Assistant record has no message object",
                        source,
                    );
                    return;
                };
                if string(message, "model") == Some("<synthetic>") {
                    self.diagnostic("synthetic_request_excluded", "Synthetic assistant record was excluded from provider request and usage counts", source);
                    return;
                }
                let request = string(record, "requestId")
                    .or_else(|| string(message, "requestId"))
                    .or_else(|| string(message, "id"));
                self.assistant(record, message, request, "tool_use", "input", source);
            }
            "user" => {
                if let Some(parts) = record.pointer("/message/content").and_then(Value::as_array) {
                    for part in parts {
                        if string(part, "type") == Some("tool_result") {
                            let ts = self.timestamp(record, source);
                            self.tool_result(
                                string(part, "tool_use_id"),
                                part.get("content"),
                                part,
                                record.get("toolUseResult"),
                                ts,
                                source,
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn omp(&mut self, record: &Value, kind: &str, source: &SourceRef) {
        match kind {
            "model_change" => {
                if let Some(model) = string(record, "model") {
                    self.model = model.to_owned();
                }
                self.context("model_change", record, source);
            }
            "compaction" => self.context("compaction", record, source),
            "ttsr_injection" | "custom_message" => self.context(kind, record, source),
            "message" => {
                let Some(message) = record.get("message").filter(|v| v.is_object()) else {
                    self.diagnostic(
                        "invalid_record",
                        "Message record has no message object",
                        source,
                    );
                    return;
                };
                match string(message, "role") {
                    Some("assistant") => {
                        let request = string(message, "responseId")
                            .or_else(|| string(message, "requestId"))
                            .or_else(|| string(message, "request_id"))
                            .or_else(|| string(message, "id"));
                        self.assistant(record, message, request, "toolCall", "arguments", source);
                    }
                    Some("toolResult") => {
                        let ts = self.timestamp(record, source);
                        self.tool_result(
                            string(message, "toolCallId"),
                            message.get("content"),
                            message,
                            message.get("details"),
                            ts,
                            source,
                        );
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn assistant(
        &mut self,
        record: &Value,
        message: &Value,
        raw_request: Option<&str>,
        tool_kind: &str,
        args_key: &str,
        source: &SourceRef,
    ) {
        let ts_ms = self.timestamp(record, source);
        let model = string(message, "model").unwrap_or(&self.model).to_owned();
        let aliases = [
            string(message, "responseId"),
            string(record, "requestId"),
            string(message, "requestId"),
            string(message, "request_id"),
            string(message, "id"),
        ];
        let request_id = raw_request.map(|raw| {
            aliases
                .iter()
                .flatten()
                .find_map(|alias| self.request_aliases.get(*alias))
                .cloned()
                .unwrap_or_else(|| {
                    format!(
                        "{}:request:{}",
                        self.source.source_id,
                        fingerprint(raw.as_bytes())
                    )
                })
        });
        if let Some(id) = &request_id {
            for alias in aliases.into_iter().flatten() {
                self.request_aliases.insert(alias.to_owned(), id.clone());
            }
        }
        let key = match &request_id {
            Some(id) => id.clone(),
            None => match source.record_id.as_deref() {
                Some(id) => format!("entry:{id}"),
                None => format!("line:{}", source.line),
            },
        };
        let batch = format!(
            "{}:batch:{}",
            self.source.source_id,
            fingerprint(key.as_bytes())
        );
        let status = request_status(message);
        if !self.snapshots.contains_key(&key) {
            let request = request_id.as_ref().map(|id| {
                let index = self.ledger.requests.len();
                self.ledger.requests.push(LlmRequest {
                    id: id.clone(),
                    session_id: self.source.source_id.clone(),
                    ts_ms,
                    model: model.clone(),
                    status: status.to_owned(),
                    source: source.clone(),
                });
                index
            });
            let observation = self.ledger.observations.len();
            self.ledger.observations.push(UsageObservation {
                id: format!(
                    "{}:usage:{}",
                    self.source.source_id,
                    fingerprint(key.as_bytes())
                ),
                session_id: self.source.source_id.clone(),
                request_id: request_id.clone(),
                ts_ms,
                model: model.clone(),
                basis: if request_id.is_some() {
                    "request"
                } else {
                    "unattributed"
                }
                .to_owned(),
                usage: Usage::default(),
                cache_counters_complete: false,
                reported_usd: None,
                estimated_usd: None,
                pricing_version: None,
                source: source.clone(),
            });
            self.snapshots.insert(
                key.clone(),
                Snapshot {
                    observation,
                    request,
                    raw_usage: None,
                },
            );
            if request_id.is_none() {
                self.diagnostic("missing_request_identity", "Assistant usage has no provider request or response identity; transcript entry identity is used only for deduplication and tool batches", source);
            }
        }
        let snapshot = self.snapshots.get_mut(&key).expect("snapshot inserted");
        if let Some(index) = snapshot.request {
            let request = &mut self.ledger.requests[index];
            if request.ts_ms.is_none() {
                request.ts_ms = ts_ms;
            }
            if !model.is_empty() {
                request.model = model.clone();
            }
            if status != "unknown" {
                request.status = status.to_owned();
            }
        }
        let observation = &mut self.ledger.observations[snapshot.observation];
        if !model.is_empty() {
            observation.model = model;
        }
        if let Some(usage) = message.get("usage").filter(|v| v.is_object()) {
            let changed = match snapshot.raw_usage.as_mut() {
                Some(previous) => merge_snapshot(previous, usage),
                None => {
                    snapshot.raw_usage = Some(usage.clone());
                    true
                }
            };
            // Preserve the actual evidence and time for the richest snapshot.
            if changed {
                observation.source = source.clone();
                observation.ts_ms = ts_ms.or(observation.ts_ms);
            }
        }
        if let Some(parts) = message.get("content").and_then(Value::as_array) {
            for (position, part) in parts.iter().enumerate() {
                if string(part, "type") == Some(tool_kind) {
                    self.tool_call(
                        string(part, "id"),
                        string(part, "name"),
                        part.get(args_key),
                        request_id.clone(),
                        &batch,
                        position,
                        ts_ms,
                        source,
                    );
                }
            }
        }
    }

    fn codex(&mut self, record: &Value, kind: &str, source: &SourceRef) {
        if kind == "compacted" {
            self.context("compaction", record, source);
            self.compact_boundary = true;
            self.codex_batch = None;
            return;
        }
        let Some(payload) = record.get("payload").filter(|v| v.is_object()) else {
            if matches!(
                kind,
                "session_meta" | "turn_context" | "event_msg" | "response_item"
            ) {
                self.diagnostic(
                    "invalid_record",
                    "Codex record has no payload object",
                    source,
                );
            }
            return;
        };
        match kind {
            "turn_context" => {
                if let Some(model) = string(payload, "model") {
                    self.model = model.to_owned();
                }
                if let Some(cwd) = string(payload, "cwd") {
                    self.set_cwd(cwd);
                }
                self.codex_batch = None;
            }
            "event_msg" => match string(payload, "type") {
                Some("token_count") => {
                    self.codex_usage(record, payload, source);
                    self.codex_batch = None;
                }
                Some("context_compacted") => {
                    if !self.compact_boundary {
                        self.context("compaction", record, source);
                    }
                    self.compact_boundary = false;
                    self.codex_batch = None;
                }
                Some("user_message" | "task_started" | "task_complete" | "turn_aborted") => {
                    self.codex_batch = None;
                    self.compact_boundary = false;
                }
                _ => {}
            },
            "response_item" => {
                match string(payload, "type") {
                    Some(
                        "function_call" | "custom_tool_call" | "web_search_call"
                        | "local_shell_call",
                    ) => {
                        let ts = self.timestamp(record, source);
                        let batch = self
                            .codex_batch
                            .get_or_insert_with(|| {
                                format!("{}:batch:line:{}", self.source.source_id, source.line)
                            })
                            .clone();
                        let tool_kind = string(payload, "type").unwrap_or("");
                        let raw_args = match tool_kind {
                            "custom_tool_call" => payload.get("input").map(|v| json!({"input": v})),
                            "web_search_call" | "local_shell_call" => {
                                payload.get("action").cloned()
                            }
                            _ => payload.get("arguments").map(|v| match v.as_str() {
                                Some(raw) => serde_json::from_str(raw)
                                    .unwrap_or_else(|_| Value::String(raw.to_owned())),
                                None => v.clone(),
                            }),
                        };
                        let name = match tool_kind {
                            "web_search_call" => Some("web_search"),
                            "local_shell_call" => Some("shell"),
                            _ => string(payload, "name"),
                        };
                        let index = self.tool_call(
                            string(payload, "call_id").or_else(|| string(payload, "id")),
                            name,
                            raw_args.as_ref(),
                            None,
                            &batch,
                            0,
                            ts,
                            source,
                        );
                        if tool_kind == "web_search_call" {
                            if let Some(index) = index {
                                let status = explicit_status(payload).unwrap_or("unknown");
                                self.ledger.tools[index].status = status.to_owned();
                                if status == "success" || status == "failure" {
                                    self.ledger.tools[index].result = Some(source.clone());
                                    self.ledger.tools[index].end_ms = ts;
                                }
                            }
                        }
                    }
                    Some(
                        "function_call_output"
                        | "custom_tool_call_output"
                        | "local_shell_call_output",
                    ) => {
                        let ts = self.timestamp(record, source);
                        // Structured result JSON is a harness envelope, not arbitrary stdout.
                        let structured = payload
                            .get("output")
                            .and_then(Value::as_str)
                            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                            .filter(|v| v.get("metadata").is_some() && v.get("output").is_some());
                        self.tool_result(
                            string(payload, "call_id"),
                            payload.get("output"),
                            payload,
                            structured.as_ref(),
                            ts,
                            source,
                        );
                        self.codex_batch = None;
                    }
                    Some("message") if string(payload, "role") == Some("user") => {
                        self.codex_batch = None
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn codex_usage(&mut self, record: &Value, payload: &Value, source: &SourceRef) {
        let ts_ms = self.timestamp(record, source);
        let total = payload.pointer("/info/total_token_usage");
        let counters = total.and_then(|v| {
            Some(Counters {
                input: v.get("input_tokens")?.as_u64()?,
                cached: v
                    .get("cached_input_tokens")
                    .map(Value::as_u64)
                    .unwrap_or(Some(0))?,
                output: v.get("output_tokens")?.as_u64()?,
            })
        });
        let mut usage = Usage::default();
        let mut complete = false;
        let mut basis = "unattributed";
        if let Some(current) = counters.filter(|c| c.cached <= c.input) {
            if self.cumulative == Some(current) {
                return;
            }
            let reset = self.cumulative.is_some_and(|old| {
                current.input < old.input
                    || current.cached < old.cached
                    || current.output < old.output
            });
            let delta = match self.cumulative.filter(|_| !reset) {
                Some(old) => Counters {
                    input: current.input - old.input,
                    cached: current.cached - old.cached,
                    output: current.output - old.output,
                },
                None => current,
            };
            self.cumulative = Some(current);
            if reset {
                self.diagnostic("cumulative_reset", "Codex cumulative usage decreased; the new counter epoch is counted without negative or saturated deltas", source);
            }
            basis = "cumulative_delta";
            if delta.cached <= delta.input {
                usage.input_tokens = delta.input - delta.cached;
                usage.cache_read_tokens = delta.cached;
                usage.output_tokens = delta.output;
                complete = true;
            } else {
                self.diagnostic("invalid_usage_delta", "Cached input delta exceeds input delta; usage and price for this interval are unknown", source);
            }
        } else {
            self.diagnostic(
                if total.is_some_and(|v| !v.is_null()) { "invalid_usage" } else { "missing_cumulative_usage" },
                "Codex cumulative counters are missing or invalid; last_token_usage is not billed again and the previous valid baseline is retained",
                source,
            );
        }
        let estimated_usd = complete
            .then(|| estimate_cost_strict(&self.model, &usage))
            .flatten();
        self.ledger.observations.push(UsageObservation {
            id: format!("{}:usage:line:{}", self.source.source_id, source.line),
            session_id: self.source.source_id.clone(),
            request_id: None,
            ts_ms,
            model: self.model.clone(),
            basis: basis.to_owned(),
            usage,
            cache_counters_complete: false,
            reported_usd: None,
            estimated_usd,
            pricing_version: estimated_usd.map(|_| PRICING_VERSION.to_owned()),
            source: source.clone(),
        });
        if complete && estimated_usd.is_none() {
            self.diagnostic(
                "unknown_pricing",
                "Observed token usage has no supported model price",
                source,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn tool_call(
        &mut self,
        raw_id: Option<&str>,
        name: Option<&str>,
        args: Option<&Value>,
        request_id: Option<String>,
        batch: &str,
        position: usize,
        ts: Option<i64>,
        source: &SourceRef,
    ) -> Option<usize> {
        let Some(name) = name else {
            self.diagnostic("invalid_tool_call", "Tool call has no tool name", source);
            return None;
        };
        let null = Value::Null;
        let args = args.unwrap_or(&null);
        if args.is_null() {
            self.diagnostic(
                "missing_tool_arguments",
                "Tool call has no arguments; its identity is incomplete",
                source,
            );
        }
        let (operation, object, args_fingerprint) =
            semantic_identity(name, args, self.cwd.as_deref());
        let id = raw_id
            .map(|id| {
                format!(
                    "{}:tool:{}",
                    self.source.source_id,
                    fingerprint(id.as_bytes())
                )
            })
            .unwrap_or_else(|| {
                format!(
                    "{}:tool:line:{}:{position}",
                    self.source.source_id, source.line
                )
            });
        if let Some(&index) = self.tools.get(&id) {
            let existing = &mut self.ledger.tools[index];
            if existing.batch_id == batch && existing.result.is_none() {
                if existing.args_fingerprint != args_fingerprint {
                    existing.call = source.clone();
                    existing.script =
                        super::script_generation::metadata(name, args, self.cwd.as_deref());
                }
                existing.operation = operation;
                existing.object = object;
                existing.args_fingerprint = args_fingerprint;
                existing.start_ms = existing.start_ms.or(ts);
            } else if existing.args_fingerprint != args_fingerprint {
                self.diagnostic("conflicting_tool_identity", "A repeated tool-call ID has different arguments; the original execution was retained", source);
            }
            return Some(index);
        }
        if raw_id.is_none() {
            self.diagnostic(
                "missing_tool_identity",
                "Tool call has no provider call ID; its result cannot be matched by position",
                source,
            );
        }
        let index = self.ledger.tools.len();
        self.tools.insert(id.clone(), index);
        self.ledger.tools.push(ToolExecution {
            id,
            session_id: self.source.source_id.clone(),
            request_id,
            batch_id: batch.to_owned(),
            name: name.to_owned(),
            operation,
            object,
            args_fingerprint,
            script: super::script_generation::metadata(name, args, self.cwd.as_deref()),
            start_ms: ts,
            end_ms: None,
            status: "pending".to_owned(),
            result_bytes: 0,
            result_fingerprint: None,
            call: source.clone(),
            result: None,
        });
        Some(index)
    }

    fn tool_result(
        &mut self,
        raw_id: Option<&str>,
        body: Option<&Value>,
        envelope: &Value,
        details: Option<&Value>,
        ts: Option<i64>,
        source: &SourceRef,
    ) {
        let index = raw_id
            .map(|id| {
                format!(
                    "{}:tool:{}",
                    self.source.source_id,
                    fingerprint(id.as_bytes())
                )
            })
            .and_then(|id| self.tools.get(&id).copied());
        let Some(index) = index else {
            self.diagnostic("orphan_tool_result", "Tool result has no preceding matching call; it was not attached to another execution", source);
            return;
        };
        let tool = &self.ledger.tools[index];
        let bytes = result_bytes(body);
        let result_fingerprint = fingerprint(&bytes);
        let status = result_status(envelope, details, body, is_shell(&tool.name));
        if tool.result.is_some() && matches!(tool.status.as_str(), "success" | "failure") {
            if tool.result_fingerprint.as_deref() != Some(result_fingerprint.as_str()) {
                self.diagnostic("conflicting_tool_result", "Repeated tool result differs from an already terminal result; original evidence was retained", source);
            }
            return;
        }
        if ts
            .zip(tool.start_ms)
            .is_some_and(|(end, start)| end < start)
        {
            self.diagnostic("non_monotonic_tool_time", "Tool result timestamp precedes its call; file chronology was retained without manufacturing a duration", source);
        }
        let tool = &mut self.ledger.tools[index];
        tool.end_ms = ts;
        tool.status = status.to_owned();
        tool.result_bytes = bytes.len() as u64;
        tool.result_fingerprint = Some(result_fingerprint);
        tool.result = Some(source.clone());
    }

    fn finish(mut self) -> Ledger {
        for (_, snapshot) in std::mem::take(&mut self.snapshots) {
            let index = snapshot.observation;
            let source = self.ledger.observations[index].source.clone();
            let Some(raw) = snapshot.raw_usage else {
                self.diagnostic(
                    "missing_usage",
                    "Assistant record has no usage; its monetary cost is unknown, not zero",
                    &source,
                );
                continue;
            };
            let (usage, complete, cache_complete) = assistant_usage(&raw, self.provider);
            let reported = usage.cost_override;
            let observation = &mut self.ledger.observations[index];
            observation.usage = usage;
            observation.cache_counters_complete = observation.basis == "request" && cache_complete;
            observation.reported_usd = reported;
            if reported.is_none() && complete {
                observation.estimated_usd = estimate_cost_strict(&observation.model, &usage);
                observation.pricing_version = observation
                    .estimated_usd
                    .map(|_| PRICING_VERSION.to_owned());
            }
            let unknown_price =
                reported.is_none() && complete && observation.estimated_usd.is_none();
            if !complete {
                self.diagnostic("incomplete_usage", "Usage omits or invalidates required token counters; absent counters are not treated as a complete zero-cost request", &source);
            }
            if unknown_price {
                self.diagnostic(
                    "unknown_pricing",
                    "Observed token usage has no supported model price",
                    &source,
                );
            }
        }
        if self.provider == Agent::Codex
            && self.ledger.observations.is_empty()
            && !self.ledger.tools.is_empty()
        {
            let first = &self.ledger.tools[0];
            let source = first.call.clone();
            self.ledger.observations.push(UsageObservation {
                id: format!("{}:usage:unavailable", self.source.source_id),
                session_id: self.source.source_id.clone(),
                request_id: None,
                ts_ms: first.start_ms,
                model: self.model.clone(),
                basis: "unattributed".to_owned(),
                usage: Usage::default(),
                cache_counters_complete: false,
                reported_usd: None,
                estimated_usd: None,
                pricing_version: None,
                source: source.clone(),
            });
            self.diagnostic("missing_usage", "Codex tool activity has no cumulative usage observations; its cost and request count are unknown", &source);
        }
        if !self.saw_record {
            let source = self.source.clone();
            self.diagnostic(
                "empty_source",
                "Source contains no valid JSON records",
                &source,
            );
        }
        if self.ledger.sessions[0].project.is_empty() {
            let source = self.source.clone();
            self.diagnostic("missing_project", "No transcript cwd or project override is available; relative paths cannot be resolved", &source);
        }
        self.ledger.diagnostics.sort_by(|a, b| {
            a.source
                .as_ref()
                .map(|s| s.line)
                .cmp(&b.source.as_ref().map(|s| s.line))
                .then_with(|| a.code.cmp(&b.code))
        });
        self.ledger
    }
}

fn string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn request_status(message: &Value) -> &'static str {
    match string(message, "stopReason").or_else(|| string(message, "stop_reason")) {
        Some("error" | "aborted" | "cancelled") => "failure",
        Some(
            "stop" | "end_turn" | "toolUse" | "tool_use" | "length" | "max_tokens"
            | "stop_sequence",
        ) => "success",
        _ => explicit_status(message).unwrap_or("unknown"),
    }
}

// Streaming usage fields are cumulative within a response. Per-field maxima
// prevent an interleaved duplicate of an earlier snapshot from reducing billing.
fn merge_snapshot(previous: &mut Value, current: &Value) -> bool {
    let mut changed = false;
    if let (Some(previous), Some(current)) = (previous.as_object_mut(), current.as_object()) {
        for (key, value) in current {
            if value.is_null() {
                continue;
            }
            match previous.get_mut(key) {
                Some(old) if old.is_object() && value.is_object() => {
                    changed |= merge_snapshot(old, value);
                }
                Some(old) => {
                    if old == value
                        || old
                            .as_f64()
                            .zip(value.as_f64())
                            .is_some_and(|(old, new)| old > new)
                    {
                        continue;
                    }
                    *old = value.clone();
                    changed = true;
                }
                None => {
                    previous.insert(key.clone(), value.clone());
                    changed = true;
                }
            }
        }
    }
    changed
}

fn assistant_usage(raw: &Value, provider: Agent) -> (Usage, bool, bool) {
    let mut usage = Usage::default();
    let (input, output, cache_read, cache_write) = if provider == Agent::Omp {
        ("input", "output", "cacheRead", "cacheWrite")
    } else {
        (
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        )
    };
    let read = |key: &str| raw.get(key).and_then(Value::as_u64);
    usage.input_tokens = read(input).unwrap_or(0);
    usage.output_tokens = read(output).unwrap_or(0);
    usage.cache_read_tokens = read(cache_read).unwrap_or(0);
    usage.cache_creation_tokens = read(cache_write).unwrap_or(0);
    let mut complete = read(input).is_some()
        && read(output).is_some()
        && [cache_read, cache_write]
            .iter()
            .all(|key| raw.get(*key).is_none() || read(key).is_some());
    // Billing historically permits omitted cache categories. Cache analysis
    // requires explicit categories so absence cannot become a zero-hit claim.
    let mut cache_complete =
        read(input).is_some() && read(cache_read).is_some() && read(cache_write).is_some();
    if provider == Agent::Omp {
        usage.cost_override = raw
            .pointer("/cost/total")
            .and_then(Value::as_f64)
            .filter(|c| c.is_finite() && *c >= 0.0);
        // omp's optional cttl partitions cacheWrite; it is not additive.
        if let Some(cttl) = raw.get("cttl").filter(|v| !v.is_null()) {
            let one_hour = cttl
                .get("ephemeral1h")
                .map(Value::as_u64)
                .unwrap_or(Some(0));
            if let Some(one_hour) =
                one_hour.filter(|h| cttl.is_object() && *h <= usage.cache_creation_tokens)
            {
                usage.cache_creation_1h_tokens = one_hour;
                usage.cache_creation_5m_tokens = usage.cache_creation_tokens - one_hour;
                usage.cache_creation_tokens = 0;
            } else {
                complete = false;
            }
        }
    } else {
        if let Some(cache) = raw.get("cache_creation").filter(|v| !v.is_null()) {
            let five = cache
                .get("ephemeral_5m_input_tokens")
                .and_then(Value::as_u64);
            let hour = cache
                .get("ephemeral_1h_input_tokens")
                .and_then(Value::as_u64);
            let split_total = five.zip(hour).and_then(|(a, b)| a.checked_add(b));
            let aggregate = read(cache_write);
            if split_total.is_some()
                && match aggregate {
                    Some(total) => Some(total) == split_total,
                    None => true,
                }
            {
                usage.cache_creation_5m_tokens = five.unwrap_or(0);
                usage.cache_creation_1h_tokens = hour.unwrap_or(0);
                usage.cache_creation_tokens = 0;
                cache_complete = read(input).is_some() && read(cache_read).is_some();
            } else {
                complete = false;
                // Keep an independently reported aggregate when the TTL split
                // is invalid/incomplete. Never count the same tokens twice.
                if aggregate.is_none() {
                    usage.cache_creation_5m_tokens = five.unwrap_or(0);
                    usage.cache_creation_1h_tokens = hour.unwrap_or(0);
                }
            }
        }
        if let Some(search) = raw.pointer("/server_tool_use/web_search_requests") {
            usage.web_search_calls = search.as_u64().unwrap_or(0);
            complete &= search.as_u64().is_some();
        }
    }
    (usage, complete, complete && cache_complete)
}

fn canonical_tool(name: &str) -> &str {
    name.strip_prefix("functions.")
        .or_else(|| name.strip_prefix("function."))
        .unwrap_or(name)
}

fn is_shell(name: &str) -> bool {
    matches!(
        canonical_tool(name).to_ascii_lowercase().as_str(),
        "bash" | "shell" | "shell_command" | "exec_command" | "local_shell" | "write_stdin"
    )
}

pub(super) fn resolve_path(path: &str, cwd: Option<&str>) -> Option<String> {
    if path.is_empty() || path.starts_with('~') {
        return None;
    }
    if path.contains("://") && !has_drive_prefix(path) {
        return Some(path.to_owned());
    }
    if source_absolute(path) {
        return normalize_source_absolute(path);
    }
    // Drive-relative C:foo and root-relative \foo depend on source-host state.
    if has_drive_prefix(path) || path.starts_with('\\') {
        return None;
    }
    let cwd = cwd.filter(|p| source_absolute(p))?;
    normalize_source_absolute(&format!("{}/{path}", cwd.trim_end_matches('/')))
}

fn has_drive_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn source_absolute(path: &str) -> bool {
    path.starts_with('/')
        || path.starts_with("\\\\")
        || (has_drive_prefix(path) && matches!(path.as_bytes().get(2), Some(b'/' | b'\\')))
}

// Source paths are lexical data, not paths on the machine running auditui.
// Keep case: neither drive-letter nor file case equivalence is assumed.
fn normalize_source_absolute(path: &str) -> Option<String> {
    let windows = has_drive_prefix(path) || path.starts_with("\\\\") || path.starts_with("//");
    let path = if windows {
        Cow::Owned(path.replace('\\', "/"))
    } else {
        Cow::Borrowed(path)
    };
    let (prefix, body, protected) = if has_drive_prefix(&path) {
        (&path[..3], &path[3..], 0)
    } else if let Some(body) = path.strip_prefix("//") {
        ("//", body, 2)
    } else {
        ("/", path.strip_prefix('/')?, 0)
    };
    let mut parts = Vec::new();
    for part in body.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.len() > protected {
                    parts.pop();
                }
            }
            _ => parts.push(part),
        }
    }
    if parts.len() < protected {
        return None;
    }
    Some(format!("{prefix}{}", parts.join("/")))
}

fn semantic_identity(
    name: &str,
    args: &Value,
    cwd: Option<&str>,
) -> (String, Option<String>, String) {
    let tool = canonical_tool(name).to_ascii_lowercase();
    let mut normalized = args.clone();
    let known_metadata = matches!(
        tool.as_str(),
        "bash"
            | "shell"
            | "shell_command"
            | "exec_command"
            | "read"
            | "write"
            | "edit"
            | "glob"
            | "grep"
            | "eval"
            | "hub"
            | "task"
            | "web_search"
            | "yield"
    );
    let effective_cwd = match ["cwd", "workdir"].iter().find_map(|key| string(args, key)) {
        Some(path) => resolve_path(path, cwd),
        None => cwd.map(str::to_owned),
    };
    let mut operation = tool.clone();
    for key in ["op", "action", "language"] {
        if let Some(value) = string(args, key) {
            operation.push(':');
            operation.push_str(value);
        }
    }
    let mut object = None;
    if let Some(map) = normalized.as_object_mut() {
        if known_metadata {
            map.remove("i");
            map.remove("description");
        }
        for key in ["path", "file_path", "file", "cwd", "workdir"] {
            if let Some(raw) = map.get(key).and_then(Value::as_str) {
                let base = if matches!(key, "cwd" | "workdir") {
                    cwd
                } else {
                    effective_cwd.as_deref()
                };
                if let Some(resolved) = resolve_path(raw, base) {
                    if !matches!(key, "cwd" | "workdir") && object.is_none() {
                        object = Some(resolved.clone());
                    }
                    map.insert(key.to_owned(), Value::String(resolved));
                }
            }
        }
        // The resolved cwd is represented once in the enclosing fingerprint.
        // An unresolved explicit cwd must remain a meaningful argument.
        if effective_cwd.is_some() {
            map.remove("cwd");
            map.remove("workdir");
        }
    }
    if is_shell(name) || tool == "eval" {
        let program = ["command", "cmd", "code", "input"]
            .iter()
            .find_map(|key| args.get(*key));
        if let Some(program) = program {
            let identity = fingerprint(&canonical_bytes(program));
            object = effective_cwd
                .as_ref()
                .map(|cwd| format!("cwd:{cwd}:command:{identity}"));
        }
    }
    if object.is_none() {
        object = ["url", "query"]
            .iter()
            .find_map(|key| string(args, key))
            .map(|value| format!("target:{}", fingerprint(value.as_bytes())));
    }
    // Include effective cwd even if omitted by one caller. Never normalize code,
    // flags, timeout values or nested payload descriptions into generic placeholders.
    let bytes =
        canonical_bytes(&json!({"tool": tool, "cwd": effective_cwd, "arguments": normalized}));
    (operation, object, fingerprint(&bytes))
}

fn canonical_bytes(value: &Value) -> Vec<u8> {
    // serde_json::Map is a BTreeMap in this crate (no preserve_order feature).
    serde_json::to_vec(value).expect("JSON values serialize")
}

fn result_bytes(body: Option<&Value>) -> Cow<'_, [u8]> {
    match body {
        Some(Value::String(text)) => Cow::Borrowed(text.as_bytes()),
        Some(Value::Array(parts))
            if parts.iter().all(|p| {
                string(p, "type") == Some("text") && p.get("text").is_some_and(Value::is_string)
            }) =>
        {
            Cow::Owned(
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
                    .into_bytes(),
            )
        }
        Some(value) => Cow::Owned(canonical_bytes(value)),
        None => Cow::Borrowed(&[]),
    }
}

fn explicit_status(value: &Value) -> Option<&'static str> {
    for key in ["is_error", "isError"] {
        if let Some(error) = value.get(key).and_then(Value::as_bool) {
            return Some(if error { "failure" } else { "success" });
        }
    }
    if let Some(success) = value.get("success").and_then(Value::as_bool) {
        return Some(if success { "success" } else { "failure" });
    }
    match string(value, "status") {
        Some("success" | "completed" | "succeeded") => Some("success"),
        Some("failure" | "failed" | "error" | "cancelled" | "aborted") => Some("failure"),
        Some("pending" | "running" | "in_progress") => Some("pending"),
        _ => None,
    }
}

fn exit_code(value: &Value) -> Option<i64> {
    [
        "/exit_code",
        "/exitCode",
        "/metadata/exit_code",
        "/metadata/exitCode",
        "/details/exitCode",
        "/details/exit_code",
    ]
    .iter()
    .find_map(|pointer| value.pointer(pointer).and_then(Value::as_i64))
}

fn result_status(
    envelope: &Value,
    details: Option<&Value>,
    body: Option<&Value>,
    shell: bool,
) -> &'static str {
    let explicit = explicit_status(envelope).or_else(|| details.and_then(explicit_status));
    if explicit == Some("failure") {
        return "failure";
    }
    if let Some(code) = exit_code(envelope).or_else(|| details.and_then(exit_code)) {
        return if code == 0 { "success" } else { "failure" };
    }
    if let Some(status) = explicit {
        return status;
    }
    if shell {
        if let Some(status) = body.and_then(Value::as_str).and_then(shell_envelope_status) {
            return status;
        }
    }
    "unknown"
}

// Only the header before Output:/Final output: is metadata. Error strings and
// exit-code lookalikes in file contents/stdout never classify an execution.
fn shell_envelope_status(text: &str) -> Option<&'static str> {
    let mut header = false;
    let mut status = None;
    for line in text.lines() {
        if line == "Output:" || line == "Final output:" {
            return header.then_some(status).flatten();
        }
        if line.starts_with("Chunk ID: ") || line.starts_with("Wall time: ") {
            header = true;
        } else if let Some(code) = line
            .strip_prefix("Process exited with code ")
            .or_else(|| line.strip_prefix("Exit code: "))
        {
            status = code
                .parse::<i64>()
                .ok()
                .map(|c| if c == 0 { "success" } else { "failure" });
        } else if line.starts_with("Process running with session ID ") {
            status = Some("pending");
        } else if !line.starts_with("Original token count: ") && !line.is_empty() {
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn fixture(name: &str, provider: Agent) -> Ledger {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        parse_file(&path, provider, None, None).unwrap()
    }

    struct TempSource(PathBuf);

    impl TempSource {
        fn new(bytes: &[u8]) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "auditui-ledger-parse-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&dir).unwrap();
            let path = dir.join("source.jsonl");
            std::fs::write(&path, bytes).unwrap();
            Self(path)
        }
    }

    impl Drop for TempSource {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
        }
    }

    #[test]
    fn claude_streaming_requests_usage_and_tools_are_deduplicated_across_interleaving() {
        let ledger = fixture("ledger_claude.jsonl", Agent::Claude);
        assert_eq!(ledger.requests.len(), 3);
        assert_eq!(ledger.observations.len(), 3);
        assert_eq!(ledger.tools.len(), 3);
        let usage = &ledger.observations[0];
        assert_eq!(usage.usage.input_tokens, 100);
        assert_eq!(usage.usage.output_tokens, 10);
        assert_eq!(usage.usage.cache_read_tokens, 20);
        assert_eq!(usage.usage.cache_creation_5m_tokens, 10);
        assert_eq!(usage.usage.cache_creation_1h_tokens, 20);
        assert_eq!(usage.usage.cache_creation_total(), 30);
        assert_eq!(
            usage.source.line, 5,
            "old duplicate must not move billing to a later timestamp"
        );
        assert_eq!(usage.ts_ms, Some(1779530404000));
        assert!(usage.estimated_usd.is_some());
        assert!(usage.reported_usd.is_none());
        assert_eq!(ledger.tools[0].batch_id, ledger.tools[1].batch_id);
        assert_ne!(ledger.tools[0].batch_id, ledger.tools[2].batch_id);
        assert_eq!(
            ledger.tools[0].object.as_deref(),
            Some("/workspace/demo/file.rs")
        );
        assert_eq!(ledger.tools[0].status, "success");
        assert_eq!(ledger.tools[1].status, "success");
        assert_eq!(ledger.tools[2].status, "pending");
        assert_eq!(ledger.tools[0].call.line, 2);
        assert_eq!(ledger.tools[0].result.as_ref().unwrap().line, 4);
        assert_eq!(
            ledger
                .contexts
                .iter()
                .filter(|c| c.kind == "compaction")
                .count(),
            1
        );
        let missing = &ledger.observations[2];
        assert!(missing.ts_ms.is_none());
        assert!(missing.reported_usd.is_none());
        assert!(missing.estimated_usd.is_none());
        assert!(ledger.diagnostics.iter().any(|d| d.code == "missing_usage"));
        assert!(ledger
            .diagnostics
            .iter()
            .any(|d| d.code == "missing_timestamp"));
    }

    #[test]
    fn codex_cumulative_deltas_partition_cached_input_and_reset_without_fake_requests() {
        let ledger = fixture("ledger_codex.jsonl", Agent::Codex);
        assert!(ledger.requests.is_empty());
        assert!(ledger.observations.iter().all(|o| o.request_id.is_none()));
        assert!(ledger
            .observations
            .iter()
            .all(|o| !o.cache_counters_complete));
        assert!(ledger.tools.iter().all(|t| t.request_id.is_none()));
        assert_eq!(ledger.observations.len(), 5);
        let known: Vec<_> = ledger
            .observations
            .iter()
            .filter(|o| o.basis == "cumulative_delta")
            .collect();
        assert_eq!(known.iter().map(|o| o.usage.input_tokens).sum::<u64>(), 123);
        assert_eq!(
            known.iter().map(|o| o.usage.cache_read_tokens).sum::<u64>(),
            57
        );
        assert_eq!(known.iter().map(|o| o.usage.output_tokens).sum::<u64>(), 31);
        assert_eq!(
            known[2].usage.input_tokens, 25,
            "counter reset starts a new epoch"
        );
        assert_eq!(
            known[3].usage.input_tokens, 8,
            "missing counters do not erase the prior baseline"
        );
        assert!(ledger.observations[3].estimated_usd.is_none());
        assert_eq!(ledger.observations[3].basis, "unattributed");
        assert_eq!(ledger.tools[0].batch_id, ledger.tools[1].batch_id);
        assert_ne!(ledger.tools[0].batch_id, ledger.tools[2].batch_id);
        assert_eq!(ledger.tools[0].status, "failure");
        assert_eq!(ledger.tools[1].status, "success");
        assert_eq!(ledger.tools[2].status, "success");
        assert_ne!(
            ledger.tools[0].args_fingerprint,
            ledger.tools[1].args_fingerprint
        );
        assert_eq!(
            ledger.tools[0].args_fingerprint,
            ledger.tools[2].args_fingerprint
        );
        assert!(ledger
            .diagnostics
            .iter()
            .any(|d| d.code == "cumulative_reset"));
        assert!(ledger
            .diagnostics
            .iter()
            .any(|d| d.code == "missing_cumulative_usage"));
    }

    #[test]
    fn omp_uses_response_identity_message_time_and_reported_fee_not_entry_identity() {
        let ledger = fixture("ledger_omp.jsonl", Agent::Omp);
        assert_eq!(ledger.requests.len(), 2);
        assert_eq!(ledger.observations.len(), 3);
        assert_eq!(ledger.tools.len(), 2);
        let observed = &ledger.observations[0];
        assert_eq!(observed.reported_usd, Some(0.15));
        assert_eq!(observed.usage.cost_override, Some(0.15));
        assert!(observed.estimated_usd.is_none());
        assert_eq!(observed.ts_ms, Some(1779530401000));
        assert_eq!(observed.usage.cache_creation_5m_tokens, 2);
        assert_eq!(observed.usage.cache_creation_1h_tokens, 8);
        assert_eq!(observed.usage.cache_creation_total(), 10);
        assert_eq!(observed.basis, "request");
        assert_eq!(ledger.tools[0].start_ms, Some(1779530401000));
        assert_eq!(ledger.tools[0].end_ms, Some(1779530402000));
        assert_eq!(ledger.tools[0].status, "success");
        assert_eq!(ledger.tools[1].status, "failure");
        assert_eq!(ledger.tools[0].batch_id, ledger.tools[1].batch_id);
        assert_eq!(ledger.observations[1].reported_usd, Some(0.04));
        assert!(ledger.observations[1].request_id.is_none());
        assert_eq!(ledger.observations[1].basis, "unattributed");
        assert!(ledger.observations[2].reported_usd.is_none());
        assert!(ledger.observations[2].estimated_usd.is_none());
        assert!(ledger.observations[2].ts_ms.is_none());
    }

    #[test]
    fn source_identity_is_path_scoped_stable_across_same_length_changes_and_canonical_aliases() {
        let bytes = b"{\"type\":\"session\",\"id\":\"same-id\",\"cwd\":\"/project/a\"}\n";
        let source = TempSource::new(bytes);
        let copied = TempSource::new(bytes);
        let first = parse_file(&source.0, Agent::Omp, None, Some("parent")).unwrap();
        let copy = parse_file(&copied.0, Agent::Omp, None, None).unwrap();
        assert_ne!(first.sessions[0].id, copy.sessions[0].id);
        assert_eq!(first.sessions[0].source.version, fingerprint(bytes));
        assert_eq!(
            first.sessions[0].source.version,
            copy.sessions[0].source.version
        );
        let alias = source.0.parent().unwrap().join(".").join("source.jsonl");
        assert_eq!(
            first.sessions[0].id,
            parse_file(&alias, Agent::Omp, None, None).unwrap().sessions[0].id
        );
        std::fs::write(
            &source.0,
            b"{\"type\":\"session\",\"id\":\"same-id\",\"cwd\":\"/project/b\"}\n",
        )
        .unwrap();
        let updated = parse_file(&source.0, Agent::Omp, None, None).unwrap();
        assert_eq!(first.sessions[0].id, updated.sessions[0].id);
        assert_ne!(
            first.sessions[0].source.version,
            updated.sessions[0].source.version
        );
        assert_eq!(first.sessions[0].source.line, 1);
        assert_eq!(first.sessions[0].parent_id.as_deref(), Some("parent"));
        assert_eq!(updated.sessions[0].project, "/project/b");
    }

    #[test]
    fn synthetic_assistants_are_excluded_with_explicit_coverage_evidence() {
        let source = TempSource::new(b"{\"type\":\"assistant\",\"cwd\":\"/project\",\"timestamp\":\"2026-05-23T10:00:00Z\",\"message\":{\"id\":\"real-response\",\"model\":\"claude-sonnet-4-20250514\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1},\"content\":[]}}\n{\"type\":\"assistant\",\"uuid\":\"synthetic-record\",\"requestId\":\"synthetic-request\",\"timestamp\":\"2026-05-23T10:00:01Z\",\"message\":{\"id\":\"synthetic-response\",\"model\":\"<synthetic>\",\"usage\":{\"input_tokens\":0,\"output_tokens\":0},\"content\":[]}}\n");
        let ledger = parse_file(&source.0, Agent::Claude, None, None).unwrap();
        assert_eq!(ledger.requests.len(), 1);
        assert_eq!(ledger.observations.len(), 1);
        assert_eq!(ledger.observations[0].usage.input_tokens, 10);
        let excluded: Vec<_> = ledger
            .diagnostics
            .iter()
            .filter(|d| d.code == "synthetic_request_excluded")
            .collect();
        assert_eq!(excluded.len(), 1);
        assert_eq!(excluded[0].source.as_ref().unwrap().line, 2);
        assert_eq!(
            excluded[0].source.as_ref().unwrap().record_id.as_deref(),
            Some("synthetic-record")
        );
    }

    #[test]
    fn malformed_partial_and_invalid_records_preserve_line_evidence_without_content_leaks() {
        let source = TempSource::new(b"{\"type\":\"session\",\"id\":\"session\",\"cwd\":\"/project\"}\nnot-json-secret-value\n42\n{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"responseId\":\"r\",\"content\":[]}}\n{\"secret\":\"unfinished");
        let ledger = parse_file(&source.0, Agent::Omp, None, None).unwrap();
        let errors: Vec<_> = ledger
            .diagnostics
            .iter()
            .filter(|d| {
                matches!(
                    d.code.as_str(),
                    "malformed_record" | "invalid_record" | "partial_record"
                )
            })
            .map(|d| (d.code.as_str(), d.source.as_ref().unwrap().line))
            .collect();
        assert_eq!(
            errors,
            vec![
                ("malformed_record", 2),
                ("invalid_record", 3),
                ("partial_record", 5)
            ]
        );
        assert!(!serde_json::to_string(&ledger.diagnostics)
            .unwrap()
            .contains("secret-value"));
        assert!(!serde_json::to_string(&ledger.diagnostics)
            .unwrap()
            .contains("unfinished"));
        assert!(ledger.observations[0].estimated_usd.is_none());
        assert!(ledger.observations[0].ts_ms.is_none());
        assert_eq!(ledger.observations[0].source.line, 4);
    }

    #[test]
    fn semantic_identity_keeps_scripts_flags_and_payloads_but_not_descriptive_metadata() {
        let identity =
            |name: &str, args: Value, cwd: Option<&str>| semantic_identity(name, &args, cwd);
        let a = identity(
            "functions.bash",
            json!({"command":"python -c 'print(1)'","i":"First description"}),
            Some("/project"),
        );
        let b = identity(
            "Bash",
            json!({"command":"python -c 'print(1)'","cwd":"/project","description":"Second description"}),
            Some("/project"),
        );
        assert_eq!(a, b);
        let script = identity(
            "bash",
            json!({"command":"python -c 'print(2)'"}),
            Some("/project"),
        );
        assert_ne!(a.1, script.1);
        assert_ne!(a.2, script.2);
        let flags = identity(
            "bash",
            json!({"command":"python -c 'print(1)'","timeout":30}),
            Some("/project"),
        );
        assert_ne!(a.2, flags.2);
        assert_eq!(a.1, flags.1);
        let nested_a = identity(
            "write",
            json!({"path":"a.json","content":{"description":"business A"}}),
            Some("/project"),
        );
        let nested_b = identity(
            "write",
            json!({"path":"a.json","content":{"description":"business B"}}),
            Some("/project"),
        );
        assert_ne!(nested_a.2, nested_b.2);
        let custom_a = identity(
            "custom",
            json!({"description":"business A"}),
            Some("/project"),
        );
        let custom_b = identity(
            "custom",
            json!({"description":"business B"}),
            Some("/project"),
        );
        assert_ne!(custom_a.2, custom_b.2);
        let relative = identity("read", json!({"path":"src/../a.rs"}), Some("/project"));
        let absolute = identity(
            "read",
            json!({"path":"/project/a.rs","i":"Explain the read"}),
            Some("/project"),
        );
        assert_eq!(relative, absolute);
        assert_eq!(relative.1.as_deref(), Some("/project/a.rs"));
        assert!(identity("read", json!({"path":"a.rs"}), None).1.is_none());
        assert_ne!(
            identity("read", json!({"path":"a.rs"}), Some("/other")).1,
            relative.1
        );
    }

    #[test]
    fn tool_failures_require_structured_status_or_shell_envelope_not_stdout_words() {
        let text = json!("Error: reference text\nProcess exited with code 1");
        assert_eq!(
            result_status(&json!({"isError":false}), None, Some(&text), true),
            "success"
        );
        assert_eq!(
            result_status(&json!({}), None, Some(&text), true),
            "unknown"
        );
        assert_eq!(
            result_status(&json!({}), None, Some(&text), false),
            "unknown"
        );
        assert_eq!(
            result_status(
                &json!({"isError":false}),
                Some(&json!({"exitCode":7})),
                Some(&text),
                true
            ),
            "failure"
        );
        assert_eq!(
            result_status(
                &json!({}),
                Some(&json!({"metadata":{"exit_code":0}})),
                Some(&text),
                true
            ),
            "success"
        );
        assert_eq!(shell_envelope_status("Chunk ID: x\nWall time: 2 seconds\nProcess running with session ID 123\nOutput:\nError: still running"), Some("pending"));
        assert_eq!(shell_envelope_status("Chunk ID: x\nWall time: 2 seconds\nProcess exited with code 0\nOutput:\nProcess exited with code 9"), Some("success"));
        assert_eq!(result_bytes(Some(&json!([{"type":"text","text":"a"},{"type":"text","text":""},{"type":"text","text":"b"}]))).as_ref(), b"a\n\nb");
    }

    #[test]
    fn missing_codex_usage_and_orphan_results_are_not_success_or_free_requests() {
        let source = TempSource::new(b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"s\",\"cwd\":\"/project\"}}\n{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"call_id\":\"before-call\",\"output\":\"Error: example\"}}\n{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"call_id\":\"before-call\",\"name\":\"exec_command\",\"arguments\":\"{\\\"cmd\\\":\\\"pwd\\\"}\"}}\n");
        let ledger = parse_file(&source.0, Agent::Codex, None, None).unwrap();
        assert!(ledger.requests.is_empty());
        assert_eq!(ledger.tools[0].status, "pending");
        assert!(ledger.tools[0].result.is_none());
        assert!(ledger.tools[0].start_ms.is_none());
        assert_eq!(ledger.observations.len(), 1);
        assert!(ledger.observations[0].estimated_usd.is_none());
        assert!(ledger
            .diagnostics
            .iter()
            .any(|d| d.code == "orphan_tool_result"));
        assert!(ledger.diagnostics.iter().any(|d| d.code == "missing_usage"));
    }

    #[test]
    fn streamed_script_evidence_tracks_changed_arguments_not_replayed_snapshots() {
        let body: String = (0..20)
            .map(|index| {
                format!(
                    "value_{index} = \"{}\"\n",
                    "streamed-script-evidence-marker".repeat(3)
                )
            })
            .collect();
        let assistant = |entry: &str, response: &str, call: &str, code: &str| {
            json!({
                "type":"message", "id":entry,
                "message":{
                    "role":"assistant", "responseId":response, "model":"fixture",
                    "usage":{"input":10,"output":10,"cacheRead":0,"cacheWrite":0,"cost":{"total":0.01}},
                    "content":[{"type":"toolCall","id":call,"name":"write",
                        "arguments":{"path":"script.py","content":code}}]
                }
            })
        };
        let records = [
            json!({"type":"session","id":"streaming","cwd":"/workspace"}),
            assistant("partial", "response-a", "write-a", "value_0 = 1\n"),
            assistant("complete", "response-a", "write-a", &body),
            assistant("replay", "response-a", "write-a", &body),
            json!({"type":"message","id":"result","message":{
                "role":"toolResult","toolCallId":"write-a","isError":false,
                "content":[{"type":"text","text":"written"}]
            }}),
            assistant("repeat", "response-b", "write-b", &body),
        ];
        let text: String = records
            .into_iter()
            .enumerate()
            .map(|(index, mut record)| {
                record["timestamp"] = json!(index as u64 + 1);
                format!("{record}\n")
            })
            .collect();
        let source = TempSource::new(text.as_bytes());
        let ledger = parse_file(&source.0, Agent::Omp, None, None).unwrap();
        assert_eq!(ledger.tools.len(), 2);
        let first = &ledger.tools[0];
        assert_eq!(
            first.script.as_ref().unwrap().code_fingerprint,
            fingerprint(body.as_bytes())
        );
        assert_eq!(first.call.line, 3);
        assert_eq!(first.call.record_id.as_deref(), Some("complete"));
        let candidates =
            super::super::analysis::candidates(&ledger, &super::super::Query::default());
        let candidate = candidates
            .iter()
            .find(|candidate| candidate.kind == "repeated_script_generation")
            .expect("the final streamed body establishes repetition");
        assert!(candidate.evidence.iter().any(|evidence| evidence.line == 3));
        assert!(!candidate.evidence.iter().any(|evidence| evidence.line == 4));
    }

    #[test]
    fn cache_presence_distinguishes_missing_zero_and_unattributed_usage() {
        let mut parser = Parser::new(
            SourceRef {
                source_id: "cache-presence".into(),
                version: "fixture".into(),
                ..SourceRef::default()
            },
            Agent::Omp,
            Some("/workspace"),
            None,
        );
        let mut usage = json!({"input":20000,"output":10,"cacheWrite":0});
        for (index, cache_read) in [None, Some(json!(0)), Some(Value::Null), Some(json!(0))]
            .into_iter()
            .enumerate()
        {
            if let Some(value) = cache_read {
                usage["cacheRead"] = value;
            }
            let mut message = json!({
                "role":"assistant", "model":"unknown", "usage":usage,
                "content":[], "timestamp":index as u64 + 1
            });
            if index != 3 {
                message["responseId"] = json!(format!("response-{index}"));
            }
            parser.record(
                &json!({"type":"message","id":format!("entry-{index}"),"message":message}),
                index as u64 + 1,
            );
        }
        let ledger = parser.finish();
        assert_eq!(
            ledger
                .observations
                .iter()
                .map(|o| o.cache_counters_complete)
                .collect::<Vec<_>>(),
            vec![false, true, false, false]
        );
        assert!(ledger
            .observations
            .iter()
            .all(|o| o.usage.cache_read_tokens == 0));
    }

    #[test]
    fn cache_presence_requires_a_complete_consistent_write_partition() {
        let mut usage = json!({
            "input_tokens":20000, "output_tokens":10, "cache_read_input_tokens":0,
            "cache_creation":{"ephemeral_5m_input_tokens":4096,"ephemeral_1h_input_tokens":0}
        });
        let (normalized, _, cache_complete) = assistant_usage(&usage, Agent::Claude);
        assert!(cache_complete);
        assert_eq!(normalized.cache_creation_total(), 4096);
        usage["cache_creation_input_tokens"] = json!(8192);
        assert!(!assistant_usage(&usage, Agent::Claude).2);
        usage
            .as_object_mut()
            .unwrap()
            .remove("cache_creation_input_tokens");
        usage["cache_creation"]["ephemeral_1h_input_tokens"] = Value::Null;
        assert!(!assistant_usage(&usage, Agent::Claude).2);
    }

    #[test]
    fn incomplete_cache_categories_prevent_a_plausible_but_incomplete_estimate() {
        let (usage, complete, _) = assistant_usage(
            &json!({
                "input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":10,
                "cache_creation":{"ephemeral_5m_input_tokens":3,"ephemeral_1h_input_tokens":2}
            }),
            Agent::Claude,
        );
        assert!(!complete);
        assert_eq!(usage.cache_creation_tokens, 10);
        assert_eq!(usage.cache_creation_total(), 10);
        assert_eq!(usage.cache_creation_5m_tokens, 0);
        assert_eq!(usage.cache_creation_1h_tokens, 0);
        let (partial, complete, _) = assistant_usage(
            &json!({
                "input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":10,
                "cache_creation":{"ephemeral_5m_input_tokens":3}
            }),
            Agent::Claude,
        );
        assert!(!complete);
        assert_eq!(partial.cache_creation_total(), 10);
        let (invalid_ttl, complete, _) = assistant_usage(
            &json!({
                "input":1,"output":1,"cacheWrite":10,"cttl":{"ephemeral1h":-1}
            }),
            Agent::Omp,
        );
        assert!(!complete);
        assert_eq!(invalid_ttl.cache_creation_total(), 10);
        assert!(
            !assistant_usage(
                &json!({"input":1,"output":null,"cost":{"total":0.1}}),
                Agent::Omp
            )
            .1
        );
        assert_eq!(
            assistant_usage(&json!({"cost":{"total":0.0}}), Agent::Omp)
                .0
                .cost_override,
            Some(0.0)
        );
    }

    #[test]
    fn transcript_paths_are_portable_across_host_operating_systems() {
        assert_eq!(
            resolve_path("src/../a.rs", Some("/repo")),
            Some("/repo/a.rs".to_owned())
        );
        assert_eq!(
            resolve_path(r"src\..\a.rs", Some(r"C:\repo")),
            Some("C:/repo/a.rs".to_owned())
        );
        assert_eq!(
            resolve_path(r"C:\repo\..\a.rs", Some("/other")),
            Some("C:/a.rs".to_owned())
        );
        assert_eq!(
            resolve_path("/repo/a.rs", Some(r"C:\other")),
            Some("/repo/a.rs".to_owned())
        );
        assert_eq!(
            resolve_path(r"\\server\share\folder\..\a.rs", None),
            Some("//server/share/a.rs".to_owned())
        );
        assert_eq!(
            resolve_path(r"..\..\a.rs", Some(r"\\server\share\folder")),
            Some("//server/share/a.rs".to_owned())
        );
        assert!(resolve_path("C:relative", Some("/repo")).is_none());
        assert!(resolve_path(r"\source-root-relative", Some("/repo")).is_none());
        assert!(resolve_path("~/a.rs", Some("/repo")).is_none());
        assert_eq!(
            resolve_path(r"literal\name.rs", Some("/repo")),
            Some(r"/repo/literal\name.rs".to_owned())
        );
    }
}
