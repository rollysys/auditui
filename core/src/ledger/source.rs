//! Recursive, read-only transcript loading and version-checked evidence access.
//!
//! Every load hashes the complete source. The separate parsed-ledger cache avoids
//! parsing unchanged content; it is not a byte-offset/incremental JSONL parser.
//! Appends, truncation and same-size rewrites all invalidate the cached ledger.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use super::{fingerprint, parse, redact, Diagnostic, Ledger, Query, SourceRef};
use crate::providers::Agent;

// Bump whenever parser semantics or serialized ledger fields change.
const PARSED_CACHE_VERSION: u32 = 3;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SourceSpec {
    provider: Option<Agent>,
    project: Option<String>,
    parent: Option<PathBuf>,
    task_id: Option<String>,
    work_type: Option<String>,
    // More-specific manifest entries override recursively inherited metadata.
    priority: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    sources: Vec<ManifestSource>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestSource {
    path: PathBuf,
    provider: Agent,
    project: Option<String>,
    parent: Option<PathBuf>,
    task_id: Option<String>,
    work_type: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct ParsedCache {
    schema: u32,
    source_id: String,
    version: String,
    pricing_version: String,
    ledger: Ledger,
}

fn provider_name(provider: Agent) -> &'static str {
    match provider {
        Agent::Claude => "claude",
        Agent::Codex => "codex",
        Agent::Omp => "omp",
        Agent::Hermes => "hermes",
        Agent::Qwen => "qwen",
    }
}

fn supported(provider: Agent) -> bool {
    matches!(provider, Agent::Claude | Agent::Codex | Agent::Omp)
}

fn source_ref(path: &Path, provider: Option<Agent>, version: String) -> SourceRef {
    SourceRef {
        source_id: format!(
            "{}:{}",
            provider.map(provider_name).unwrap_or("unknown"),
            fingerprint(path.as_os_str().as_encoded_bytes())
        ),
        version,
        path: path.to_owned(),
        line: 1,
        record_id: None,
    }
}

fn diagnostic(ledger: &mut Ledger, code: &str, message: &str, source: Option<SourceRef>) {
    ledger.diagnostics.push(Diagnostic {
        code: code.into(),
        message: message.into(),
        source,
    });
}

fn relative_source(root: &Path, relative: &Path) -> Result<PathBuf> {
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative.components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("manifest paths must be relative and cannot traverse outside the source root");
    }
    let path = root
        .join(relative)
        .canonicalize()
        .context("resolve manifest source")?;
    if !path.starts_with(root) {
        bail!("manifest source resolves outside the source root");
    }
    Ok(path)
}

fn insert_source(
    sources: &mut BTreeMap<PathBuf, SourceSpec>,
    path: PathBuf,
    spec: &SourceSpec,
    ledger: &mut Ledger,
) -> Result<()> {
    if let Some(previous) = sources.get(&path) {
        if previous.priority < spec.priority {
            sources.insert(path, spec.clone());
            return Ok(());
        }
        if previous.priority > spec.priority {
            return Ok(());
        }
        if previous != spec {
            bail!("equally specific source entries have conflicting metadata");
        }
        diagnostic(
            ledger,
            "duplicate_source_path",
            "Repeated source entry was loaded only once.",
            Some(source_ref(&path, spec.provider, String::new())),
        );
    } else {
        sources.insert(path, spec.clone());
    }
    Ok(())
}

fn discover(
    path: &Path,
    boundary: &Path,
    spec: &SourceSpec,
    sources: &mut BTreeMap<PathBuf, SourceSpec>,
    ledger: &mut Ledger,
) -> Result<()> {
    if path.is_file() {
        if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            insert_source(sources, path.to_owned(), spec, ledger)?;
            // Native omp and Claude child layouts both live below <parent-stem>/.
            let children = path.with_extension("");
            if children.is_dir() {
                let children = children
                    .canonicalize()
                    .context("resolve child source directory")?;
                if !children.starts_with(boundary) {
                    bail!("child source directory resolves outside source root");
                }
                let mut child_spec = spec.clone();
                child_spec.parent = None;
                discover(&children, boundary, &child_spec, sources, ledger)?;
            }
        } else {
            diagnostic(
                ledger,
                "unsupported_source",
                "Only JSONL transcript sources are supported.",
                Some(source_ref(path, spec.provider, String::new())),
            );
        }
        return Ok(());
    }
    for entry in WalkDir::new(path).follow_links(false).sort_by_file_name() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                diagnostic(
                    ledger,
                    "source_traversal_error",
                    "A source directory entry could not be read.",
                    None,
                );
                continue;
            }
        };
        if entry.file_type().is_symlink() {
            diagnostic(
                ledger,
                "source_symlink_skipped",
                "Recursive discovery does not follow symbolic links.",
                Some(source_ref(entry.path(), spec.provider, String::new())),
            );
            continue;
        }
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(|s| s.to_str()) != Some("jsonl")
        {
            continue;
        }
        let canonical = entry
            .path()
            .canonicalize()
            .context("resolve discovered source")?;
        if !canonical.starts_with(boundary) {
            bail!("discovered source resolves outside source root");
        }
        insert_source(sources, canonical, spec, ledger)?;
    }
    Ok(())
}

fn collect_sources(
    query: &Query,
    ledger: &mut Ledger,
) -> Result<(BTreeMap<PathBuf, SourceSpec>, Vec<PathBuf>)> {
    let mut sources = BTreeMap::new();
    let mut boundaries = Vec::new();
    if let Some(root) = &query.root {
        let root = root.canonicalize().context("resolve source root")?;
        let boundary = if root.is_dir() {
            root.clone()
        } else {
            root.parent()
                .context("source has no parent directory")?
                .to_owned()
        };
        boundaries.push(boundary.clone());
        let manifest_path = if root.is_dir() {
            root.join("manifest.json")
        } else {
            root.clone()
        };
        if manifest_path.file_name().and_then(|s| s.to_str()) == Some("manifest.json")
            && manifest_path.exists()
        {
            let manifest_path = manifest_path
                .canonicalize()
                .context("resolve source manifest")?;
            if !manifest_path.starts_with(&boundary) {
                bail!("manifest resolves outside the source root");
            }
            let manifest: Manifest = serde_json::from_reader(File::open(manifest_path)?)
                .context("decode source manifest")?;
            for entry in manifest.sources {
                if !supported(entry.provider) {
                    bail!("manifest provider is not supported by the ledger");
                }
                let path = relative_source(&boundary, &entry.path)?;
                let parent = entry
                    .parent
                    .as_deref()
                    .map(|p| relative_source(&boundary, p))
                    .transpose()?;
                if parent.as_ref().is_some_and(|p| !p.is_file()) {
                    bail!("manifest parent must name a transcript file");
                }
                let spec = SourceSpec {
                    provider: Some(entry.provider),
                    project: entry.project,
                    parent,
                    task_id: entry.task_id,
                    work_type: entry.work_type,
                    priority: path.components().count(),
                };
                discover(&path, &boundary, &spec, &mut sources, ledger)?;
            }
        } else {
            discover(
                &root,
                &boundary,
                &SourceSpec::default(),
                &mut sources,
                ledger,
            )?;
        }
    } else if let Some(home) = dirs::home_dir() {
        for (relative, provider) in [
            (".claude/projects", Agent::Claude),
            (".codex/sessions", Agent::Codex),
            (".omp/agent/sessions", Agent::Omp),
        ] {
            let root = home.join(relative);
            if !root.exists() {
                continue;
            }
            let root = root.canonicalize().context("resolve native source root")?;
            boundaries.push(root.clone());
            discover(
                &root,
                &root,
                &SourceSpec {
                    provider: Some(provider),
                    ..SourceSpec::default()
                },
                &mut sources,
                ledger,
            )?;
        }
    } else {
        diagnostic(
            ledger,
            "missing_home",
            "Cannot discover native sources without a home directory.",
            None,
        );
    }
    Ok((sources, boundaries))
}

fn content_version(path: &Path) -> Result<String> {
    let mut reader =
        BufReader::new(File::open(path).context("open source for content verification")?);
    let mut hash = Sha256::new();
    loop {
        let bytes = reader.fill_buf()?;
        if bytes.is_empty() {
            break;
        }
        hash.update(bytes);
        let length = bytes.len();
        reader.consume(length);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn record_provider(value: &Value) -> Option<Agent> {
    let kind = value.get("type").and_then(Value::as_str)?;
    match kind {
        "session"
            if value.get("id").is_some()
                && (value.get("version").is_some() || value.get("cwd").is_some()) =>
        {
            Some(Agent::Omp)
        }
        "message" if value.pointer("/message/role").is_some() => Some(Agent::Omp),
        "session_meta" if value.get("payload").is_some() => Some(Agent::Codex),
        "response_item" | "event_msg" | "turn_context" if value.get("payload").is_some() => {
            Some(Agent::Codex)
        }
        "assistant" | "user" | "system"
            if value.get("sessionId").is_some() || value.get("uuid").is_some() =>
        {
            Some(Agent::Claude)
        }
        "assistant" | "user"
            if value.pointer("/message/role").and_then(Value::as_str) == Some(kind) =>
        {
            Some(Agent::Claude)
        }
        _ => None,
    }
}

fn detect_provider(path: &Path) -> Result<Option<Agent>> {
    // Probe metadata/early records only; parsing still consumes the entire file.
    // A bounded probe avoids parsing huge payloads twice solely for discovery.
    let reader = BufReader::new(File::open(path)?.take(2 * 1024 * 1024));
    for line in reader.split(b'\n').take(256) {
        let line = line?;
        if let Ok(value) = serde_json::from_slice::<Value>(&line) {
            if let Some(provider) = record_provider(&value) {
                return Ok(Some(provider));
            }
        }
    }
    Ok(None)
}

fn cache_location(cache: &Path, boundaries: &[PathBuf]) -> Option<PathBuf> {
    // Resolve existing ancestors before creating anything: even a symlinked
    // cache parent must never redirect our writes into the read-only sources.
    let mut ancestor = cache;
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        suffix.push(ancestor.file_name()?.to_owned());
        ancestor = ancestor.parent()?;
    }
    let mut resolved = ancestor.canonicalize().ok()?;
    for part in suffix.into_iter().rev() {
        resolved.push(part);
    }
    if boundaries.iter().any(|root| resolved.starts_with(root)) {
        None
    } else {
        Some(resolved)
    }
}

fn cached_parse(
    path: &Path,
    provider: Agent,
    reference: &SourceRef,
    cache_dir: Option<&Path>,
    ledger: &mut Ledger,
) -> Result<Ledger> {
    let cache_path = cache_dir.map(|dir| {
        dir.join(format!(
            "{}.bin",
            fingerprint(reference.source_id.as_bytes())
        ))
    });
    if let Some(cache_path) = &cache_path {
        // No size/mtime shortcut: caller has already hashed all current bytes.
        if let Ok(bytes) = fs::read(cache_path) {
            match bincode::deserialize::<ParsedCache>(&bytes) {
                Ok(cached)
                    if cached.schema == PARSED_CACHE_VERSION
                        && cached.source_id == reference.source_id
                        && cached.version == reference.version
                        && cached.pricing_version == crate::cost::PRICING_VERSION =>
                {
                    return Ok(cached.ledger);
                }
                Ok(_) => (),
                Err(_) => diagnostic(
                    ledger,
                    "invalid_parsed_cache",
                    "An invalid parsed cache entry was ignored and rebuilt.",
                    Some(reference.clone()),
                ),
            }
        }
    }
    let parsed = parse::parse_file(path, provider, None, None)?;
    if parsed
        .sessions
        .iter()
        .any(|session| session.source.version != reference.version)
        || content_version(path)? != reference.version
    {
        bail!("source changed during parsing; retry after the writer settles");
    }
    if let Some(cache_path) = cache_path {
        let cached = ParsedCache {
            schema: PARSED_CACHE_VERSION,
            source_id: reference.source_id.clone(),
            version: reference.version.clone(),
            pricing_version: crate::cost::PRICING_VERSION.into(),
            ledger: parsed,
        };
        if save_cache(&cache_path, &cached).is_err() {
            diagnostic(
                ledger,
                "parsed_cache_write_failed",
                "The parsed ledger is usable but its separate cache could not be written.",
                Some(reference.clone()),
            );
        }
        return Ok(cached.ledger);
    }
    Ok(parsed)
}

fn save_cache(path: &Path, cache: &ParsedCache) -> Result<()> {
    let parent = path.parent().context("cache has no parent")?;
    fs::create_dir_all(parent)?;
    let temp = path.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    let result = (|| -> Result<()> {
        bincode::serialize_into(&mut file, cache)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn inferred_parent(path: &Path, paths: &BTreeMap<PathBuf, SourceSpec>) -> Option<PathBuf> {
    for directory in path.ancestors().skip(1) {
        let mut name = directory.as_os_str().to_owned();
        name.push(".jsonl");
        let candidate = PathBuf::from(name);
        if candidate != path && paths.contains_key(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn merge(target: &mut Ledger, mut source: Ledger) {
    target.sessions.append(&mut source.sessions);
    target.requests.append(&mut source.requests);
    target.observations.append(&mut source.observations);
    target.tools.append(&mut source.tools);
    target.contexts.append(&mut source.contexts);
    target.diagnostics.append(&mut source.diagnostics);
}

/// Load full histories, including children, before any event-time filtering.
/// Source-content duplicates are excluded once across providers/projects, with
/// a diagnostic: copying a transcript must not multiply its billed usage.
pub fn load(query: &Query) -> Result<Ledger> {
    let cache = crate::cache::cache_root().map(|root| root.join("ledger"));
    load_at(query, cache.as_deref())
}

fn load_at(query: &Query, cache: Option<&Path>) -> Result<Ledger> {
    let mut ledger = Ledger::default();
    let (sources, boundaries) = collect_sources(query, &mut ledger)?;
    let cache = cache.and_then(|cache| cache_location(cache, &boundaries));
    let mut versions: HashMap<String, (String, SourceSpec)> = HashMap::new();
    let mut session_paths: HashMap<PathBuf, String> = HashMap::new();
    for (path, spec) in &sources {
        let version = match content_version(path) {
            Ok(version) => version,
            Err(_) => {
                diagnostic(
                    &mut ledger,
                    "source_read_error",
                    "A transcript could not be read.",
                    Some(source_ref(path, spec.provider, String::new())),
                );
                continue;
            }
        };
        let provider = match detect_provider(path) {
            Ok(Some(provider)) => provider,
            Ok(None) => {
                diagnostic(
                    &mut ledger,
                    "unsupported_schema",
                    "No supported transcript schema was recognized in the bounded metadata probe.",
                    Some(source_ref(path, spec.provider, version)),
                );
                continue;
            }
            Err(_) => {
                diagnostic(
                    &mut ledger,
                    "source_read_error",
                    "Transcript schema detection could not read the source.",
                    Some(source_ref(path, spec.provider, version)),
                );
                continue;
            }
        };
        let reference = source_ref(path, Some(provider), version.clone());
        if spec.provider.is_some_and(|expected| expected != provider) {
            diagnostic(&mut ledger, "provider_mismatch", "Detected transcript schema differs from its declared source provider; source excluded.", Some(reference));
            continue;
        }
        if !query.agents.is_empty() && !query.agents.contains(&provider) {
            continue;
        }
        if let Some((id, original_spec)) = versions.get(&version) {
            session_paths.insert(path.clone(), id.clone());
            diagnostic(
                &mut ledger,
                "duplicate_source_content",
                "An exact transcript copy was excluded to avoid counting its usage twice.",
                Some(reference.clone()),
            );
            if (
                original_spec.provider,
                &original_spec.project,
                &original_spec.parent,
                &original_spec.task_id,
                &original_spec.work_type,
            ) != (
                spec.provider,
                &spec.project,
                &spec.parent,
                &spec.task_id,
                &spec.work_type,
            ) {
                diagnostic(&mut ledger, "duplicate_source_metadata_conflict", "Exact transcript copies have conflicting attribution; the first canonical path's metadata is retained.", Some(reference));
            }
            continue;
        }
        match cached_parse(path, provider, &reference, cache.as_deref(), &mut ledger) {
            Ok(mut parsed) => {
                if let Some(first) = parsed.sessions.first() {
                    session_paths.insert(path.clone(), first.id.clone());
                    versions.insert(version, (first.id.clone(), spec.clone()));
                }
                for session in &mut parsed.sessions {
                    if let Some(project) = &spec.project {
                        session.project.clone_from(project);
                    }
                    if let Some(task) = &spec.task_id {
                        session.task_id = Some(task.clone());
                    }
                    if let Some(work_type) = &spec.work_type {
                        session.work_type = Some(work_type.clone());
                    }
                }
                merge(&mut ledger, parsed);
            }
            Err(_) => diagnostic(
                &mut ledger,
                "source_parse_error",
                "A transcript could not be parsed consistently; no usage from it was included.",
                Some(reference),
            ),
        }
    }
    for session in &mut ledger.sessions {
        let Some(spec) = sources.get(&session.source.path) else {
            continue;
        };
        let parent_path = spec
            .parent
            .clone()
            .or_else(|| inferred_parent(&session.source.path, &sources));
        if let Some(parent_path) = parent_path {
            if let Some(parent_id) = session_paths.get(&parent_path) {
                if parent_id == &session.id && spec.parent.is_some() {
                    bail!("a source cannot explicitly declare itself as its parent");
                }
                if parent_id != &session.id {
                    session.parent_id = Some(parent_id.clone());
                }
            } else {
                ledger.diagnostics.push(Diagnostic {
                    code: "missing_parent_source".into(),
                    message:
                        "A declared or discovered parent was not available in the loaded sources."
                            .into(),
                    source: Some(session.source.clone()),
                });
            }
        }
    }
    let parents: HashMap<_, _> = ledger
        .sessions
        .iter()
        .map(|s| (s.id.as_str(), s.parent_id.as_deref()))
        .collect();
    for session in &ledger.sessions {
        let mut visited = HashSet::new();
        let mut current = Some(session.id.as_str());
        while let Some(id) = current {
            if !visited.insert(id) {
                bail!("source parent relationships contain a cycle");
            }
            current = parents.get(id).copied().flatten();
        }
    }
    // An explicit mapping wins; missing child attribution follows the nearest
    // ancestor that actually supplies it, independent of traversal order.
    let sessions: HashMap<_, _> = ledger.sessions.iter().map(|s| (s.id.as_str(), s)).collect();
    let inherited: Vec<_> = ledger
        .sessions
        .iter()
        .map(|session| {
            let mut project = None;
            let mut task = None;
            let mut work_type = None;
            let mut parent = session.parent_id.as_deref();
            while let Some(ancestor) = parent.and_then(|id| sessions.get(id)) {
                if session.project.is_empty() && project.is_none() && !ancestor.project.is_empty() {
                    project = Some(ancestor.project.clone());
                }
                if session.task_id.is_none() && task.is_none() {
                    task = ancestor.task_id.clone();
                }
                if session.work_type.is_none() && work_type.is_none() {
                    work_type = ancestor.work_type.clone();
                }
                parent = ancestor.parent_id.as_deref();
            }
            (project, task, work_type)
        })
        .collect();
    for (session, (project, task, work_type)) in ledger.sessions.iter_mut().zip(inherited) {
        if let Some(project) = project {
            session.project = project;
            ledger.diagnostics.push(Diagnostic {
                code: "inherited_project_missing_cwd".into(),
                message: "Child project attribution was inherited; relative tool objects remain unresolved without an observed execution directory.".into(),
                source: Some(session.source.clone()),
            });
        }
        if let Some(task) = task {
            session.task_id = Some(task);
        }
        if let Some(work_type) = work_type {
            session.work_type = Some(work_type);
        }
    }
    let duplicates = ledger
        .diagnostics
        .iter()
        .filter(|d| d.code == "duplicate_source_content")
        .count();
    let errors = ledger
        .diagnostics
        .iter()
        .filter(|d| {
            matches!(
                d.code.as_str(),
                "source_read_error"
                    | "source_parse_error"
                    | "source_traversal_error"
                    | "provider_mismatch"
            )
        })
        .count();
    let unsupported = ledger
        .diagnostics
        .iter()
        .filter(|d| matches!(d.code.as_str(), "unsupported_schema" | "unsupported_source"))
        .count();
    let coverage = format!("Discovered {} distinct JSONL paths; loaded {} distinct source versions before project/time filtering; excluded {} exact copies; {} source errors; {} unsupported sources. Schema probes inspect at most 256 records / 2 MiB; every selected source is fully content-hashed.", sources.len(), versions.len(), duplicates, errors, unsupported);
    diagnostic(&mut ledger, "source_coverage", &coverage, None);
    if let Some(project) = &query.project {
        ledger
            .sessions
            .retain(|session| &session.project == project);
        let selected: HashSet<_> = ledger.sessions.iter().map(|s| s.id.as_str()).collect();
        ledger
            .requests
            .retain(|request| selected.contains(request.session_id.as_str()));
        ledger
            .observations
            .retain(|observation| selected.contains(observation.session_id.as_str()));
        ledger
            .tools
            .retain(|tool| selected.contains(tool.session_id.as_str()));
        ledger
            .contexts
            .retain(|event| selected.contains(event.session_id.as_str()));
    }
    Ok(ledger)
}

fn evidence_line(reference: &SourceRef) -> Result<Vec<u8>> {
    if reference.line == 0 {
        bail!("evidence line numbers start at one");
    }
    let mut reader = BufReader::new(File::open(&reference.path).context("open evidence source")?);
    let mut hash = Sha256::new();
    let mut line = Vec::new();
    let mut selected = None;
    let mut number = 0;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        hash.update(&line);
        number += 1;
        if number == reference.line {
            selected = Some(line.clone());
        }
    }
    if format!("{:x}", hash.finalize()) != reference.version {
        bail!("stale evidence: source content changed; reload the ledger before inspecting this reference");
    }
    selected.context("evidence line does not exist in the verified source")
}

/// Default evidence is a structural summary, never raw prompts, tool arguments,
/// result text or code. Even credential-free code is omitted rather than merely
/// passed through a secret-pattern scrubber.
pub fn read_evidence(reference: &SourceRef) -> Result<String> {
    let line = evidence_line(reference)?;
    let value: Value =
        serde_json::from_slice(&line).context("evidence record is not valid JSON")?;
    let kind = match value.get("type").and_then(Value::as_str) {
        Some(
            kind @ ("session" | "session_meta" | "assistant" | "user" | "system" | "message"
            | "response_item" | "event_msg" | "turn_context" | "compaction"
            | "model_change"),
        ) => kind,
        _ => "other",
    };
    let role = match value
        .pointer("/message/role")
        .or_else(|| value.pointer("/payload/role"))
        .and_then(Value::as_str)
    {
        Some(role @ ("assistant" | "user" | "system" | "developer" | "tool" | "toolResult")) => {
            Some(role)
        }
        _ => None,
    };
    Ok(redact(&serde_json::to_string_pretty(&serde_json::json!({
        "version": reference.version,
        "line": reference.line,
        "record_type": kind,
        "role": role,
        "record_bytes": line.len(),
        "content": "omitted by default; explicit local raw evidence access is required"
    }))?))
}

/// Explicit local opt-in. Never called by export/explanation paths by default.
pub fn read_evidence_raw(reference: &SourceRef) -> Result<String> {
    let line = evidence_line(reference)?;
    Ok(String::from_utf8(line)
        .context("evidence is not UTF-8")?
        .trim_end_matches(['\r', '\n'])
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "auditui-ledger-source-{}-{}",
                std::process::id(),
                TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn put(&self, name: &str, text: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, text).unwrap();
            path
        }
        fn query(&self) -> Query {
            Query {
                root: Some(self.0.clone()),
                ..Query::default()
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn omp(id: &str, input: u64) -> String {
        format!(
            "{}\n{}\n",
            serde_json::json!({"type":"session","version":3,"id":id,"cwd":"/project"}),
            serde_json::json!({"type":"message","id":"entry","timestamp":"2026-09-01T00:00:00Z","message":{"role":"assistant","model":"test-model","content":[{"type":"text","text":"private_source_code sk-sensitive-token"}],"usage":{"input":input,"output":1,"cost":{"total":0.1}}}})
        )
    }
    fn claude(id: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({"type":"assistant","uuid":"message","sessionId":id,"cwd":"/project","timestamp":"2026-09-01T00:00:00Z","message":{"id":"same-provider-message-id","role":"assistant","model":"test-model","content":[],"usage":{"input_tokens":3,"output_tokens":1}}})
        )
    }

    #[test]
    fn recursively_includes_both_native_child_layouts() {
        let fixture = Fixture::new();
        fixture.put("omp.jsonl", &omp("omp-root", 1));
        fixture.put("omp/Child.jsonl", &omp("omp-child", 2));
        fixture.put("omp/Child/Grandchild.jsonl", &omp("omp-grandchild", 3));
        fixture.put("claude.jsonl", &claude("claude-root"));
        fixture.put("claude/subagents/agent-a.jsonl", &claude("claude-child"));
        let ledger = load_at(&fixture.query(), None).unwrap();
        assert_eq!(ledger.sessions.len(), 5);
        let by_name = |name: &str| {
            ledger
                .sessions
                .iter()
                .find(|s| s.source.path.ends_with(name))
                .unwrap()
        };
        assert_eq!(
            by_name("omp/Child.jsonl").parent_id.as_deref(),
            Some(by_name("omp.jsonl").id.as_str())
        );
        assert_eq!(
            by_name("Grandchild.jsonl").parent_id.as_deref(),
            Some(by_name("omp/Child.jsonl").id.as_str())
        );
        assert_eq!(
            by_name("agent-a.jsonl").parent_id.as_deref(),
            Some(by_name("claude.jsonl").id.as_str())
        );
    }

    #[test]
    fn exact_source_copies_never_double_bill() {
        let fixture = Fixture::new();
        let text = omp("copy", 7);
        fixture.put("a.jsonl", &text);
        fixture.put("nested/copy.jsonl", &text);
        let ledger = load_at(&fixture.query(), None).unwrap();
        assert_eq!(ledger.observations.len(), 1);
        assert_eq!(ledger.sessions.len(), 1);
        assert!(ledger
            .diagnostics
            .iter()
            .any(|d| d.code == "duplicate_source_content"));
    }

    #[test]
    fn same_size_rewrite_invalidates_cache_and_old_evidence() {
        let fixture = Fixture::new();
        let cache = Fixture::new();
        let path = fixture.put("session.jsonl", &omp("same", 7));
        let first = load_at(&fixture.query(), Some(&cache.0)).unwrap();
        let reference = first.observations[0].source.clone();
        let second_text = omp("same", 9);
        assert_eq!(fs::metadata(&path).unwrap().len(), second_text.len() as u64);
        fs::write(&path, second_text).unwrap();
        let second = load_at(&fixture.query(), Some(&cache.0)).unwrap();
        assert_eq!(first.sessions[0].id, second.sessions[0].id);
        assert_ne!(reference.version, second.observations[0].source.version);
        assert_eq!(second.observations[0].usage.input_tokens, 9);
        assert!(read_evidence_raw(&reference)
            .unwrap_err()
            .to_string()
            .contains("stale evidence"));
    }

    #[test]
    fn append_and_truncation_rebuild_whole_source() {
        let fixture = Fixture::new();
        let cache = Fixture::new();
        let text = omp("growing", 2);
        let path = fixture.put("session.jsonl", &text);
        let original = load_at(&fixture.query(), Some(&cache.0)).unwrap();
        fs::write(&path, format!("{text}{{\"type\":\"message\"")).unwrap();
        let partial = load_at(&fixture.query(), Some(&cache.0)).unwrap();
        assert_eq!(partial.observations.len(), 1);
        assert_ne!(
            partial.sessions[0].source.version,
            original.sessions[0].source.version
        );
        fs::write(&path, text.lines().next().unwrap()).unwrap();
        let truncated = load_at(&fixture.query(), Some(&cache.0)).unwrap();
        assert!(truncated.observations.is_empty());
        assert!(read_evidence(&original.observations[0].source).is_err());
    }

    #[test]
    fn manifest_rejects_parent_path_escape() {
        let fixture = Fixture::new();
        fixture.put(
            "manifest.json",
            r#"{"sources":[{"path":"../outside.jsonl","provider":"omp"}]}"#,
        );
        assert!(load_at(&fixture.query(), None)
            .unwrap_err()
            .to_string()
            .contains("cannot traverse"));
    }

    #[cfg(unix)]
    #[test]
    fn manifest_rejects_symlink_escape() {
        let fixture = Fixture::new();
        let outside = Fixture::new();
        let path = outside.put("outside.jsonl", &omp("outside", 2));
        std::os::unix::fs::symlink(path, fixture.0.join("link.jsonl")).unwrap();
        fixture.put(
            "manifest.json",
            r#"{"sources":[{"path":"link.jsonl","provider":"omp"}]}"#,
        );
        assert!(load_at(&fixture.query(), None)
            .unwrap_err()
            .to_string()
            .contains("outside"));
    }

    #[test]
    fn manifest_imports_explicit_task_and_work_type_for_children() {
        let fixture = Fixture::new();
        fixture.put("parent.jsonl", &omp("parent", 2));
        fixture.put("parent/child.jsonl", &omp("child", 3));
        fixture.put("manifest.json", r#"{"sources":[{"path":"parent.jsonl","provider":"omp","project":"explicit-project","task_id":"task-1","work_type":"bugfix"}]}"#);
        let mut query = fixture.query();
        query.project = Some("explicit-project".into());
        query.since_ms = Some(i64::MAX);
        let ledger = load_at(&query, None).unwrap();
        assert_eq!(ledger.sessions.len(), 2);
        assert_eq!(
            ledger.observations.len(),
            2,
            "load must not discard pre-window history"
        );
        for session in &ledger.sessions {
            assert_eq!(session.task_id.as_deref(), Some("task-1"));
            assert_eq!(session.work_type.as_deref(), Some("bugfix"));
        }
    }

    #[test]
    fn duplicate_provider_ids_remain_source_namespaced() {
        let fixture = Fixture::new();
        fixture.put("a.jsonl", &omp("same-provider-id", 1));
        fixture.put("b.jsonl", &omp("same-provider-id", 2));
        let ledger = load_at(&fixture.query(), None).unwrap();
        assert_eq!(ledger.observations.len(), 2);
        assert_ne!(ledger.sessions[0].id, ledger.sessions[1].id);
        assert_ne!(ledger.observations[0].id, ledger.observations[1].id);
    }

    #[test]
    fn default_evidence_omits_raw_content_and_paths() {
        let fixture = Fixture::new();
        fixture.put("session.jsonl", &omp("private", 1));
        let ledger = load_at(&fixture.query(), None).unwrap();
        let reference = &ledger.observations[0].source;
        let safe = read_evidence(reference).unwrap();
        assert!(!safe.contains("private_source_code"));
        assert!(!safe.contains("sk-sensitive-token"));
        assert!(!safe.contains("/project"));
        assert!(read_evidence_raw(reference)
            .unwrap()
            .contains("private_source_code"));
    }

    #[test]
    fn missing_child_project_inherits_before_project_filter() {
        let fixture = Fixture::new();
        fixture.put("parent.v2.jsonl", &omp("parent", 1));
        let message = omp("child", 2).lines().nth(1).unwrap().to_owned();
        fixture.put(
            "parent.v2/child.jsonl",
            &format!(
                "{}\n{message}\n",
                serde_json::json!({"type":"session","version":3,"id":"child"})
            ),
        );
        let mut query = fixture.query();
        query.project = Some("/project".into());
        let ledger = load_at(&query, None).unwrap();
        assert_eq!(ledger.observations.len(), 2);
        assert_eq!(
            ledger
                .sessions
                .iter()
                .filter(|s| s.parent_id.is_some())
                .count(),
            1
        );
    }

    #[test]
    fn manifest_parent_overrides_directory_inference() {
        let fixture = Fixture::new();
        fixture.put("natural.jsonl", &omp("natural", 1));
        fixture.put("chosen.jsonl", &omp("chosen", 2));
        fixture.put("natural/child.jsonl", &omp("child", 3));
        fixture.put("manifest.json", r#"{"sources":[{"path":"natural.jsonl","provider":"omp"},{"path":"chosen.jsonl","provider":"omp"},{"path":"natural/child.jsonl","provider":"omp","parent":"chosen.jsonl"}]}"#);
        let ledger = load_at(&fixture.query(), None).unwrap();
        let child = ledger
            .sessions
            .iter()
            .find(|s| s.source.path.ends_with("child.jsonl"))
            .unwrap();
        let chosen = ledger
            .sessions
            .iter()
            .find(|s| s.source.path.ends_with("chosen.jsonl"))
            .unwrap();
        assert_eq!(child.parent_id.as_deref(), Some(chosen.id.as_str()));
    }

    #[test]
    fn cache_is_never_written_inside_source_root() {
        let fixture = Fixture::new();
        fixture.put("session.jsonl", &omp("cache-guard", 1));
        let cache = fixture.0.join("must-not-exist");
        let ledger = load_at(&fixture.query(), Some(&cache)).unwrap();
        assert_eq!(ledger.observations.len(), 1);
        assert!(!cache.exists());
    }
}
