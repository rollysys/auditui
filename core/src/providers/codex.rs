// Codex CLI rollout discovery (~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl).
// Data model informed by https://github.com/jhlee0409/claude-code-history-viewer (MIT).

use crate::cache::TokenEvent;
use crate::cost::Usage;
use crate::providers::Agent;
use crate::session::{parse_ts_secs, SessionMeta, TranscriptEvent, TranscriptKind};
use anyhow::Result;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub fn base_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex").join("sessions"))
}

pub fn list_sessions() -> Vec<SessionMeta> {
    let Some(root) = base_dir() else { return vec![] };
    if !root.exists() {
        return vec![];
    }
    let mut out = Vec::new();
    for entry in WalkDir::new(&root)
        .max_depth(5)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let Some(name) = p.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.starts_with("rollout-") || !name.ends_with(".jsonl") {
            continue;
        }
        if let Some(meta) = summarize(p) {
            out.push(meta);
        }
    }
    out
}

fn extract_raw_sid(fname: &str) -> Option<String> {
    // rollout-YYYY-MM-DDTHH-MM-SS-<uuid>.jsonl  (fixed 19-char datetime + '-')
    let body = fname
        .strip_prefix("rollout-")?
        .strip_suffix(".jsonl")?;
    if body.len() < 20 {
        return None;
    }
    Some(body[20..].to_string())
}

fn summarize(path: &Path) -> Option<SessionMeta> {
    let fname = path.file_name()?.to_string_lossy().to_string();
    let raw_sid = extract_raw_sid(&fname)?;
    let md = fs::metadata(path).ok()?;
    let modified = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);
    let mut cwd: Option<String> = None;
    let mut model: Option<String> = None;
    let mut prompt: Option<String> = None;
    let mut turns = 0usize;
    let mut started_at_ts = 0u64;
    let mut is_scripted = false;

    for line in reader.lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if started_at_ts == 0 {
            if let Some(ts) = v.get("timestamp").and_then(|x| x.as_str()) {
                if let Some(t) = parse_ts_secs(ts) {
                    started_at_ts = t;
                }
            }
        }
        let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        let payload = v.get("payload");
        match ty {
            "session_meta" => {
                if let Some(p) = payload {
                    cwd = p.get("cwd").and_then(|x| x.as_str()).map(|s| s.to_string());
                    let originator = p.get("originator").and_then(|x| x.as_str()).unwrap_or("");
                    let source = p.get("source").and_then(|x| x.as_str()).unwrap_or("");
                    if originator == "codex_exec" || source == "exec" {
                        is_scripted = true;
                    }
                }
            }
            "turn_context" => {
                if let Some(p) = payload {
                    if model.is_none() {
                        model = p
                            .get("model")
                            .and_then(|x| x.as_str())
                            .map(|s| s.to_string());
                    }
                }
            }
            "event_msg" => {
                if let Some(p) = payload {
                    let pt = p.get("type").and_then(|x| x.as_str()).unwrap_or("");
                    if pt == "user_message" && prompt.is_none() {
                        if let Some(m) = p.get("message").and_then(|x| x.as_str()) {
                            prompt = Some(m.chars().take(120).collect());
                        }
                    }
                    if pt == "task_complete" {
                        turns += 1;
                    }
                }
            }
            _ => {}
        }
    }

    Some(SessionMeta {
        agent: Agent::Codex,
        id: format!("codex:{raw_sid}"),
        path: path.to_path_buf(),
        cwd,
        model,
        prompt,
        turns,
        last_active_ts: modified,
        started_at_ts: if started_at_ts > 0 { started_at_ts } else { modified },
        is_scripted,
        parent_id: None,
        child_count: 0,
    })
}

// Extract per-event token events: one per token_count (delta), one per user_message (turn).
pub fn extract_events(path: &Path) -> Vec<TokenEvent> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    let mut current_model = String::new();

    for line in reader.lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let ts = v
            .get("timestamp")
            .and_then(|x| x.as_str())
            .and_then(parse_ts_secs)
            .unwrap_or(0);
        let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        let Some(p) = v.get("payload") else { continue };
        match ty {
            "turn_context" => {
                if let Some(m) = p.get("model").and_then(|x| x.as_str()) {
                    current_model = m.to_string();
                }
            }
            "event_msg" => {
                let pt = p.get("type").and_then(|x| x.as_str()).unwrap_or("");
                match pt {
                    "user_message" => {
                        out.push(TokenEvent {
                            ts,
                            usage: Usage::default(),
                            model: current_model.clone(),
                            is_user_turn: true,
                        });
                    }
                    "token_count" => {
                        if let Some(last) = p.get("info").and_then(|i| i.get("last_token_usage"))
                        {
                            let mut usage = Usage::default();
                            usage.input_tokens = last
                                .get("input_tokens")
                                .and_then(|x| x.as_u64())
                                .unwrap_or(0);
                            usage.output_tokens = last
                                .get("output_tokens")
                                .and_then(|x| x.as_u64())
                                .unwrap_or(0);
                            usage.cache_read_tokens = last
                                .get("cached_input_tokens")
                                .and_then(|x| x.as_u64())
                                .unwrap_or(0);
                            if usage.input_tokens + usage.output_tokens + usage.cache_read_tokens
                                > 0
                            {
                                out.push(TokenEvent {
                                    ts,
                                    usage,
                                    model: current_model.clone(),
                                    is_user_turn: false,
                                });
                            }
                        }
                    }
                    "web_search_end" => {
                        let mut usage = Usage::default();
                        usage.web_search_calls = 1;
                        out.push(TokenEvent {
                            ts,
                            usage,
                            model: current_model.clone(),
                            is_user_turn: false,
                        });
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    out
}

pub fn read_transcript(path: &Path) -> Result<Vec<TranscriptEvent>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let ts = v
            .get("timestamp")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        let payload = match v.get("payload") {
            Some(p) => p,
            None => continue,
        };
        match ty {
            "event_msg" => {
                let pt = payload.get("type").and_then(|x| x.as_str()).unwrap_or("");
                match pt {
                    "user_message" => {
                        if let Some(m) = payload.get("message").and_then(|x| x.as_str()) {
                            out.push(TranscriptEvent {
                                ts,
                                kind: TranscriptKind::User,
                                body: m.to_string(),
                            });
                        }
                    }
                    "exec_command_end" => {
                        let output = payload
                            .get("aggregated_output")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        let exit = payload
                            .get("exit_code")
                            .and_then(|x| x.as_i64())
                            .unwrap_or(0);
                        // Prefix with a Record-Separator (U+001E) so the
                        // transcript renderer can unambiguously detect
                        // "this ToolResult came from a codex exec_command
                        // and therefore has an exit code prefix", without
                        // accidentally matching a shell script whose raw
                        // stdout happens to start with "exit=1\n…".
                        out.push(TranscriptEvent {
                            ts,
                            kind: TranscriptKind::ToolResult,
                            body: format!("\u{1e}exit={exit}\n{output}"),
                        });
                    }
                    _ => {}
                }
            }
            "response_item" => {
                let pt = payload.get("type").and_then(|x| x.as_str()).unwrap_or("");
                match pt {
                    "message" => {
                        if payload.get("role").and_then(|x| x.as_str()) == Some("assistant") {
                            if let Some(arr) =
                                payload.get("content").and_then(|c| c.as_array())
                            {
                                for item in arr {
                                    if let Some(t) =
                                        item.get("text").and_then(|x| x.as_str())
                                    {
                                        out.push(TranscriptEvent {
                                            ts: ts.clone(),
                                            kind: TranscriptKind::Assistant,
                                            body: t.to_string(),
                                        });
                                    }
                                }
                            }
                        }
                    }
                    "function_call" => {
                        let name = payload
                            .get("name")
                            .and_then(|x| x.as_str())
                            .unwrap_or("fn");
                        let args = payload
                            .get("arguments")
                            .and_then(|x| x.as_str())
                            .unwrap_or("");
                        out.push(TranscriptEvent {
                            ts,
                            kind: TranscriptKind::ToolUse,
                            body: format!("{name}: {args}"),
                        });
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Extract tool-call timeline from a Codex rollout JSONL.
///
/// Turn signal: `event_msg / user_message` (one per genuine user input).
/// We prefer this over `response_item / message role=user` because the latter
/// includes environment-context injections, `<turn_aborted>` notifications,
/// and compacted-replay items — none of which are real user turns.
///
/// Compaction signal: `compacted` (top-level type) or equivalently
/// `event_msg / context_compacted` — both fire 1:1; we use `compacted`
/// since it appears first in the stream.
pub fn extract_tools(path: &Path) -> crate::tools::ToolTimeline {
    use crate::tools::{self, ToolCollector};

    let Ok(file) = File::open(path) else {
        return crate::tools::ToolTimeline::default();
    };
    let file_size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let reader = BufReader::new(file);
    let mut col = ToolCollector::new();

    for (idx, line) in reader.lines().map_while(Result::ok).enumerate() {
        let line_no = (idx + 1) as u32; // 1-based
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let ts = v
            .get("timestamp")
            .and_then(|x| x.as_str())
            .and_then(parse_ts_secs)
            .unwrap_or(0);
        let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");

        match ty {
            // --- turn signal ---
            "event_msg" => {
                let Some(p) = v.get("payload") else { continue };
                let pt = p.get("type").and_then(|x| x.as_str()).unwrap_or("");
                if pt == "user_message" {
                    col.user_turn();
                }
            }
            // --- compaction signal ---
            "compacted" => {
                col.compaction();
            }
            // --- tool calls & results ---
            "response_item" => {
                let Some(p) = v.get("payload") else { continue };
                let pt = p.get("type").and_then(|x| x.as_str()).unwrap_or("");
                match pt {
                    "function_call" => {
                        let call_id = p
                            .get("call_id")
                            .and_then(|x| x.as_str())
                            .unwrap_or("");
                        let name = p
                            .get("name")
                            .and_then(|x| x.as_str())
                            .unwrap_or("");
                        let args_raw = p
                            .get("arguments")
                            .and_then(|x| x.as_str())
                            .unwrap_or("{}");
                        let args: serde_json::Value =
                            serde_json::from_str(args_raw).unwrap_or_else(|_| {
                                serde_json::Value::String(args_raw.to_string())
                            });
                        // Normalize array-valued "command" to a joined string so
                        // tools.rs shell_shape/SHELL_TOOLS can match exec_command(cmd).
                        let args = normalize_command_array(name, args);
                        col.call(call_id, name, &args, ts, line_no);
                    }
                    "function_call_output" => {
                        let call_id = p
                            .get("call_id")
                            .and_then(|x| x.as_str())
                            .unwrap_or("");
                        let output = p
                            .get("output")
                            .and_then(|x| x.as_str())
                            .unwrap_or("");
                        let is_error = is_exec_error(output)
                            || tools::looks_like_error(output);
                        col.result(call_id, output, is_error, ts, line_no);
                    }
                    "custom_tool_call" => {
                        let call_id = p
                            .get("call_id")
                            .and_then(|x| x.as_str())
                            .unwrap_or("");
                        let name = p
                            .get("name")
                            .and_then(|x| x.as_str())
                            .unwrap_or("custom");
                        let input_str = p
                            .get("input")
                            .and_then(|x| x.as_str())
                            .unwrap_or("");
                        let args = serde_json::json!({"input": input_str});
                        col.call(call_id, name, &args, ts, line_no);
                    }
                    "custom_tool_call_output" => {
                        let call_id = p
                            .get("call_id")
                            .and_then(|x| x.as_str())
                            .unwrap_or("");
                        let output = p
                            .get("output")
                            .and_then(|x| x.as_str())
                            .unwrap_or("");
                        let is_error = tools::looks_like_error(output);
                        col.result(call_id, output, is_error, ts, line_no);
                    }
                    "web_search_call" => {
                        // No call_id; no paired result. Extract query from
                        // payload.action.query (primary search term).
                        let query = p
                            .get("action")
                            .and_then(|a| a.get("query"))
                            .and_then(|q| q.as_str())
                            .unwrap_or("");
                        let args = serde_json::json!({"query": query});
                        col.call("", "web_search", &args, ts, line_no);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    col.finish(file_size)
}

/// Detect non-zero exit code in Codex exec_command output.
/// Pattern: "Process exited with code N" where N ≠ 0.
fn is_exec_error(output: &str) -> bool {
    // Fast path: skip scanning if the marker isn't present at all.
    let Some(pos) = output.find("Process exited with code ") else {
        return false;
    };
    let after = &output[pos + "Process exited with code ".len()..];
    // The code is the next integer token (possibly negative).
    let code_str: String = after
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    code_str != "0" && !code_str.is_empty()
}

/// If `args["command"]` is a JSON array (some Codex versions serialize the
/// command as `["cmd", "arg1", ...]`), join it into a single string and wrap
/// back into `{"command": "cmd arg1 ..."}` so that `tools.rs` SHELL_TOOLS
/// shape normalization works. Also maps exec_command's `cmd` key.
fn normalize_command_array(name: &str, mut args: serde_json::Value) -> serde_json::Value {
    // exec_command uses "cmd"; shell/shell_command use "command".
    let keys = match name {
        "exec_command" => &["cmd", "command"][..],
        _ => &["command"][..],
    };
    for key in keys {
        if let Some(arr) = args.get(*key).and_then(|v| v.as_array()) {
            let joined: String = arr
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            args[*key] = serde_json::Value::String(joined);
            return args;
        }
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("codex_tools.jsonl")
    }

    #[test]
    fn extract_tools_fixture() {
        let tl = extract_tools(&fixture_path());

        // Fixture has 13 lines:
        //  1: event_msg/user_message  (turn 0)
        //  2: function_call exec_command (success)
        //  3: function_call_output     (code 0)
        //  4: function_call exec_command (error)
        //  5: function_call_output     (code 1)
        //  6: custom_tool_call apply_patch
        //  7: custom_tool_call_output
        //  8: web_search_call          (no result)
        //  9: compacted
        // 10: event_msg/context_compacted
        // 11: event_msg/user_message  (turn 1)
        // 12: function_call write_stdin
        // 13: function_call_output

        // 2 user turns
        assert_eq!(tl.turns, 2, "turns");

        // 1 compaction
        assert_eq!(tl.compactions, 1, "compactions");

        // 5 tool calls: 2 exec_command + 1 apply_patch + 1 web_search + 1 write_stdin
        assert_eq!(tl.events.len(), 5, "events count");

        // Check names
        let names: Vec<&str> = tl.events.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["exec_command", "exec_command", "apply_patch", "web_search", "write_stdin"]
        );

        // First exec_command: success (code 0)
        assert!(!tl.events[0].is_error, "first exec_command should succeed");
        assert!(tl.events[0].result_bytes > 0, "should have result bytes");

        // Second exec_command: error (code 1)
        assert!(tl.events[1].is_error, "second exec_command should be error");

        // apply_patch: paired with output
        assert!(tl.events[2].result_bytes > 0, "apply_patch should have result");

        // web_search: no result (no call_id, empty id)
        assert_eq!(tl.events[3].result_bytes, 0, "web_search has no result");

        // write_stdin: paired with output, turn 1
        assert_eq!(tl.events[4].turn, 1, "write_stdin should be in turn 1");
        assert!(tl.events[4].result_bytes > 0, "write_stdin should have result");

        // Exec command should produce a shell shape
        assert!(!tl.events[0].shape.is_empty(), "exec_command should have a shape");
    }

    #[test]
    fn is_exec_error_detects_nonzero() {
        assert!(is_exec_error("Process exited with code 1\nstuff"));
        assert!(is_exec_error("Process exited with code -1\n"));
        assert!(is_exec_error("Chunk ID: abc\nProcess exited with code 127\n"));
        assert!(!is_exec_error("Process exited with code 0\nok"));
        assert!(!is_exec_error("no exit info here"));
    }

    #[test]
    fn normalize_command_array_joins() {
        let args = serde_json::json!({"command": ["ls", "-la", "/tmp"]});
        let result = normalize_command_array("shell", args);
        assert_eq!(result["command"].as_str().unwrap(), "ls -la /tmp");

        // Non-array passes through
        let args = serde_json::json!({"cmd": "pwd"});
        let result = normalize_command_array("exec_command", args.clone());
        assert_eq!(result, args);
    }

    #[test]
    fn extract_tools_missing_file() {
        let tl = extract_tools(Path::new("/nonexistent/file.jsonl"));
        assert_eq!(tl.events.len(), 0);
        assert_eq!(tl.turns, 0);
    }
}
