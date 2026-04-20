# auditui

[English](README.md) · [中文](README.zh.md)


**Terminal UI for browsing Claude Code / Codex / Qwen coding-agent session logs.**

Read-only. No hooks. No daemon. No network. Parses the transcript files your agent
already writes, indexes them in parallel, caches aggregations on disk, and gives you
a single-binary TUI to look at what actually happened.

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
`~/.codex/sessions/...`, `~/.qwen/tmp/<cwd>/logs/chats/...`. After a few weeks you have
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
- No editing. `auditui` will never write to `~/.claude/`, `~/.codex/`, `~/.qwen/`.
- No sharing / multi-user. If you want a dashboard other people can see, this is not it.
- No analytics upload. Nothing leaves your machine.

## Install

### One-liner (recommended)

```bash
curl -fsSL https://raw.githubusercontent.com/rollysys/auditui/main/install.sh | bash
```

Detects your platform, downloads the latest release tarball from GitHub, verifies its SHA-256, and installs the `auditui` binary to `~/.local/bin`. Env overrides: `PREFIX=/usr/local/bin`, `TAG=v0.1.0` to pin a version.

### Prebuilt binaries (manual)

Download from the [GitHub Releases](https://github.com/rollysys/auditui/releases) page. Available targets:

- `aarch64-apple-darwin` — macOS Apple Silicon (M1/M2/M3/M4)
- `x86_64-unknown-linux-gnu` — Linux x86_64

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

## Usage

```bash
auditui                  # run the TUI

auditui --dry-run        # show session counts (sanity check)
auditui --bench          # time a full dashboard compute across ranges
auditui --memory-dump    # list memory + skills files found
auditui --group-dump     # show session-grouping histogram
```

### Keys

| View | Key | Action |
|------|-----|--------|
| global | `S` / `D` / `M` / `K` | Sessions / Dashboard / Memory / Skills |
| global | `f` | cycle agent filter (all / claude / codex / qwen) |
| global | `p` | toggle scripted-session filter (SDK/headless) |
| global | `r` | re-index sessions + invalidate caches |
| global | `q` / Ctrl-C | quit |
| sessions | `↑`/`↓`, `PgUp`/`PgDn`, `Home`/`End` | move |
| sessions | `Enter` | open session (or toggle group if on header) |
| sessions | `Space` | expand/collapse group at cursor |
| sessions | `[` / `]` | previous / next session in the same group |
| sessions | `Tab` | switch focus between list and detail |
| sessions | `/` | full-text search across transcripts |
| dashboard | `←` / `→` | change time range |
| dashboard | `u` | toggle unit: `$` ↔ `calls/hr` |
| dashboard | `v` | toggle overview ↔ per-group bar chart |

## Data sources

`auditui` reads from these locations, all read-only:

| Agent | Transcripts | Memory | Skills |
|-------|-------------|--------|--------|
| Claude Code | `~/.claude/projects/<encoded-cwd>/<sid>.jsonl` | `~/.claude/CLAUDE.md`, project `CLAUDE.md`, `.../memory/*.md` | `~/.claude/skills/<name>/SKILL.md` |
| Codex | `~/.codex/sessions/<yyyy>/<mm>/<dd>/rollout-*.jsonl` | `~/.codex/AGENTS.md`, `~/.codex/rules/default.rules` | `~/.codex/skills/<name>/` |
| Qwen | `~/.qwen/tmp/<encoded-cwd>/logs/chats/<session>.json` | `~/.qwen/settings.json`, `~/.qwen/output-language.md` | `~/.qwen/skills/<name>/` |

## Cache

`auditui` caches per-session timelines on disk at `~/.claude-audit/_tui_cache/<agent>/<sid>.bin`.
Keyed by file size; changes are detected automatically. Delete the directory to force a rebuild.

## Status

Pre-1.0, fast-moving. Works on macOS + Linux (x86_64 + aarch64). Tested with:

- Claude Code (all recent versions)
- Codex
- Qwen Code

## License

MIT — see `LICENSE`.
