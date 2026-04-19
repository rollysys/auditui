// Qwen Code chat discovery (~/.qwen/projects/<encoded>/chats/<sid>.jsonl).

use crate::cache::TokenEvent;
use crate::cost::Usage;
use crate::providers::Agent;
use crate::session::{parse_ts_secs, SessionMeta, TranscriptEvent, TranscriptKind};
use anyhow::Result;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub fn base_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".qwen").join("projects"))
}

pub fn list_sessions() -> Vec<SessionMeta> {
    let Some(root) = base_dir() else { return vec![] };
    if !root.exists() {
        return vec![];
    }
    let mut out = Vec::new();
    let Ok(projs) = fs::read_dir(&root) else {
        return vec![];
    };
    for proj in projs.flatten() {
        let chats = proj.path().join("chats");
        if !chats.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(&chats) else { continue };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(meta) = summarize(&p) {
                out.push(meta);
            }
        }
    }
    out
}

fn summarize(path: &Path) -> Option<SessionMeta> {
    let stem = path.file_stem()?.to_string_lossy().to_string();
    let md = fs::metadata(path).ok()?;
    let modified = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);
    let mut prompt: Option<String> = None;
    let mut model: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut turns = 0usize;
    let mut started_at_ts = 0u64;
    let mut saw_interactive_pid = false;
    let mut saw_scripted_pid = false;
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
        if cwd.is_none() {
            cwd = v.get("cwd").and_then(|x| x.as_str()).map(|s| s.to_string());
        }
        if model.is_none() {
            model = v
                .get("model")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
        }
        let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        if ty == "user" {
            turns += 1;
            if prompt.is_none() {
                if let Some(parts) = v
                    .get("message")
                    .and_then(|m| m.get("parts"))
                    .and_then(|x| x.as_array())
                {
                    if let Some(text) = parts.iter().find_map(|p| {
                        let is_thought = p.get("thought").and_then(|x| x.as_bool()).unwrap_or(false);
                        if is_thought {
                            return None;
                        }
                        p.get("text").and_then(|x| x.as_str())
                    }) {
                        prompt = Some(text.chars().take(120).collect());
                    }
                }
            }
        } else if ty == "system"
            && v.get("subtype").and_then(|x| x.as_str()) == Some("ui_telemetry")
        {
            if let Some(ui) = v
                .get("systemPayload")
                .and_then(|p| p.get("uiEvent"))
            {
                if ui.get("event.name").and_then(|x| x.as_str())
                    == Some("qwen-code.api_response")
                {
                    if let Some(pid) = ui.get("prompt_id").and_then(|x| x.as_str()) {
                        if pid.contains("########") {
                            saw_interactive_pid = true;
                        } else {
                            saw_scripted_pid = true;
                        }
                    }
                }
            }
        }
    }

    let is_scripted = saw_scripted_pid && !saw_interactive_pid;

    Some(SessionMeta {
        agent: Agent::Qwen,
        id: format!("qwen:{stem}"),
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

// Extract per-message token events for Qwen.
pub fn extract_events(path: &Path) -> Vec<TokenEvent> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let reader = BufReader::new(file);
    let mut out = Vec::new();

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
        match ty {
            "user" => {
                out.push(TokenEvent {
                    ts,
                    usage: Usage::default(),
                    model: String::new(),
                    is_user_turn: true,
                });
            }
            "assistant" => {
                let model = v
                    .get("model")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(um) = v.get("usageMetadata") {
                    let mut usage = Usage::default();
                    usage.input_tokens = um
                        .get("promptTokenCount")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                    usage.output_tokens = um
                        .get("candidatesTokenCount")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                    usage.cache_read_tokens = um
                        .get("cachedContentTokenCount")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                    if usage.input_tokens + usage.output_tokens + usage.cache_read_tokens > 0 {
                        out.push(TokenEvent {
                            ts,
                            usage,
                            model,
                            is_user_turn: false,
                        });
                    }
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
        if ty == "system" {
            if let Some(payload) = v.get("systemPayload") {
                let body = match payload.as_str() {
                    Some(s) => s.to_string(),
                    None => payload.to_string(),
                };
                out.push(TranscriptEvent {
                    ts: ts.clone(),
                    kind: TranscriptKind::System,
                    body,
                });
            }
            continue;
        }
        let Some(parts) = v
            .get("message")
            .and_then(|m| m.get("parts"))
            .and_then(|x| x.as_array())
        else {
            continue;
        };
        let default_kind = match ty {
            "user" => TranscriptKind::User,
            "assistant" => TranscriptKind::Assistant,
            "tool_result" => TranscriptKind::ToolResult,
            _ => TranscriptKind::System,
        };
        for part in parts {
            if let Some(fc) = part.get("functionCall") {
                let name = fc.get("name").and_then(|x| x.as_str()).unwrap_or("fn");
                let args = fc.get("args").map(|x| x.to_string()).unwrap_or_default();
                out.push(TranscriptEvent {
                    ts: ts.clone(),
                    kind: TranscriptKind::ToolUse,
                    body: format!("{name}: {args}"),
                });
                continue;
            }
            if let Some(fr) = part.get("functionResponse") {
                let name = fr.get("name").and_then(|x| x.as_str()).unwrap_or("");
                let resp = fr.get("response");
                let body_text = match resp {
                    Some(r) => match r.as_str() {
                        Some(s) => s.to_string(),
                        None => r
                            .get("output")
                            .and_then(|x| x.as_str())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| r.to_string()),
                    },
                    None => String::new(),
                };
                let status = v
                    .get("toolCallResult")
                    .and_then(|t| t.get("status"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let is_error = !status.is_empty() && status != "success";
                let prefix = match (is_error, name.is_empty()) {
                    (true, false) => format!("exit=1\n[{}] ", name),
                    (true, true) => "exit=1\n".to_string(),
                    (false, false) => format!("[{}] ", name),
                    (false, true) => String::new(),
                };
                out.push(TranscriptEvent {
                    ts: ts.clone(),
                    kind: TranscriptKind::ToolResult,
                    body: format!("{prefix}{body_text}"),
                });
                continue;
            }
            if let Some(text) = part.get("text").and_then(|x| x.as_str()) {
                let is_thought = part.get("thought").and_then(|x| x.as_bool()).unwrap_or(false);
                let kind = if is_thought {
                    TranscriptKind::Thinking
                } else {
                    default_kind
                };
                out.push(TranscriptEvent {
                    ts: ts.clone(),
                    kind,
                    body: text.to_string(),
                });
            }
        }
    }
    Ok(out)
}
