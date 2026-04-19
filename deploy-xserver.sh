#!/usr/bin/env bash
# Sync auditui source to xserver (local LAN Linux host) and rebuild in place.
# Usage: ./deploy-xserver.sh
# Env overrides: REMOTE=<host> REMOTE_DIR=<path> REMOTE_CARGO=<cargo-path>

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REMOTE="${REMOTE:-xserver}"
# Values below are passed verbatim to rsync/ssh; the remote shell expands ~.
REMOTE_DIR="${REMOTE_DIR:-auditit-tui}"
REMOTE_CARGO="${REMOTE_CARGO:-~/.cargo/bin/cargo}"

echo "[deploy] rsync $ROOT/ → $REMOTE:$REMOTE_DIR/"
rsync -avz --delete \
	--exclude target \
	--exclude .git \
	--exclude .DS_Store \
	"$ROOT/" "$REMOTE:$REMOTE_DIR/"

echo "[deploy] ssh $REMOTE: cargo build --release"
ssh "$REMOTE" "cd $REMOTE_DIR && $REMOTE_CARGO build --release"

echo "[deploy] ssh $REMOTE: remove stale 'auditit' binary if present"
ssh "$REMOTE" "cd $REMOTE_DIR && rm -f target/release/auditit target/release/auditit.d"

echo "[deploy] ssh $REMOTE: smoke via --dry-run"
ssh "$REMOTE" "cd $REMOTE_DIR && ./target/release/auditui --dry-run | head -3"

echo "[deploy] done"
