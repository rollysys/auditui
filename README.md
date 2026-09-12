# auditui

[English](README.md) · [中文](README.zh.md)


**Terminal UI for browsing Claude Code / Codex / oh-my-pi / Qwen coding-agent session logs.**

Read-only transcript access. No hooks or daemon. Parses the transcript files your
agent already writes and provides a single-binary TUI. Audit commands are offline
by default; only an explicitly configured `audit explain --execute` can send a
sanitized analysis payload. The ordinary TUI has an optional update check.

```
┌ Sessions (235 groups · 4534 / 4534) ──────────┐┌ 1ca3c5bd · claude-opus-4-7 · ~/auditit · [3of8 [ ]] ──┐
│ ▶ CLA 04-19 14:03 ~/auditit   [8 sessions]    ││ 2026-04-19 14:03:29  USER                             │
│ ▼ CLA 04-18 19:40 ~/argus     [21 sessions]   ││ > 再加一个功能: Sessions 列表按会话组折叠展示          │
│   └ CLA 04-18 19:40  fix bar chart …          ││                                                        │
│   └ CLA 04-18 18:12  cost by sessions …       ││ 2026-04-19 14:03:32  ASSIS                            │
│   ...                                          ││ 让我先探查一下 transcript 里是否有线索 …             │
│ ▶ COD 04-17 09:22 ~/ArgusV4   [1105 sessions] ││                                                        │
└─────────────────────────────────────────────────┘└────────────────────────────────────────────────────────┘
 ↑/↓ move · Enter open / toggle · Space expand · [ ] group-nav · / search · Tab detail · D dashboard · r · q
```

## Why

Coding agents produce a lot of transcripts — `~/.claude/projects/<cwd>/<session>.jsonl`,
`~/.codex/sessions/...`, `~/.omp/agent/sessions/...`, `~/.qwen/tmp/<cwd>/logs/chats/...`. After a few weeks you have
thousands of them, across dozens of repos, and no good way to:

- Find *that one session* where you figured out the tricky thing
- See how much you've actually spent on tokens this week, broken down by project
- Compare your use of Claude vs. Codex vs. Qwen
- Read a past session without `cat`-ing raw JSONL

`auditui` does that. It's a TUI, not a web app, deliberately:

- **No server, no port, no secrets leaking to your LAN**
- **No hooks** — your agent writes its files; `auditui` only reads them
- **One binary** — works over SSH, on a headless box, inside tmux, wherever
- **Fast** — parallel index, on-disk cache, sub-second reloads after the first scan

## Features

### Sessions view
- Group sessions by `cwd + agent + time gap < 24h` (the natural "I was working on X" unit)
- Expand/collapse groups; single-session groups render as one line
- Full-text search across transcripts (`/`)
- Live transcript preview (user / assistant / tool_use / tool_result / thinking / system)
- Prev/next in group (`[` / `]`)

### Dashboard
- Time ranges: 1h / 4h / 1d / 7d / 30d / all (window-scoped cost, not lifetime)
- Unit toggle (`u`): dollars vs. `calls/hr` — useful for local-LLM usage where dollars don't apply
- Aggregations by agent, by model, by session-group
- Horizontal bar chart (`v`) of top-20 groups by cost/rate
- Line chart of cost-or-calls over time

### Memory / Skills browser
- Browse every `CLAUDE.md`, `AGENTS.md`, `SKILL.md`, and auto-memory file you have
- Grouped by project (latest modified first)
- Markdown rendered with color, bold, lists, code blocks, tables

### Non-goals
- No transcript editing. `auditui` never writes agent transcript directories, including `~/.claude/`, `~/.codex/`, `~/.omp/`, and `~/.qwen/`.
- No sharing / multi-user server. If you want a hosted dashboard, this is not it.
- No automatic analytics upload. Audit analysis stays local unless you explicitly execute an explanation with a configured endpoint. Intervention/outcome records live in a separate user-selected state directory.

## Install

### One-liner (recommended)

```bash
curl -fsSL https://github.com/rollysys/auditui/releases/latest/download/install.sh | bash
```

Detects your platform, downloads the latest release tarball from GitHub, verifies its SHA-256, and installs the `auditui` binary to `~/.local/bin`. Env overrides: `PREFIX=/usr/local/bin`, `TAG=v0.1.0` to pin a version.

> The installer only contacts `github.com` (and the `objects.githubusercontent.com` CDN that release-asset URLs 302-redirect to). It does not hit `raw.githubusercontent.com` or `api.github.com`, which are commonly blocked on corporate networks.

### Prebuilt binaries (manual)

Download from the [GitHub Releases](https://github.com/rollysys/auditui/releases) page. Available targets:

- `aarch64-apple-darwin` — macOS Apple Silicon (M1/M2/M3/M4)
- `x86_64-unknown-linux-musl` — Linux x86_64 (static; runs on any glibc, incl. old distros)

> **Intel Mac (x86_64-apple-darwin)**: build from source with the steps below — Intel runners on GitHub Actions are deprecated and prebuilt artifacts are no longer published.

### From source

```bash
git clone https://github.com/rollysys/auditui
cd auditui
cargo build --release
./target/release/auditui
```

Single ~5 MB static-ish binary; copy it anywhere:

```bash
cp target/release/auditui ~/.local/bin/
```

### Remote deployment and Windows prerequisites

`make deploy-xserver` builds locally, copies workspace build inputs over SSH,
then builds a release and runs `--dry-run` on the remote host. Set `REMOTE`,
`REMOTE_PLATFORM=auto|windows|posix`, `REMOTE_DIR`, and optionally `REMOTE_CARGO`
(an executable path, not a shell command). It does not copy user transcripts.

Native Windows requires **Visual Studio C++ Build Tools and the Windows SDK**,
including an available MSVC `link.exe`, in addition to Rust. The native Windows
deployment path was exercised on xserver, but that host lacks `link.exe`; this
is a host prerequisite failure, not a successful native build or a source-code
build failure. The second-host validation passed on the same xserver in Ubuntu
22.04 WSL with isolated official Rust 1.98.1 and offline vendored dependencies:
workspace tests, release build, and actual audit CLI smoke checks all passed.
The smoke checks covered 31 requests with $0.31 reported cost, no recovery
candidate for a parallel failure, and a candidate for a genuine recovery chain.
That WSL setup is separate: the deployment script does **not** automate WSL
provisioning or execution.

## Usage

```bash
auditui                  # run the TUI

auditui --dry-run        # show session counts (sanity check)
auditui --bench          # time a full dashboard compute across ranges
auditui --memory-dump    # list memory + skills files found
auditui --group-dump     # show session-grouping histogram
auditui --check-update   # check GitHub for a newer release, print result
```

### Audit ledger and improvement tracking

`auditui audit` is separate from the ordinary Sessions/Dashboard TUI. The former
`--audit` entry has been removed. It measures **LLM requests and usage observations**,
not tool-call counts as a proxy for dollar cost:

```bash
auditui audit costs --since 7d
auditui audit candidates --root ./transcripts --since all --json
auditui audit costs --since 2026-09-01T00:00:00Z --until 2026-09-08T00:00:00Z
auditui audit explain --root ./transcripts --since all          # offline preview
auditui audit explain --root ./transcripts --config explain.json # preview + budget quote
auditui audit explain --root ./transcripts --config explain.json --execute
auditui audit record --file intervention.json
auditui audit outcome --file outcome.json
auditui audit compare --intervention reduce-recovery
auditui audit browse --root ./transcripts --since all
make audit ARGS="--since 7d --json"
```

Flags are strict: unknown/duplicate flags, missing values, unknown agents, and
inverted or empty time windows fail rather than broaden the scan. Costs,
candidates, explanation, and browse accept `--root`, `--since 7d|30d|all|RFC3339`
(default `30d`), `--until RFC3339`, `--project PROJECT`, and
`--agent claude,codex,omp`. Project selection is exact. The start is inclusive,
the end exclusive, and selection uses **observation time**, not session modification
time. Unknown timestamps are excluded from bounded totals and diagnosed; `all`
retains them. Machine-readable commands accept `--json`; `browse` requires a terminal.
`compare` accepts root/project/agent selection but **rejects `--since`/`--until`**:
it needs complete recorded task attempts, not a clipped cost window.

**How to read the result.** Provider-reported actual USD, table-estimated USD,
and observations with unknown cost are separate buckets; actual and estimated
amounts are never added and labeled an invoice. Coverage diagnostics show missing
usage/timestamps, unsupported or malformed records, and attribution limitations.
Repeated diagnostics with the same code/message are grouped with an occurrence
count and the first source reference; the underlying ledger diagnostics remain
intact. The displayed diagnostic count is therefore not a raw occurrence count.
Zero detected diagnostics does not prove complete billing. Candidates separate
observed recovery chains, context growth, and repeated multi-operation workflows
from hypotheses. Their related request cost is **not avoidable cost or a savings
claim**, and a tool batch is not an LLM request.

**Supported sources and scan behavior.** The ledger reads Claude JSONL under
`~/.claude/projects`, Codex JSONL under `~/.codex/sessions`, and oh-my-pi JSONL under
`~/.omp/agent/sessions`, including nested children. Qwen/Hermes remain available
in the ordinary viewer but are not ledger providers. `--root` selects a local
source file or tree instead of scanning default homes. A directory can contain
`manifest.json` to enumerate sources and explicitly supply project/task metadata:

```json
{
  "sources": [
    {"path": "before/session.jsonl", "provider": "omp", "project": "demo", "task_id": "task-before", "work_type": "bugfix"},
    {"path": "before/child.jsonl", "provider": "omp", "project": "demo", "parent": "before/session.jsonl", "task_id": "task-before", "work_type": "bugfix"},
    {"path": "after/session.jsonl", "provider": "omp", "project": "demo", "task_id": "task-after", "work_type": "bugfix"}
  ]
}
```

Manifest paths are relative to the root; escaping it is rejected. Audit performs
a **full source-content scan on each invocation or browse refresh**, with a
SHA-256-validated parsed-ledger cache separate from the ordinary timeline cache.
This is not byte-offset incremental indexing. Evidence references contain the
whole-file SHA-256 version; changed files, including same-size changes, must be
rescanned before evidence can be opened.

**Local evidence and privacy.** JSON exports redact credentials and local paths
by default, and never persist tool argument bodies or raw transcript code.
To save an executable local evidence reference, opt in to sensitive path export:

```bash
auditui audit candidates --root ./transcripts --since all --json --include-paths > local-candidates.json
jq '.candidates[0].evidence[0]' local-candidates.json > source-ref.json
auditui audit evidence --file source-ref.json          # sanitized, version checked
auditui audit evidence --file source-ref.json --raw    # explicit local raw record
```

Use a real nonempty candidate/evidence entry. `--include-paths` is available only
on costs/candidates/compare and exposes sensitive filesystem paths; treat those
exports as local private files. It never enables raw bodies or changes the
explanation payload. `--raw` applies only to local evidence viewing. Do not share
raw evidence or assume heuristic redaction makes every user-written secret safe.

**Optional explanation.** Without `--execute`, explanation prints a sanitized
preview and does not read API credentials or make a model request. Optional
`--config PATH` adds endpoint/model/budget validation to the preview. Execution
requires config with the following exact fields:

```json
{
  "endpoint": "http://127.0.0.1:8080/v1/chat/completions",
  "model": "your-local-model",
  "api_key_env": "AUDIT_EXPLAIN_API_KEY",
  "max_cost_usd": 0.05,
  "input_usd_per_million": 1.0,
  "output_usd_per_million": 2.0,
  "max_output_tokens": 1000,
  "allow_remote": false
}
```

Replace the example model and prices with your endpoint's real configuration;
store the key in the named environment variable, never in the JSON. Remote
endpoints require HTTPS and `allow_remote: true` as well as `--execute`. The model
receives sanitized candidate facts and evidence IDs, **not raw source records**;
it cannot execute tools. A conservative input-byte/output-token budget gate runs
before the request. Provider analysis usage/cost and explanation cache are stored
separately from the work ledger. Missing analysis usage is unknown, not free.
Audit commands never trigger the ordinary TUI update checker.
Model prose is an **unverified interpretation**, not ledger truth: schema and
evidence citations are checked, but recommendations and factual prose still need
human review. Never use model-written numbers as measured cost or causal savings.

**Durable PDCA records.** Explain/compare/record/outcome/browse accept
`--state-dir PATH` (default `~/.claude-audit/ledger-state`), separate from the
disposable TUI cache. State paths inside source roots or agent data directories
are refused, including resolved aliases; choose a separate private directory.
`record` and `outcome` import one JSON object each. An intervention has:

```json
{
  "id": "reduce-recovery", "candidate_id": "ID_FROM_CANDIDATES", "project": "demo",
  "work_type": "bugfix", "description": "Use a checked input contract",
  "artifact": "commit or skill reference", "effective_ms": 1788825600000,
  "status": "candidate", "quality_criteria": "Acceptance checks pass without regression"
}
```

Statuses are `candidate`, `confirmed`, `implemented`, `pending_validation`,
`effective`, `ineffective`, and `insufficient_evidence`; the tracker validates
transitions. An outcome records an explicit logical task, **all of its session
attempts including failures and children**, externally checked quality, and
comparison metadata:

```json
{
  "task_id": "task-before", "session_ids": ["LEDGER_SESSION_ID"], "passed": true,
  "quality_notes": "Acceptance checks passed", "model": "observed-model",
  "harness_version": "harness-version", "project_version": "commit-id", "cohort": "before"
}
```

Import corresponding `after` tasks as separate outcomes. Use actual ledger
session IDs from cost JSON, not filenames; manifest task/work-type metadata is
explicit, not guessed from user turns or final assistant prose. Comparison
reports before/after samples, distribution, failed attempts and total cohort
cost per completed task with quality/comparability gates and confounders.
Missing or incomparable evidence yields `insufficient_evidence`, not fabricated
causal savings. Source filters that omit required sessions cannot establish a
complete task comparison.

**Audit browse controls.** `Tab` / `Left` / `Right` select Costs, Candidates, or
Effects; `Up`/`Down` or `j`/`k` select a row (`Home`/`End` jump). `Enter` or `e`
opens version-checked sanitized evidence, `[`/`]` change the evidence reference,
and uppercase `R` explicitly reveals its raw local record. `Esc` returns;
`PgUp`/`PgDn` scroll details, and `j`/`k` scroll an open evidence view. `r` does a
full rescan; `q`/`Ctrl-C` exits. Effects use complete task history in the selected
root/project/agent, independently of the Costs/Candidates time window. The
ordinary TUI and its shortcuts below are unchanged.

### Update check

On startup the TUI spawns a background worker that hits GitHub's `releases/latest` at most once per 24h and shows a small yellow `↑ vX.Y.Z` in the topbar if a newer release is out. The cache lives at `~/.auditui.json`. Set `AUDITUI_NO_UPDATE_CHECK=1` to disable the check entirely (no network, no cache write).

### Keys

| View | Key | Action |
|------|-----|--------|
| global | `S` / `D` / `M` / `K` | Sessions / Dashboard / Memory / Skills |
| global | `f` | cycle agent filter (all / claude / codex / hermes / omp / qwen) |
| global | `p` | toggle scripted-session filter (SDK/headless) |
| global | `r` | re-index sessions + invalidate caches |
| global | `q` / Ctrl-C | quit |
| sessions | `↑`/`↓`, `PgUp`/`PgDn`, `Home`/`End` | move |
| sessions | `Enter` | open session (or toggle group if on header) |
| sessions | `Space` | expand/collapse group at cursor |
| sessions | `[` / `]` | previous / next session in the same group |
| sessions | `Tab` | switch focus between list and detail |
| sessions | `/` | full-text search across transcripts |
| sessions | `z` | toggle fold mode: smart (default — collapse long tool_result / system / thinking) ↔ expand all |
| sessions | `x` | in smart mode, toggle fold of the event under the scroll cursor |
| dashboard | `←` / `→` | change time range |
| dashboard | `u` | toggle unit: `$` ↔ `calls/hr` |
| dashboard | `v` | toggle overview ↔ per-group bar chart |

## Data sources

`auditui` reads from these locations, all read-only:

| Agent | Transcripts | Memory | Skills |
|-------|-------------|--------|--------|
| Claude Code | `~/.claude/projects/<encoded-cwd>/<sid>.jsonl` | `~/.claude/CLAUDE.md`, project `CLAUDE.md`, `.../memory/*.md` | `~/.claude/skills/<name>/SKILL.md` |
| Codex | `~/.codex/sessions/<yyyy>/<mm>/<dd>/rollout-*.jsonl` | `~/.codex/AGENTS.md`, `~/.codex/rules/default.rules` | `~/.codex/skills/<name>/` |
| oh-my-pi | `~/.omp/agent/sessions/<encoded-cwd>/<ts>_<uuid>.jsonl` | `~/.omp/agent/memories/<encoded-cwd>/*.md` (+ `rollout_summaries/`) | `~/.omp/agent/memories/<encoded-cwd>/skills/<name>/` |
| Qwen | `~/.qwen/tmp/<encoded-cwd>/logs/chats/<session>.json` | `~/.qwen/settings.json`, `~/.qwen/output-language.md` | `~/.qwen/skills/<name>/` |

> oh-my-pi (pi-agent) records its own per-message cost inline, so `auditui` uses
> that figure verbatim rather than pricing it from the table below.

## Cache

The ordinary Sessions/Dashboard viewer caches per-session timelines on disk at
`~/.claude-audit/_tui_cache/<agent>/<sid>.bin`, keyed by file size.
The audit ledger instead reads complete source contents and validates its parsed
cache under `~/.claude-audit/_tui_cache/ledger` by SHA-256 before reuse; same-size
edits cannot reuse a stale version. This is not byte-offset incremental indexing.
Disposable caches can be removed to force reparsing. They contain local source
identities/paths and should be treated as private files, not shareable exports.
Durable interventions/outcomes and explanation spend/cache live under
`--state-dir`; clearing `_tui_cache` does not erase them.

## Status

Pre-1.0, fast-moving. Works on macOS + Linux (x86_64 + aarch64). Tested with:

- Claude Code (all recent versions)
- Codex
- Qwen Code

## License

MIT — see `LICENSE`.
