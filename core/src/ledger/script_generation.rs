//! Code-body repetition is a reuse/skill candidate, never a waste verdict.
//! Bodies are inspected only during parsing; the ledger retains hashes and counts.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::analysis::{finish, result_before_call, selected, Index};
use super::parse::resolve_path;
use super::{fingerprint, Candidate, Ledger, Query, ScriptMetadata, SourceRef, ToolExecution};

const MIN_BYTES: u64 = 1_024;
const MIN_LINES: u64 = 16;
const MAX_LINE_HASHES: u64 = 512;
const EXACT: &str = "repeated_script_generation";
const REWRITE: &str = "mostly_unchanged_script_rewrite";

/// Extract only explicit code-bearing arguments, never tool results or prose.
/// Shell bodies are opaque: no execution, expansion, or guessed output filenames.
pub(super) fn metadata(name: &str, args: &Value, cwd: Option<&str>) -> Option<ScriptMetadata> {
    let tool = name
        .strip_prefix("functions.")
        .or_else(|| name.strip_prefix("function."))
        .unwrap_or(name)
        .to_ascii_lowercase();
    let field = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| args.get(*key).and_then(Value::as_str))
    };
    let effective_cwd = match field(&["cwd", "workdir"]) {
        Some(path) => resolve_path(path, cwd),
        None => cwd.and_then(|path| resolve_path(path, None)),
    };
    let (kind, body, target) = match tool.as_str() {
        "write" | "write_file" | "create_file" => {
            let path = field(&["path", "file_path", "file"])?;
            // File extensions establish code-bearing intent; documents/data and
            // opaque tool-device URIs are not guessed to contain scripts.
            let extension = path.rsplit_once('.')?.1.to_ascii_lowercase();
            if !matches!(
                extension.as_str(),
                "py" | "pyw"
                    | "js"
                    | "mjs"
                    | "cjs"
                    | "ts"
                    | "tsx"
                    | "jsx"
                    | "sh"
                    | "bash"
                    | "zsh"
                    | "fish"
                    | "rb"
                    | "pl"
                    | "lua"
                    | "ps1"
                    | "r"
                    | "jl"
            ) || path.contains("://")
            {
                return None;
            }
            let path = resolve_path(path, effective_cwd.as_deref())?;
            // Append/partial-write flags cannot establish full replacement.
            if ["append", "insert", "patch"].iter().any(|key| {
                args.get(*key)
                    .is_some_and(|value| value != &Value::Bool(false))
            }) || field(&["mode"])
                .is_some_and(|mode| !matches!(mode, "w" | "write" | "overwrite"))
            {
                return None;
            }
            (
                "full_write",
                field(&["content", "contents", "text"])?,
                json!({"kind": "full_write", "path": path}),
            )
        }
        "eval" | "python" | "run_python" | "execute_python" | "exec" | "execute" => (
            "inline",
            field(&["code", "script", "input", "command", "cmd"])?,
            json!({"kind": "inline", "runtime": tool, "language": field(&["language"]), "cwd": effective_cwd.as_deref()?}),
        ),
        "bash" | "shell" | "shell_command" | "exec_command" | "local_shell" => (
            "inline",
            field(&["command", "cmd", "code", "script"])?,
            json!({"kind": "inline", "runtime": tool, "cwd": effective_cwd.as_deref()?}),
        ),
        _ => return None,
    };
    let bytes = body.len() as u64;
    if bytes < MIN_BYTES {
        return None;
    }
    let lines = body.lines().filter(|line| !line.trim().is_empty()).count() as u64;
    if lines < MIN_LINES {
        return None;
    }
    // No unbounded sketch and no sample pretending to describe the full body.
    // Long bodies still support exact SHA-256 matches, not near-rewrite claims.
    let mut line_fingerprints = if kind == "full_write" && lines <= MAX_LINE_HASHES {
        body.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| fingerprint(line.as_bytes()))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    line_fingerprints.sort_unstable();
    Some(ScriptMetadata {
        kind: kind.into(),
        target_fingerprint: fingerprint(target.to_string().as_bytes()),
        code_fingerprint: fingerprint(body.as_bytes()),
        bytes,
        lines,
        line_fingerprints,
    })
}

fn substantial(script: &ScriptMetadata) -> bool {
    matches!(script.kind.as_str(), "inline" | "full_write")
        && script.bytes >= MIN_BYTES
        && script.lines >= MIN_LINES
        && !script.target_fingerprint.is_empty()
        && !script.code_fingerprint.is_empty()
}

/// Multiset overlap preserves repeated lines rather than letting one common
/// boilerplate line stand in for an arbitrarily large rewritten body.
fn mostly_unchanged(a: &ScriptMetadata, b: &ScriptMetadata) -> bool {
    if a.kind != "full_write"
        || b.kind != "full_write"
        || a.lines > MAX_LINE_HASHES
        || b.lines > MAX_LINE_HASHES
        || a.line_fingerprints.len() as u64 != a.lines
        || b.line_fingerprints.len() as u64 != b.lines
        || u128::from(a.bytes.min(b.bytes)) * 10 < u128::from(a.bytes.max(b.bytes)) * 9
    {
        return false;
    }
    let (mut i, mut j, mut common) = (0, 0, 0_u64);
    while i < a.line_fingerprints.len() && j < b.line_fingerprints.len() {
        match a.line_fingerprints[i].cmp(&b.line_fingerprints[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                common += 1;
                i += 1;
                j += 1;
            }
        }
    }
    common * 10 >= a.lines.max(b.lines) * 9
}

fn ordered(first: &ToolExecution, second: &ToolExecution) -> bool {
    !first.batch_id.is_empty()
        && !second.batch_id.is_empty()
        && first.result.as_ref().is_some_and(|result| {
            result.source_id == first.call.source_id && result.version == first.call.version
        })
        && result_before_call(first, second)
}

pub(super) fn detect(ledger: &Ledger, query: &Query) -> Vec<Candidate> {
    let index = Index::new(ledger, query);
    let mut found = Vec::new();
    for session in ledger
        .sessions
        .iter()
        .filter(|session| selected(session, query))
    {
        // Session and immutable source version are hard boundaries even where
        // timestamps happen to suggest an order across unrelated files.
        let mut groups = BTreeMap::<(&str, &str, &str, &str), Vec<&ToolExecution>>::new();
        for tool in index
            .tools
            .get(session.id.as_str())
            .into_iter()
            .flatten()
            .copied()
            .filter(|tool| {
                !tool.call.source_id.is_empty()
                    && !tool.call.version.is_empty()
                    && index.tool_in_window(tool)
            })
        {
            let Some(script) = tool.script.as_ref().filter(|script| substantial(script)) else {
                continue;
            };
            groups
                .entry((
                    tool.call.source_id.as_str(),
                    tool.call.version.as_str(),
                    script.kind.as_str(),
                    script.target_fingerprint.as_str(),
                ))
                .or_default()
                .push(tool);
        }
        for tools in groups.values_mut() {
            tools.sort_by_key(|tool| {
                (
                    tool.call.line,
                    tool.call.record_id.as_deref(),
                    tool.id.as_str(),
                )
            });
            tools.dedup_by(|a, b| {
                a.call.line == b.call.line && a.call.record_id == b.call.record_id && a.id == b.id
            });
            let mut previous_exact = BTreeMap::<&str, &ToolExecution>::new();
            let mut matches = BTreeMap::<&str, Vec<(&ToolExecution, &ToolExecution)>>::new();
            for (position, &tool) in tools.iter().enumerate() {
                let script = tool.script.as_ref().unwrap();
                let exact = previous_exact
                    .get(script.code_fingerprint.as_str())
                    .copied();
                if let Some(first) = exact.filter(|first| ordered(first, tool)) {
                    matches.entry(EXACT).or_default().push((first, tool));
                } else if position > 0 {
                    let first = tools[position - 1];
                    let earlier = first.script.as_ref().unwrap();
                    if earlier.code_fingerprint != script.code_fingerprint
                        && ordered(first, tool)
                        && mostly_unchanged(earlier, script)
                    {
                        matches.entry(REWRITE).or_default().push((first, tool));
                    }
                }
                previous_exact.insert(script.code_fingerprint.as_str(), tool);
            }
            for (kind, pairs) in matches {
                let mut evidence = Vec::<&SourceRef>::new();
                let mut repeats = Vec::with_capacity(pairs.len());
                let (mut bytes, mut lines) = (0_u64, 0_u64);
                for (first, repeated) in &pairs {
                    evidence.push(&first.call);
                    evidence.extend(first.result.as_ref());
                    evidence.push(&repeated.call);
                    let script = repeated.script.as_ref().unwrap();
                    bytes = bytes.saturating_add(script.bytes);
                    lines = lines.saturating_add(script.lines);
                    repeats.push(*repeated);
                }
                let mut limitations = vec![
                    "A first observed body is comparison evidence, not a finding; only subsequent matched generations link requests. Earlier history outside the selected window is not inferred.".into(),
                    "Byte and nonblank-line counts describe submitted code structure, not billed tokens. Linked output tokens describe whole requests, not script tokens.".into(),
                    "Repetition can be necessary rerunning, validation, recovery, or deliberate regeneration. A reuse/skill opportunity is a hypothesis, not demonstrated waste or savings.".into(),
                    "These are observed code submissions, including possible failed or pending attempts; execution success and resulting file contents are not established.".into(),
                    "Only explicit supported code arguments of at least 1024 UTF-8 bytes and 16 nonblank lines qualify. Missing metadata, partial edits, single-line commands, and unresolved execution directories are not inferred.".into(),
                    "Matching stays within one session, target, and immutable source version, with an observed prior result before a distinct later batch. No cross-session or parallel order is inferred.".into(),
                ];
                if kind == REWRITE {
                    limitations.push("Mostly unchanged means at least 90% exact nonblank-line multiset overlap and a byte-size ratio of at least 90%, not semantic equivalence. Line order, intent, and correctness are not established. Bodies over 512 nonblank lines only support exact repetition detection.".into());
                } else {
                    limitations.push("Exact repetition compares the complete submitted body. Inline shell code is opaque; outputs, shell expansion, external state, and written filenames are not inferred. Changed inline code is not assumed to share a task.".into());
                }
                let related_cost = index.linked(&repeats, &[], &mut evidence, &mut limitations);
                let summary = if kind == EXACT {
                    format!("{} repeated substantial code-body generations", pairs.len())
                } else {
                    format!(
                        "{} mostly-unchanged full script rewrite attempts",
                        pairs.len()
                    )
                };
                let observed = vec![
                    format!("{} subsequent generations matched earlier completed calls at one hashed target; repeated submissions total {bytes} UTF-8 bytes and {lines} nonblank lines.", pairs.len()),
                    if kind == EXACT {
                        "Complete code-body SHA-256 fingerprints match exactly; intent/description fields do not determine the match.".into()
                    } else {
                        "Full writes retain at least 90% of the larger body's exact nonblank-line multiset and at least 90% of its byte size.".into()
                    },
                    format!("Whole explicitly linked repeated-generation requests account for {} recorded output tokens; no fraction is allocated to the code body.", related_cost.output_tokens),
                ];
                found.push(finish(query, Candidate {
                    kind: kind.into(),
                    project: session.project.clone(),
                    summary,
                    observed,
                    hypothesis: "Consider preserving a reusable script, parametrizing the repeated operation, or applying a targeted edit after reviewing the evidence and task intent.".into(),
                    related_cost,
                    limitations,
                    ..Candidate::default()
                }, evidence));
            }
        }
    }
    found.sort_by(|a, b| a.id.cmp(&b.id));
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::Usage;
    use crate::ledger::{LlmRequest, Session, UsageObservation};
    use crate::providers::Agent;

    fn body(lines: usize) -> String {
        (0..lines)
            .map(|i| format!("value_{i:04} = transform(source_values[{i}], configuration=runtime_configuration)\n"))
            .collect()
    }

    fn source(line: u64) -> SourceRef {
        SourceRef {
            source_id: "transcript".into(),
            version: "snapshot".into(),
            line,
            ..SourceRef::default()
        }
    }

    fn add(ledger: &mut Ledger, line: u64, name: &str, args: Value) {
        let request = format!("request-{line}");
        ledger.requests.push(LlmRequest {
            id: request.clone(),
            session_id: "session".into(),
            ts_ms: Some(line as i64),
            source: source(line),
            ..LlmRequest::default()
        });
        ledger.observations.push(UsageObservation {
            id: format!("usage-{line}"),
            session_id: "session".into(),
            request_id: Some(request.clone()),
            ts_ms: Some(line as i64),
            basis: "request".into(),
            reported_usd: Some(1.0),
            usage: Usage {
                output_tokens: 123,
                ..Usage::default()
            },
            source: source(line),
            ..UsageObservation::default()
        });
        ledger.tools.push(ToolExecution {
            id: format!("tool-{line}"),
            session_id: "session".into(),
            batch_id: request.clone(),
            request_id: Some(request),
            name: name.into(),
            script: metadata(name, &args, Some("/project")),
            call: source(line),
            result: Some(source(line + 1)),
            start_ms: Some(line as i64),
            end_ms: Some(line as i64 + 1),
            status: "success".into(),
            ..ToolExecution::default()
        });
    }

    fn ledger() -> Ledger {
        Ledger {
            sessions: vec![Session {
                id: "session".into(),
                provider: Agent::Omp,
                project: "/project".into(),
                parent_id: None,
                task_id: None,
                work_type: None,
                source: source(1),
            }],
            ..Ledger::default()
        }
    }

    fn exact_pair() -> Ledger {
        let mut ledger = ledger();
        for line in [2, 4] {
            add(
                &mut ledger,
                line,
                "eval",
                json!({"language": "py", "code": body(24)}),
            );
        }
        ledger
    }

    #[test]
    fn exact_generation_excludes_baseline_spend_and_keeps_money_provenance() {
        let mut ledger = exact_pair();
        ledger.observations[0].reported_usd = Some(100.0);
        ledger.observations[1].reported_usd = None;
        ledger.observations[1].estimated_usd = Some(2.0);
        add(
            &mut ledger,
            6,
            "eval",
            json!({"language": "py", "code": body(24)}),
        );
        ledger.observations[2].reported_usd = None;
        add(
            &mut ledger,
            8,
            "eval",
            json!({"language": "py", "code": body(24)}),
        );
        let mut unrelated = ledger.observations[0].clone();
        unrelated.id = "unattributed".into();
        unrelated.request_id = None;
        unrelated.basis = "unattributed".into();
        ledger.observations.push(unrelated);
        let found = detect(&ledger, &Query::default());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, EXACT);
        assert_eq!(found[0].related_cost.reported_usd, 1.0);
        assert_eq!(found[0].related_cost.estimated_usd, 2.0);
        assert_eq!(found[0].related_cost.unknown_observations, 1);
        assert_eq!(found[0].related_cost.output_tokens, 369);
        assert_eq!(found[0].related_cost.requests, 3);
        assert!(found[0]
            .evidence
            .iter()
            .any(|reference| reference.line == 2));
    }

    #[test]
    fn minor_full_write_is_detected_but_changed_inline_task_is_not() {
        let original = body(24);
        let changed = original.replacen("value_0000", "other_0000", 1);
        let mut writes = ledger();
        add(
            &mut writes,
            2,
            "Write",
            json!({"file_path": "job.py", "content": original}),
        );
        assert!(detect(&writes, &Query::default()).is_empty());
        add(
            &mut writes,
            4,
            "write",
            json!({"path": "/project/job.py", "content": changed}),
        );
        assert_eq!(detect(&writes, &Query::default())[0].kind, REWRITE);
        let mut inline = ledger();
        add(
            &mut inline,
            2,
            "eval",
            json!({"language": "py", "code": original}),
        );
        add(
            &mut inline,
            4,
            "eval",
            json!({"language": "py", "code": changed}),
        );
        assert!(detect(&inline, &Query::default()).is_empty());
    }

    #[test]
    fn distinct_targets_sources_sessions_and_parallel_batches_do_not_join() {
        let base = exact_pair();
        let mut different_source = base.clone();
        different_source.tools[1].call.source_id = "other-source".into();
        assert!(detect(&different_source, &Query::default()).is_empty());
        let mut different_version = base.clone();
        different_version.tools[1].call.version = "other-version".into();
        assert!(detect(&different_version, &Query::default()).is_empty());
        let mut different_session = base.clone();
        different_session.tools[1].session_id = "another-session".into();
        assert!(detect(&different_session, &Query::default()).is_empty());
        let mut parallel = base.clone();
        parallel.tools[1].batch_id = parallel.tools[0].batch_id.clone();
        assert!(detect(&parallel, &Query::default()).is_empty());
        let mut late_result = base.clone();
        late_result.tools[0].result = Some(source(5));
        assert!(detect(&late_result, &Query::default()).is_empty());
        let mut different_cwd = base;
        different_cwd.tools[1].script = metadata(
            "eval",
            &json!({"language": "py", "code": body(24)}),
            Some("/other-project"),
        );
        assert!(detect(&different_cwd, &Query::default()).is_empty());
        let mut different_target = ledger();
        add(
            &mut different_target,
            2,
            "write",
            json!({"path": "a.py", "content": body(24)}),
        );
        add(
            &mut different_target,
            4,
            "write",
            json!({"path": "b.py", "content": body(24)}),
        );
        assert!(detect(&different_target, &Query::default()).is_empty());
    }

    #[test]
    fn repetition_uses_source_order_and_stable_evidence_identity() {
        let mut ledger = exact_pair();
        let expected = detect(&ledger, &Query::default());
        ledger.tools.reverse();
        ledger.requests.reverse();
        ledger.observations.reverse();
        assert_eq!(detect(&ledger, &Query::default())[0].id, expected[0].id);
        ledger.tools[0].script = None;
        assert!(detect(&ledger, &Query::default()).is_empty());
    }

    #[test]
    fn window_needs_observed_baseline_and_filters_project_and_provider() {
        let ledger = exact_pair();
        for query in [
            Query {
                since_ms: Some(4),
                ..Query::default()
            },
            Query {
                until_ms: Some(4),
                ..Query::default()
            },
            Query {
                project: Some("/other".into()),
                ..Query::default()
            },
            Query {
                agents: vec![Agent::Claude],
                ..Query::default()
            },
        ] {
            assert!(detect(&ledger, &query).is_empty());
        }
    }

    #[test]
    fn large_bodies_keep_exact_detection_without_partial_similarity_sketches() {
        let original = body(513);
        let mut ledger = ledger();
        add(
            &mut ledger,
            2,
            "write",
            json!({"path": "job.py", "content": original}),
        );
        add(
            &mut ledger,
            4,
            "write",
            json!({"path": "job.py", "content": original.replacen("value_0000", "other_0000", 1)}),
        );
        assert!(detect(&ledger, &Query::default()).is_empty());
        add(
            &mut ledger,
            6,
            "write",
            json!({"path": "job.py", "content": original}),
        );
        assert_eq!(detect(&ledger, &Query::default())[0].kind, EXACT);
    }

    #[test]
    fn short_commands_documents_partial_edits_and_unknown_cwd_have_no_metadata() {
        for (name, args, cwd) in [
            (
                "bash",
                json!({"command": "python job.py"}),
                Some("/project"),
            ),
            ("eval", json!({"code": "x".repeat(2048)}), Some("/project")),
            (
                "write",
                json!({"path": "notes.md", "content": body(24)}),
                Some("/project"),
            ),
            (
                "edit",
                json!({"path": "job.py", "content": body(24)}),
                Some("/project"),
            ),
            (
                "write",
                json!({"path": "job.py", "content": body(24), "append": true}),
                Some("/project"),
            ),
            ("eval", json!({"code": body(24)}), None),
            (
                "write",
                json!({"path": "job.py", "content": body(24)}),
                None,
            ),
        ] {
            assert!(metadata(name, &args, cwd).is_none(), "{name}");
        }
    }

    #[test]
    fn line_multiset_does_not_turn_shared_boilerplate_into_a_full_rewrite() {
        let original = body(24);
        let changed = original.lines().take(12).map(|line| format!("{line}\n")).collect::<String>()
            + &"a_completely_different_operation(runtime_configuration, source_values, execution_context)\n".repeat(12);
        let mut ledger = ledger();
        add(
            &mut ledger,
            2,
            "write",
            json!({"path": "job.py", "content": original}),
        );
        add(
            &mut ledger,
            4,
            "write",
            json!({"path": "job.py", "content": changed}),
        );
        assert!(detect(&ledger, &Query::default()).is_empty());
    }

    #[test]
    fn shell_script_regeneration_ignores_intent_text_but_not_executed_body() {
        let command = format!("python3 - <<'PY'\n{}PY", body(24));
        let mut ledger = ledger();
        add(
            &mut ledger,
            2,
            "functions.bash",
            json!({"command": command, "i": "Generating initial script"}),
        );
        add(
            &mut ledger,
            4,
            "functions.bash",
            json!({"command": command, "i": "Running script again"}),
        );
        let found = detect(&ledger, &Query::default());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, EXACT);
        ledger.tools[1].script = metadata(
            "functions.bash",
            &json!({
                "command": command.replacen("value_0000", "other_0000", 1)
            }),
            Some("/project"),
        );
        assert!(detect(&ledger, &Query::default()).is_empty());
    }

    #[test]
    fn ninety_percent_line_overlap_is_an_inclusive_rewrite_boundary() {
        let original = body(20);
        for (changed_lines, expected) in [(2, true), (3, false)] {
            let changed = original.replacen("value_", "other_", changed_lines);
            let mut ledger = ledger();
            add(
                &mut ledger,
                2,
                "write",
                json!({"path": "job.py", "content": original}),
            );
            add(
                &mut ledger,
                4,
                "write",
                json!({"path": "job.py", "content": changed}),
            );
            assert_eq!(!detect(&ledger, &Query::default()).is_empty(), expected);
        }
    }

    #[test]
    fn parsed_omp_full_rewrite_keeps_code_out_of_ledger_and_candidate_exports() {
        use std::sync::atomic::{AtomicU64, Ordering};

        struct TempSource(std::path::PathBuf);
        impl Drop for TempSource {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let file = TempSource(std::env::temp_dir().join(format!(
            "auditui-script-generation-{}-{}.jsonl",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )));
        let original = format!(
            "sensitive_canary = 'SCRIPT_BODY_NOT_FOR_EXPORT'\n{}",
            body(24)
        );
        let changed = original.replacen("value_0000", "other_0000", 1);
        let mut records = vec![json!({
            "type": "session", "id": "script-fixture", "cwd": "/project",
            "timestamp": "2026-05-23T10:00:00Z"
        })];
        for (i, code) in [&original, &changed].into_iter().enumerate() {
            records.push(json!({
                "type": "message", "id": format!("entry-{i}"),
                "message": {
                    "role": "assistant", "responseId": format!("response-{i}"),
                    "timestamp": 1779530401000_i64 + i as i64 * 2000,
                    "model": "unknown-provider/custom-model",
                    "usage": {"input": 100, "output": 60, "cost": {"total": 1.5}},
                    "content": [{
                        "type": "toolCall", "id": format!("call-{i}"), "name": "write",
                        "arguments": {"path": "job.py", "content": code}
                    }]
                }
            }));
            records.push(json!({
                "type": "message", "id": format!("result-{i}"),
                "message": {
                    "role": "toolResult", "toolCallId": format!("call-{i}"),
                    "timestamp": 1779530402000_i64 + i as i64 * 2000,
                    "isError": false, "content": [{"type": "text", "text": "Written"}]
                }
            }));
        }
        let content = records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&file.0, content).unwrap();
        let ledger = crate::ledger::parse::parse_file(&file.0, Agent::Omp, None, None).unwrap();
        let found = detect(&ledger, &Query::default());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, REWRITE);
        assert_eq!(found[0].related_cost.reported_usd, 1.5);
        assert_eq!(found[0].related_cost.output_tokens, 60);
        for export in [
            serde_json::to_string(&ledger).unwrap(),
            serde_json::to_string(&found).unwrap(),
        ] {
            assert!(!export.contains("SCRIPT_BODY_NOT_FOR_EXPORT"));
            assert!(!export.contains("transform(source_values"));
        }
    }
}
