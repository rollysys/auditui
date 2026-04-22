// Pricing and cost computation. Mirrors server.py PRICING table (2026-04-17 snapshot).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_5m_tokens: u64,
    pub cache_creation_1h_tokens: u64,
    pub cache_creation_tokens: u64, // legacy single-bucket; attributed to 5m when 5m/1h are zero
    pub web_search_calls: u64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_creation_5m_tokens += other.cache_creation_5m_tokens;
        self.cache_creation_1h_tokens += other.cache_creation_1h_tokens;
        self.cache_creation_tokens += other.cache_creation_tokens;
        self.web_search_calls += other.web_search_calls;
    }

    pub fn cache_creation_total(&self) -> u64 {
        self.cache_creation_tokens
            + self.cache_creation_5m_tokens
            + self.cache_creation_1h_tokens
    }
}

struct Price {
    inp: f64,
    out: f64,
    cw5m: f64,
    cw1h: f64,
    cr: f64,
}

const PRICING: &[(&str, Price)] = &[
    // Claude Opus
    ("claude-opus-4-6",   Price { inp: 5.00,  out: 25.00, cw5m: 6.25,  cw1h: 10.00, cr: 0.50 }),
    ("claude-opus-4-5",   Price { inp: 5.00,  out: 25.00, cw5m: 6.25,  cw1h: 10.00, cr: 0.50 }),
    ("claude-opus-4-1",   Price { inp: 15.00, out: 75.00, cw5m: 18.75, cw1h: 30.00, cr: 1.50 }),
    ("claude-opus-4",     Price { inp: 15.00, out: 75.00, cw5m: 18.75, cw1h: 30.00, cr: 1.50 }),
    // Claude Sonnet
    ("claude-sonnet-4-6", Price { inp: 3.00,  out: 15.00, cw5m: 3.75,  cw1h: 6.00,  cr: 0.30 }),
    ("claude-sonnet-4-5", Price { inp: 3.00,  out: 15.00, cw5m: 3.75,  cw1h: 6.00,  cr: 0.30 }),
    ("claude-sonnet-4",   Price { inp: 3.00,  out: 15.00, cw5m: 3.75,  cw1h: 6.00,  cr: 0.30 }),
    // Claude Haiku
    ("claude-haiku-4-5",  Price { inp: 1.00,  out: 5.00,  cw5m: 1.25,  cw1h: 2.00,  cr: 0.10 }),
    ("claude-haiku-3-5",  Price { inp: 0.80,  out: 4.00,  cw5m: 1.00,  cw1h: 1.60,  cr: 0.08 }),
    ("claude-haiku-3",    Price { inp: 0.25,  out: 1.25,  cw5m: 0.30,  cw1h: 0.50,  cr: 0.03 }),
    // OpenAI (Codex)
    ("gpt-5.4",           Price { inp: 5.00,  out: 15.00, cw5m: 0.0, cw1h: 0.0, cr: 1.25 }),
    ("gpt-5.3-codex",     Price { inp: 3.50,  out: 14.00, cw5m: 0.0, cw1h: 0.0, cr: 0.875 }),
    ("gpt-5.3",           Price { inp: 2.00,  out: 8.00,  cw5m: 0.0, cw1h: 0.0, cr: 0.50 }),
    ("gpt-4.1",           Price { inp: 2.00,  out: 8.00,  cw5m: 0.0, cw1h: 0.0, cr: 0.50 }),
    ("gpt-4.1-mini",      Price { inp: 0.40,  out: 1.60,  cw5m: 0.0, cw1h: 0.0, cr: 0.10 }),
    ("gpt-4.1-nano",      Price { inp: 0.10,  out: 0.40,  cw5m: 0.0, cw1h: 0.0, cr: 0.025 }),
    ("o3",                Price { inp: 2.00,  out: 8.00,  cw5m: 0.0, cw1h: 0.0, cr: 0.50 }),
    ("o3-pro",            Price { inp: 20.00, out: 80.00, cw5m: 0.0, cw1h: 0.0, cr: 5.00 }),
    ("o4-mini",           Price { inp: 1.10,  out: 4.40,  cw5m: 0.0, cw1h: 0.0, cr: 0.275 }),
    // Qwen (Alibaba DashScope, CNY→USD @ 7.3)
    ("qwen3.6-plus",      Price { inp: 0.548, out: 2.192, cw5m: 0.0, cw1h: 0.0, cr: 0.137 }),
    ("qwen3-coder",       Price { inp: 0.548, out: 2.192, cw5m: 0.0, cw1h: 0.0, cr: 0.137 }),
    ("qwen-plus",         Price { inp: 0.548, out: 2.192, cw5m: 0.0, cw1h: 0.0, cr: 0.137 }),
    ("qwen-max",          Price { inp: 2.740, out: 10.96, cw5m: 0.0, cw1h: 0.0, cr: 0.685 }),
    // Local / open-weights inference served by Hermes (LM Studio, llama.cpp,
    // vllm, etc.). No metered cost to the user.
    ("gemma",             Price { inp: 0.0,   out: 0.0,   cw5m: 0.0, cw1h: 0.0, cr: 0.0 }),
    ("llama",             Price { inp: 0.0,   out: 0.0,   cw5m: 0.0, cw1h: 0.0, cr: 0.0 }),
    (".gguf",             Price { inp: 0.0,   out: 0.0,   cw5m: 0.0, cw1h: 0.0, cr: 0.0 }),
];

pub const DEFAULT_CTX_WINDOW: u64 = 200_000;

const CTX_WINDOW: &[(&str, u64)] = &[
    ("claude-opus-4-6",   1_000_000),
    ("claude-sonnet-4-6", 1_000_000),
    ("claude-opus-4-5",     200_000),
    ("claude-opus-4-1",     200_000),
    ("claude-opus-4",       200_000),
    ("claude-sonnet-4-5",   200_000),
    ("claude-sonnet-4",     200_000),
    ("claude-haiku-4-5",    200_000),
    ("claude-haiku-3-5",    200_000),
    ("claude-haiku-3",      200_000),
];

fn match_pricing(model: &str) -> Option<&'static Price> {
    if model.is_empty() {
        return None;
    }
    let m = model.to_lowercase();
    if let Some(entry) = PRICING.iter().find(|(k, _)| *k == m) {
        return Some(&entry.1);
    }
    // longest prefix contained in model
    let mut best: Option<(&str, &Price)> = None;
    for (k, p) in PRICING {
        if m.contains(k) {
            match best {
                None => best = Some((k, p)),
                Some((prev, _)) if k.len() > prev.len() => best = Some((k, p)),
                _ => {}
            }
        }
    }
    best.map(|(_, p)| p)
}

pub fn ctx_window(model: &str) -> u64 {
    if model.is_empty() {
        return DEFAULT_CTX_WINDOW;
    }
    let m = model.to_lowercase();
    if let Some(entry) = CTX_WINDOW.iter().find(|(k, _)| *k == m) {
        return entry.1;
    }
    let mut best: Option<(&str, u64)> = None;
    for (k, w) in CTX_WINDOW {
        if m.contains(k) {
            match best {
                None => best = Some((k, *w)),
                Some((prev, _)) if k.len() > prev.len() => best = Some((k, *w)),
                _ => {}
            }
        }
    }
    best.map(|(_, w)| w).unwrap_or(DEFAULT_CTX_WINDOW)
}

pub fn compute_cost(model: &str, u: &Usage) -> f64 {
    let Some(p) = match_pricing(model) else {
        return u.web_search_calls as f64 * 0.01;
    };
    let m = 1_000_000.0;
    let mut cw5m = u.cache_creation_5m_tokens;
    let cw1h = u.cache_creation_1h_tokens;
    if u.cache_creation_tokens > 0 && cw5m == 0 && cw1h == 0 {
        cw5m = u.cache_creation_tokens;
    }
    let cost = u.input_tokens as f64 * p.inp / m
        + u.output_tokens as f64 * p.out / m
        + u.cache_read_tokens as f64 * p.cr / m
        + cw5m as f64 * p.cw5m / m
        + cw1h as f64 * p.cw1h / m
        + u.web_search_calls as f64 * 0.01;
    (cost * 1_000_000.0).round() / 1_000_000.0
}
