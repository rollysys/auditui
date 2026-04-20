// Claude Code transcript discovery.
// Data model informed by https://github.com/jhlee0409/claude-code-history-viewer (MIT).

use crate::cache::TokenEvent;
use crate::cost::Usage;
use crate::providers::Agent;
use crate::session::{parse_ts_secs, SessionMeta, TranscriptEvent, TranscriptKind};
use anyhow::Result;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub fn base_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("projects"))
}

pub fn list_sessions() -> Vec<SessionMeta> {
    let Some(root) = base_dir() else { return vec![] };
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(&root) else { return vec![] };
    for ent in entries.flatten() {
        let proj = ent.path();
        if !proj.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(&proj) else { continue };
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
    let file = File::open(path).ok()?;
    let md = file.metadata().ok()?;
    let modified = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let reader = BufReader::new(file);
    let mut first_user: Option<String> = None;
    let mut turns = 0usize;
    let mut cwd: Option<String> = None;
    let mut model: Option<String> = None;
    let mut started_at_ts = 0u64;
    let mut is_scripted = false;
    let mut first_line = true;

    for line in reader.lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        if first_line {
            first_line = false;
            if ty == "queue-operation" {
                is_scripted = true;
            }
        }
        if cwd.is_none() {
            if let Some(c) = v.get("cwd").and_then(|x| x.as_str()) {
                cwd = Some(c.to_string());
            }
        }
        if started_at_ts == 0 {
            if let Some(ts) = v.get("timestamp").and_then(|x| x.as_str()) {
                if let Some(t) = parse_ts_secs(ts) {
                    started_at_ts = t;
                }
            }
        }
        if !is_scripted {
            if let Some(ep) = v.get("entrypoint").and_then(|x| x.as_str()) {
                if ep == "sdk-cli" {
                    is_scripted = true;
                }
            }
        }
        match ty {
            "user" => {
                turns += 1;
                if first_user.is_none() {
                    if let Some(raw) = user_text(&v) {
                        first_user = Some(raw.chars().take(120).collect());
                    }
                }
            }
            "assistant" => {
                if model.is_none() {
                    if let Some(m) = v
                        .get("message")
                        .and_then(|m| m.get("model"))
                        .and_then(|x| x.as_str())
                    {
                        model = Some(m.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    Some(SessionMeta {
        agent: Agent::Claude,
        id: stem,
        path: path.to_path_buf(),
        cwd,
        model,
        prompt: first_user,
        turns,
        last_active_ts: modified,
        started_at_ts: if started_at_ts > 0 { started_at_ts } else { modified },
        is_scripted,
    })
}

pub fn read_transcript(path: &Path) -> Result<Vec<TranscriptEvent>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if v.get("isMeta").and_then(|x| x.as_bool()).unwrap_or(false) {
            continue;
        }
        let ts = v
            .get("timestamp")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        match ty {
            "user" => {
                // A user turn can carry BOTH text and tool_results in the
                // same content array (e.g. user typed a follow-up after a
                // tool ran). The previous if/else path emitted text XOR
                // tool_results, so any tool_result on the same line as a
                // (non-empty) text block got dropped — and even a
                // whitespace-only text like `Some("")` from the parser
                // was enough to silently swallow the tool_result body.
                if let Some(text) = user_text(&v).filter(|t| !t.trim().is_empty()) {
                    out.push(TranscriptEvent {
                        ts: ts.clone(),
                        kind: TranscriptKind::User,
                        body: text,
                    });
                }
                for tr in user_tool_results(&v) {
                    out.push(TranscriptEvent {
                        ts: ts.clone(),
                        kind: TranscriptKind::ToolResult,
                        body: tr,
                    });
                }
            }
            "assistant" => {
                for ev in assistant_events(&v, &ts) {
                    out.push(ev);
                }
            }
            "system" => {
                if let Some(c) = v.get("content").and_then(|x| x.as_str()) {
                    out.push(TranscriptEvent {
                        ts,
                        kind: TranscriptKind::System,
                        body: c.to_string(),
                    });
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

fn user_text(v: &serde_json::Value) -> Option<String> {
    let c = v.get("message")?.get("content")?;
    let raw = if let Some(s) = c.as_str() {
        Some(s.to_string())
    } else if let Some(arr) = c.as_array() {
        arr.iter()
            .find(|it| it.get("type").and_then(|x| x.as_str()) == Some("text"))
            .and_then(|it| it.get("text").and_then(|x| x.as_str()))
            .map(|s| s.to_string())
    } else {
        None
    };
    raw.and_then(simplify_slash_command)
}

// Collapse Claude Code's slash-command XML wrappers to one-liners.
// Returns None for caveat-only messages (they carry no user-visible info).
fn simplify_slash_command(text: String) -> Option<String> {
    use regex::Regex;
    use std::sync::LazyLock;
    static CAVEAT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?s)^\s*<local-command-caveat>.*?</local-command-caveat>\s*$").unwrap());
    static STDOUT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?s)^\s*<local-command-stdout>(.*?)</local-command-stdout>\s*$").unwrap());
    static CMD_NAME: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"<command-name>([^<]+)</command-name>").unwrap());
    static CMD_ARGS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?s)<command-args>(.*?)</command-args>").unwrap());
    // Whole message is just caveat + optional whitespace: drop.
    if CAVEAT.is_match(&text) {
        return None;
    }
    if let Some(cap) = STDOUT.captures(&text) {
        let body = strip_ansi(cap.get(1).map(|m| m.as_str()).unwrap_or("").trim());
        return Some(format!("[stdout] {body}"));
    }
    // If the message is a <command-*> block (possibly also with command-message / command-contents),
    // fold to "/<name> <args>". Require command-name to be present and no free-form text outside tags.
    if let Some(cap) = CMD_NAME.captures(&text) {
        if is_pure_command_block(&text) {
            let name = cap.get(1).map(|m| m.as_str().trim()).unwrap_or("");
            let args = CMD_ARGS
                .captures(&text)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().trim())
                .unwrap_or("");
            return Some(if args.is_empty() {
                name.to_string()
            } else {
                format!("{name} {args}")
            });
        }
    }
    Some(text)
}

// True if every non-whitespace char is inside a <command-*>…</command-*> tag pair.
fn is_pure_command_block(text: &str) -> bool {
    use regex::Regex;
    use std::sync::LazyLock;
    static BLOCK: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?s)<command-[a-z]+>.*?</command-[a-z]+>").unwrap());
    let stripped = BLOCK.replace_all(text, "");
    stripped.trim().is_empty()
}

fn strip_ansi(s: &str) -> String {
    // Replace the common ESC [ ... m sequences used for colors/bold; keep everything else verbatim.
    use regex::Regex;
    use std::sync::LazyLock;
    static ANSI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").unwrap());
    ANSI.replace_all(s, "").into_owned()
}

fn user_tool_results(v: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(arr) = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    else {
        return out;
    };
    for item in arr {
        if item.get("type").and_then(|x| x.as_str()) == Some("tool_result") {
            let body = item
                .get("content")
                .map(|c| match c.as_str() {
                    Some(s) => s.to_string(),
                    None => c.to_string(),
                })
                .unwrap_or_default();
            out.push(body);
        }
    }
    out
}

fn assistant_events(v: &serde_json::Value, ts: &str) -> Vec<TranscriptEvent> {
    let mut out = Vec::new();
    let Some(arr) = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    else {
        return out;
    };
    for item in arr {
        match item.get("type").and_then(|x| x.as_str()) {
            Some("text") => {
                if let Some(t) = item.get("text").and_then(|x| x.as_str()) {
                    out.push(TranscriptEvent {
                        ts: ts.to_string(),
                        kind: TranscriptKind::Assistant,
                        body: t.to_string(),
                    });
                }
            }
            Some("thinking") => {
                if let Some(t) = item.get("thinking").and_then(|x| x.as_str()) {
                    out.push(TranscriptEvent {
                        ts: ts.to_string(),
                        kind: TranscriptKind::Thinking,
                        body: t.to_string(),
                    });
                }
            }
            Some("tool_use") => {
                let name = item
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("tool");
                let input = item.get("input").map(|x| x.to_string()).unwrap_or_default();
                out.push(TranscriptEvent {
                    ts: ts.to_string(),
                    kind: TranscriptKind::ToolUse,
                    body: format!("{name}: {input}"),
                });
            }
            _ => {}
        }
    }
    out
}

// Extract per-message token events. Events are in file order (ts ascending).
// Streaming-merge: an assistant message produces N JSONL lines sharing message.id;
// the parser keeps the last line's usage (output_tokens > 0 gate) as cumulative.
pub fn extract_events(path: &Path) -> Vec<TokenEvent> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let reader = BufReader::new(file);
    let mut out = Vec::new();

    let mut pending_mid = String::new();
    let mut pending_usage: Option<serde_json::Value> = None;
    let mut pending_ts: u64 = 0;
    let mut pending_model = String::new();

    let flush = |out: &mut Vec<TokenEvent>,
                 pu: &Option<serde_json::Value>,
                 pts: u64,
                 pmodel: &str| {
        let Some(u) = pu.as_ref() else { return };
        let mut usage = Usage::default();
        merge_claude_usage(&mut usage, u);
        if usage.input_tokens == 0
            && usage.output_tokens == 0
            && usage.cache_read_tokens == 0
            && usage.cache_creation_total() == 0
        {
            return;
        }
        out.push(TokenEvent {
            ts: pts,
            usage,
            model: pmodel.to_string(),
            is_user_turn: false,
        });
    };

    for line in reader.lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if v.get("isMeta").and_then(|x| x.as_bool()).unwrap_or(false) {
            continue;
        }
        let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        match ty {
            "user" => {
                let ts = v
                    .get("timestamp")
                    .and_then(|x| x.as_str())
                    .and_then(parse_ts_secs)
                    .unwrap_or(0);
                out.push(TokenEvent {
                    ts,
                    usage: Usage::default(),
                    model: String::new(),
                    is_user_turn: true,
                });
                if !pending_mid.is_empty() {
                    flush(&mut out, &pending_usage, pending_ts, &pending_model);
                    pending_mid.clear();
                    pending_usage = None;
                    pending_ts = 0;
                    pending_model.clear();
                }
            }
            "assistant" => {
                let Some(msg) = v.get("message") else { continue };
                let m = msg.get("model").and_then(|x| x.as_str()).unwrap_or("");
                if m == "<synthetic>" {
                    continue;
                }
                let mid = msg
                    .get("id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let u = msg.get("usage").cloned();
                let ts = v
                    .get("timestamp")
                    .and_then(|x| x.as_str())
                    .and_then(parse_ts_secs)
                    .unwrap_or(0);

                if !mid.is_empty() && mid == pending_mid {
                    if let Some(uu) = &u {
                        if uu.get("output_tokens").and_then(|x| x.as_i64()).unwrap_or(0) > 0 {
                            pending_usage = u;
                            pending_ts = ts;
                            if !m.is_empty() {
                                pending_model = m.to_string();
                            }
                        }
                    }
                } else {
                    if !pending_mid.is_empty() {
                        flush(&mut out, &pending_usage, pending_ts, &pending_model);
                    }
                    pending_mid = if mid.is_empty() { "_anon".to_string() } else { mid };
                    pending_usage = u;
                    pending_ts = ts;
                    pending_model = m.to_string();
                }
            }
            _ => {
                if !pending_mid.is_empty() {
                    flush(&mut out, &pending_usage, pending_ts, &pending_model);
                    pending_mid.clear();
                    pending_usage = None;
                    pending_ts = 0;
                    pending_model.clear();
                }
            }
        }
    }
    if !pending_mid.is_empty() {
        flush(&mut out, &pending_usage, pending_ts, &pending_model);
    }
    out
}

fn merge_claude_usage(dst: &mut Usage, u: &serde_json::Value) {
    let get_u = |k: &str| -> u64 {
        u.get(k)
            .and_then(|x| x.as_u64())
            .unwrap_or(0)
    };
    dst.input_tokens += get_u("input_tokens");
    dst.output_tokens += get_u("output_tokens");
    dst.cache_read_tokens += get_u("cache_read_input_tokens");
    dst.cache_creation_tokens += get_u("cache_creation_input_tokens");
    if let Some(cc) = u.get("cache_creation").and_then(|x| x.as_object()) {
        dst.cache_creation_5m_tokens += cc
            .get("ephemeral_5m_input_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        dst.cache_creation_1h_tokens += cc
            .get("ephemeral_1h_input_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
    }
}
