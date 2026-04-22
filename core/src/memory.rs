use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct MemoryFile {
    pub category: String,
    pub path: PathBuf,
    pub size: u64,
    pub mtime: u64,
}

#[derive(Clone, Debug)]
pub struct MemoryProject {
    pub name: String,
    pub cwd: String,
    pub encoded: String,
    pub files: Vec<MemoryFile>,
    pub latest_mtime: u64,
    pub agent: String,
}

#[derive(Clone, Debug)]
pub struct MemoryIndex {
    pub global: Option<MemoryFile>,
    pub projects: Vec<MemoryProject>,
}

fn stat_entry(path: &Path, category: &str) -> Option<MemoryFile> {
    let md = fs::metadata(path).ok()?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Some(MemoryFile {
        category: category.to_string(),
        path: path.to_path_buf(),
        size: md.len(),
        mtime,
    })
}

fn resolve_project_cwd(project_dir: &Path) -> Option<String> {
    let entries = fs::read_dir(project_dir).ok()?;
    let mut jsonls: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            if let Ok(md) = p.metadata() {
                if let Ok(mt) = md.modified() {
                    jsonls.push((p, mt));
                }
            }
        }
    }
    jsonls.sort_by(|a, b| b.1.cmp(&a.1));
    for (p, _) in jsonls.iter().take(3) {
        let Ok(f) = File::open(p) else { continue };
        let r = BufReader::new(f);
        for ln in r.lines().map_while(Result::ok) {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&ln) else {
                continue;
            };
            if let Some(cwd) = v.get("cwd").and_then(|x| x.as_str()) {
                return Some(cwd.to_string());
            }
        }
    }
    None
}

fn collect_project_files(
    cwd: &Path,
    encoded_dir: &Path,
    global_path: Option<&Path>,
) -> Vec<MemoryFile> {
    let mut files = Vec::new();
    let proj_md = cwd.join("CLAUDE.md");
    if proj_md.is_file() {
        if let Some(f) = stat_entry(&proj_md, "CLAUDE.md") {
            files.push(f);
        }
    }
    let local_md = cwd.join(".claude").join("CLAUDE.md");
    if local_md.is_file() {
        let is_global_alias = global_path
            .and_then(|g| g.canonicalize().ok())
            .zip(local_md.canonicalize().ok())
            .map(|(g, l)| g == l)
            .unwrap_or(false);
        if !is_global_alias {
            if let Some(f) = stat_entry(&local_md, ".claude/CLAUDE.md") {
                files.push(f);
            }
        }
    }
    let memory_dir = encoded_dir.join("memory");
    if memory_dir.is_dir() {
        if let Ok(entries) = fs::read_dir(&memory_dir) {
            let mut mds: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md"))
                .collect();
            mds.sort();
            for p in mds {
                if let Some(f) = stat_entry(&p, "auto-memory") {
                    files.push(f);
                }
            }
        }
    }
    files
}

pub fn build() -> MemoryIndex {
    let Some(home) = dirs::home_dir() else {
        return MemoryIndex {
            global: None,
            projects: vec![],
        };
    };
    let global_md = home.join(".claude").join("CLAUDE.md");
    let global = if global_md.is_file() {
        stat_entry(&global_md, "global")
    } else {
        None
    };

    let mut projects = Vec::new();
    let projects_dir = home.join(".claude").join("projects");
    if let Ok(entries) = fs::read_dir(&projects_dir) {
        for e in entries.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let cwd_str = match resolve_project_cwd(&p) {
                Some(c) => c,
                None => continue,
            };
            let cwd_path = PathBuf::from(&cwd_str);
            let files = collect_project_files(&cwd_path, &p, Some(&global_md));
            if files.is_empty() {
                continue;
            }
            let latest_mtime = files.iter().map(|f| f.mtime).max().unwrap_or(0);
            let name = cwd_path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| cwd_str.clone());
            projects.push(MemoryProject {
                name,
                cwd: cwd_str,
                encoded: p
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default(),
                files,
                latest_mtime,
                agent: "claude".to_string(),
            });
        }
    }

    let codex_home = home.join(".codex");
    let mut codex_files = Vec::new();
    for (p, cat) in [
        (codex_home.join("AGENTS.md"), "AGENTS.md"),
        (codex_home.join("rules").join("default.rules"), "default.rules"),
    ] {
        if p.is_file() {
            if let Some(f) = stat_entry(&p, cat) {
                codex_files.push(f);
            }
        }
    }
    if !codex_files.is_empty() {
        let latest_mtime = codex_files.iter().map(|f| f.mtime).max().unwrap_or(0);
        projects.push(MemoryProject {
            name: "codex (global)".to_string(),
            cwd: codex_home.display().to_string(),
            encoded: "_codex_global".to_string(),
            files: codex_files,
            latest_mtime,
            agent: "codex".to_string(),
        });
    }

    let qwen_home = home.join(".qwen");
    let mut qwen_files = Vec::new();
    for (p, cat) in [
        (qwen_home.join("settings.json"), "settings.json"),
        (qwen_home.join("output-language.md"), "output-language.md"),
    ] {
        if p.is_file() {
            if let Some(f) = stat_entry(&p, cat) {
                qwen_files.push(f);
            }
        }
    }
    if !qwen_files.is_empty() {
        let latest_mtime = qwen_files.iter().map(|f| f.mtime).max().unwrap_or(0);
        projects.push(MemoryProject {
            name: "qwen (global)".to_string(),
            cwd: qwen_home.display().to_string(),
            encoded: "_qwen_global".to_string(),
            files: qwen_files,
            latest_mtime,
            agent: "qwen".to_string(),
        });
    }

    projects.sort_by(|a, b| {
        b.latest_mtime
            .cmp(&a.latest_mtime)
            .then(a.encoded.cmp(&b.encoded))
    });

    MemoryIndex { global, projects }
}

pub fn read_file(path: &Path) -> std::io::Result<String> {
    fs::read_to_string(path)
}
