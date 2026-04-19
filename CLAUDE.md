# CLAUDE.md

This repository is **auditui** (github.com/rollysys/auditui), the active mainline. A read-only Rust TUI for browsing Claude Code / Codex / Qwen session transcripts.

## Scope

- New features and bug fixes go here.
- The parent repo `/Users/x/auditit/` is **legacy** (Python Web UI + hooks, frozen for security + port-collision reasons). Do not touch it except for archival docs.
- Do not reintroduce hooks, a daemon, or a web server unless explicitly asked.

## Architecture

| File | Role |
|---|---|
| `src/main.rs` | App entry, CLI flag parsing, view switching |
| `src/providers/` | Per-agent transcript discovery + parsing (claude / codex / qwen) |
| `src/session.rs` | Normalized `SessionMeta`, `SessionGroup`, transcript events |
| `src/cache.rs` | On-disk timeline cache (`~/.claude-audit/_tui_cache/<agent>/<sid>.bin`) |
| `src/dashboard.rs` | Time-windowed aggregations, per-range compute |
| `src/memory.rs` / `src/skills.rs` / `src/md.rs` | Memory/skills discovery + markdown rendering |
| `src/tui.rs` | Layout, input handling, detail rendering |
| `src/cost.rs` | Per-model pricing table |

## Common commands

```bash
make release            # cargo build --release (default)
make run                # build + run TUI
make dry-run            # session discovery sanity check
make group-dump         # session-grouping histogram
make bench              # dashboard compute benchmark
make memory-dump        # print memory + skills index
make deploy-xserver     # rsync src + rebuild on xserver
```

## Guardrails

- **Read-only** against `~/.claude/`, `~/.codex/`, `~/.qwen/`. Never write or delete session transcripts under those trees.
- When Claude Code runtime behavior is in question, **check `~/claude-code` source** instead of guessing (Read limit, `--bare`, `stream-json`, settings merge, etc.).
- **Two-host validation**: any Rust change must build + smoke on both Mac and xserver before declaring done. Use `make deploy-xserver`.
- **Keep the worktree small** — commit incremental units immediately (see the global `~/.claude/CLAUDE.md` Git Hygiene rule).
- Prefer fixing parser heuristics with **concrete transcript fixtures**, not assumptions.
- Pricing / context-window tables in `src/cost.rs` must be updated manually when a new model appears upstream.

## Data sources

| Agent | Transcript | Memory | Skills |
|---|---|---|---|
| Claude Code | `~/.claude/projects/<encoded-cwd>/<sid>.jsonl` | `~/.claude/CLAUDE.md`, project `CLAUDE.md`, `.../memory/*.md` | `~/.claude/skills/<name>/SKILL.md` |
| Codex | `~/.codex/sessions/<yyyy>/<mm>/<dd>/rollout-*.jsonl` | `~/.codex/AGENTS.md`, `~/.codex/rules/default.rules` | `~/.codex/skills/<name>/` |
| Qwen | `~/.qwen/tmp/<encoded-cwd>/logs/chats/<session>.json` | `~/.qwen/settings.json`, `~/.qwen/output-language.md` | `~/.qwen/skills/<name>/` |

## Session classification

- `entrypoint=sdk-cli` → scripted (reliable signal for claude-mem observer, SDK calls, headless rollouts)
- `entrypoint=cli` → interactive (may include some `claude -p` we cannot distinguish without hooks)
- `permissionMode=bypassPermissions` → soft scripted signal; use alongside entrypoint, not alone (interactive users sometimes run with `--dangerously-skip-permissions`)

## Release

Tag push triggers `.github/workflows/release.yml`:

```bash
git tag v0.1.1 && git push origin v0.1.1
```

Matrix-builds macOS x86_64/aarch64 + Linux x86_64, publishes tarballs + SHA256 to GitHub Release.
