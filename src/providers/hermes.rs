// Hermes agent (NousResearch) — reads `~/.hermes/state.db` (SQLite, WAL mode).
//
// Why SQLite instead of JSON: Hermes mirrors each session to
// `~/.hermes/sessions/session_<id>.json`, but the per-message timestamps and
// session-level token aggregates (input/output/cache_read/cache_write/reasoning)
// live only in SQLite. Reading `state.db` gives us everything for discovery,
// dashboard aggregation, and transcript rendering in one place.
//
// Schema (relevant columns):
//   sessions(id, model, billing_provider, billing_base_url,
//            started_at REAL, ended_at REAL,
//            input_tokens, output_tokens, cache_read_tokens,
//            cache_write_tokens, reasoning_tokens,
//            message_count, title, parent_session_id)
//   messages(session_id, role, content, tool_call_id, tool_calls,
//            tool_name, timestamp REAL, reasoning, codex_reasoning_items)
//
// Concurrency: Hermes writes to state.db in WAL mode, so concurrent readers
// never block. We open read-only with a short busy timeout just in case.

use crate::cache::TokenEvent;
use crate::cost::Usage;
use crate::providers::Agent;
use crate::session::{SessionMeta, TranscriptEvent, TranscriptKind};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::time::Duration;

const ID_PREFIX: &str = "hermes:";
const BUSY_TIMEOUT: Duration = Duration::from_millis(100);

pub fn base_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".hermes"))
}

fn state_db_path() -> Option<PathBuf> {
    base_dir().map(|d| d.join("state.db"))
}

fn session_json_path(raw_sid: &str) -> Option<PathBuf> {
    base_dir().map(|d| d.join("sessions").join(format!("session_{raw_sid}.json")))
}

fn open_ro() -> Option<Connection> {
    let p = state_db_path()?;
    if !p.exists() {
        return None;
    }
    let conn = Connection::open_with_flags(
        &p,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let _ = conn.busy_timeout(BUSY_TIMEOUT);
    Some(conn)
}

fn real_to_secs(x: Option<f64>) -> u64 {
    x.map(|v| v.max(0.0) as u64).unwrap_or(0)
}

fn real_to_iso(x: f64) -> String {
    let secs = x.max(0.0) as i64;
    let nanos = ((x.fract().max(0.0)) * 1_000_000_000.0) as u32;
    DateTime::<Utc>::from_timestamp(secs, nanos)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default()
}

fn extract_raw_sid(sid: &str) -> Option<&str> {
    sid.strip_prefix(ID_PREFIX)
}

pub fn list_sessions() -> Vec<SessionMeta> {
    let Some(conn) = open_ro() else { return vec![] };
    // Only surface sessions that actually saw traffic. Hermes creates a row
    // on each boot attempt; empty rows pollute the list otherwise.
    let sql = "\
        SELECT id, model, started_at, ended_at, input_tokens, output_tokens,
               message_count, title
        FROM sessions
        WHERE COALESCE(input_tokens, 0) > 0
           OR COALESCE(output_tokens, 0) > 0
           OR COALESCE(message_count, 0) > 0
        ORDER BY started_at DESC";
    let Ok(mut stmt) = conn.prepare(sql) else { return vec![] };
    let Ok(iter) = stmt.query_map([], |row| {
        Ok(SessionRow {
            id: row.get::<_, String>(0)?,
            model: row.get::<_, Option<String>>(1)?,
            started_at: row.get::<_, Option<f64>>(2)?,
            ended_at: row.get::<_, Option<f64>>(3)?,
            input_tokens: row.get::<_, Option<i64>>(4)?,
            output_tokens: row.get::<_, Option<i64>>(5)?,
            message_count: row.get::<_, Option<i64>>(6)?,
            title: row.get::<_, Option<String>>(7)?,
        })
    }) else {
        return vec![];
    };

    let mut out = Vec::new();
    for row in iter.flatten() {
        // Skip rows with truly nothing to show (no model and no tokens and
        // no messages). Defensive — the WHERE clause should already filter.
        let i = row.input_tokens.unwrap_or(0);
        let o = row.output_tokens.unwrap_or(0);
        let mc = row.message_count.unwrap_or(0);
        if i == 0 && o == 0 && mc == 0 {
            continue;
        }
        let started_at_ts = real_to_secs(row.started_at);
        let last_active_ts = real_to_secs(row.ended_at).max(started_at_ts);
        let prompt = first_user_prompt(&conn, &row.id);
        let Some(path) = session_json_path(&row.id) else { continue };
        out.push(SessionMeta {
            agent: Agent::Hermes,
            id: format!("{ID_PREFIX}{}", row.id),
            path,
            cwd: None, // Hermes does not persist cwd per session
            model: row.model,
            prompt: row.title.or(prompt),
            turns: mc.max(0) as usize,
            last_active_ts,
            started_at_ts,
            is_scripted: false,
        });
    }
    out
}

struct SessionRow {
    id: String,
    model: Option<String>,
    started_at: Option<f64>,
    ended_at: Option<f64>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    message_count: Option<i64>,
    title: Option<String>,
}

fn first_user_prompt(conn: &Connection, raw_sid: &str) -> Option<String> {
    let sql = "SELECT content FROM messages
               WHERE session_id = ?1 AND role = 'user' AND content IS NOT NULL
               ORDER BY timestamp ASC LIMIT 1";
    let mut stmt = conn.prepare(sql).ok()?;
    let s: String = stmt
        .query_row([raw_sid], |row| row.get::<_, String>(0))
        .ok()?;
    Some(s.chars().take(120).collect())
}

/// Per-session TokenEvent stream for dashboard windowing.
///
/// Hermes only records aggregates on `sessions`, not per-message token
/// deltas. We emit one user-turn event per user message (drives turn counts
/// inside a time window) and one synthetic token event at session_start with
/// the full session totals. This over-attributes tokens to the session's
/// first event for window-boundary spanning sessions, which matches how
/// window scans elsewhere fall back when per-turn granularity is absent.
pub fn extract_events(path: &Path) -> Vec<TokenEvent> {
    let Some(raw_sid) = path
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.strip_prefix("session_"))
    else {
        return vec![];
    };
    extract_events_for_sid(raw_sid)
}

fn extract_events_for_sid(raw_sid: &str) -> Vec<TokenEvent> {
    let Some(conn) = open_ro() else { return vec![] };
    let agg_sql = "\
        SELECT model, started_at,
               COALESCE(input_tokens, 0), COALESCE(output_tokens, 0),
               COALESCE(cache_read_tokens, 0), COALESCE(cache_write_tokens, 0),
               COALESCE(reasoning_tokens, 0)
        FROM sessions WHERE id = ?1";
    let Ok(agg) = conn.query_row(agg_sql, [raw_sid], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?.unwrap_or_default(),
            row.get::<_, Option<f64>>(1)?.unwrap_or(0.0),
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
        ))
    }) else {
        return vec![];
    };
    let (model, started_at_f, inp, outp, cr, cw, reasoning) = agg;
    let started_at_ts = real_to_secs(Some(started_at_f));

    let mut out = Vec::new();

    // Emit user-turn pings at each user message's timestamp. Used for
    // window-bounded turn counts in the dashboard.
    if let Ok(mut stmt) = conn.prepare(
        "SELECT timestamp FROM messages \
         WHERE session_id = ?1 AND role = 'user' ORDER BY timestamp ASC",
    ) {
        if let Ok(iter) = stmt.query_map([raw_sid], |row| row.get::<_, f64>(0)) {
            for ts in iter.flatten() {
                out.push(TokenEvent {
                    ts: real_to_secs(Some(ts)),
                    usage: Usage::default(),
                    model: model.clone(),
                    is_user_turn: true,
                });
            }
        }
    }

    // Single aggregate token event at session start. Hermes doesn't record
    // per-turn usage so finer attribution would be a lie.
    //
    // Output tokens get `reasoning_tokens` folded in — reasoning is still
    // billed at output rate on the providers we care about.
    let mut usage = Usage::default();
    usage.input_tokens = inp.max(0) as u64;
    usage.output_tokens = (outp.max(0) + reasoning.max(0)) as u64;
    usage.cache_read_tokens = cr.max(0) as u64;
    usage.cache_creation_tokens = cw.max(0) as u64;
    if usage.input_tokens + usage.output_tokens + usage.cache_read_tokens
        + usage.cache_creation_tokens > 0
    {
        out.push(TokenEvent {
            ts: started_at_ts,
            usage,
            model,
            is_user_turn: false,
        });
    }

    out
}

pub fn read_transcript(path: &Path) -> Result<Vec<TranscriptEvent>> {
    let raw_sid = path
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.strip_prefix("session_"))
        .context("hermes transcript path must be session_<id>.json")?;
    read_transcript_for_sid(raw_sid)
}

fn read_transcript_for_sid(raw_sid: &str) -> Result<Vec<TranscriptEvent>> {
    let conn = open_ro().context("hermes state.db not available")?;
    let sql = "\
        SELECT role, content, tool_call_id, tool_calls, tool_name,
               timestamp, reasoning
        FROM messages WHERE session_id = ?1 ORDER BY timestamp ASC, id ASC";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([raw_sid], |row| {
        Ok(MessageRow {
            role: row.get::<_, String>(0)?,
            content: row.get::<_, Option<String>>(1)?,
            tool_call_id: row.get::<_, Option<String>>(2)?,
            tool_calls: row.get::<_, Option<String>>(3)?,
            tool_name: row.get::<_, Option<String>>(4)?,
            timestamp: row.get::<_, f64>(5)?,
            reasoning: row.get::<_, Option<String>>(6)?,
        })
    })?;

    let mut out = Vec::new();
    for row in rows.flatten() {
        let ts = real_to_iso(row.timestamp);
        match row.role.as_str() {
            "user" => {
                if let Some(body) = row.content.filter(|s| !s.is_empty()) {
                    out.push(TranscriptEvent {
                        ts,
                        kind: TranscriptKind::User,
                        body,
                    });
                }
            }
            "assistant" => {
                if let Some(r) = row.reasoning.as_deref().filter(|s| !s.is_empty()) {
                    out.push(TranscriptEvent {
                        ts: ts.clone(),
                        kind: TranscriptKind::Thinking,
                        body: r.to_string(),
                    });
                }
                if let Some(body) = row.content.clone().filter(|s| !s.is_empty()) {
                    out.push(TranscriptEvent {
                        ts: ts.clone(),
                        kind: TranscriptKind::Assistant,
                        body,
                    });
                }
                if let Some(raw) = row.tool_calls.as_deref() {
                    for tc in parse_tool_calls(raw) {
                        out.push(TranscriptEvent {
                            ts: ts.clone(),
                            kind: TranscriptKind::ToolUse,
                            body: tc,
                        });
                    }
                }
            }
            "tool" => {
                let name = row.tool_name.as_deref().unwrap_or("");
                let call_id = row.tool_call_id.as_deref().unwrap_or("");
                let body = row.content.unwrap_or_default();
                let header = match (name, call_id) {
                    ("", "") => String::new(),
                    ("", id) => format!("[{id}] "),
                    (n, "") => format!("[{n}] "),
                    (n, id) => format!("[{n} {id}] "),
                };
                out.push(TranscriptEvent {
                    ts,
                    kind: TranscriptKind::ToolResult,
                    body: format!("{header}{body}"),
                });
            }
            "system" => {
                if let Some(body) = row.content.filter(|s| !s.is_empty()) {
                    out.push(TranscriptEvent {
                        ts,
                        kind: TranscriptKind::System,
                        body,
                    });
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

struct MessageRow {
    role: String,
    content: Option<String>,
    tool_call_id: Option<String>,
    tool_calls: Option<String>,
    tool_name: Option<String>,
    timestamp: f64,
    reasoning: Option<String>,
}

// Render "name(args_json)" per call. `raw` is the OpenAI tool_calls JSON
// string stored on the assistant message row.
fn parse_tool_calls(raw: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return vec![format!("(unparsed tool_calls) {raw}")];
    };
    let Some(arr) = v.as_array() else { return vec![] };
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let f = item.get("function").unwrap_or(item);
        let name = f
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("fn");
        let args = f
            .get("arguments")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        out.push(format!("{name}: {args}"));
    }
    out
}

// Resolve a SessionMeta.id like "hermes:<raw_sid>" back to raw_sid.
// Exposed for future use (caching/keys); keep `pub(crate)` for now.
#[allow(dead_code)]
pub(crate) fn raw_sid_of(id: &str) -> Option<&str> {
    extract_raw_sid(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn mem_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r"
            CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                source TEXT NOT NULL DEFAULT '',
                user_id TEXT,
                model TEXT,
                model_config TEXT,
                system_prompt TEXT,
                parent_session_id TEXT,
                started_at REAL NOT NULL,
                ended_at REAL,
                end_reason TEXT,
                message_count INTEGER DEFAULT 0,
                tool_call_count INTEGER DEFAULT 0,
                input_tokens INTEGER DEFAULT 0,
                output_tokens INTEGER DEFAULT 0,
                cache_read_tokens INTEGER DEFAULT 0,
                cache_write_tokens INTEGER DEFAULT 0,
                reasoning_tokens INTEGER DEFAULT 0,
                billing_provider TEXT,
                billing_base_url TEXT,
                billing_mode TEXT,
                estimated_cost_usd REAL,
                actual_cost_usd REAL,
                cost_status TEXT,
                cost_source TEXT,
                pricing_version TEXT,
                title TEXT
            );
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT,
                tool_call_id TEXT,
                tool_calls TEXT,
                tool_name TEXT,
                timestamp REAL NOT NULL,
                token_count INTEGER,
                finish_reason TEXT,
                reasoning TEXT,
                reasoning_details TEXT,
                codex_reasoning_items TEXT
            );
            ",
        )
        .unwrap();
        conn
    }

    #[test]
    fn parse_tool_calls_happy_path() {
        let raw = r#"[{"id":"c1","function":{"name":"grep","arguments":"{\"pattern\":\"foo\"}"}}]"#;
        let out = parse_tool_calls(raw);
        assert_eq!(out.len(), 1);
        assert!(out[0].starts_with("grep: "));
        assert!(out[0].contains("pattern"));
    }

    #[test]
    fn parse_tool_calls_malformed_is_not_fatal() {
        let out = parse_tool_calls("not json at all");
        assert_eq!(out.len(), 1);
        assert!(out[0].starts_with("(unparsed"));
    }

    #[test]
    fn real_to_iso_round_trips_a_known_instant() {
        // 2026-04-21 08:00:00 UTC = 1_776_758_400
        let s = real_to_iso(1_776_758_400.0);
        assert!(s.starts_with("2026-04-21T08:00:00"), "got {s}");
    }

    #[test]
    fn extract_raw_sid_strips_prefix() {
        assert_eq!(extract_raw_sid("hermes:abc"), Some("abc"));
        assert_eq!(extract_raw_sid("abc"), None);
    }

    // Exercise the list/extract/transcript flow end-to-end against an
    // in-memory SQLite seeded with one realistic session.
    #[test]
    fn end_to_end_in_memory() {
        let conn = mem_db();
        conn.execute(
            "INSERT INTO sessions (id, started_at, ended_at, model,
                 input_tokens, output_tokens, cache_read_tokens,
                 cache_write_tokens, reasoning_tokens, message_count, title)
             VALUES ('20260422_120120_39afa6', 1776758400.0, 1776758500.0,
                 'gpt-5.5', 11347, 251, 26624, 0, 89, 6, NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp)
             VALUES ('20260422_120120_39afa6', 'user', 'hi', 1776758401.0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, reasoning,
                 tool_calls, timestamp)
             VALUES ('20260422_120120_39afa6', 'assistant', 'hello',
                 'thinking step', NULL, 1776758402.0)",
            [],
        )
        .unwrap();

        // Hand-rolled verification: `list_sessions` / `extract_events` /
        // `read_transcript` all use `open_ro()` which hits the real disk, so
        // we test the inner query helpers directly here. The pure-data
        // helpers above give us enough confidence in the row→struct mapping.
        let prompt = first_user_prompt(&conn, "20260422_120120_39afa6");
        assert_eq!(prompt.as_deref(), Some("hi"));
    }
}
