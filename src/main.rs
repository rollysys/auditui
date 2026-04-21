mod cache;
mod cost;
mod dashboard;
mod md;
mod memory;
mod providers;
mod session;
mod skills;
mod tui;
mod update;

use anyhow::Result;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--dry-run") {
        let list = session::index_all();
        println!("indexed {} sessions", list.len());
        let mut by_agent = std::collections::BTreeMap::new();
        let mut scripted_by_agent = std::collections::BTreeMap::new();
        for s in &list {
            *by_agent.entry(s.agent.short()).or_insert(0usize) += 1;
            if s.is_scripted {
                *scripted_by_agent.entry(s.agent.short()).or_insert(0usize) += 1;
            }
        }
        for (k, v) in &by_agent {
            let scr = scripted_by_agent.get(k).copied().unwrap_or(0);
            println!("  {k}: {v}  (scripted={scr})");
        }
        for s in list.iter().take(5) {
            let prompt = s.prompt.as_deref().unwrap_or("");
            let prompt: String = prompt.chars().take(60).collect();
            println!("  {} {} turns={} {}", s.agent.short(), s.id, s.turns, prompt);
        }
        return Ok(());
    }
    if args.iter().any(|a| a == "--bench") {
        return bench();
    }
    if let Some(pos) = args.iter().position(|a| a == "--md-dump") {
        let Some(path) = args.get(pos + 1) else {
            eprintln!("usage: --md-dump <path>");
            std::process::exit(2);
        };
        let text = std::fs::read_to_string(path)?;
        let lines = md::to_lines(&text);
        for (i, l) in lines.iter().enumerate() {
            let flat: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            println!("{:3}: {}", i, flat);
        }
        return Ok(());
    }
    if args.iter().any(|a| a == "--group-dump") {
        let sessions = session::index_all();
        let t = std::time::Instant::now();
        let groups = session::group_sessions(&sessions, session::DEFAULT_GROUP_GAP_SECS);
        println!(
            "sessions={} groups={} (gap={}s) elapsed={}ms",
            sessions.len(),
            groups.len(),
            session::DEFAULT_GROUP_GAP_SECS,
            t.elapsed().as_millis()
        );
        let mut hist = std::collections::BTreeMap::new();
        for g in &groups {
            *hist.entry(g.members.len()).or_insert(0usize) += 1;
        }
        println!("group size histogram:");
        for (sz, n) in &hist {
            println!("  size={:3} → {} groups", sz, n);
        }
        println!("\ntop 10 largest groups:");
        let mut sorted = groups.clone();
        sorted.sort_by_key(|g| std::cmp::Reverse(g.members.len()));
        for g in sorted.iter().take(10) {
            println!(
                "  {:3} sess  {}  {}",
                g.members.len(),
                g.key,
                g.cwd.as_deref().unwrap_or("-")
            );
        }
        return Ok(());
    }
    if args.iter().any(|a| a == "--memory-dump") {
        let idx = memory::build();
        if let Some(g) = &idx.global {
            println!("global: {}  ({} bytes)", g.path.display(), g.size);
        }
        println!("projects: {}", idx.projects.len());
        for p in idx.projects.iter().take(10) {
            println!("  [{}] {} ({}) — {} files", p.agent, p.name, p.cwd, p.files.len());
        }
        let sk = skills::list();
        println!("skills: {}", sk.len());
        for s in sk.iter().take(10) {
            println!("  [{}] {} — {}", s.agent, s.name, s.description);
        }
        return Ok(());
    }
    if args.iter().any(|a| a == "--check-update") {
        let current = env!("CARGO_PKG_VERSION");
        match update::check_latest(current) {
            update::UpdateStatus::Available { current_version, latest_version } => {
                println!("auditui {current_version} → update available: {latest_version}");
                println!("upgrade: curl -fsSL https://raw.githubusercontent.com/rollysys/auditui/main/install.sh | bash");
            }
            update::UpdateStatus::UpToDate { current_version, .. } => {
                println!("auditui {current_version} is up to date");
            }
            update::UpdateStatus::Failed => eprintln!("auditui {current} update check failed"),
            update::UpdateStatus::Unchecked | update::UpdateStatus::Checking => {
                eprintln!("auditui {current} update check did not complete");
            }
        }
        return Ok(());
    }
    let refresh_secs: u64 = args
        .iter()
        .position(|a| a == "--refresh")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let update_state = update::UpdateState::new();
    update::spawn_check(update_state.clone());
    tui::run(refresh_secs, update_state)
}

fn bench() -> Result<()> {
    use std::sync::Arc;
    use std::time::Instant;
    let t0 = Instant::now();
    let sessions = session::index_all();
    println!("index_all: {} sessions in {:.2}s", sessions.len(), t0.elapsed().as_secs_f32());

    let cache = Arc::new(cache::CacheStore::new());

    for (i, range) in [
        dashboard::Range::D1,
        dashboard::Range::D1,
        dashboard::Range::D7,
        dashboard::Range::D30,
        dashboard::Range::All,
    ].iter().enumerate() {
        let t = Instant::now();
        let s = dashboard::compute(&sessions, &cache, *range);
        println!(
            "pass {} range={} compute={}ms total_cost=${:.2} sessions={} cache_size={}",
            i + 1,
            s.range,
            t.elapsed().as_millis(),
            s.total_cost,
            s.total_sessions,
            cache.len()
        );
    }
    Ok(())
}
