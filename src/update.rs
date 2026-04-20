//! Background check for a newer GitHub release of auditui.
//!
//! Philosophy: cheap, opt-out-able, never blocks UI. On startup the main
//! thread spawns a single worker; worker hits GitHub's `releases/latest`
//! API at most once per 24h (cached to `~/.auditui.json`) and publishes
//! the newest tag (stripped to just the string) to a shared `Arc<Mutex<_>>`
//! when it's semver-greater than the binary's own `CARGO_PKG_VERSION`.
//! The TUI topbar polls this on each draw and renders a subtle yellow
//! "↑ vX.Y.Z" nudge when present.
//!
//! Opt-out: set `AUDITUI_NO_UPDATE_CHECK=1`. No check fires; no network;
//! no cache write.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const API_URL: &str = "https://api.github.com/repos/rollysys/auditui/releases/latest";
const CACHE_TTL_SECS: u64 = 86_400; // 24h
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

fn user_agent() -> String {
    format!("auditui/{}", env!("CARGO_PKG_VERSION"))
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
struct CacheEntry {
    latest_tag: String,
    checked_at: u64,
}

fn cache_path() -> Option<PathBuf> {
    dirs::home_dir().map(|d| d.join(".auditui.json"))
}

fn load_cache() -> Option<CacheEntry> {
    let p = cache_path()?;
    let data = std::fs::read_to_string(&p).ok()?;
    serde_json::from_str(&data).ok()
}

fn save_cache(entry: &CacheEntry) {
    // Write via tmp + rename so a crash mid-write never leaves the cache
    // file half-written (next read would fail JSON parse and force a
    // network roundtrip every launch).
    let Some(p) = cache_path() else { return };
    let Ok(s) = serde_json::to_string_pretty(entry) else { return };
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, s).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn fetch_latest_tag() -> Option<String> {
    let resp = ureq::get(API_URL)
        .set("User-Agent", &user_agent())
        .set("Accept", "application/vnd.github+json")
        .timeout(HTTP_TIMEOUT)
        .call()
        .ok()?;
    let v: serde_json::Value = resp.into_json().ok()?;
    v.get("tag_name")
        .and_then(|t| t.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Parse "v1.2.3" / "1.2.3" / "1.2" into (major, minor, patch). Returns
/// `None` if the first two components aren't integers. Extra suffixes
/// like "-rc1" are ignored on the last component.
fn parse_semver(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.strip_prefix('v').unwrap_or(s);
    let mut parts = s.split('.');
    let major = parts.next()?.parse::<u32>().ok()?;
    let minor = parts.next()?.parse::<u32>().ok()?;
    let patch_str = parts.next().unwrap_or("0");
    // Trim any "-foo" suffix off the patch component before parsing.
    let patch_clean = patch_str.split(|c: char| !c.is_ascii_digit()).next().unwrap_or("0");
    let patch = patch_clean.parse::<u32>().unwrap_or(0);
    Some((major, minor, patch))
}

fn is_newer(candidate_tag: &str, current_version: &str) -> bool {
    match (parse_semver(candidate_tag), parse_semver(current_version)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// Return Some(tag) if a newer release is available, else None.
/// Uses cache when fresh; otherwise hits GitHub once and updates cache.
pub fn check_latest(current_version: &str) -> Option<String> {
    let now = now_secs();
    if let Some(c) = load_cache() {
        if now.saturating_sub(c.checked_at) < CACHE_TTL_SECS && !c.latest_tag.is_empty() {
            return is_newer(&c.latest_tag, current_version).then_some(c.latest_tag);
        }
    }
    let tag = fetch_latest_tag()?;
    save_cache(&CacheEntry {
        latest_tag: tag.clone(),
        checked_at: now,
    });
    is_newer(&tag, current_version).then_some(tag)
}

/// Shared handle to the "there's a newer version" hint. `latest()` returns
/// `Some(tag)` once the background worker has determined an update is
/// available; otherwise `None`.
#[derive(Clone, Default)]
pub struct UpdateState(Arc<Mutex<Option<String>>>);

impl UpdateState {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn latest(&self) -> Option<String> {
        self.0.lock().ok().and_then(|g| g.clone())
    }
    fn set(&self, tag: String) {
        if let Ok(mut g) = self.0.lock() {
            *g = Some(tag);
        }
    }
}

/// Spawn a background worker that checks for updates. No-op when the
/// `AUDITUI_NO_UPDATE_CHECK` env var is set to `1`. Never panics — all
/// errors (no network, timeout, parse failure) silently leave the hint
/// empty.
pub fn spawn_check(state: UpdateState) {
    if std::env::var("AUDITUI_NO_UPDATE_CHECK").ok().as_deref() == Some("1") {
        return;
    }
    std::thread::spawn(move || {
        if let Some(tag) = check_latest(env!("CARGO_PKG_VERSION")) {
            state.set(tag);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_semver() {
        assert_eq!(parse_semver("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_semver("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_semver("0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse_semver("v0.1.2-rc1"), Some((0, 1, 2)));
        assert_eq!(parse_semver("v0.10.0"), Some((0, 10, 0)));
        assert_eq!(parse_semver("v1.2"), Some((1, 2, 0)));
        assert_eq!(parse_semver("garbage"), None);
        assert_eq!(parse_semver(""), None);
    }

    #[test]
    fn compares_semver_numerically_not_lexically() {
        // Lexical would call "0.9.0" > "0.10.0"; semver must not.
        assert!(is_newer("v0.10.0", "0.9.0"));
        assert!(!is_newer("v0.9.0", "0.10.0"));
        assert!(is_newer("v0.1.2", "0.1.1"));
        assert!(!is_newer("v0.1.1", "0.1.1"));
        assert!(!is_newer("v0.1.0", "0.1.1"));
        assert!(is_newer("v1.0.0", "0.99.99"));
    }

    #[test]
    fn malformed_tags_do_not_trigger_update() {
        assert!(!is_newer("garbage", "0.1.0"));
        assert!(!is_newer("", "0.1.0"));
    }
}
