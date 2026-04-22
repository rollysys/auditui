use crate::cache::CacheStore;
use crate::dashboard::{self, Range, Stats};
use crate::md;
use crate::memory::{self, MemoryIndex};
use crate::providers::Agent;
use crate::session::{self, SessionGroup, SessionMeta, TranscriptEvent, TranscriptKind};
use crate::skills::{self, Skill};
use std::path::PathBuf;
use anyhow::Result;
use chrono::{DateTime, Local, TimeZone};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use rayon::prelude::*;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::symbols;
use ratatui::widgets::{
    Axis, Bar, BarChart, BarGroup, Block, Borders, Chart, Clear, Dataset, GraphType, List,
    ListItem, ListState, Paragraph, Wrap,
};
use ratatui::Terminal;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

const DEFAULT_AUTO_REFRESH_SECS: u64 = 30;
const MIN_AUTO_REFRESH_SECS: u64 = 5;
const MAX_AUTO_REFRESH_SECS: u64 = 3600;

const PREVIEW_CAP: usize = 32;
const PREVIEW_DEBOUNCE_MS: u64 = 80;
const SEARCH_DEBOUNCE_MS: u64 = 150;
const SEARCH_HIT_CAP: usize = 500;
const SNIPPET_RADIUS: usize = 40;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Search,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct FilterState {
    agent: Option<Agent>, // None = all
    exclude_scripted: bool,
}

impl FilterState {
    fn label_agent(&self) -> &'static str {
        match self.agent {
            None => "all",
            Some(Agent::Claude) => "claude",
            Some(Agent::Codex) => "codex",
            Some(Agent::Hermes) => "hermes",
            Some(Agent::Qwen) => "qwen",
        }
    }
    fn cycle_agent(&mut self) {
        self.agent = match self.agent {
            None => Some(Agent::Claude),
            Some(Agent::Claude) => Some(Agent::Codex),
            Some(Agent::Codex) => Some(Agent::Hermes),
            Some(Agent::Hermes) => Some(Agent::Qwen),
            Some(Agent::Qwen) => None,
        };
    }
    fn matches(&self, m: &SessionMeta) -> bool {
        if let Some(a) = self.agent {
            if m.agent != a {
                return false;
            }
        }
        if self.exclude_scripted && m.is_scripted {
            return false;
        }
        true
    }
}

#[derive(Clone)]
struct Hit {
    sid: String,
    event_index: usize,
    agent: Agent,
    ts: String,
    kind: TranscriptKind,
    snippet: String,
    match_start: usize, // byte offset inside snippet
    match_len: usize,   // byte len of match inside snippet
    session_last_active: u64,
}

struct SearchState {
    query: String,
    dirty_at: Option<Instant>,
    results: Vec<Hit>,
    list_state: ListState,
    last_status: String,
}

impl SearchState {
    fn new() -> Self {
        Self {
            query: String::new(),
            dirty_at: None,
            results: Vec::new(),
            list_state: ListState::default(),
            last_status: "type to search · Esc exit · Enter open · ↑↓ navigate".to_string(),
        }
    }
}

type TranscriptCache = RwLock<HashMap<String, Arc<Vec<TranscriptEvent>>>>;

struct PreviewCache {
    map: HashMap<String, Arc<Vec<TranscriptEvent>>>,
    order: VecDeque<String>,
    cap: usize,
}

impl PreviewCache {
    fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap,
        }
    }
    fn get(&self, sid: &str) -> Option<Arc<Vec<TranscriptEvent>>> {
        self.map.get(sid).cloned()
    }
    fn put(&mut self, sid: String, data: Arc<Vec<TranscriptEvent>>) {
        if self.map.contains_key(&sid) {
            self.map.insert(sid, data);
            return;
        }
        if self.order.len() >= self.cap {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
        self.order.push_back(sid.clone());
        self.map.insert(sid, data);
    }
    fn invalidate(&mut self, sid: &str) {
        self.map.remove(sid);
        self.order.retain(|s| s != sid);
    }
}

/// Drop-guard that restores the terminal to its pre-auditui state even if
/// the inner app panics. Without this, a panic below `enable_raw_mode`
/// leaves the user's terminal stuck in raw + alternate-screen mode
/// (blind-typed `reset` is the only recovery).
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

pub fn run(refresh_secs: u64, update_state: crate::update::UpdateState) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    // From here until `_guard` drops, terminal state is always cleaned up.
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    term.draw(|f| draw_splash(f, "Indexing sessions…", "Scanning ~/.claude, ~/.codex, ~/.qwen"))?;

    let mut app = App::new(refresh_secs, update_state);
    let res = app.run(&mut term);

    term.show_cursor().ok();
    res
}

fn draw_splash(f: &mut ratatui::Frame<'_>, title: &str, sub: &str) {
    let area = f.area();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" auditit ")
        .style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            title,
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(sub, Style::default().fg(Color::DarkGray))),
        Line::from(""),
        Line::from(Span::styled(
            "please wait…",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        )),
    ];
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(inner.height.saturating_sub(6) / 2),
            Constraint::Min(6),
        ])
        .split(inner);
    let p = Paragraph::new(lines).alignment(ratatui::layout::Alignment::Center);
    f.render_widget(p, v[1]);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Sessions,
    Dashboard,
    Memory,
    Skills,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DashboardUnit {
    Dollars,
    Calls,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DashboardMode {
    Overview,
    Sessions,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FoldMode {
    /// Default: long tool_result / system / thinking are collapsed to a
    /// single placeholder line; user / assistant / tool_use headers are
    /// always shown.
    Smart,
    /// Nothing is folded regardless of length or kind.
    Expanded,
}

#[derive(Clone)]
struct MemoryRow {
    label: String,
    agent: String,
    path: PathBuf,
}

enum Focus {
    List,
    Detail,
}

struct App {
    sessions: Vec<SessionMeta>,
    list_state: ListState,
    preview: PreviewCache,
    transcript_for: Option<String>, // sid currently shown on the right
    pending_preview: Option<(String, Instant)>, // (sid, fire_at)
    scroll: u16,
    focus: Focus,
    status: String,
    last_refresh: Instant,
    view: View,
    range: Range,
    stats: Option<Stats>,
    stats_for: Option<(String, usize)>, // (range, n_sessions)
    dashboard_scroll: u16,
    dashboard_unit: DashboardUnit,
    dashboard_mode: DashboardMode,
    cache: Arc<CacheStore>,
    mode: Mode,
    search: SearchState,
    search_cache: Arc<TranscriptCache>,
    pending_jump: Option<(String, usize)>, // (sid, event_index) apply at next draw_detail
    filter: FilterState,
    refresh_rx: mpsc::Receiver<Vec<SessionMeta>>,
    memory_index: Option<MemoryIndex>,
    memory_rows: Vec<MemoryRow>,
    memory_list_state: ListState,
    memory_content: Option<String>,
    memory_content_for: Option<PathBuf>,
    memory_scroll: u16,
    skills_list: Vec<Skill>,
    skills_list_state: ListState,
    skills_content: Option<String>,
    skills_content_for: Option<PathBuf>,
    skills_scroll: u16,
    groups_cache: Option<Vec<SessionGroup>>,
    expanded_groups: HashSet<String>,
    refresh_secs: Arc<AtomicU64>,
    update_state: crate::update::UpdateState,
    /// Full rendered line count from the last draw_detail pass. Written by
    /// draw_detail, read by move_scroll to clamp scroll to a correct upper
    /// bound. Zero before the first render.
    detail_row_count: u16,
    /// Global fold state for the transcript detail pane.
    fold_mode: FoldMode,
    /// Per-session "explicitly-expanded" event indices. Persists across
    /// session switches so flipping back to a session preserves your
    /// per-event choices. Only consulted in FoldMode::Smart.
    fold_overrides: HashMap<String, HashSet<usize>>,
    /// Event-start row offsets from the last draw_detail pass, matched to
    /// the current transcript. Written by draw_detail, read by handle_key
    /// when the user presses `x` to toggle fold of the event under scroll.
    detail_event_offsets: Vec<u16>,
}

#[derive(Clone)]
enum ListRow {
    GroupHeader { group_idx: usize },
    Child { group_idx: usize, sid: String },
    Solo { group_idx: usize, sid: String },
}

impl ListRow {
    fn sid(&self) -> Option<&str> {
        match self {
            ListRow::GroupHeader { .. } => None,
            ListRow::Child { sid, .. } | ListRow::Solo { sid, .. } => Some(sid.as_str()),
        }
    }
    fn group_idx(&self) -> usize {
        match self {
            ListRow::GroupHeader { group_idx }
            | ListRow::Child { group_idx, .. }
            | ListRow::Solo { group_idx, .. } => *group_idx,
        }
    }
}

impl App {
    fn new(initial_refresh_secs: u64, update_state: crate::update::UpdateState) -> Self {
        let t0 = Instant::now();
        let sessions = session::index_all();
        let elapsed = t0.elapsed();
        let mut st = ListState::default();
        if !sessions.is_empty() {
            st.select(Some(0));
        }
        let initial = if initial_refresh_secs == 0 {
            0
        } else {
            initial_refresh_secs.clamp(MIN_AUTO_REFRESH_SECS, MAX_AUTO_REFRESH_SECS)
        };
        let refresh_secs = Arc::new(AtomicU64::new(initial));
        let status = format!(
            "indexed {} sessions in {:.2}s · auto-refresh {} · warming cache...",
            sessions.len(),
            elapsed.as_secs_f32(),
            format_refresh(initial),
        );
        let cache = Arc::new(CacheStore::new());
        let (tx, rx) = mpsc::channel();
        let rs_clone = refresh_secs.clone();
        thread::spawn(move || {
            let mut last_fire = Instant::now();
            loop {
                thread::sleep(Duration::from_secs(1));
                let target = rs_clone.load(Ordering::Relaxed);
                if target == 0 {
                    last_fire = Instant::now();
                    continue;
                }
                if last_fire.elapsed().as_secs() >= target {
                    let list = session::index_all();
                    if tx.send(list).is_err() {
                        break;
                    }
                    last_fire = Instant::now();
                }
            }
        });
        let mut app = Self {
            sessions,
            list_state: st,
            preview: PreviewCache::new(PREVIEW_CAP),
            transcript_for: None,
            pending_preview: None,
            scroll: 0,
            focus: Focus::List,
            status,
            last_refresh: Instant::now(),
            view: View::Sessions,
            range: Range::D1,
            stats: None,
            stats_for: None,
            dashboard_scroll: 0,
            dashboard_unit: DashboardUnit::Dollars,
            dashboard_mode: DashboardMode::Overview,
            cache,
            mode: Mode::Normal,
            search: SearchState::new(),
            search_cache: Arc::new(RwLock::new(HashMap::new())),
            pending_jump: None,
            filter: FilterState::default(),
            refresh_rx: rx,
            memory_index: None,
            memory_rows: Vec::new(),
            memory_list_state: ListState::default(),
            memory_content: None,
            memory_content_for: None,
            memory_scroll: 0,
            skills_list: Vec::new(),
            skills_list_state: ListState::default(),
            skills_content: None,
            skills_content_for: None,
            skills_scroll: 0,
            groups_cache: None,
            expanded_groups: HashSet::new(),
            refresh_secs,
            update_state,
            detail_row_count: 0,
            fold_mode: FoldMode::Smart,
            fold_overrides: HashMap::new(),
            detail_event_offsets: Vec::new(),
        };
        app.spawn_warm();
        app.request_preview_for_selected();
        app
    }

    fn tick_auto_refresh(&mut self) {
        let mut latest: Option<Vec<SessionMeta>> = None;
        while let Ok(new) = self.refresh_rx.try_recv() {
            latest = Some(new);
        }
        if let Some(new_sessions) = latest {
            self.apply_refreshed_sessions(new_sessions);
        }
    }

    fn apply_refreshed_sessions(&mut self, new_sessions: Vec<SessionMeta>) {
        let prev_sid = self.selected_sid();
        self.sessions = new_sessions;
        self.invalidate_groups();
        self.last_refresh = Instant::now();
        self.stats_for = None;
        self.spawn_warm();

        let rows = self.list_rows();
        if rows.is_empty() {
            self.list_state.select(None);
            self.transcript_for = None;
            self.pending_preview = None;
            return;
        }
        let new_idx = prev_sid
            .as_deref()
            .and_then(|sid| self.locate_row_by_sid(sid))
            .unwrap_or(0);
        self.list_state.select(Some(new_idx));
        if self.view == View::Dashboard {
            self.load_stats_if_needed();
        }
        // Invalidate + reload the transcript currently displayed in the detail
        // pane: auto-refresh only replaced session meta, so without this step
        // an actively-growing .jsonl keeps showing its stale preview from
        // before the refresh. load_preview goes through the size-keyed
        // timeline cache so unchanged sessions are cheap (hash lookup) and
        // grown ones re-parse exactly once. Scroll position is deliberately
        // preserved (auto-refresh should not yank the user's reading position).
        if let Some(sid) = self.transcript_for.clone() {
            self.preview.invalidate(&sid);
            self.load_preview(&sid);
        }
    }

    fn spawn_warm(&self) {
        let cache = self.cache.clone();
        let sessions = self.sessions.clone();
        thread::spawn(move || {
            sessions.par_iter().for_each(|m| {
                let _ = session::get_timeline(m, &cache);
            });
        });
    }

    fn run<B: ratatui::backend::Backend>(&mut self, term: &mut Terminal<B>) -> Result<()> {
        loop {
            term.draw(|f| self.draw(f))?;
            if event::poll(Duration::from_millis(40))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    // Ctrl-C always quits.
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        return Ok(());
                    }
                    if matches!(self.mode, Mode::Search) {
                        self.handle_key_search(key.code);
                    } else {
                        if matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q')) {
                            return Ok(());
                        }
                        self.handle_key(key.code);
                    }
                }
            }
            self.tick_preview();
            self.tick_search();
            self.tick_auto_refresh();
        }
    }

    fn tick_preview(&mut self) {
        let Some((sid, fire_at)) = self.pending_preview.as_ref() else {
            return;
        };
        if Instant::now() < *fire_at {
            return;
        }
        let sid = sid.clone();
        self.pending_preview = None;
        self.load_preview(&sid);
    }

    fn request_preview_for_selected(&mut self) {
        let Some(sid) = self.selected_sid() else {
            return;
        };
        if self.preview.get(&sid).is_some() {
            self.transcript_for = Some(sid);
            self.scroll = 0;
            self.pending_preview = None;
            return;
        }
        self.transcript_for = Some(sid.clone());
        self.scroll = 0;
        self.pending_preview = Some((sid, Instant::now() + Duration::from_millis(PREVIEW_DEBOUNCE_MS)));
    }

    fn load_preview(&mut self, sid: &str) {
        let Some(meta) = self.sessions.iter().find(|m| m.id == sid).cloned() else {
            return;
        };
        match session::read_transcript(&meta) {
            Ok(events) => {
                let arc = Arc::new(events);
                self.preview.put(sid.to_string(), arc.clone());
                self.status = format!("{} events · {}", arc.len(), sid);
            }
            Err(e) => {
                self.status = format!("preview failed: {e}");
            }
        }
    }

    fn handle_key(&mut self, code: KeyCode) {
        // View-independent shortcuts
        match code {
            KeyCode::Char('s') | KeyCode::Char('S') => {
                self.view = View::Sessions;
                return;
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                if self.view != View::Dashboard {
                    self.view = View::Dashboard;
                    self.load_stats_if_needed();
                }
                return;
            }
            KeyCode::Char('m') | KeyCode::Char('M') => {
                if self.view != View::Memory {
                    self.view = View::Memory;
                    self.load_memory_if_needed();
                }
                return;
            }
            KeyCode::Char('k') | KeyCode::Char('K') => {
                if self.view != View::Skills {
                    self.view = View::Skills;
                    self.load_skills_if_needed();
                }
                return;
            }
            KeyCode::Char('f') | KeyCode::Char('F') => {
                self.filter.cycle_agent();
                self.on_filter_changed();
                return;
            }
            KeyCode::Char('p') | KeyCode::Char('P') => {
                self.filter.exclude_scripted = !self.filter.exclude_scripted;
                self.on_filter_changed();
                return;
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                let cur = self.refresh_secs.load(Ordering::Relaxed);
                let next = if cur == 0 {
                    MIN_AUTO_REFRESH_SECS
                } else {
                    (cur * 2).min(MAX_AUTO_REFRESH_SECS)
                };
                self.refresh_secs.store(next, Ordering::Relaxed);
                self.status = format!("auto-refresh: {}", format_refresh(next));
                return;
            }
            KeyCode::Char('-') | KeyCode::Char('_') => {
                let cur = self.refresh_secs.load(Ordering::Relaxed);
                let next = if cur == 0 {
                    0
                } else if cur <= MIN_AUTO_REFRESH_SECS {
                    0
                } else {
                    (cur / 2).max(MIN_AUTO_REFRESH_SECS)
                };
                self.refresh_secs.store(next, Ordering::Relaxed);
                self.status = format!("auto-refresh: {}", format_refresh(next));
                return;
            }
            KeyCode::Char('0') => {
                self.refresh_secs.store(0, Ordering::Relaxed);
                self.status = format!("auto-refresh: {}", format_refresh(0));
                return;
            }
            KeyCode::Char('r') => {
                let t0 = Instant::now();
                self.sessions = session::index_all();
                self.invalidate_groups();
                self.status = format!(
                    "refreshed: {} sessions in {:.2}s",
                    self.sessions.len(),
                    t0.elapsed().as_secs_f32()
                );
                self.last_refresh = Instant::now();
                let n_rows = self.list_rows().len();
                if n_rows == 0 {
                    self.list_state.select(None);
                } else {
                    let cur = self.list_state.selected().unwrap_or(0);
                    self.list_state.select(Some(cur.min(n_rows - 1)));
                }
                self.preview = PreviewCache::new(PREVIEW_CAP);
                self.transcript_for = None;
                self.pending_preview = None;
                self.stats = None;
                self.stats_for = None;
                self.memory_index = None;
                self.skills_list.clear();
                self.spawn_warm();
                self.request_preview_for_selected();
                if self.view == View::Dashboard {
                    self.load_stats_if_needed();
                }
                return;
            }
            _ => {}
        }

        match self.view {
            View::Sessions => self.handle_key_sessions(code),
            View::Dashboard => self.handle_key_dashboard(code),
            View::Memory => self.handle_key_memory(code),
            View::Skills => self.handle_key_skills(code),
        }
    }

    fn handle_key_sessions(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('/') => {
                self.enter_search_mode();
            }
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::List => Focus::Detail,
                    Focus::Detail => Focus::List,
                };
            }
            KeyCode::Char('[') => {
                self.jump_group_rel(-1);
                if matches!(self.focus, Focus::Detail) {
                    if let Some((sid, _)) = self.pending_preview.take() {
                        self.load_preview(&sid);
                    }
                }
            }
            KeyCode::Char(']') => {
                self.jump_group_rel(1);
                if matches!(self.focus, Focus::Detail) {
                    if let Some((sid, _)) = self.pending_preview.take() {
                        self.load_preview(&sid);
                    }
                }
            }
            KeyCode::Char(' ') => {
                if matches!(self.focus, Focus::List) {
                    self.toggle_expand_at_selection();
                }
            }
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown | KeyCode::Home
            | KeyCode::End => match self.focus {
                Focus::List => self.move_list(code),
                Focus::Detail => self.move_scroll(code),
            },
            KeyCode::Char('z') | KeyCode::Char('Z') => {
                self.fold_mode = match self.fold_mode {
                    FoldMode::Smart => FoldMode::Expanded,
                    FoldMode::Expanded => FoldMode::Smart,
                };
            }
            KeyCode::Char('x') | KeyCode::Char('X') => {
                self.toggle_fold_at_scroll();
            }
            KeyCode::Enter => {
                if matches!(self.focus, Focus::List) {
                    let is_header = matches!(
                        self.list_state.selected().and_then(|i| self.list_rows().get(i).cloned()),
                        Some(ListRow::GroupHeader { .. })
                    );
                    if is_header {
                        self.toggle_expand_at_selection();
                        return;
                    }
                }
                self.focus = Focus::Detail;
                // Enter forces immediate load if still pending
                if let Some((sid, _)) = self.pending_preview.take() {
                    self.load_preview(&sid);
                }
            }
            KeyCode::Esc => {
                self.focus = Focus::List;
            }
            _ => {}
        }
    }

    fn jump_group_rel(&mut self, delta: i32) {
        let Some(cur_sid) = self.selected_sid() else { return; };
        let Some(g_idx) = self.selected_group_idx() else { return; };
        let g = {
            let groups = self.groups();
            groups.get(g_idx).cloned()
        };
        let Some(g) = g else { return; };
        let Some(pos) = g.members.iter().position(|m| m.id == cur_sid) else { return; };
        let new_pos = (pos as i32 + delta).clamp(0, g.len() as i32 - 1) as usize;
        if new_pos == pos { return; }
        let target_sid = g.members[new_pos].id.clone();
        if let Some(row_idx) = self.locate_row_by_sid(&target_sid) {
            self.list_state.select(Some(row_idx));
            self.request_preview_for_selected();
        }
    }

    fn handle_key_memory(&mut self, code: KeyCode) {
        match code {
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::List => Focus::Detail,
                    Focus::Detail => Focus::List,
                };
            }
            KeyCode::Esc => {
                self.focus = Focus::List;
            }
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
            | KeyCode::Home | KeyCode::End => match self.focus {
                Focus::List => self.move_memory_list(code),
                Focus::Detail => self.move_memory_scroll(code),
            },
            KeyCode::Enter => {
                self.focus = Focus::Detail;
                self.load_memory_content();
            }
            _ => {}
        }
    }

    fn handle_key_skills(&mut self, code: KeyCode) {
        match code {
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::List => Focus::Detail,
                    Focus::Detail => Focus::List,
                };
            }
            KeyCode::Esc => {
                self.focus = Focus::List;
            }
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
            | KeyCode::Home | KeyCode::End => match self.focus {
                Focus::List => self.move_skills_list(code),
                Focus::Detail => self.move_skills_scroll(code),
            },
            KeyCode::Enter => {
                self.focus = Focus::Detail;
                self.load_skill_content();
            }
            _ => {}
        }
    }

    fn load_memory_if_needed(&mut self) {
        if self.memory_index.is_some() {
            return;
        }
        let idx = memory::build();
        let mut rows: Vec<MemoryRow> = Vec::new();
        if let Some(g) = &idx.global {
            rows.push(MemoryRow {
                label: "~/.claude/CLAUDE.md (global)".to_string(),
                agent: "claude".to_string(),
                path: g.path.clone(),
            });
        }
        for proj in &idx.projects {
            for f in &proj.files {
                let label = format!("[{}] {} · {}", proj.agent, proj.name, f.category);
                rows.push(MemoryRow {
                    label,
                    agent: proj.agent.clone(),
                    path: f.path.clone(),
                });
            }
        }
        if !rows.is_empty() {
            self.memory_list_state.select(Some(0));
        }
        self.status = format!("memory: {} files", rows.len());
        self.memory_rows = rows;
        self.memory_index = Some(idx);
        self.load_memory_content();
    }

    fn load_memory_content(&mut self) {
        let Some(i) = self.memory_list_state.selected() else {
            self.memory_content = None;
            self.memory_content_for = None;
            return;
        };
        let Some(row) = self.memory_rows.get(i).cloned() else {
            return;
        };
        if self.memory_content_for.as_ref() == Some(&row.path) {
            return;
        }
        match memory::read_file(&row.path) {
            Ok(s) => {
                self.memory_content = Some(s);
                self.memory_content_for = Some(row.path);
                self.memory_scroll = 0;
            }
            Err(e) => {
                self.memory_content = Some(format!("(read error: {e})"));
                self.memory_content_for = Some(row.path);
                self.memory_scroll = 0;
            }
        }
    }

    fn move_memory_list(&mut self, code: KeyCode) {
        let n = self.memory_rows.len();
        if n == 0 {
            return;
        }
        let cur = self.memory_list_state.selected().unwrap_or(0);
        let next = match code {
            KeyCode::Up => cur.saturating_sub(1),
            KeyCode::Down => (cur + 1).min(n - 1),
            KeyCode::PageUp => cur.saturating_sub(10),
            KeyCode::PageDown => (cur + 10).min(n - 1),
            KeyCode::Home => 0,
            KeyCode::End => n - 1,
            _ => cur,
        };
        if next != cur {
            self.memory_list_state.select(Some(next));
            self.load_memory_content();
        }
    }

    fn move_memory_scroll(&mut self, code: KeyCode) {
        let max = self
            .memory_content
            .as_ref()
            .map(|s| s.lines().count() as u16)
            .unwrap_or(0);
        let cur = self.memory_scroll;
        self.memory_scroll = match code {
            KeyCode::Up => cur.saturating_sub(1),
            KeyCode::Down => (cur + 1).min(max),
            KeyCode::PageUp => cur.saturating_sub(15),
            KeyCode::PageDown => (cur + 15).min(max),
            KeyCode::Home => 0,
            KeyCode::End => max,
            _ => cur,
        };
    }

    fn load_skills_if_needed(&mut self) {
        if !self.skills_list.is_empty() {
            return;
        }
        let list = skills::list();
        if !list.is_empty() {
            self.skills_list_state.select(Some(0));
        }
        self.status = format!("skills: {} entries", list.len());
        self.skills_list = list;
        self.load_skill_content();
    }

    fn load_skill_content(&mut self) {
        let Some(i) = self.skills_list_state.selected() else {
            self.skills_content = None;
            self.skills_content_for = None;
            return;
        };
        let Some(sk) = self.skills_list.get(i).cloned() else {
            return;
        };
        let md = {
            let a = sk.path.join("SKILL.md");
            if a.is_file() { a } else { sk.path.join("AGENTS.md") }
        };
        if self.skills_content_for.as_ref() == Some(&md) {
            return;
        }
        match skills::read_file(&md) {
            Ok(s) => {
                self.skills_content = Some(s);
                self.skills_content_for = Some(md);
                self.skills_scroll = 0;
            }
            Err(_) => {
                let mut body = format!("path: {}\n", sk.path.display());
                if !sk.description.is_empty() {
                    body.push_str(&format!("description: {}\n", sk.description));
                }
                body.push_str(&format!("\nfiles ({}):\n", sk.files.len()));
                for f in &sk.files {
                    body.push_str(&format!("  {}  ({} bytes)\n", f.rel_path, f.size));
                }
                self.skills_content = Some(body);
                self.skills_content_for = Some(md);
                self.skills_scroll = 0;
            }
        }
    }

    fn move_skills_list(&mut self, code: KeyCode) {
        let n = self.skills_list.len();
        if n == 0 {
            return;
        }
        let cur = self.skills_list_state.selected().unwrap_or(0);
        let next = match code {
            KeyCode::Up => cur.saturating_sub(1),
            KeyCode::Down => (cur + 1).min(n - 1),
            KeyCode::PageUp => cur.saturating_sub(10),
            KeyCode::PageDown => (cur + 10).min(n - 1),
            KeyCode::Home => 0,
            KeyCode::End => n - 1,
            _ => cur,
        };
        if next != cur {
            self.skills_list_state.select(Some(next));
            self.load_skill_content();
        }
    }

    fn move_skills_scroll(&mut self, code: KeyCode) {
        let max = self
            .skills_content
            .as_ref()
            .map(|s| s.lines().count() as u16)
            .unwrap_or(0);
        let cur = self.skills_scroll;
        self.skills_scroll = match code {
            KeyCode::Up => cur.saturating_sub(1),
            KeyCode::Down => (cur + 1).min(max),
            KeyCode::PageUp => cur.saturating_sub(15),
            KeyCode::PageDown => (cur + 15).min(max),
            KeyCode::Home => 0,
            KeyCode::End => max,
            _ => cur,
        };
    }

    fn handle_key_dashboard(&mut self, code: KeyCode) {
        match code {
            KeyCode::Left => {
                self.range = prev_range(self.range);
                self.stats = None;
                self.stats_for = None;
                self.dashboard_scroll = 0;
                self.load_stats_if_needed();
            }
            KeyCode::Right => {
                self.range = next_range(self.range);
                self.stats = None;
                self.stats_for = None;
                self.dashboard_scroll = 0;
                self.load_stats_if_needed();
            }
            KeyCode::Char('u') | KeyCode::Char('U') => {
                self.dashboard_unit = match self.dashboard_unit {
                    DashboardUnit::Dollars => DashboardUnit::Calls,
                    DashboardUnit::Calls => DashboardUnit::Dollars,
                };
            }
            KeyCode::Char('v') | KeyCode::Char('V') => {
                self.dashboard_mode = match self.dashboard_mode {
                    DashboardMode::Overview => DashboardMode::Sessions,
                    DashboardMode::Sessions => DashboardMode::Overview,
                };
                self.dashboard_scroll = 0;
            }
            KeyCode::Up => self.dashboard_scroll = self.dashboard_scroll.saturating_sub(1),
            KeyCode::Down => self.dashboard_scroll = self.dashboard_scroll.saturating_add(1),
            KeyCode::PageUp => self.dashboard_scroll = self.dashboard_scroll.saturating_sub(10),
            KeyCode::PageDown => self.dashboard_scroll = self.dashboard_scroll.saturating_add(10),
            KeyCode::Home => self.dashboard_scroll = 0,
            _ => {}
        }
    }

    fn move_list(&mut self, code: KeyCode) {
        let n = self.list_rows().len();
        if n == 0 {
            return;
        }
        let cur = self.list_state.selected().unwrap_or(0);
        let next = match code {
            KeyCode::Up => cur.saturating_sub(1),
            KeyCode::Down => (cur + 1).min(n - 1),
            KeyCode::PageUp => cur.saturating_sub(10),
            KeyCode::PageDown => (cur + 10).min(n - 1),
            KeyCode::Home => 0,
            KeyCode::End => n - 1,
            _ => cur,
        };
        if next != cur {
            self.list_state.select(Some(next));
            self.request_preview_for_selected();
        }
    }

    /// Toggle the fold override for the event containing the current scroll
    /// top. No-op if no transcript is loaded or the scroll isn't inside any
    /// event. Works in both Smart and Expanded modes; in Expanded mode the
    /// override has no visible effect until the user returns to Smart.
    fn toggle_fold_at_scroll(&mut self) {
        let Some(sid) = self.transcript_for.clone() else { return };
        if self.detail_event_offsets.is_empty() {
            return;
        }
        let scroll = self.scroll;
        let ev_idx = self
            .detail_event_offsets
            .iter()
            .rposition(|&o| o <= scroll)
            .unwrap_or(0);
        let set = self.fold_overrides.entry(sid).or_default();
        if !set.insert(ev_idx) {
            set.remove(&ev_idx);
        }
    }

    fn move_scroll(&mut self, code: KeyCode) {
        // detail_row_count is the full rendered line count from the last
        // draw_detail pass. Using events.len() * 3 as a proxy (the old code)
        // cuts scroll off at ~12 for a 4-event session even when the body
        // renders to hundreds of lines.
        let max = self.detail_row_count.saturating_sub(1);
        let cur = self.scroll;
        let next = match code {
            KeyCode::Up => cur.saturating_sub(1),
            KeyCode::Down => (cur + 1).min(max),
            KeyCode::PageUp => cur.saturating_sub(15),
            KeyCode::PageDown => (cur + 15).min(max),
            KeyCode::Home => 0,
            KeyCode::End => max,
            _ => cur,
        };
        self.scroll = next;
    }

    fn current_transcript(&self) -> Option<Arc<Vec<TranscriptEvent>>> {
        self.transcript_for.as_ref().and_then(|sid| self.preview.get(sid))
    }

    fn filtered_sessions(&self) -> Vec<SessionMeta> {
        self.sessions
            .iter()
            .filter(|s| self.filter.matches(s))
            .cloned()
            .collect()
    }

    fn groups(&mut self) -> &[SessionGroup] {
        if self.groups_cache.is_none() {
            let filtered = self.filtered_sessions();
            let g = session::group_sessions(&filtered, session::DEFAULT_GROUP_GAP_SECS);
            self.groups_cache = Some(g);
        }
        self.groups_cache.as_deref().unwrap()
    }

    fn invalidate_groups(&mut self) {
        self.groups_cache = None;
    }

    fn list_rows(&mut self) -> Vec<ListRow> {
        let expanded = self.expanded_groups.clone();
        let groups = self.groups();
        let mut rows = Vec::with_capacity(groups.len());
        for (gi, g) in groups.iter().enumerate() {
            if g.len() == 1 {
                let sid = g.members[0].id.clone();
                rows.push(ListRow::Solo { group_idx: gi, sid });
            } else {
                rows.push(ListRow::GroupHeader { group_idx: gi });
                if expanded.contains(&g.key) {
                    for m in &g.members {
                        rows.push(ListRow::Child {
                            group_idx: gi,
                            sid: m.id.clone(),
                        });
                    }
                }
            }
        }
        rows
    }

    fn selected_sid(&mut self) -> Option<String> {
        let idx = self.list_state.selected()?;
        let rows = self.list_rows();
        rows.get(idx).and_then(|r| r.sid().map(|s| s.to_string()))
    }

    fn selected_group_idx(&mut self) -> Option<usize> {
        let idx = self.list_state.selected()?;
        let rows = self.list_rows();
        rows.get(idx).map(|r| r.group_idx())
    }

    fn expand_group_containing(&mut self, sid: &str) {
        let key = {
            let groups = self.groups();
            groups
                .iter()
                .find(|g| g.len() > 1 && g.members.iter().any(|m| m.id == sid))
                .map(|g| g.key.clone())
        };
        if let Some(k) = key {
            self.expanded_groups.insert(k);
        }
    }

    fn locate_row_by_sid(&mut self, sid: &str) -> Option<usize> {
        self.expand_group_containing(sid);
        let rows = self.list_rows();
        rows.iter().position(|r| r.sid() == Some(sid))
    }

    fn toggle_expand_at_selection(&mut self) {
        let Some(idx) = self.list_state.selected() else { return; };
        let rows = self.list_rows();
        let Some(row) = rows.get(idx).cloned() else { return; };
        if let ListRow::GroupHeader { group_idx } = row {
            let key = {
                let groups = self.groups();
                groups.get(group_idx).map(|g| g.key.clone())
            };
            if let Some(key) = key {
                if !self.expanded_groups.remove(&key) {
                    self.expanded_groups.insert(key);
                }
            }
        }
    }

    fn on_filter_changed(&mut self) {
        self.invalidate_groups();
        let n = self.list_rows().len();
        if n == 0 {
            self.list_state.select(None);
            self.transcript_for = None;
            self.pending_preview = None;
        } else {
            self.list_state.select(Some(0));
            self.request_preview_for_selected();
        }
        self.stats = None;
        self.stats_for = None;
        if self.view == View::Dashboard {
            self.load_stats_if_needed();
        }
    }

    fn enter_search_mode(&mut self) {
        self.view = View::Sessions;
        self.mode = Mode::Search;
        self.search = SearchState::new();
    }

    fn exit_search_mode(&mut self) {
        self.mode = Mode::Normal;
    }

    fn handle_key_search(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.exit_search_mode(),
            KeyCode::Enter => {
                if let Some(idx) = self.search.list_state.selected() {
                    self.jump_to_hit(idx);
                }
                self.exit_search_mode();
            }
            KeyCode::Backspace => {
                self.search.query.pop();
                self.search.dirty_at =
                    Some(Instant::now() + Duration::from_millis(SEARCH_DEBOUNCE_MS));
            }
            KeyCode::Up => self.move_search_selection(-1),
            KeyCode::Down => self.move_search_selection(1),
            KeyCode::PageUp => self.move_search_selection(-10),
            KeyCode::PageDown => self.move_search_selection(10),
            KeyCode::Char(c) => {
                self.search.query.push(c);
                self.search.dirty_at =
                    Some(Instant::now() + Duration::from_millis(SEARCH_DEBOUNCE_MS));
            }
            _ => {}
        }
    }

    fn move_search_selection(&mut self, delta: i32) {
        let n = self.search.results.len();
        if n == 0 {
            return;
        }
        let cur = self.search.list_state.selected().unwrap_or(0) as i32;
        let next = (cur + delta).clamp(0, n as i32 - 1) as usize;
        if next != cur as usize {
            self.search.list_state.select(Some(next));
            self.preview_search_selection();
        }
    }

    fn preview_search_selection(&mut self) {
        let Some(idx) = self.search.list_state.selected() else {
            return;
        };
        let Some(hit) = self.search.results.get(idx).cloned() else {
            return;
        };
        if self.preview.get(&hit.sid).is_none() {
            if let Ok(cache) = self.search_cache.read() {
                if let Some(arc) = cache.get(&hit.sid).cloned() {
                    self.preview.put(hit.sid.clone(), arc);
                }
            }
        }
        self.transcript_for = Some(hit.sid.clone());
        self.pending_preview = None;
        self.pending_jump = Some((hit.sid, hit.event_index));
    }

    fn jump_to_hit(&mut self, idx: usize) {
        let Some(hit) = self.search.results.get(idx).cloned() else {
            return;
        };
        if self.preview.get(&hit.sid).is_none() {
            if let Ok(cache) = self.search_cache.read() {
                if let Some(arc) = cache.get(&hit.sid).cloned() {
                    self.preview.put(hit.sid.clone(), arc);
                }
            }
        }
        self.transcript_for = Some(hit.sid.clone());
        self.pending_preview = None;
        self.pending_jump = Some((hit.sid.clone(), hit.event_index));
        self.focus = Focus::Detail;
        // Sync outer session list selection to the hit's session so refreshing
        // the view shows the right row highlighted.
        if let Some(pos) = self.sessions.iter().position(|s| s.id == hit.sid) {
            self.list_state.select(Some(pos));
        }
    }

    fn tick_search(&mut self) {
        let Some(fire_at) = self.search.dirty_at else {
            return;
        };
        if Instant::now() < fire_at {
            return;
        }
        self.search.dirty_at = None;
        let q = self.search.query.clone();
        if q.is_empty() {
            self.search.results.clear();
            self.search.list_state.select(None);
            self.search.last_status = "type to search".to_string();
            return;
        }
        // Use regex with the case-insensitive flag to search the original
        // body bytes. The earlier `body.to_lowercase()` + `find(&ql)` path
        // returned byte offsets in the LOWERCASED string, which diverge
        // from the original string when `to_lowercase()` changes length
        // (ß → ss, İ → i̇, etc.). Those offsets were then fed into
        // make_snippet against the original body, producing wrong high-
        // lights and a latent char-boundary panic risk.
        let re = match regex::RegexBuilder::new(&regex::escape(&q))
            .case_insensitive(true)
            .build()
        {
            Ok(r) => r,
            Err(_) => {
                self.search.last_status = "invalid query".to_string();
                return;
            }
        };
        let sessions = self.sessions.clone();
        let cache = self.search_cache.clone();
        let t0 = Instant::now();
        use rayon::prelude::*;
        let mut hits: Vec<Hit> = sessions
            .par_iter()
            .flat_map_iter(|s| {
                let events = match get_or_read_transcript(&s.id, s, &cache) {
                    Some(a) => a,
                    None => return Vec::<Hit>::new().into_iter(),
                };
                let mut out: Vec<Hit> = Vec::new();
                for (i, ev) in events.iter().enumerate() {
                    if let Some(m) = re.find(&ev.body) {
                        let snip = make_snippet(&ev.body, m.start(), m.end() - m.start());
                        out.push(Hit {
                            sid: s.id.clone(),
                            event_index: i,
                            agent: s.agent,
                            ts: ev.ts.clone(),
                            kind: ev.kind,
                            snippet: snip.text,
                            match_start: snip.match_start,
                            match_len: snip.match_len,
                            session_last_active: s.last_active_ts,
                        });
                    }
                }
                out.into_iter()
            })
            .collect();
        // Order: newest session first, then by event_index asc.
        hits.sort_by(|a, b| {
            b.session_last_active
                .cmp(&a.session_last_active)
                .then(a.sid.cmp(&b.sid))
                .then(a.event_index.cmp(&b.event_index))
        });
        let total = hits.len();
        if hits.len() > SEARCH_HIT_CAP {
            hits.truncate(SEARCH_HIT_CAP);
        }
        self.search.last_status = format!(
            "{}{} hits in {:.2}s",
            if total > SEARCH_HIT_CAP { "≥" } else { "" },
            total,
            t0.elapsed().as_secs_f32()
        );
        self.search.results = hits;
        if self.search.results.is_empty() {
            self.search.list_state.select(None);
            self.transcript_for = None;
        } else {
            self.search.list_state.select(Some(0));
            self.preview_search_selection();
        }
    }

    fn load_stats_if_needed(&mut self) {
        let filtered = self.filtered_sessions();
        let filter_tag = format!(
            "{}|{}",
            self.filter.label_agent(),
            if self.filter.exclude_scripted { "noscr" } else { "all" }
        );
        let key = (
            format!("{}·{}", self.range.label(), filter_tag),
            filtered.len(),
        );
        if self.stats_for.as_ref() == Some(&key) {
            return;
        }
        let s = dashboard::compute(&filtered, &self.cache, self.range);
        let rs = self.refresh_secs.load(Ordering::Relaxed);
        self.status = format!(
            "stats [{}]: {} sessions · ${:.2} · {} turns · compute {}ms per {} · cache={}",
            s.range,
            s.total_sessions,
            s.total_cost,
            s.total_turns,
            s.elapsed_ms,
            format_refresh(rs),
            self.cache.len()
        );
        self.stats = Some(s);
        self.stats_for = Some(key);
    }

    fn draw(&mut self, f: &mut ratatui::Frame) {
        let main = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(1),
            ])
            .split(f.area());

        self.draw_topbar(f, main[0]);
        match self.view {
            View::Sessions => self.draw_sessions(f, main[1]),
            View::Dashboard => self.draw_dashboard(f, main[1]),
            View::Memory => self.draw_memory(f, main[1]),
            View::Skills => self.draw_skills(f, main[1]),
        }
        self.draw_status(f, main[2]);
    }

    fn draw_topbar(&self, f: &mut ratatui::Frame, area: Rect) {
        let mut spans = vec![Span::raw(" auditit  ")];
        for (v, label, key) in [
            (View::Sessions, "Sessions", "S"),
            (View::Dashboard, "Dashboard", "D"),
            (View::Memory, "Memory", "M"),
            (View::Skills, "Skills", "K"),
        ] {
            let active = self.view == v;
            let style = if active {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            spans.push(Span::styled(format!(" {label} [{key}] "), style));
            spans.push(Span::raw("  "));
        }
        // Filter indicator
        spans.push(Span::styled("│  ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            "agent=",
            Style::default().fg(Color::DarkGray),
        ));
        let agent_active = self.filter.agent.is_some();
        spans.push(Span::styled(
            format!("[{}]", self.filter.label_agent()),
            if agent_active {
                Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            },
        ));
        spans.push(Span::styled(" [f]", Style::default().fg(Color::DarkGray)));
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            "scripted=",
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::styled(
            if self.filter.exclude_scripted { "hide" } else { "show" },
            if self.filter.exclude_scripted {
                Style::default().fg(Color::Black).bg(Color::Rgb(200, 140, 80)).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            },
        ));
        spans.push(Span::styled(" [p]", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled("  │  ", Style::default().fg(Color::DarkGray)));
        let rs = self.refresh_secs.load(Ordering::Relaxed);
        spans.push(Span::styled(
            format!("auto:{}", format_refresh(rs)),
            if rs == 0 {
                Style::default().fg(Color::Rgb(180, 120, 120))
            } else {
                Style::default().fg(Color::Rgb(140, 180, 140))
            },
        ));
        spans.push(Span::styled(" [+/-/0]", Style::default().fg(Color::DarkGray)));
        if let crate::update::UpdateStatus::Available { current_version, latest_version } =
            self.update_state.status()
        {
            spans.push(Span::styled("  │  ", Style::default().fg(Color::DarkGray)));
            spans.push(Span::styled(
                format!("↑ {current_version}->{latest_version}"),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_sessions(&mut self, f: &mut ratatui::Frame, area: Rect) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(area);
        if matches!(self.mode, Mode::Search) {
            self.draw_search_list(f, cols[0]);
        } else {
            self.draw_list(f, cols[0]);
        }
        self.draw_detail(f, cols[1]);
    }

    fn draw_search_list(&mut self, f: &mut ratatui::Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .search
            .results
            .iter()
            .map(|h| {
                let agent_color = match h.agent {
                    Agent::Claude => Color::Cyan,
                    Agent::Codex => Color::Green,
                    Agent::Hermes => Color::Magenta,
                    Agent::Qwen => Color::Blue,
                };
                let agent_tag = match h.agent {
                    Agent::Claude => "cla",
                    Agent::Codex => "cdx",
                    Agent::Hermes => "her",
                    Agent::Qwen => "qwn",
                };
                let header = Line::from(vec![
                    Span::styled(
                        format!("{} ", agent_tag),
                        Style::default().fg(agent_color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{} ", h.kind.label().trim()),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(short_time(&h.ts), Style::default().fg(Color::DarkGray)),
                ]);
                let snip_spans = snippet_spans(&h.snippet, h.match_start, h.match_len);
                let body = Line::from(snip_spans);
                ListItem::new(vec![header, body])
            })
            .collect();
        let title = format!(
            " /{}{}  ({})",
            self.search.query,
            if self.search.dirty_at.is_some() { "…" } else { "" },
            self.search.results.len()
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(Color::Yellow));
        let list = List::new(items)
            .block(block)
            .highlight_style(
                Style::default()
                    .bg(Color::Rgb(60, 60, 80))
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");
        f.render_stateful_widget(list, area, &mut self.search.list_state);
    }

    fn draw_list(&mut self, f: &mut ratatui::Frame, area: Rect) {
        let rows = self.list_rows();
        let expanded = self.expanded_groups.clone();
        let groups: Vec<SessionGroup> = self.groups().to_vec();
        let total_filtered = self.filtered_sessions().len();
        let items: Vec<ListItem> = rows
            .iter()
            .map(|row| match row {
                ListRow::GroupHeader { group_idx } => {
                    let g = &groups[*group_idx];
                    let agent_color = agent_color(g.agent);
                    let is_expanded = expanded.contains(&g.key);
                    let chevron = if is_expanded { "▼" } else { "▶" };
                    let ts = format_ts(g.latest_active_ts);
                    let cwd = g
                        .cwd
                        .as_deref()
                        .map(shorten_cwd)
                        .unwrap_or_else(|| "-".to_string());
                    let first_prompt = g
                        .members
                        .last()
                        .and_then(|m| m.prompt.clone())
                        .unwrap_or_default()
                        .replace('\n', " ");
                    let first_prompt: String = first_prompt.chars().take(30).collect();
                    let spans = vec![
                        Span::styled(
                            format!("{} ", chevron),
                            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("{:3} ", agent_short(g.agent)),
                            Style::default().fg(agent_color).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(ts, Style::default().fg(Color::DarkGray)),
                        Span::raw(" "),
                        Span::styled(
                            cwd,
                            Style::default().fg(Color::Rgb(120, 170, 200)).add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(" "),
                        Span::styled(
                            format!("[{} sessions]", g.len()),
                            Style::default().fg(Color::Yellow),
                        ),
                        Span::raw(" "),
                        Span::styled(first_prompt, Style::default().fg(Color::DarkGray)),
                    ];
                    ListItem::new(Line::from(spans))
                }
                ListRow::Solo { group_idx, .. } => {
                    let s = &groups[*group_idx].members[0];
                    list_item_for_session(s, false)
                }
                ListRow::Child { group_idx, sid } => {
                    let g = &groups[*group_idx];
                    let s = g.members.iter().find(|m| &m.id == sid).unwrap_or(&g.members[0]);
                    list_item_for_session(s, true)
                }
            })
            .collect();

        let n_groups = groups.len();
        let title = format!(
            " Sessions ({} groups · {} / {}) ",
            n_groups,
            total_filtered,
            self.sessions.len()
        );
        let focus_on = matches!(self.focus, Focus::List);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(focus_border(focus_on));
        let list = List::new(items)
            .block(block)
            .highlight_style(
                Style::default()
                    .bg(Color::Rgb(60, 60, 80))
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");
        f.render_stateful_widget(list, area, &mut self.list_state);
    }

    fn draw_detail(&mut self, f: &mut ratatui::Frame, area: Rect) {
        let focus_on = matches!(self.focus, Focus::Detail);
        let sel_sid = self.selected_sid();
        let title = match sel_sid.as_deref() {
            None => " Transcript ".to_string(),
            Some(sid) => {
                let meta_clone = self.sessions.iter().find(|m| m.id == sid).cloned();
                let group_pos = {
                    let g_idx = self.selected_group_idx();
                    if let Some(gi) = g_idx {
                        let groups = self.groups();
                        groups.get(gi).and_then(|g| {
                            if g.len() > 1 {
                                g.members
                                    .iter()
                                    .position(|m| m.id == sid)
                                    .map(|p| (p + 1, g.len()))
                            } else { None }
                        })
                    } else { None }
                };
                match meta_clone {
                    Some(m) => {
                        let model = m.model.as_deref().unwrap_or("-");
                        let cwd = m.cwd.as_deref().unwrap_or("-");
                        match group_pos {
                            Some((pos, total)) => format!(
                                " {} · {} · {} · [{}of{} [ ]] ",
                                m.id, model, cwd, pos, total
                            ),
                            None => format!(" {} · {} · {} ", m.id, model, cwd),
                        }
                    }
                    None => " Transcript ".to_string(),
                }
            }
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(focus_border(focus_on));

        let inner_width = block.inner(area).width;
        let cur_t = self.current_transcript();
        let empty_overrides: HashSet<usize> = HashSet::new();
        let overrides_ref: &HashSet<usize> = match self.transcript_for.as_ref() {
            Some(sid) => self.fold_overrides.get(sid).unwrap_or(&empty_overrides),
            None => &empty_overrides,
        };
        let content: Vec<Line> = if let Some(events) = cur_t.as_ref() {
            // Single pass: render_detail produces both the Line buffer and
            // the per-event row offsets matching the squashed layout. Same
            // offsets vec serves pending-jump resolution and the cache the
            // `x` handler consults, so we never re-render twice per frame.
            let (rendered, offsets) = render_detail(
                events.as_ref(),
                inner_width,
                self.fold_mode,
                overrides_ref,
            );
            if let Some((pj_sid, pj_idx)) = self.pending_jump.clone() {
                if Some(&pj_sid) == self.transcript_for.as_ref() {
                    if let Some(row) = offsets.get(pj_idx).copied() {
                        self.scroll = row;
                    }
                    self.pending_jump = None;
                }
            }
            self.detail_event_offsets = offsets;
            rendered
        } else if self.pending_preview.is_some() {
            vec![Line::from(Span::styled(
                "previewing…",
                Style::default().fg(Color::DarkGray),
            ))]
        } else {
            vec![
                Line::from("↑/↓ select · auto preview"),
                Line::from("Tab to scroll · q quit · r refresh"),
            ]
        };

        // Stash the rendered line count so move_scroll has a real upper bound
        // instead of the old `event_count × 3` heuristic (which caps at 12 for
        // a 4-event session and hides everything past the first screenful —
        // e.g. a single 125 KB prompt event renders to hundreds of lines but
        // was unreachable by ↓/PgDn).
        self.detail_row_count = content.len().min(u16::MAX as usize) as u16;

        // Scroll was sized against the *previous* frame's content length. If
        // the content just shrank — e.g. the user pressed End to jump to the
        // bottom of a 3000-line expanded transcript and then pressed `z` to
        // fold long tool_result blocks, collapsing the total to ~200 lines —
        // scroll is now past the new content and Paragraph renders an empty
        // inner area. Clamp here so the bottom of the now-shorter transcript
        // stays visible. move_scroll only clamps on the next key press, which
        // is too late.
        let max_scroll = self.detail_row_count.saturating_sub(1);
        self.scroll = self.scroll.min(max_scroll);

        // `.wrap(Wrap { trim: false })`: md::to_lines_width emits paragraph
        // text as one long Line (pulldown-cmark's SoftBreak collapses source
        // newlines into spaces). Without ratatui-side wrap the right edge
        // of any long user/assistant paragraph gets silently clipped. Code
        // blocks and tool-result bodies are already manually wrapped via
        // wrap_to_width, so they pass through unchanged.
        let para = Paragraph::new(content)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((self.scroll, 0));
        f.render_widget(para, area);
    }

    fn draw_memory(&mut self, f: &mut ratatui::Frame, area: Rect) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(area);

        let focus_list = matches!(self.focus, Focus::List);
        let focus_detail = matches!(self.focus, Focus::Detail);

        let items: Vec<ListItem> = self
            .memory_rows
            .iter()
            .map(|r| {
                let agent_color = match r.agent.as_str() {
                    "claude" => Color::Cyan,
                    "codex" => Color::Green,
                    "qwen" => Color::Blue,
                    _ => Color::Gray,
                };
                ListItem::new(Line::from(vec![Span::styled(
                    r.label.clone(),
                    Style::default().fg(agent_color),
                )]))
            })
            .collect();

        let title = format!(" Memory ({}) ", self.memory_rows.len());
        let block_list = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(focus_border(focus_list));
        let list = List::new(items)
            .block(block_list)
            .highlight_style(
                Style::default()
                    .bg(Color::Rgb(60, 60, 80))
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");
        f.render_stateful_widget(list, cols[0], &mut self.memory_list_state);

        let detail_title = match self.memory_content_for.as_ref() {
            Some(p) => format!(" {} ", p.display()),
            None => " (no selection) ".to_string(),
        };
        let block_detail = Block::default()
            .borders(Borders::ALL)
            .title(detail_title)
            .border_style(focus_border(focus_detail));
        let body = self.memory_content.as_deref().unwrap_or("(empty)");
        let is_md = self
            .memory_content_for
            .as_ref()
            .map(|p| is_markdown_path(p))
            .unwrap_or(false);
        let inner_w = block_detail.inner(cols[1]).width as usize;
        let lines: Vec<Line> = if is_md {
            crate::md::to_lines_width(body, inner_w)
        } else {
            plain_lines(body)
        };
        let para = Paragraph::new(lines)
            .block(block_detail)
            .wrap(Wrap { trim: false })
            .scroll((self.memory_scroll, 0));
        f.render_widget(para, cols[1]);
    }

    fn draw_skills(&mut self, f: &mut ratatui::Frame, area: Rect) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(area);

        let focus_list = matches!(self.focus, Focus::List);
        let focus_detail = matches!(self.focus, Focus::Detail);

        let items: Vec<ListItem> = self
            .skills_list
            .iter()
            .map(|sk| {
                let agent_color = match sk.agent.as_str() {
                    "claude" => Color::Cyan,
                    "codex" => Color::Green,
                    "qwen" => Color::Blue,
                    _ => Color::Gray,
                };
                let mut spans = vec![
                    Span::styled(
                        format!("[{}] ", sk.agent),
                        Style::default().fg(agent_color).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        sk.name.clone(),
                        Style::default().fg(Color::White),
                    ),
                ];
                if !sk.description.is_empty() {
                    let desc: String = sk.description.chars().take(60).collect();
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(
                        desc,
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();

        let title = format!(" Skills ({}) ", self.skills_list.len());
        let block_list = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(focus_border(focus_list));
        let list = List::new(items)
            .block(block_list)
            .highlight_style(
                Style::default()
                    .bg(Color::Rgb(60, 60, 80))
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");
        f.render_stateful_widget(list, cols[0], &mut self.skills_list_state);

        let detail_title = match self
            .skills_list_state
            .selected()
            .and_then(|i| self.skills_list.get(i))
        {
            Some(sk) => format!(" {} · {} ", sk.agent, sk.path.display()),
            None => " (no selection) ".to_string(),
        };
        let block_detail = Block::default()
            .borders(Borders::ALL)
            .title(detail_title)
            .border_style(focus_border(focus_detail));
        let body = self.skills_content.as_deref().unwrap_or("(empty)");
        let is_md = self
            .skills_content_for
            .as_ref()
            .map(|p| is_markdown_path(p))
            .unwrap_or(false);
        let inner_w = block_detail.inner(cols[1]).width as usize;
        let lines: Vec<Line> = if is_md {
            crate::md::to_lines_width(body, inner_w)
        } else {
            plain_lines(body)
        };
        let para = Paragraph::new(lines)
            .block(block_detail)
            .wrap(Wrap { trim: false })
            .scroll((self.skills_scroll, 0));
        f.render_widget(para, cols[1]);
    }

    fn draw_dashboard(&mut self, f: &mut ratatui::Frame, area: Rect) {
        let unit_label = match self.dashboard_unit {
            DashboardUnit::Dollars => "$",
            DashboardUnit::Calls => "calls/hr",
        };
        let mode_label = match self.dashboard_mode {
            DashboardMode::Overview => "overview",
            DashboardMode::Sessions => "sessions",
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(
                " Dashboard · {} · {} · {} [v] ",
                self.range.label(),
                unit_label,
                mode_label,
            ))
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let Some(stats) = &self.stats else {
            let p = Paragraph::new("computing...");
            f.render_widget(p, inner);
            return;
        };
        let stats = stats.clone();

        match self.dashboard_mode {
            DashboardMode::Overview => self.draw_dashboard_overview(f, inner, &stats),
            DashboardMode::Sessions => self.draw_dashboard_sessions(f, inner, &stats),
        }
    }

    fn draw_dashboard_overview(
        &mut self,
        f: &mut ratatui::Frame,
        inner: Rect,
        stats: &Stats,
    ) {
        // Split: top = summary tables (scrollable), bottom = charts.
        let vsplit = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(10), Constraint::Length(14)])
            .split(inner);
        let top = vsplit[0];
        let bottom = vsplit[1];

        let unit = self.dashboard_unit;
        let hours = stats.window_hours.max(0.0001);

        let mut lines: Vec<Line> = Vec::new();
        let mut picker_spans: Vec<Span> = vec![Span::raw("Range (← →): ")];
        for r in Range::ALL {
            let style = if r == self.range {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            picker_spans.push(Span::styled(format!(" {} ", r.label()), style));
            picker_spans.push(Span::raw(" "));
        }
        lines.push(Line::from(picker_spans));
        lines.push(Line::from(Span::styled(
            format!("  Unit [u]: $ / calls-per-hour (window {:.1}h)", hours),
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::from(""));

        lines.push(Line::from(Span::styled(
            "── Total ─────────────",
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::from(format!(
            "groups:    {}  ({} sessions)",
            stats.total_groups, stats.total_sessions
        )));
        lines.push(Line::from(format!("turns:     {}", stats.total_turns)));
        lines.push(Line::from(format!("calls:     {}", stats.total_calls)));
        lines.push(Line::from(match unit {
            DashboardUnit::Dollars => format!("cost:      ${:.4}", stats.total_cost),
            DashboardUnit::Calls => format!(
                "calls/hr:  {:.2}",
                stats.total_calls as f64 / hours
            ),
        }));
        lines.push(Line::from(""));

        lines.push(Line::from(Span::styled(
            "── By Agent ──────────",
            Style::default().fg(Color::DarkGray),
        )));
        for (ag, row) in &stats.by_agent {
            let color = match *ag {
                "claude" => Color::Cyan,
                "codex" => Color::Green,
                "qwen" => Color::Blue,
                _ => Color::White,
            };
            let metric = match unit {
                DashboardUnit::Dollars => format!("cost=${:.4}", row.cost),
                DashboardUnit::Calls => format!(
                    "calls/hr={:.2}",
                    row.calls as f64 / hours
                ),
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{:8}", ag),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(
                    " sessions={:5}  turns={:5}  calls={:5}  {}",
                    row.sessions, row.turns, row.calls, metric
                )),
            ]));
        }
        lines.push(Line::from(""));

        lines.push(Line::from(Span::styled(
            "── By Model ──────────",
            Style::default().fg(Color::DarkGray),
        )));
        let metric_header = match unit {
            DashboardUnit::Dollars => "cost",
            DashboardUnit::Calls => "calls/hr",
        };
        lines.push(Line::from(format!(
            "{:22} {:>7} {:>7} {:>7} {:>12} {:>12} {:>12} {:>12}",
            "model", "sess", "turns", "calls", metric_header, "in_tok", "out_tok", "cache_rd"
        )));
        for m in &stats.by_model {
            let metric = match unit {
                DashboardUnit::Dollars => format!("${:.4}", m.cost),
                DashboardUnit::Calls => format!("{:.2}", m.calls as f64 / hours),
            };
            lines.push(Line::from(format!(
                "{:22} {:>7} {:>7} {:>7} {:>12} {:>12} {:>12} {:>12}",
                truncate(&m.model, 22),
                m.sessions,
                m.turns,
                m.calls,
                metric,
                fmt_int(m.usage.input_tokens),
                fmt_int(m.usage.output_tokens),
                fmt_int(m.usage.cache_read_tokens),
            )));
        }

        let para = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((self.dashboard_scroll, 0));
        f.render_widget(para, top);

        let hsplit = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(bottom);
        draw_agent_bars(f, hsplit[0], stats, unit, hours);
        draw_time_chart(f, hsplit[1], stats, unit, hours);
    }

    fn draw_dashboard_sessions(
        &mut self,
        f: &mut ratatui::Frame,
        inner: Rect,
        stats: &Stats,
    ) {
        let unit = self.dashboard_unit;
        let hours = stats.window_hours.max(0.0001);
        const TOP_N: usize = 20;

        let mut rows: Vec<(&dashboard::GroupRow, f64)> = stats
            .by_group
            .iter()
            .map(|r| {
                let v = match unit {
                    DashboardUnit::Dollars => r.cost,
                    DashboardUnit::Calls => r.calls as f64 / hours,
                };
                (r, v)
            })
            .filter(|(_, v)| *v > 0.0)
            .collect();
        rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let total: f64 = rows.iter().map(|(_, v)| *v).sum();
        let top: Vec<_> = rows.iter().take(TOP_N).collect();

        if top.is_empty() {
            let p = Paragraph::new(Line::from(Span::styled(
                "(no data in range)",
                Style::default().fg(Color::DarkGray),
            )));
            f.render_widget(p, inner);
            return;
        }

        let unit_label = match unit {
            DashboardUnit::Dollars => "$",
            DashboardUnit::Calls => "calls/hr",
        };
        let bar_width = inner.width.saturating_sub(60).max(10) as usize;
        let max_v = top.iter().map(|(_, v)| *v).fold(0f64, f64::max).max(1e-9);

        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(Span::styled(
            format!(
                "Top {} groups by {} — total {:.4} {} across {} groups",
                top.len(),
                unit_label,
                total,
                unit_label,
                rows.len()
            ),
            Style::default().fg(Color::Gray),
        )));
        lines.push(Line::from(""));

        for (r, v) in &top {
            let agent_col = match r.agent {
                "claude" => Color::Cyan,
                "codex" => Color::Green,
                "qwen" => Color::Blue,
                _ => Color::White,
            };
            let pct = if total > 0.0 { *v * 100.0 / total } else { 0.0 };
            let bar_len = ((*v / max_v) * bar_width as f64).round() as usize;
            let bar = "█".repeat(bar_len);
            let value_str = match unit {
                DashboardUnit::Dollars => format!("${:>8.4}", v),
                DashboardUnit::Calls => format!("{:>7.2}/h", v),
            };
            let cwd_short = if r.cwd.is_empty() {
                "-".to_string()
            } else {
                shorten_cwd(&r.cwd)
            };
            let prompt: String = r.prompt.chars().take(20).collect();
            let prompt = prompt.replace('\n', " ");
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{:6}", r.agent),
                    Style::default().fg(agent_col).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(
                    fmt_col(&cwd_short, 20),
                    Style::default().fg(Color::Rgb(120, 170, 200)),
                ),
                Span::raw(" "),
                Span::styled(
                    format!("{:>4}s", r.n_sessions),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(" "),
                Span::styled(
                    fmt_col(&prompt, 18),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(" "),
                Span::styled(value_str, Style::default().fg(Color::Yellow)),
                Span::raw(format!(" {:>5.1}% ", pct)),
                Span::styled(bar, Style::default().fg(agent_col)),
            ]));
        }

        let para = Paragraph::new(lines)
            .scroll((self.dashboard_scroll, 0));
        f.render_widget(para, inner);
    }

    fn draw_status(&self, f: &mut ratatui::Frame, area: Rect) {
        if matches!(self.mode, Mode::Search) {
            let line = Line::from(vec![
                Span::styled(" / ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::styled(
                    self.search.query.clone(),
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                ),
                Span::styled("▏", Style::default().fg(Color::Yellow)),
                Span::raw("  "),
                Span::styled(
                    self.search.last_status.clone(),
                    Style::default().fg(Color::DarkGray),
                ),
            ]);
            f.render_widget(Paragraph::new(line), area);
            return;
        }
        let hint = match (self.view, &self.focus) {
            (View::Sessions, Focus::List) => {
                "↑/↓ move · Enter open / toggle · Space expand · [ ] group-nav · / search · Tab detail · D dashboard · r · q"
            }
            (View::Sessions, Focus::Detail) => {
                "↑/↓ scroll · PgUp/PgDn · [ ] prev/next in group · Tab list · Esc · S · q"
            }
            (View::Dashboard, _) => {
                "← → range · u unit · v view · ↑/↓ scroll · +/- refresh · S · r · q"
            }
            (View::Memory, Focus::List) => {
                "↑/↓ move · Tab content · S sessions · q quit"
            }
            (View::Memory, Focus::Detail) => {
                "↑/↓ scroll · Tab list · Esc list · q quit"
            }
            (View::Skills, Focus::List) => {
                "↑/↓ move · Tab content · S sessions · q quit"
            }
            (View::Skills, Focus::Detail) => {
                "↑/↓ scroll · Tab list · Esc list · q quit"
            }
        };
        let line = Line::from(vec![
            Span::styled(format!(" {} ", hint), Style::default().fg(Color::DarkGray)),
            Span::raw("│ "),
            Span::raw(self.status.clone()),
        ]);
        f.render_widget(Paragraph::new(line), area);
    }
}

fn format_refresh(secs: u64) -> String {
    if secs == 0 {
        "off".to_string()
    } else if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

fn is_markdown_path(p: &std::path::Path) -> bool {
    matches!(
        p.extension().and_then(|s| s.to_str()).map(|s| s.to_ascii_lowercase()).as_deref(),
        Some("md") | Some("markdown") | Some("mdown") | Some("mkd")
    )
}

fn plain_lines(text: &str) -> Vec<Line<'static>> {
    text.split('\n').map(|s| Line::from(s.to_string())).collect()
}

fn agent_color(a: Agent) -> Color {
    match a {
        Agent::Claude => Color::Cyan,
        Agent::Codex => Color::Green,
        Agent::Hermes => Color::Magenta,
        Agent::Qwen => Color::Blue,
    }
}

fn agent_short(a: Agent) -> &'static str {
    a.short()
}

fn list_item_for_session(s: &SessionMeta, indent: bool) -> ListItem<'static> {
    let agent_col = agent_color(s.agent);
    let ts = format_ts(s.last_active_ts);
    let prompt = s
        .prompt
        .as_deref()
        .unwrap_or("(no prompt)")
        .replace('\n', " ");
    let prompt_short: String = prompt.chars().take(50).collect();
    let mut spans: Vec<Span> = Vec::new();
    if indent {
        spans.push(Span::styled(
            "  └ ",
            Style::default().fg(Color::DarkGray),
        ));
    }
    spans.push(Span::styled(
        format!("{:3} ", s.agent.short()),
        Style::default().fg(agent_col).add_modifier(Modifier::BOLD),
    ));
    if s.is_scripted {
        spans.push(Span::styled(
            "·sdk ",
            Style::default().fg(Color::Rgb(170, 130, 80)),
        ));
    }
    spans.push(Span::styled(ts, Style::default().fg(Color::DarkGray)));
    spans.push(Span::raw(" "));
    if !indent {
        if let Some(cwd) = s.cwd.as_deref() {
            spans.push(Span::styled(
                shorten_cwd(cwd),
                Style::default().fg(Color::Rgb(120, 170, 200)),
            ));
            spans.push(Span::raw(" "));
        }
    }
    spans.push(Span::raw(prompt_short));
    ListItem::new(Line::from(spans))
}

fn focus_border(on: bool) -> Style {
    if on {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn shorten_cwd(cwd: &str) -> String {
    let leading_slash = cwd.starts_with('/');
    let segs: Vec<&str> = cwd.split('/').filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        return cwd.to_string();
    }
    let last_idx = segs.len() - 1;
    let mut out = String::new();
    if leading_slash {
        out.push('/');
    }
    for (i, seg) in segs.iter().enumerate() {
        if i == last_idx {
            out.push_str(seg);
        } else {
            if let Some(ch) = seg.chars().next() {
                out.push(ch);
            }
            out.push('/');
        }
    }
    out
}

fn format_ts(unix: u64) -> String {
    if unix == 0 {
        return "                ".to_string();
    }
    let dt: DateTime<Local> = Local
        .timestamp_opt(unix as i64, 0)
        .single()
        .unwrap_or_else(|| Local.timestamp_opt(0, 0).single().unwrap());
    dt.format("%m-%d %H:%M").to_string()
}

/// Minimum rendered rows before tool_result / system / thinking events are
/// collapsed to a single placeholder in Smart fold mode. Short bodies stay
/// inline — folding them would add chrome without saving screen space.
const FOLD_THRESHOLD_ROWS: usize = 10;

fn render_one_event(ev: &TranscriptEvent, w: usize) -> Vec<Line<'static>> {
    let mut buf = Vec::new();
    match ev.kind {
        TranscriptKind::User => render_user(ev, &mut buf, w),
        TranscriptKind::Assistant => render_assistant(ev, &mut buf, w),
        TranscriptKind::Thinking => render_thinking(ev, &mut buf, w),
        TranscriptKind::ToolUse => render_tool_use(ev, &mut buf, w),
        TranscriptKind::ToolResult => render_tool_result(ev, &mut buf, w),
        TranscriptKind::System => render_system(ev, &mut buf, w),
    }
    buf
}

fn is_foldable(kind: TranscriptKind, rendered_rows: usize) -> bool {
    matches!(
        kind,
        TranscriptKind::ToolResult | TranscriptKind::System | TranscriptKind::Thinking
    ) && rendered_rows > FOLD_THRESHOLD_ROWS
}

fn folded_placeholder(ev: &TranscriptEvent, rows: usize) -> Line<'static> {
    let (tag, color) = match ev.kind {
        TranscriptKind::ToolResult => ("TOOL<", Color::LightGreen),
        TranscriptKind::System => ("SYS", Color::Gray),
        TranscriptKind::Thinking => ("THINK", Color::Magenta),
        _ => ("EVENT", Color::White),
    };
    Line::from(vec![
        Span::styled(
            format!("|{:<5} ", tag),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(short_time(&ev.ts), Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("  - {} lines folded  [+ x]", rows),
            Style::default().fg(Color::Rgb(140, 140, 160)),
        ),
    ])
}

fn line_is_blank(l: &Line<'_>) -> bool {
    l.spans
        .iter()
        .all(|s| s.content.chars().all(|c| c.is_whitespace()))
}

/// Render events and compute each event's starting row in a single pass.
///
/// The "single pass" matters because consecutive blank lines are squashed
/// to at most one — without that, a chat with markdown paragraph breaks
/// or user prompts containing `\n\n\n` feels very empty. Doing render and
/// row-offset calc together means the offsets reflect the final (squashed)
/// layout instead of having to replay the same logic twice.
fn render_detail(
    events: &[TranscriptEvent],
    inner_width: u16,
    fold_mode: FoldMode,
    overrides: &HashSet<usize>,
) -> (Vec<Line<'static>>, Vec<u16>) {
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(events.len() * 4);
    let mut offsets: Vec<u16> = Vec::with_capacity(events.len());
    let w = inner_width.max(1) as usize;
    let mut prev_blank = false;

    for (i, ev) in events.iter().enumerate() {
        offsets.push(lines.len().min(u16::MAX as usize) as u16);

        let buf = render_one_event(ev, w);
        let foldable = is_foldable(ev.kind, buf.len());
        let should_fold = match fold_mode {
            FoldMode::Expanded => false,
            FoldMode::Smart => foldable && !overrides.contains(&i),
        };
        let event_lines: Vec<Line<'static>> = if should_fold {
            vec![folded_placeholder(ev, buf.len())]
        } else {
            buf
        };

        for line in event_lines {
            let is_blank = line_is_blank(&line);
            if is_blank && prev_blank {
                continue; // squash consecutive blanks
            }
            prev_blank = is_blank;
            lines.push(line);
        }

        // One blank between events — squashed if the event already ended
        // on a blank line.
        if !prev_blank {
            lines.push(Line::from(""));
            prev_blank = true;
        }
    }
    (lines, offsets)
}


fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

fn wrap_to_width(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![s.to_string()];
    }
    if s.is_empty() {
        return vec![String::new()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut buf_w: usize = 0;
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if buf_w + cw > width && !buf.is_empty() {
            out.push(std::mem::take(&mut buf));
            buf_w = 0;
        }
        buf.push(ch);
        buf_w += cw;
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}


fn short_time(ts: &str) -> String {
    // ISO-8601 like "2026-04-15T14:52:34.554Z" → "14:52:34"
    if ts.len() >= 19 && ts.as_bytes().get(10) == Some(&b'T') {
        return ts[11..19].to_string();
    }
    ts.to_string()
}

// Background colors for ASSIS / TOOL→ / TOOL← rows only. USER / THINK / SYS
// render with no row background by design — the user only wants emphasis on
// assistant output and tool interactions.
const BG_ASSIS: Color = Color::Rgb(14, 30, 36);
const BG_TOOL_USE: Color = Color::Rgb(40, 32, 14);
const BG_TOOL_OK: Color = Color::Rgb(16, 34, 20);
const BG_TOOL_ERR: Color = Color::Rgb(48, 18, 18);
const BG_TOOL_DEFAULT: Color = Color::Rgb(20, 32, 26);

fn header(
    tag: &'static str,
    tag_color: Color,
    ts: &str,
    suffix: Option<String>,
    bg: Color,
    width: usize,
) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            format!("|{:<5} ", tag),
            Style::default().fg(tag_color).bg(bg).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            short_time(ts),
            Style::default().fg(Color::DarkGray).bg(bg),
        ),
    ];
    if let Some(s) = suffix {
        spans.push(Span::styled(" ", Style::default().bg(bg)));
        spans.push(Span::styled(
            s,
            Style::default().fg(Color::DarkGray).bg(bg),
        ));
    }
    let used: usize = spans.iter().map(|s| display_width(&s.content)).sum();
    if used < width {
        spans.push(Span::styled(
            " ".repeat(width - used),
            Style::default().bg(bg),
        ));
    }
    Line::from(spans).style(Style::default().bg(bg))
}

fn body_line(content: String, style: Style) -> Line<'static> {
    Line::from(Span::styled(content, style))
}

fn header_plain(tag: &'static str, tag_color: Color, ts: &str, suffix: Option<String>) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            format!("|{:<5} ", tag),
            Style::default().fg(tag_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(short_time(ts), Style::default().fg(Color::DarkGray)),
    ];
    if let Some(s) = suffix {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(s, Style::default().fg(Color::DarkGray)));
    }
    Line::from(spans)
}

fn render_user(ev: &TranscriptEvent, out: &mut Vec<Line<'static>>, width: usize) {
    out.push(header_plain("USER", Color::LightBlue, &ev.ts, None));
    // User prompts occasionally include markdown — ``` fences, lists, code —
    // so pipe through the same renderer Memory/Skills uses. Empty body
    // tolerated (happens with tool-result-only user turns the parser has
    // already stripped).
    if ev.body.trim().is_empty() {
        return;
    }
    out.extend(md::to_lines_width(&ev.body, width));
}

fn render_assistant(ev: &TranscriptEvent, out: &mut Vec<Line<'static>>, width: usize) {
    out.push(header("ASSIS", Color::Cyan, &ev.ts, None, BG_ASSIS, width));
    if ev.body.trim().is_empty() {
        return;
    }
    out.extend(md::to_lines_width(&ev.body, width));
}

fn render_thinking(ev: &TranscriptEvent, out: &mut Vec<Line<'static>>, width: usize) {
    out.push(header_plain("THINK", Color::Magenta, &ev.ts, None));
    let style = Style::default()
        .fg(Color::Rgb(160, 150, 200))
        .add_modifier(Modifier::ITALIC);
    let inner_w = width.saturating_sub(2).max(1);
    for line in ev.body.lines() {
        for piece in wrap_to_width(line, inner_w) {
            out.push(Line::from(Span::styled(format!("  {piece}"), style)));
        }
    }
}

fn render_tool_use(ev: &TranscriptEvent, out: &mut Vec<Line<'static>>, width: usize) {
    let (name, args) = match ev.body.split_once(": ") {
        Some((n, a)) => (n.to_string(), a.to_string()),
        None => (ev.body.clone(), String::new()),
    };
    out.push(header(
        "TOOL>",
        Color::Yellow,
        &ev.ts,
        Some(name),
        BG_TOOL_USE,
        width,
    ));
    if args.is_empty() {
        return;
    }
    let pretty = match serde_json::from_str::<serde_json::Value>(&args) {
        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| args.clone()),
        Err(_) => args.clone(),
    };
    let arg_style = Style::default().fg(Color::Rgb(210, 180, 100));
    let inner_w = width.saturating_sub(2).max(1);
    for line in pretty.lines() {
        for piece in wrap_to_width(line, inner_w) {
            out.push(body_line(format!("  {piece}"), arg_style));
        }
    }
}

fn render_tool_result(ev: &TranscriptEvent, out: &mut Vec<Line<'static>>, width: usize) {
    let mut body = ev.body.as_str();
    let mut exit_code: Option<i64> = None;
    // Only codex's exec_command_end events carry an exit code, and they
    // tag the body with a Record-Separator prefix (\u{1e}) so we don't
    // misparse a generic tool_result whose body happens to start with
    // "exit=0\n…" (which can and does occur in real shell output).
    if let Some(rest) = body.strip_prefix("\u{1e}exit=") {
        if let Some(nl) = rest.find('\n') {
            if let Ok(n) = rest[..nl].parse::<i64>() {
                exit_code = Some(n);
                body = &rest[nl + 1..];
            }
        } else if let Ok(n) = rest.parse::<i64>() {
            exit_code = Some(n);
            body = "";
        }
    }
    let (tag_color, suffix, bg) = match exit_code {
        Some(0) => (Color::Green, Some("exit=0".to_string()), BG_TOOL_OK),
        Some(n) => (Color::Red, Some(format!("exit={n}")), BG_TOOL_ERR),
        None => (Color::LightGreen, None, BG_TOOL_DEFAULT),
    };
    out.push(header("TOOL<", tag_color, &ev.ts, suffix, bg, width));
    let default = Color::Rgb(190, 190, 190);
    for line in body.lines() {
        for piece in wrap_to_width(line, width) {
            out.push(Line::from(highlight_keywords(&piece, default)));
        }
    }
}

fn render_system(ev: &TranscriptEvent, out: &mut Vec<Line<'static>>, width: usize) {
    out.push(header_plain("SYS", Color::Gray, &ev.ts, None));
    let style = Style::default().fg(Color::DarkGray);
    let inner_w = width.saturating_sub(2).max(1);
    for line in ev.body.lines() {
        for piece in wrap_to_width(line, inner_w) {
            out.push(Line::from(Span::styled(format!("  {piece}"), style)));
        }
    }
}

fn highlight_keywords(line: &str, default: Color) -> Vec<Span<'static>> {
    let lower = line.to_lowercase();
    let has_err = lower.contains("error")
        || lower.contains("failed")
        || lower.contains("fatal")
        || lower.contains("panic")
        || lower.contains("traceback");
    let has_ok = !has_err
        && (lower.contains("success") || lower.contains(" ok ") || lower.ends_with(" ok"));
    let color = if has_err {
        Color::Rgb(240, 120, 120)
    } else if has_ok {
        Color::Rgb(130, 210, 130)
    } else {
        default
    };
    vec![Span::styled(line.to_string(), Style::default().fg(color))]
}

fn prev_range(r: Range) -> Range {
    let all = Range::ALL;
    let idx = all.iter().position(|x| *x == r).unwrap_or(0);
    if idx == 0 {
        all[all.len() - 1]
    } else {
        all[idx - 1]
    }
}

fn next_range(r: Range) -> Range {
    let all = Range::ALL;
    let idx = all.iter().position(|x| *x == r).unwrap_or(0);
    if idx + 1 == all.len() {
        all[0]
    } else {
        all[idx + 1]
    }
}

/// Truncate + right-pad a string to exactly `target_cols` display columns,
/// counting CJK / wide characters as 2. Needed because Rust's `{:<N}` pads by
/// char count, which misaligns columns when content has non-ASCII runs.
fn fmt_col(s: &str, target_cols: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::new();
    let mut used = 0usize;
    let mut truncated = false;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        // reserve one col for "…" if we know we'll need to truncate
        if used + w > target_cols {
            truncated = true;
            break;
        }
        out.push(c);
        used += w;
    }
    if truncated && target_cols >= 1 {
        // Back off until there's room for the "…" marker.
        while used + 1 > target_cols {
            if let Some(ch) = out.pop() {
                used -= ch.width().unwrap_or(0);
            } else {
                break;
            }
        }
        out.push('…');
        used += 1;
    }
    if used < target_cols {
        out.push_str(&" ".repeat(target_cols - used));
    }
    out
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}

fn fmt_int(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}

fn snippet_spans(text: &str, match_start: usize, match_len: usize) -> Vec<Span<'static>> {
    let len = text.len();
    let start = match_start.min(len);
    let end = (match_start + match_len).min(len);
    let mut out: Vec<Span<'static>> = Vec::new();
    if start > 0 {
        out.push(Span::styled(
            text[..start].to_string(),
            Style::default().fg(Color::Rgb(170, 170, 170)),
        ));
    }
    if end > start {
        out.push(Span::styled(
            text[start..end].to_string(),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if end < len {
        out.push(Span::styled(
            text[end..].to_string(),
            Style::default().fg(Color::Rgb(170, 170, 170)),
        ));
    }
    out
}

fn get_or_read_transcript(
    sid: &str,
    meta: &SessionMeta,
    cache: &Arc<TranscriptCache>,
) -> Option<Arc<Vec<TranscriptEvent>>> {
    if let Ok(r) = cache.read() {
        if let Some(a) = r.get(sid).cloned() {
            return Some(a);
        }
    }
    let events = session::read_transcript(meta).ok()?;
    let arc = Arc::new(events);
    if let Ok(mut w) = cache.write() {
        w.insert(sid.to_string(), arc.clone());
    }
    Some(arc)
}

struct SnippetOut {
    text: String,
    match_start: usize, // byte offset in text
    match_len: usize,   // byte len in text (== query len)
}

fn make_snippet(body: &str, match_byte_pos: usize, q_len_bytes: usize) -> SnippetOut {
    // Walk char boundaries backward/forward to respect UTF-8.
    let start_byte = {
        let mut b = match_byte_pos;
        for _ in 0..SNIPPET_RADIUS {
            if b == 0 {
                break;
            }
            let mut i = b - 1;
            while i > 0 && !body.is_char_boundary(i) {
                i -= 1;
            }
            b = i;
        }
        b
    };
    let end_byte = {
        let mut b = match_byte_pos + q_len_bytes;
        for _ in 0..SNIPPET_RADIUS {
            if b >= body.len() {
                break;
            }
            let mut i = b + 1;
            while i < body.len() && !body.is_char_boundary(i) {
                i += 1;
            }
            b = i;
        }
        b.min(body.len())
    };
    let mut text = String::new();
    if start_byte > 0 {
        text.push('…');
    }
    let lead_len = text.len();
    text.push_str(&body[start_byte..end_byte]);
    if end_byte < body.len() {
        text.push('…');
    }
    let text = text.replace(['\n', '\r', '\t'], " ");
    let match_start = lead_len + (match_byte_pos - start_byte);
    SnippetOut {
        text,
        match_start,
        match_len: q_len_bytes,
    }
}


fn draw_agent_bars(
    f: &mut ratatui::Frame,
    area: Rect,
    stats: &Stats,
    unit: DashboardUnit,
    hours: f64,
) {
    let title = match unit {
        DashboardUnit::Dollars => " Cost by agent ($) ",
        DashboardUnit::Calls => " Calls/hr by agent ",
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(Clear, area);
    if stats.by_agent.is_empty() {
        let p = Paragraph::new("no data").block(block);
        f.render_widget(p, area);
        return;
    }
    let hours = hours.max(0.0001);

    // Use horizontal bars (Paragraph + span rows) instead of BarChart.
    // BarChart normalizes all bars to the tallest one, so when claude ≈ $6
    // and codex / qwen ≈ $0.1, the smaller bars collapse to near-zero
    // height and their text_value gets clipped entirely.
    let bar_budget = inner.width.saturating_sub(22).max(5) as usize;
    let max_v = stats
        .by_agent
        .iter()
        .map(|(_, row)| match unit {
            DashboardUnit::Dollars => row.cost,
            DashboardUnit::Calls => row.calls as f64 / hours,
        })
        .fold(0f64, f64::max)
        .max(1e-9);

    let mut lines: Vec<Line> = Vec::new();
    for (agent, row) in &stats.by_agent {
        let color = match *agent {
            "claude" => Color::Cyan,
            "codex" => Color::Green,
            "qwen" => Color::Blue,
            _ => Color::White,
        };
        let (v, text) = match unit {
            DashboardUnit::Dollars => (row.cost, format!("${:.4}", row.cost)),
            DashboardUnit::Calls => {
                let per_hr = row.calls as f64 / hours;
                (per_hr, format!("{:.2}/h", per_hr))
            }
        };
        let bar_len = if v > 0.0 {
            (((v / max_v) * bar_budget as f64).round() as usize).max(1)
        } else {
            0
        };
        let bar = "█".repeat(bar_len);
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:<7}", agent),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(bar, Style::default().fg(color)),
            Span::raw(" "),
            Span::styled(text, Style::default().fg(Color::Yellow)),
        ]));
    }

    let para = Paragraph::new(lines).block(block);
    f.render_widget(para, area);
}

fn draw_time_chart(
    f: &mut ratatui::Frame,
    area: Rect,
    stats: &Stats,
    unit: DashboardUnit,
    hours: f64,
) {
    let title_unit = match unit {
        DashboardUnit::Dollars => "$",
        DashboardUnit::Calls => "calls/hr",
    };
    let outer_title_multi = if stats.by_time_agent.len() >= 2 {
        " · per-agent"
    } else {
        ""
    };
    let outer_block = Block::default()
        .borders(Borders::ALL)
        .title(format!(
            " {} over time · bucket={}{} ",
            title_unit,
            humanize_secs(stats.bucket_seconds),
            outer_title_multi,
        ))
        .border_style(Style::default().fg(Color::DarkGray));
    f.render_widget(Clear, area);
    if stats.by_time.is_empty() {
        let p = Paragraph::new("no data").block(outer_block);
        f.render_widget(p, area);
        return;
    }
    let inner = outer_block.inner(area);
    f.render_widget(outer_block, area);

    let bucket_hours = (stats.bucket_seconds as f64 / 3600.0).max(0.0001);
    let _ = hours;

    let first_ts = stats.by_time.first().map(|(t, _)| *t).unwrap_or(0);
    let last_ts = stats.by_time.last().map(|(t, _)| *t).unwrap_or(0);
    let mid_ts = first_ts + (last_ts - first_ts) / 2;
    let x_max = (stats.by_time.len() as f64 - 1.0).max(1.0);

    // Multi-agent: render one mini-chart per agent in a vertical stack, each
    // with its own Y-axis (so small-magnitude agents like qwen remain
    // readable). Shared X-axis bounds; X tick labels only on the bottom strip.
    //
    // Pick the series set that matches the current unit — the two maps are
    // filtered independently in dashboard::compute(), so in Calls mode an
    // agent without any LLM-call buckets should not appear as an empty strip,
    // and a calls-only agent (cost=0, e.g. local-model session) should still
    // appear.
    let peak_for = |a: &Agent| -> f64 {
        match unit {
            DashboardUnit::Dollars => stats
                .by_time_agent
                .get(a)
                .map(|v| v.iter().cloned().fold(0f64, f64::max))
                .unwrap_or(0.0),
            DashboardUnit::Calls => stats
                .by_time_agent_calls
                .get(a)
                .map(|v| v.iter().cloned().fold(0u64, u64::max) as f64 / bucket_hours)
                .unwrap_or(0.0),
        }
    };
    let mut active_agents: Vec<Agent> = match unit {
        DashboardUnit::Dollars => stats.by_time_agent.keys().copied().collect(),
        DashboardUnit::Calls => stats.by_time_agent_calls.keys().copied().collect(),
    };
    if active_agents.len() >= 2 {
        // Sort agents by peak descending so the biggest is first (most salient),
        // smallest at the bottom (right next to shared X-axis labels).
        active_agents.sort_by(|a, b| {
            peak_for(b)
                .partial_cmp(&peak_for(a))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let agents = active_agents;
        // Unify Y-axis label width across all mini-charts. Without this,
        // panels with bigger peaks ("120.30" — 6 chars) reserve a wider
        // left gutter than panels with small peaks ("0.01" — 4 chars), so
        // the chart plot areas start at different X positions and the
        // shared X timeline labels at the bottom no longer line up with
        // the data columns above.
        let y_label_width = agents
            .iter()
            .map(|a| format!("{:.2}", peak_for(a).max(0.0001)).len())
            .max()
            .unwrap_or(4);
        let n = agents.len() as u32;
        let constraints: Vec<Constraint> =
            (0..n).map(|_| Constraint::Ratio(1, n)).collect();
        let strips = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(inner);
        for (i, agent) in agents.iter().enumerate() {
            let strip = strips[i];
            let is_last = i + 1 == agents.len();
            draw_agent_strip(
                f, strip, *agent, stats, unit, bucket_hours, x_max, first_ts, mid_ts,
                last_ts, is_last, y_label_width,
            );
        }
        return;
    }

    // Single aggregate line (original behavior, no inner block).
    let agg_points: Vec<(f64, f64)> = match unit {
        DashboardUnit::Dollars => stats
            .by_time
            .iter()
            .enumerate()
            .map(|(i, (_, v))| (i as f64, *v))
            .collect(),
        DashboardUnit::Calls => stats
            .by_time_calls
            .iter()
            .enumerate()
            .map(|(i, (_, c))| (i as f64, *c as f64 / bucket_hours))
            .collect(),
    };
    let y_max = agg_points
        .iter()
        .map(|(_, v)| *v)
        .fold(0.0f64, f64::max)
        .max(0.0001);
    let datasets = vec![Dataset::default()
        .marker(symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Yellow))
        .data(&agg_points)];

    let x_axis = Axis::default()
        .style(Style::default().fg(Color::DarkGray))
        .bounds([0.0, x_max])
        .labels(vec![
            Span::raw(short_ts_label(first_ts)),
            Span::raw(short_ts_label(mid_ts)),
            Span::raw(short_ts_label(last_ts)),
        ]);
    let (y0, ymid, ytop) = match unit {
        DashboardUnit::Dollars => (
            "$0".to_string(),
            format!("${:.2}", y_max / 2.0),
            format!("${:.2}", y_max),
        ),
        DashboardUnit::Calls => (
            "0/h".to_string(),
            format!("{:.2}/h", y_max / 2.0),
            format!("{:.2}/h", y_max),
        ),
    };
    let y_axis = Axis::default()
        .style(Style::default().fg(Color::DarkGray))
        .bounds([0.0, y_max * 1.1])
        .labels(vec![Span::raw(y0), Span::raw(ymid), Span::raw(ytop)]);

    let chart = Chart::new(datasets).x_axis(x_axis).y_axis(y_axis);
    f.render_widget(chart, inner);
}

#[allow(clippy::too_many_arguments)]
fn draw_agent_strip(
    f: &mut ratatui::Frame,
    area: Rect,
    agent: Agent,
    stats: &Stats,
    unit: DashboardUnit,
    bucket_hours: f64,
    x_max: f64,
    first_ts: u64,
    mid_ts: u64,
    last_ts: u64,
    is_last: bool,
    y_label_width: usize,
) {
    let color = agent_color(agent);
    let points: Vec<(f64, f64)> = match unit {
        DashboardUnit::Dollars => stats
            .by_time_agent
            .get(&agent)
            .map(|v| {
                v.iter()
                    .enumerate()
                    .map(|(i, cost)| (i as f64, *cost))
                    .collect()
            })
            .unwrap_or_default(),
        DashboardUnit::Calls => stats
            .by_time_agent_calls
            .get(&agent)
            .map(|v| {
                v.iter()
                    .enumerate()
                    .map(|(i, c)| (i as f64, *c as f64 / bucket_hours))
                    .collect()
            })
            .unwrap_or_default(),
    };
    let peak = points
        .iter()
        .map(|(_, v)| *v)
        .fold(0.0f64, f64::max)
        .max(0.0001);
    let peak_label = match unit {
        DashboardUnit::Dollars => format!("${:.4}", peak),
        DashboardUnit::Calls => format!("{:.2}/h", peak),
    };
    let title_line = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            agent_short(agent),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" · "),
        Span::styled(peak_label, Style::default().fg(Color::Yellow)),
        Span::raw(" "),
    ]);
    let block = Block::default()
        .borders(Borders::TOP)
        .title(title_line)
        .border_style(Style::default().fg(Color::DarkGray));

    let datasets = vec![Dataset::default()
        .marker(symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(color))
        .data(&points)];

    let x_labels = if is_last {
        vec![
            Span::raw(short_ts_label(first_ts)),
            Span::raw(short_ts_label(mid_ts)),
            Span::raw(short_ts_label(last_ts)),
        ]
    } else {
        Vec::new()
    };
    // Force Right alignment: with the default Center alignment ratatui
    // reserves half the first X label's width in the left gutter (see
    // ratatui Chart::max_width_of_labels_left_of_y_axis). On the bottom
    // panel (which has X labels) that would widen the left gutter by
    // ~5-6 chars and break vertical alignment with the panels above.
    let x_axis = Axis::default()
        .style(Style::default().fg(Color::DarkGray))
        .bounds([0.0, x_max])
        .labels_alignment(Alignment::Right)
        .labels(x_labels);
    let w = y_label_width.max(1);
    let y_axis = Axis::default()
        .style(Style::default().fg(Color::DarkGray))
        .bounds([0.0, peak * 1.1])
        .labels(vec![
            Span::raw(format!("{:>w$}", "0", w = w)),
            Span::raw(format!("{:>w$.2}", peak, w = w)),
        ]);

    let chart = Chart::new(datasets)
        .block(block)
        .x_axis(x_axis)
        .y_axis(y_axis);
    f.render_widget(chart, area);
}

fn humanize_secs(s: u64) -> String {
    if s >= 86_400 {
        format!("{}d", s / 86_400)
    } else if s >= 3_600 {
        format!("{}h", s / 3_600)
    } else if s >= 60 {
        format!("{}m", s / 60)
    } else {
        format!("{}s", s)
    }
}

fn short_ts_label(ts: u64) -> String {
    if ts == 0 {
        return String::new();
    }
    let dt: DateTime<Local> = Local
        .timestamp_opt(ts as i64, 0)
        .single()
        .unwrap_or_else(|| Local.timestamp_opt(0, 0).single().unwrap());
    dt.format("%m-%d %H:%M").to_string()
}

#[cfg(test)]
mod transcript_render_tests {
    //! Regression coverage for issue #17 ("按 z 展开/折叠显示乱码").
    //!
    //! Root cause: transcript decorators used East-Asian Ambiguous-Width
    //! characters (U+258C ▌, U+2190 ←, U+2192 →, U+00B7 ·). Under a CJK
    //! locale these render as 2 cells each; `unicode-width`'s default
    //! `width()` reports 1 cell. The mismatch compounds through ratatui
    //! 0.28's `set_stringn` (which has a separate open bug — ratatui
    //! PR #1764 — around multi-width cells), producing visible overlap
    //! and stale-fragment artefacts when the fold state toggles and the
    //! line count changes.
    //!
    //! Fix: decorators use ASCII only. These tests guard against a
    //! regression where someone swaps an ASCII decorator back to a
    //! pretty-but-ambiguous glyph.
    use crate::session::{TranscriptEvent, TranscriptKind};

    const AMBIGUOUS_DECORATORS: &[char] = &[
        '\u{258C}', // ▌ left half block
        '\u{2190}', // ← leftwards arrow
        '\u{2192}', // → rightwards arrow
        '\u{00B7}', // · middle dot
        '\u{2500}', // ─ box drawings
        '\u{2502}', // │ box drawings
    ];

    fn mk_event(kind: TranscriptKind) -> TranscriptEvent {
        TranscriptEvent {
            ts: "2026-04-22T00:00:00Z".to_string(),
            kind,
            body: "body".to_string(),
        }
    }

    fn scan_for_ambiguous(lines: &[ratatui::text::Line<'_>]) -> Vec<(usize, char)> {
        let mut hits = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            for span in &line.spans {
                for c in span.content.chars() {
                    if AMBIGUOUS_DECORATORS.contains(&c) {
                        hits.push((i, c));
                    }
                }
            }
        }
        hits
    }

    #[test]
    fn headers_do_not_use_ambiguous_width_decorators() {
        let kinds = [
            TranscriptKind::User,
            TranscriptKind::Assistant,
            TranscriptKind::Thinking,
            TranscriptKind::ToolUse,
            TranscriptKind::ToolResult,
            TranscriptKind::System,
        ];
        for k in kinds {
            let buf = super::render_one_event(&mk_event(k), 80);
            let hits = scan_for_ambiguous(&buf);
            assert!(
                hits.is_empty(),
                "header for {k:?} contains East-Asian ambiguous-width chars {hits:?} — will corrupt layout under CJK locale (issue #17)"
            );
        }
    }

    #[test]
    fn fold_placeholder_does_not_use_ambiguous_width_decorators() {
        let ev = mk_event(TranscriptKind::ToolResult);
        let line = super::folded_placeholder(&ev, 42);
        let hits = scan_for_ambiguous(std::slice::from_ref(&line));
        assert!(
            hits.is_empty(),
            "fold placeholder contains ambiguous-width chars {hits:?} (issue #17)"
        );
    }

    /// Guard: clamping `u16` scroll against a shrunk row count must make
    /// the new scroll land within `0..=new_max`. This is the invariant
    /// `draw_detail` depends on after `render_detail` recomputes
    /// `detail_row_count` — it's the repro for "End-then-z shows blank
    /// transcript" reported on issue #17's follow-up.
    #[test]
    fn scroll_clamps_to_new_content_length_after_shrink() {
        let prev_scroll: u16 = 2999; // user scrolled to row 2999 of expanded transcript
        let new_row_count: u16 = 200; // z pressed, fold collapsed transcript to 200 rows
        let max_scroll = new_row_count.saturating_sub(1);
        let clamped = prev_scroll.min(max_scroll);
        assert_eq!(clamped, 199);

        // Degenerate case: content is empty → max_scroll underflows to 0.
        let clamped_empty = 100u16.min(0u16.saturating_sub(1));
        assert_eq!(clamped_empty, 0);
    }

    #[test]
    fn transcript_kind_labels_are_ascii_only() {
        for k in [
            TranscriptKind::User,
            TranscriptKind::Assistant,
            TranscriptKind::Thinking,
            TranscriptKind::ToolUse,
            TranscriptKind::ToolResult,
            TranscriptKind::System,
        ] {
            let label = k.label();
            assert!(
                label.is_ascii(),
                "TranscriptKind::{k:?}.label() = {label:?} is not ASCII — will misalign under CJK locale"
            );
        }
    }
}
