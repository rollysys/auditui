// Tool-call layer: one `ToolEvent` per tool invocation, paired with its result.
//
// Providers walk their transcript once and feed a `ToolCollector`; it pairs
// calls with results by id, assigns user-turn indices, fingerprints arguments
// (exact + shape-normalized) and derives harness-smell flags. The audit
// detectors (`crate::audit`) run on the resulting `ToolTimeline` only — they
// never touch raw transcripts.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

/// Bit flags describing harness smells derived from the call/result text.
pub mod flag {
    /// Command runs through `ssh` (remote work done by hand).
    pub const SSH: u16 = 1 << 0;
    /// Two or more escaped-quote constructs (`\"` / `'"'"'`) in a shell command.
    pub const QUOTE_HELL: u16 = 1 << 1;
    /// `--help`, `-h`, `man`, `--version` lookup: agent re-learning a CLI.
    pub const HELP: u16 = 1 << 2;
    /// Result text mentions a timeout.
    pub const TIMEOUT: u16 = 1 << 3;
    /// Inline script (`python3 -c`, `node -e`, heredoc) — throwaway code.
    pub const INLINE_SCRIPT: u16 = 1 << 4;
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ToolEvent {
    /// Unix seconds of the call.
    pub ts: u64,
    /// 1-based JSONL line of the call (evidence anchor); 0 if unknown.
    pub line: u32,
    /// 1-based JSONL line of the result; 0 if the call never got one.
    pub result_line: u32,
    /// 0-based user-turn index this call belongs to.
    pub turn: u32,
    pub name: String,
    /// Fingerprint of `(name, canonical args)` — exact-duplicate detection.
    pub args_fp: u64,
    /// Fingerprint of `(name, shape(args))` — recurring-pattern detection.
    /// Equal to `args_fp` for tools without a shell-like command.
    pub shape_fp: u64,
    /// Human-readable shape (`python3 -c STR`, `ssh -i PATH root@host STR`)
    /// for shell-like tools; the primary argument (path / query) otherwise.
    pub shape: String,
    /// ≤ `PREVIEW_MAX` chars of the primary argument for display.
    pub preview: String,
    pub flags: u16,
    pub result_bytes: u32,
    pub is_error: bool,
    /// `result.ts − call.ts` in ms; 0 if unknown.
    pub duration_ms: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ToolTimeline {
    pub version: u32,
    pub file_size: u64,
    pub events: Vec<ToolEvent>,
    /// Number of user turns seen.
    pub turns: u32,
    /// Context compactions / summarizations observed.
    pub compactions: u32,
}

pub const TOOLS_CACHE_VERSION: u32 = 1;
pub const PREVIEW_MAX: usize = 160;

/// Tools whose primary argument is a shell command (across providers).
const SHELL_TOOLS: &[(&str, &str)] = &[
    ("bash", "command"),
    ("Bash", "command"),
    ("shell", "command"),
    ("exec_command", "cmd"),
    ("local_shell", "command"),
    ("shell_command", "command"),
];

/// Tools whose primary argument is a path/query worth showing verbatim.
const PRIMARY_ARG: &[(&str, &str)] = &[
    ("read", "path"),
    ("Read", "file_path"),
    ("read_file", "path"),
    ("write", "path"),
    ("Write", "file_path"),
    ("edit", "input"),
    ("Edit", "file_path"),
    ("grep", "pattern"),
    ("Grep", "pattern"),
    ("glob", "path"),
    ("Glob", "pattern"),
    ("search", "pattern"),
    ("task", "context"),
    ("Task", "description"),
    ("web_search", "query"),
    ("WebSearch", "query"),
];

fn shell_command<'a>(name: &str, args: &'a serde_json::Value) -> Option<&'a str> {
    SHELL_TOOLS
        .iter()
        .find(|(n, _)| *n == name)
        .and_then(|(_, key)| args.get(key))
        .and_then(|v| v.as_str())
}

fn primary_arg<'a>(name: &str, args: &'a serde_json::Value) -> Option<&'a str> {
    PRIMARY_ARG
        .iter()
        .find(|(n, _)| *n == name)
        .and_then(|(_, key)| args.get(key))
        .and_then(|v| v.as_str())
}

/// Normalize a shell command to its "shape": string literals → `STR`,
/// path-like tokens → `PATH`, integers → `N`, whitespace collapsed.
pub fn shell_shape(cmd: &str) -> String {
    let mut out = String::with_capacity(cmd.len().min(PREVIEW_MAX));
    let mut chars = cmd.chars().peekable();
    let mut last_space = true;
    while let Some(c) = chars.next() {
        match c {
            '"' | '\'' => {
                // Skip quoted literal (unterminated → to end).
                while let Some(n) = chars.next() {
                    if n == '\\' {
                        chars.next();
                    } else if n == c {
                        break;
                    }
                }
                out.push_str("STR");
                last_space = false;
            }
            c if c.is_whitespace() => {
                if !last_space {
                    out.push(' ');
                    last_space = true;
                }
            }
            _ => {
                let mut tok = String::new();
                tok.push(c);
                while let Some(&n) = chars.peek() {
                    if n.is_whitespace() || n == '"' || n == '\'' {
                        break;
                    }
                    tok.push(n);
                    chars.next();
                }
                let digits = tok.trim_start_matches('-');
                if tok.contains('/') || tok.starts_with('~') {
                    out.push_str("PATH");
                } else if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                    out.push_str(&tok[..tok.len() - digits.len()]);
                    out.push('N');
                } else {
                    out.push_str(&tok);
                }
                last_space = false;
            }
        }
        if out.len() >= PREVIEW_MAX {
            break;
        }
    }
    truncate(out.trim_end().to_string(), PREVIEW_MAX)
}

fn truncate(mut s: String, max: usize) -> String {
    if s.chars().count() > max {
        let cut = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
        s.truncate(cut);
        s.push('…');
    }
    s
}

fn fnv1a(parts: &[&str]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for b in p.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn canonical_args(args: &serde_json::Value) -> String {
    // serde_json::Value objects are BTreeMap-backed by default → key-sorted.
    serde_json::to_string(args).unwrap_or_default()
}

fn call_flags(cmd: &str) -> u16 {
    let mut f = 0;
    if cmd.split(|c: char| !c.is_alphanumeric()).any(|w| w == "ssh") {
        f |= flag::SSH;
    }
    if cmd.matches("\\\"").count() + cmd.matches("'\"'\"'").count() >= 2 {
        f |= flag::QUOTE_HELL;
    }
    if cmd.contains("--help") || cmd.contains(" -h") || cmd.starts_with("man ") || cmd.contains("--version") {
        f |= flag::HELP;
    }
    if cmd.contains("python3 -c") || cmd.contains("python -c") || cmd.contains("node -e") || cmd.contains("<<") {
        f |= flag::INLINE_SCRIPT;
    }
    f
}

fn result_flags(body: &str) -> u16 {
    let head: String = body.chars().take(400).collect::<String>().to_ascii_lowercase();
    if head.contains("timed out") || head.contains("timeout") {
        flag::TIMEOUT
    } else {
        0
    }
}

/// Heuristic error detection for providers that don't carry an explicit flag.
pub fn looks_like_error(body: &str) -> bool {
    let head: String = body.trim_start().chars().take(80).collect::<String>().to_ascii_lowercase();
    head.starts_with("error")
        || head.starts_with("command failed")
        || head.starts_with("traceback")
        || head.starts_with("exit code")
        || head.contains("process exited with code") && !head.contains("code 0")
}

pub struct ToolCollector {
    events: Vec<ToolEvent>,
    pending: HashMap<String, usize>,
    turn: u32,
    saw_user: bool,
    compactions: u32,
}

impl Default for ToolCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolCollector {
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            pending: HashMap::new(),
            turn: 0,
            saw_user: false,
            compactions: 0,
        }
    }

    /// A real user message (not a tool result) starts a new turn.
    pub fn user_turn(&mut self) {
        if self.saw_user {
            self.turn += 1;
        }
        self.saw_user = true;
    }

    pub fn compaction(&mut self) {
        self.compactions += 1;
    }

    pub fn call(&mut self, id: &str, name: &str, args: &serde_json::Value, ts: u64, line: u32) {
        let canon = canonical_args(args);
        let args_fp = fnv1a(&[name, &canon]);
        let (shape, shape_fp, flags, preview) = if let Some(cmd) = shell_command(name, args) {
            let shape = shell_shape(cmd);
            let fp = fnv1a(&[name, &shape]);
            (shape, fp, call_flags(cmd), truncate(cmd.to_string(), PREVIEW_MAX))
        } else if let Some(p) = primary_arg(name, args) {
            let p = truncate(p.lines().next().unwrap_or("").to_string(), PREVIEW_MAX);
            (p.clone(), args_fp, 0, p)
        } else {
            (String::new(), args_fp, 0, truncate(canon, PREVIEW_MAX))
        };
        let idx = self.events.len();
        self.events.push(ToolEvent {
            ts,
            line,
            result_line: 0,
            turn: self.turn,
            name: name.to_string(),
            args_fp,
            shape_fp,
            shape,
            preview,
            flags,
            result_bytes: 0,
            is_error: false,
            duration_ms: 0,
        });
        if !id.is_empty() {
            self.pending.insert(id.to_string(), idx);
        }
    }

    /// Attach a result to its call. `is_error` = provider's explicit flag OR'd
    /// with `looks_like_error` by the caller as appropriate. Unknown ids are
    /// ignored (orphan results carry no attributable cost).
    pub fn result(&mut self, id: &str, body: &str, is_error: bool, ts: u64, line: u32) {
        let Some(idx) = self.pending.remove(id) else {
            return;
        };
        let ev = &mut self.events[idx];
        ev.result_line = line;
        ev.result_bytes = body.len().min(u32::MAX as usize) as u32;
        ev.is_error = is_error;
        ev.flags |= result_flags(body);
        if ts > ev.ts {
            ev.duration_ms = ((ts - ev.ts) * 1000).min(u32::MAX as u64) as u32;
        }
    }

    /// Same as `result` but with millisecond timestamps (omp style).
    pub fn result_ms(&mut self, id: &str, body: &str, is_error: bool, ts_ms: u64, call_ts_ms: u64, line: u32) {
        self.result(id, body, is_error, 0, line);
        if let Some(ev) = self.events.iter_mut().rev().find(|e| e.result_line == line) {
            if ts_ms > call_ts_ms {
                ev.duration_ms = (ts_ms - call_ts_ms).min(u32::MAX as u64) as u32;
            }
        }
    }

    pub fn finish(self, file_size: u64) -> ToolTimeline {
        ToolTimeline {
            version: TOOLS_CACHE_VERSION,
            file_size,
            events: self.events,
            turns: if self.saw_user { self.turn + 1 } else { 0 },
            compactions: self.compactions,
        }
    }
}

impl Hash for ToolEvent {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.args_fp.hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_normalizes_literals_paths_numbers() {
        assert_eq!(shell_shape("grep -n 'foo bar' /a/b.rs | head -20"), "grep -n STR PATH | head -N");
        assert_eq!(shell_shape("python3 -c \"import x\\\"y\""), "python3 -c STR");
        assert_eq!(shell_shape("tsl q --help 2>&1 | head -40"), "tsl q --help 2>&1 | head -N");
    }

    #[test]
    fn flags_detect_ssh_and_quote_hell() {
        let f = call_flags(r#"ssh host "grep \"a\" f | sed \"s/x/y/\"""#);
        assert!(f & flag::SSH != 0);
        assert!(f & flag::QUOTE_HELL != 0);
        assert_eq!(call_flags("ls -la") & flag::SSH, 0);
        assert_eq!(call_flags("rsync a b") & flag::SSH, 0);
    }

    #[test]
    fn collector_pairs_and_counts_turns() {
        let mut c = ToolCollector::new();
        c.user_turn();
        c.call("1", "bash", &serde_json::json!({"command": "ls /tmp"}), 100, 3);
        c.result("1", "a\nb", false, 102, 4);
        c.user_turn();
        c.call("2", "bash", &serde_json::json!({"command": "ls /var"}), 200, 7);
        c.result("2", "Error: nope", true, 200, 8);
        c.result("zzz", "orphan", false, 201, 9);
        let t = c.finish(42);
        assert_eq!(t.turns, 2);
        assert_eq!(t.events.len(), 2);
        assert_eq!(t.events[0].duration_ms, 2000);
        assert_eq!(t.events[0].turn, 0);
        assert_eq!(t.events[1].turn, 1);
        assert!(t.events[1].is_error);
        assert_eq!(t.events[0].shape_fp, t.events[1].shape_fp);
        assert_ne!(t.events[0].args_fp, t.events[1].args_fp);
    }
}
