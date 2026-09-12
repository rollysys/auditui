// oh-my-pi (pi-agent) session discovery.
// Sessions live at ~/.omp/agent/sessions/<encoded-cwd>/<ts>_<uuid>.jsonl.
// Each line is one event; `type` is one of:
//   session                — meta (version, id, timestamp, cwd, title, ...)
//   model_change           — switches the active model (cursor; not on each message)
//   thinking_level_change  — noise, skipped
//   message                — role ∈ {user, assistant, toolResult}; content parts
//                            are {text | thinking | toolCall}
//   custom_message         — injected system reminders
//   compaction             — context-compaction marker with a `summary`
//
// Notable: assistant `message.usage` carries a precomputed `cost` (pi-agent does
// its own pricing). We honor that inline cost rather than recomputing it from
// cost.rs, since auditui's pricing table has no deepseek/ollama/pi entries.

use crate::cache::TokenEvent;
use crate::cost::Usage;
use crate::providers::Agent;
use crate::session::{parse_ts_secs, SessionMeta, TranscriptEvent, TranscriptKind};
use anyhow::Result;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub fn base_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".omp").join("agent").join("sessions"))
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
        let dir = proj.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(&dir) else { continue };
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
    let mut id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut model: Option<String> = None;
    let mut prompt: Option<String> = None;
    let mut turns = 0usize;
    let mut started_at_ts = 0u64;
    for line in reader.lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match v.get("type").and_then(|x| x.as_str()).unwrap_or("") {
            "session" => {
                id = v.get("id").and_then(|x| x.as_str()).map(|s| s.to_string());
                cwd = v.get("cwd").and_then(|x| x.as_str()).map(|s| s.to_string());
                if started_at_ts == 0 {
                    if let Some(t) = v
                        .get("timestamp")
                        .and_then(|x| x.as_str())
                        .and_then(parse_ts_secs)
                    {
                        started_at_ts = t;
                    }
                }
            }
            "model_change" => {
                if model.is_none() {
                    model = v
                        .get("model")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string());
                }
            }
            "message" => {
                let m = v.get("message");
                let role = m
                    .and_then(|x| x.get("role"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                if role == "user" {
                    turns += 1;
                    if prompt.is_none() {
                        if let Some(text) = first_text(m) {
                            prompt = Some(text.chars().take(120).collect());
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Prefer the explicit session-line id; fall back to the uuid in the filename.
    let raw_id = id.unwrap_or_else(|| uuid_from_stem(&stem));

    // Sub-agent transcripts (oh-my-pi `task`/`job` children) live in a sibling
    // directory named after this file's stem. Count them cheaply now (dir
    // listing only); the actual child SessionMetas are parsed lazily via
    // `list_children` when the user expands the parent.
    let child_count = count_child_jsonl(&child_dir_of(path));

    Some(SessionMeta {
        agent: Agent::Omp,
        id: format!("omp:{raw_id}"),
        path: path.to_path_buf(),
        cwd,
        model,
        prompt,
        turns,
        last_active_ts: modified,
        started_at_ts: if started_at_ts > 0 {
            started_at_ts
        } else {
            modified
        },
        // pi-agent transcripts carry no entrypoint/headless marker, so we cannot
        // distinguish scripted from interactive runs.
        is_scripted: false,
        parent_id: None,
        child_count,
    })
}

// The sibling directory holding a session's sub-agent transcripts: the parent
// file path with its `.jsonl` extension stripped.
fn child_dir_of(parent_path: &Path) -> PathBuf {
    parent_path.with_extension("")
}

// Count sub-agent transcripts without reading any file contents.
fn count_child_jsonl(dir: &Path) -> usize {
    let Ok(rd) = fs::read_dir(dir) else {
        return 0;
    };
    rd.flatten()
        .filter(|e| {
            e.path().extension().and_then(|s| s.to_str()) == Some("jsonl")
        })
        .count()
}

/// Lazily load the sub-agent transcripts spawned by `parent`. Each oh-my-pi
/// `task`/`job` child is a self-contained omp transcript in the sibling
/// directory `<parent-stem>/`. Parsed on demand (a parent can spawn hundreds),
/// so this is only called when the user expands the parent in the TUI.
pub fn list_children(parent: &SessionMeta) -> Vec<SessionMeta> {
    let dir = child_dir_of(&parent.path);
    if !dir.is_dir() {
        return vec![];
    }
    let Ok(files) = fs::read_dir(&dir) else {
        return vec![];
    };
    let parent_stem = parent
        .path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let mut out: Vec<SessionMeta> = files
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
        .filter_map(|p| summarize_child(&p, parent, &parent_stem))
        .collect();
    out.sort_by(|a, b| b.last_active_ts.cmp(&a.last_active_ts));
    out
}

// Summarize one sub-agent transcript, reusing the top-level summarizer and
// then stamping the parent linkage, a unique cache-safe id, and a readable
// label derived from the filename slug.
fn summarize_child(path: &Path, parent: &SessionMeta, parent_stem: &str) -> Option<SessionMeta> {
    let mut meta = summarize(path)?;
    let stem = path.file_stem()?.to_string_lossy().to_string();
    // `:` / `/` get sanitized to `_` by the cache layer, so this stays unique
    // and filesystem-safe even across parents that reuse a child slug.
    meta.id = format!("omp:sub:{parent_stem}:{stem}");
    meta.parent_id = Some(parent.id.clone());
    meta.child_count = 0; // sub-agents don't nest further in practice
    if meta.cwd.is_none() {
        meta.cwd = parent.cwd.clone();
    }
    // The filename slug (e.g. "ScanTrigFunctions", "fg088") identifies the
    // sub-agent far better than its first message, which is usually a large
    // injected context block shared across siblings.
    let slug = stem
        .split_once('-')
        .map(|(_, s)| s)
        .filter(|s| !s.is_empty())
        .unwrap_or(&stem)
        .to_string();
    meta.prompt = Some(match meta.prompt.take() {
        Some(p) if !p.trim().is_empty() => format!("{slug} — {p}"),
        _ => slug,
    });
    Some(meta)
}

// Filename is "<iso-ts>_<uuid>.jsonl"; the uuid is the segment after the first '_'.
fn uuid_from_stem(stem: &str) -> String {
    stem.split_once('_')
        .map(|(_, u)| u.to_string())
        .unwrap_or_else(|| stem.to_string())
}

// First non-empty text part of a message's content array.
fn first_text(message: Option<&serde_json::Value>) -> Option<String> {
    let parts = message?.get("content")?.as_array()?;
    parts.iter().find_map(|p| {
        if p.get("type").and_then(|x| x.as_str()) == Some("text") {
            p.get("text")
                .and_then(|x| x.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.to_string())
        } else {
            None
        }
    })
}

// Extract per-message token events. Walks model_change as a cursor so each
// assistant event is stamped with the model active at that point.
pub fn extract_events(path: &Path) -> Vec<TokenEvent> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    let mut cur_model = String::new();

    for line in reader.lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let ts = v
            .get("timestamp")
            .and_then(|x| x.as_str())
            .and_then(parse_ts_secs)
            .unwrap_or(0);
        match v.get("type").and_then(|x| x.as_str()).unwrap_or("") {
            "model_change" => {
                if let Some(m) = v.get("model").and_then(|x| x.as_str()) {
                    cur_model = m.to_string();
                }
            }
            "message" => {
                let Some(m) = v.get("message") else { continue };
                match m.get("role").and_then(|x| x.as_str()).unwrap_or("") {
                    "user" => out.push(TokenEvent {
                        ts,
                        usage: Usage::default(),
                        model: String::new(),
                        is_user_turn: true,
                    }),
                    "assistant" => {
                        if let Some(u) = m.get("usage") {
                            let mut usage = Usage::default();
                            usage.input_tokens = u.get("input").and_then(|x| x.as_u64()).unwrap_or(0);
                            usage.output_tokens =
                                u.get("output").and_then(|x| x.as_u64()).unwrap_or(0);
                            usage.cache_read_tokens =
                                u.get("cacheRead").and_then(|x| x.as_u64()).unwrap_or(0);
                            // pi-agent reports a single cache-write bucket; map it to
                            // the legacy single-bucket field.
                            usage.cache_creation_tokens =
                                u.get("cacheWrite").and_then(|x| x.as_u64()).unwrap_or(0);
                            // Honor pi-agent's own per-message cost.
                            usage.cost_override = u
                                .get("cost")
                                .and_then(|c| c.get("total"))
                                .and_then(|x| x.as_f64());
                            out.push(TokenEvent {
                                ts,
                                usage,
                                model: cur_model.clone(),
                                is_user_turn: false,
                            });
                        }
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
        match v.get("type").and_then(|x| x.as_str()).unwrap_or("") {
            "model_change" => {
                if let Some(m) = v.get("model").and_then(|x| x.as_str()) {
                    out.push(TranscriptEvent {
                        ts,
                        kind: TranscriptKind::System,
                        body: format!("model → {m}"),
                    });
                }
            }
            "custom_message" => {
                if let Some(c) = v.get("content").and_then(|x| x.as_str()) {
                    out.push(TranscriptEvent {
                        ts,
                        kind: TranscriptKind::System,
                        body: c.to_string(),
                    });
                }
            }
            "compaction" => {
                let summary = v
                    .get("summary")
                    .and_then(|x| x.as_str())
                    .unwrap_or("(context compacted)");
                out.push(TranscriptEvent {
                    ts,
                    kind: TranscriptKind::System,
                    body: format!("[compaction]\n{summary}"),
                });
            }
            "message" => {
                let Some(m) = v.get("message") else { continue };
                let role = m.get("role").and_then(|x| x.as_str()).unwrap_or("");
                if role == "toolResult" {
                    let name = m.get("toolName").and_then(|x| x.as_str()).unwrap_or("");
                    let text = join_text_parts(m.get("content"));
                    let prefix = if name.is_empty() {
                        String::new()
                    } else {
                        format!("[{name}] ")
                    };
                    out.push(TranscriptEvent {
                        ts,
                        kind: TranscriptKind::ToolResult,
                        body: format!("{prefix}{text}"),
                    });
                    continue;
                }
                let default_kind = match role {
                    "user" => TranscriptKind::User,
                    "assistant" => TranscriptKind::Assistant,
                    _ => TranscriptKind::System,
                };
                let Some(parts) = m.get("content").and_then(|x| x.as_array()) else {
                    continue;
                };
                for part in parts {
                    match part.get("type").and_then(|x| x.as_str()).unwrap_or("") {
                        "thinking" => {
                            if let Some(t) = part.get("thinking").and_then(|x| x.as_str()) {
                                out.push(TranscriptEvent {
                                    ts: ts.clone(),
                                    kind: TranscriptKind::Thinking,
                                    body: t.to_string(),
                                });
                            }
                        }
                        "toolCall" => {
                            let name = part.get("name").and_then(|x| x.as_str()).unwrap_or("fn");
                            let args = part
                                .get("arguments")
                                .map(|x| x.to_string())
                                .unwrap_or_default();
                            out.push(TranscriptEvent {
                                ts: ts.clone(),
                                kind: TranscriptKind::ToolUse,
                                body: format!("{name}: {args}"),
                            });
                        }
                        "text" => {
                            if let Some(t) = part.get("text").and_then(|x| x.as_str()) {
                                out.push(TranscriptEvent {
                                    ts: ts.clone(),
                                    kind: default_kind,
                                    body: t.to_string(),
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

// Concatenate the text of a content array (toolResult content is text parts).
fn join_text_parts(content: Option<&serde_json::Value>) -> String {
    let Some(arr) = content.and_then(|x| x.as_array()) else {
        return content
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();
    };
    arr.iter()
        .filter_map(|p| p.get("text").and_then(|x| x.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const FIXTURE: &str = r###"{"type":"session","version":3,"id":"019e0000-1111-7000-8000-000000000000","timestamp":"2026-05-23T10:38:08.095Z","cwd":"/tmp/demo","title":"t","titleSource":"auto"}
{"type":"model_change","id":"m1","timestamp":"2026-05-23T10:38:09.000Z","model":"deepseek/deepseek-v4-pro"}
{"type":"thinking_level_change","id":"tl","timestamp":"2026-05-23T10:38:09.100Z"}
{"type":"message","id":"u1","timestamp":"2026-05-23T10:38:10.000Z","message":{"role":"user","content":[{"type":"text","text":"hello world"}]}}
{"type":"message","id":"a1","timestamp":"2026-05-23T10:38:11.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"},{"type":"toolCall","id":"call1","name":"read","arguments":{"path":"/x"}}],"usage":{"input":100,"output":50,"cacheRead":10,"cacheWrite":5,"cost":{"total":0.0025}}}}
{"type":"message","id":"t1","timestamp":"2026-05-23T10:38:12.000Z","message":{"role":"toolResult","toolCallId":"call1","toolName":"read","content":[{"type":"text","text":"file contents"}]}}
{"type":"message","id":"a2","timestamp":"2026-05-23T10:38:13.000Z","message":{"role":"assistant","content":[{"type":"text","text":"done"}],"usage":{"input":20,"output":8,"cacheRead":90,"cacheWrite":0,"cost":{"total":0.0011}}}}
{"type":"custom_message","customType":"reminder","content":"<system-reminder>note</system-reminder>","timestamp":"2026-05-23T10:38:14.000Z"}
{"type":"compaction","id":"c1","timestamp":"2026-05-23T10:38:15.000Z","summary":"## Goal\nstuff"}
"###;

    fn write_fixture() -> PathBuf {
        let p = std::env::temp_dir().join(format!("omp_fixture_{}.jsonl", std::process::id()));
        let mut f = File::create(&p).unwrap();
        f.write_all(FIXTURE.as_bytes()).unwrap();
        p
    }

    #[test]
    fn summarize_pulls_meta_from_session_and_messages() {
        let p = write_fixture();
        let meta = summarize(&p).unwrap();
        std::fs::remove_file(&p).ok();

        assert_eq!(meta.id, "omp:019e0000-1111-7000-8000-000000000000");
        assert_eq!(meta.cwd.as_deref(), Some("/tmp/demo"));
        assert_eq!(meta.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
        assert_eq!(meta.prompt.as_deref(), Some("hello world"));
        assert_eq!(meta.turns, 1);
        assert!(!meta.is_scripted);
        assert!(meta.started_at_ts > 0);
        assert!(meta.parent_id.is_none());
        assert_eq!(meta.child_count, 0);
    }

    #[test]
    fn child_sub_agents_are_discovered_and_linked() {
        let root = std::env::temp_dir().join(format!("omp_child_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        // Parent transcript + a sibling dir holding one sub-agent transcript
        // and some non-jsonl noise (bash logs etc.) that must be ignored.
        let parent_path = root.join("2026-05-25T08-41-00-000Z_aaaa.jsonl");
        fs::write(&parent_path, FIXTURE).unwrap();
        let child_dir = root.join("2026-05-25T08-41-00-000Z_aaaa");
        fs::create_dir_all(&child_dir).unwrap();
        fs::write(child_dir.join("0-ScanTrig.jsonl"), FIXTURE).unwrap();
        fs::write(child_dir.join("10065.bash.log"), b"noise").unwrap();

        let parent = summarize(&parent_path).unwrap();
        assert_eq!(parent.child_count, 1, "only the .jsonl child is counted");
        assert!(parent.parent_id.is_none());

        let kids = list_children(&parent);
        assert_eq!(kids.len(), 1);
        let kid = &kids[0];
        assert_eq!(kid.parent_id.as_deref(), Some(parent.id.as_str()));
        assert_eq!(
            kid.id,
            "omp:sub:2026-05-25T08-41-00-000Z_aaaa:0-ScanTrig"
        );
        assert_eq!(kid.child_count, 0);
        // Filename slug drives the label; the parent's cwd is inherited.
        assert!(kid.prompt.as_deref().unwrap().starts_with("ScanTrig"));
        assert_eq!(kid.cwd.as_deref(), Some("/tmp/demo"));

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn extract_events_maps_tokens_and_honors_inline_cost() {
        let p = write_fixture();
        let events = extract_events(&p);
        std::fs::remove_file(&p).ok();

        // one user marker + two assistant token events
        assert_eq!(events.len(), 3);
        assert!(events[0].is_user_turn);
        assert!(events[0].usage.cost_override.is_none());

        let a1 = &events[1];
        assert_eq!(a1.usage.input_tokens, 100);
        assert_eq!(a1.usage.output_tokens, 50);
        assert_eq!(a1.usage.cache_read_tokens, 10);
        assert_eq!(a1.usage.cache_creation_tokens, 5); // cacheWrite → legacy bucket
        assert_eq!(a1.usage.cost_override, Some(0.0025));
        assert_eq!(a1.model, "deepseek/deepseek-v4-pro"); // model_change cursor

        assert_eq!(events[2].usage.cost_override, Some(0.0011));

        // Aggregating sums the present overrides.
        let mut total = Usage::default();
        for e in &events {
            total.add(&e.usage);
        }
        assert_eq!(total.cost_override, Some(0.0036));
    }

    #[test]
    fn read_transcript_covers_all_event_kinds() {
        let p = write_fixture();
        let events = read_transcript(&p).unwrap();
        std::fs::remove_file(&p).ok();

        let kinds: Vec<&str> = events.iter().map(|e| e.kind.label().trim()).collect();
        // model_change → SYS; thinking_level_change skipped; then the turn flow.
        assert_eq!(
            kinds,
            vec!["SYS", "USER", "THINK", "TOOL→", "TOOL←", "ASSIS", "SYS", "SYS"]
        );

        assert!(events[0].body.contains("model → deepseek/deepseek-v4-pro"));
        assert_eq!(events[1].body, "hello world");
        assert_eq!(events[2].body, "hmm");
        assert!(events[3].body.starts_with("read: "));
        assert!(events[4].body.contains("[read]") && events[4].body.contains("file contents"));
        assert_eq!(events[5].body, "done");
        assert!(events[6].body.contains("<system-reminder>"));
        assert!(events[7].body.contains("[compaction]") && events[7].body.contains("## Goal"));
    }
}
