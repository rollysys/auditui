#!/usr/bin/env python3
"""Resolve a Claude Code session id (or unique prefix) to its transcript path.

Use cases:
  - You have a session id from an error / dashboard / chat and need the file.
  - You want a ready-to-paste "please audit this session" block for another agent.

Claude Code transcript layout is:
  ~/.claude/projects/<encoded-cwd>/<session_id>.jsonl
Sub-agents are:
  ~/.claude/projects/<encoded-cwd>/<parent_sid>/subagents/agent-<id>.jsonl

Field name is `session_id` (snake_case), NOT `sessionId`.
"""
from __future__ import annotations

import argparse
import sys
from pathlib import Path


def sid_from_name(name: str) -> str | None:
    if name.endswith(".jsonl.gz"):
        return name[:-9]
    if name.endswith(".jsonl"):
        return name[:-6]
    return None


def iter_transcripts(root: Path):
    """Yield (sid, path) for every transcript directly under <root>/<project>/."""
    if not root.exists():
        return
    for proj in root.iterdir():
        if not proj.is_dir():
            continue
        # main session files
        for child in proj.iterdir():
            sid = sid_from_name(child.name) if child.is_file() else None
            if sid:
                yield sid, child
        # sub-agents: <proj>/<parent_sid>/subagents/agent-*.jsonl
        for parent_dir in proj.iterdir():
            subagents_dir = parent_dir / "subagents" if parent_dir.is_dir() else None
            if subagents_dir and subagents_dir.is_dir():
                for agent_f in subagents_dir.iterdir():
                    sid = sid_from_name(agent_f.name)
                    if sid and sid.startswith("agent-"):
                        yield sid, agent_f


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("sid", help="full session id or unique prefix")
    parser.add_argument(
        "--paste",
        action="store_true",
        help="emit a ready-to-paste audit handoff block instead of just the path",
    )
    parser.add_argument(
        "--all",
        action="store_true",
        help="when the prefix is ambiguous, list all matches instead of erroring",
    )
    args = parser.parse_args()

    root = Path.home() / ".claude" / "projects"
    matches = [
        (sid, path) for sid, path in iter_transcripts(root) if sid.startswith(args.sid)
    ]

    if not matches:
        print(f"no match: {args.sid}", file=sys.stderr)
        return 1

    if len(matches) > 1 and not args.all:
        for sid, path in sorted(matches):
            print(f"{sid}\t{path}")
        print(
            f"\nambiguous prefix ({len(matches)} matches); pass a longer sid prefix, "
            "or add --all to accept all matches.",
            file=sys.stderr,
        )
        return 2

    if args.all:
        for sid, path in sorted(matches):
            if args.paste:
                print(f"session_id: {sid}\ntranscript: {path}\n---")
            else:
                print(path)
        return 0

    sid, path = matches[0]

    if args.paste:
        print(
            "\n".join(
                [
                    "请审计以下 Claude Code session:",
                    f"transcript: {path}",
                    f"session_id: {sid}",
                    "",
                    "先 Read 整个 jsonl,然后按用户需要做具体分析。",
                ]
            )
        )
    else:
        print(path)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
