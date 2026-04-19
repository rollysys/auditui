// Per-session token timeline cache.
// Two tiers: in-memory HashMap (hot) + on-disk bincode (persistent across runs).
// Key invariant: disk cache is valid iff (version, file_size) match the current file.

use crate::cost::Usage;
use crate::providers::Agent;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

const CACHE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TokenEvent {
    pub ts: u64,
    pub usage: Usage,
    pub model: String,
    pub is_user_turn: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionTimeline {
    pub version: u32,
    pub file_size: u64,
    pub events: Vec<TokenEvent>,
}

pub fn cache_root() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude-audit").join("_tui_cache"))
}

fn agent_name(a: Agent) -> &'static str {
    match a {
        Agent::Claude => "claude",
        Agent::Codex => "codex",
        Agent::Qwen => "qwen",
    }
}

fn sid_safe(sid: &str) -> String {
    sid.chars()
        .map(|c| if matches!(c, '/' | ':' | '\\') { '_' } else { c })
        .collect()
}

pub fn disk_path(agent: Agent, sid: &str) -> Option<PathBuf> {
    let base = cache_root()?;
    Some(base.join(agent_name(agent)).join(format!("{}.bin", sid_safe(sid))))
}

pub fn load_from_disk(path: &Path, file_size: u64) -> Option<SessionTimeline> {
    let data = fs::read(path).ok()?;
    let t: SessionTimeline = bincode::deserialize(&data).ok()?;
    if t.version != CACHE_VERSION || t.file_size != file_size {
        return None;
    }
    Some(t)
}

pub fn save_to_disk(path: &Path, timeline: &SessionTimeline) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("create cache dir")?;
    }
    let data = bincode::serialize(timeline).context("bincode serialize")?;
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp).context("create tmp")?;
        f.write_all(&data).context("write tmp")?;
        f.sync_all().ok();
    }
    fs::rename(&tmp, path).context("rename tmp")?;
    Ok(())
}

pub struct CacheStore {
    mem: RwLock<HashMap<String, Arc<SessionTimeline>>>,
}

impl Default for CacheStore {
    fn default() -> Self {
        Self {
            mem: RwLock::new(HashMap::new()),
        }
    }
}

impl CacheStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_mem(&self, sid: &str) -> Option<Arc<SessionTimeline>> {
        self.mem.read().ok()?.get(sid).cloned()
    }

    pub fn put(&self, sid: String, timeline: Arc<SessionTimeline>) {
        if let Ok(mut g) = self.mem.write() {
            g.insert(sid, timeline);
        }
    }

    pub fn len(&self) -> usize {
        self.mem.read().map(|g| g.len()).unwrap_or(0)
    }
}

pub fn new_timeline(file_size: u64, events: Vec<TokenEvent>) -> SessionTimeline {
    SessionTimeline {
        version: CACHE_VERSION,
        file_size,
        events,
    }
}
