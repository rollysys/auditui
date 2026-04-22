use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Clone, Debug)]
pub struct SkillFile {
    pub rel_path: String,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct Skill {
    pub agent: String,
    pub name: String,
    pub path: PathBuf,
    pub description: String,
    pub files: Vec<SkillFile>,
}

fn parse_description(md_path: &Path) -> String {
    let Ok(text) = fs::read_to_string(md_path) else {
        return String::new();
    };
    if !text.starts_with("---") {
        return String::new();
    }
    let rest = &text[3..];
    let Some(end) = rest.find("\n---") else {
        return String::new();
    };
    let front = &rest[..end];
    for line in front.lines() {
        let line = line.trim();
        if line.to_ascii_lowercase().starts_with("description:") {
            let v = line.splitn(2, ':').nth(1).unwrap_or("").trim();
            return v.trim_matches('"').trim_matches('\'').to_string();
        }
    }
    String::new()
}

fn collect_files(skill_dir: &Path) -> Vec<SkillFile> {
    let mut files = Vec::new();
    for entry in WalkDir::new(skill_dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let p = entry.path();
        if entry.file_type().is_symlink() {
            continue;
        }
        if !p.is_file() {
            continue;
        }
        let Ok(rel) = p.strip_prefix(skill_dir) else {
            continue;
        };
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        files.push(SkillFile {
            rel_path: rel.to_string_lossy().to_string(),
            size,
        });
    }
    files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    files
}

fn scan_root(root: &Path, agent: &str, out: &mut Vec<Skill>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    for d in dirs {
        let name = match d.file_name() {
            Some(s) => s.to_string_lossy().to_string(),
            None => continue,
        };
        let mut md = d.join("SKILL.md");
        if !md.is_file() {
            md = d.join("AGENTS.md");
        }
        let description = if md.is_file() {
            parse_description(&md)
        } else {
            String::new()
        };
        let files = collect_files(&d);
        out.push(Skill {
            agent: agent.to_string(),
            name,
            path: d,
            description,
            files,
        });
    }
}

pub fn list() -> Vec<Skill> {
    let mut out = Vec::new();
    let Some(home) = dirs::home_dir() else {
        return out;
    };
    scan_root(&home.join(".claude").join("skills"), "claude", &mut out);
    scan_root(&home.join(".codex").join("skills"), "codex", &mut out);
    scan_root(&home.join(".qwen").join("skills"), "qwen", &mut out);
    out
}

pub fn read_file(path: &Path) -> std::io::Result<String> {
    fs::read_to_string(path)
}
