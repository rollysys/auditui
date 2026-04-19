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
                        out.push(TranscriptEvent {
                            ts,
                            kind: TranscriptKind::ToolResult,
                            body: format!("exit={exit}\n{output}"),
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
