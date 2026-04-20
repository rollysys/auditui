#!/usr/bin/env bash
# auditui install script — downloads the latest prebuilt binary for your
# platform from GitHub Releases, verifies the sha256, and installs to a
# PATH-reachable directory.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/rollysys/auditui/main/install.sh | bash
#
# Environment overrides:
#   PREFIX   install directory (default: $HOME/.local/bin)
#   TAG      specific release tag (default: latest)
#   REPO     owner/name (default: rollysys/auditui)

set -euo pipefail

REPO="${REPO:-rollysys/auditui}"
PREFIX="${PREFIX:-$HOME/.local/bin}"
TAG="${TAG:-}"
BIN="auditui"

red()   { printf '\033[31m%s\033[0m\n' "$1" >&2; }
green() { printf '\033[32m%s\033[0m\n' "$1"; }
blue()  { printf '\033[34m%s\033[0m\n' "$1"; }

have() { command -v "$1" >/dev/null 2>&1; }

have curl || { red "curl is required but not found in PATH"; exit 1; }

os=$(uname -s)
arch=$(uname -m)
case "$os-$arch" in
    Darwin-arm64|Darwin-aarch64)
        target="aarch64-apple-darwin" ;;
    Linux-x86_64|Linux-amd64)
        target="x86_64-unknown-linux-gnu" ;;
    Darwin-x86_64)
        red "Intel Mac (Darwin x86_64) — prebuilt binary not published."
        red "Please build from source:"
        red "  git clone https://github.com/$REPO && cd auditui && cargo build --release"
        exit 1 ;;
    Linux-aarch64|Linux-arm64)
        red "Linux aarch64 — prebuilt binary not published."
        red "Please build from source:"
        red "  git clone https://github.com/$REPO && cd auditui && cargo build --release"
        exit 1 ;;
    *)
        red "Unsupported platform: $os $arch"
        exit 1 ;;
esac

blue "[auditui] platform: $target"

if [ -z "$TAG" ]; then
    TAG=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
            | grep '"tag_name"' \
            | head -1 \
            | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')
    [ -n "$TAG" ] || { red "could not resolve latest tag from GitHub API"; exit 1; }
fi
blue "[auditui] tag:      $TAG"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

stem="${BIN}-${TAG}-${target}"
base_url="https://github.com/$REPO/releases/download/${TAG}"
tarball_url="${base_url}/${stem}.tar.gz"
sha_url="${tarball_url}.sha256"

blue "[auditui] download:  $tarball_url"
curl -fsSL -o "$tmp/${stem}.tar.gz"        "$tarball_url"
curl -fsSL -o "$tmp/${stem}.tar.gz.sha256" "$sha_url"

blue "[auditui] verify sha256"
cd "$tmp"
if have shasum; then
    shasum -a 256 -c "${stem}.tar.gz.sha256" >/dev/null
elif have sha256sum; then
    sha256sum -c "${stem}.tar.gz.sha256" >/dev/null
else
    red "neither shasum nor sha256sum available; cannot verify download"
    exit 1
fi

blue "[auditui] extract + install → $PREFIX/$BIN"
tar -xzf "${stem}.tar.gz"
mkdir -p "$PREFIX"
install -m 0755 "${stem}/${BIN}" "$PREFIX/$BIN"

green "[auditui] installed $BIN $TAG → $PREFIX/$BIN"

# PATH hint
case ":$PATH:" in
    *":$PREFIX:"*) ;;
    *)
        echo ""
        blue "note: $PREFIX is not on your PATH. Add this to your shell rc:"
        echo "    export PATH=\"$PREFIX:\$PATH\""
        ;;
esac

echo ""
"$PREFIX/$BIN" --dry-run 2>&1 | head -1 | sed 's/^/[auditui] smoke: /' || true
