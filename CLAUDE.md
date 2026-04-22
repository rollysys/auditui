# CLAUDE.md

This repository is **auditui** (github.com/rollysys/auditui), the active mainline. A read-only Rust TUI for browsing Claude Code / Codex / Qwen session transcripts.

## Scope

- New features and bug fixes go here.
- The parent repo `/Users/x/auditit/` is **legacy** (Python Web UI + hooks, frozen for security + port-collision reasons). Do not touch it except for archival docs.
- Do not reintroduce hooks, a daemon, or a web server unless explicitly asked.

## Architecture

Cargo workspace with two crates:

- **`core/`** (`auditui-core` lib) — shared data layer. Any future web frontend (e.g. a future launcher UI) depends on this.
- **`tui/`** (`auditui` bin) — the Ratatui terminal UI. Depends on `core`.

| File | Crate | Role |
|---|---|---|
| `tui/src/main.rs` | tui | App entry, CLI flag parsing, view switching |
| `tui/src/tui.rs` | tui | Layout, input handling, detail rendering |
| `tui/src/md.rs` | tui | Markdown → `ratatui::Line/Span` (TUI-only) |
| `tui/src/update.rs` | tui | Background self-update check against GitHub releases |
| `core/src/providers/` | core | Per-agent transcript discovery + parsing (claude / codex / hermes / qwen) |
| `core/src/session.rs` | core | Normalized `SessionMeta`, `SessionGroup`, transcript events |
| `core/src/cache.rs` | core | On-disk timeline cache (`~/.claude-audit/_tui_cache/<agent>/<sid>.bin`) |
| `core/src/dashboard.rs` | core | Time-windowed aggregations, per-range compute |
| `core/src/memory.rs` / `core/src/skills.rs` | core | Memory / skills discovery |
| `core/src/cost.rs` | core | Per-model pricing table |

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

## Contribution flow

For any change Claude (or any AI assistant) makes:

1. **No direct push to `main`.** Cut a feature branch, e.g. `feat/...`, `fix/...`, `docs/...`, `release/...`.
2. **Open a PR via `gh pr create --base main`.** The repo owner (`rollysys`) reviews and merges on GitHub. The merge commit attributes the change to the human reviewer, the branch commits keep AI-author + human-coauthor.
3. **Keep the `Co-Authored-By: Claude <noreply@anthropic.com>` trailer.** Honest attribution of who actually wrote the diff. The repo owner remains the GitHub author of the merge.
4. **One PR per coherent change.** Multiple thematic commits inside a PR are fine (and preferred over one giant commit), but mixing unrelated topics in one PR is not.

Direct `git push origin main` is reserved for the repo owner only (e.g., emergency hotfix, repo-wide rename).

## Release

Tag push triggers `.github/workflows/release.yml`:

```bash
git tag v0.1.1 && git push origin v0.1.1
```

Matrix-builds macOS x86_64/aarch64 + Linux x86_64, publishes tarballs + SHA256 to GitHub Release.
