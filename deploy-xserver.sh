#!/usr/bin/env bash
# Copy workspace build inputs to xserver, build release, and smoke via --dry-run.
# Env: REMOTE=xserver REMOTE_PLATFORM=auto|windows|posix REMOTE_DIR=<path>
#      REMOTE_CARGO=<executable path, not a shell command>
# Relative remote directories are under the login home. Windows defaults to
# auditui-ledger-validation; POSIX retains auditit-tui. No remote tree is deleted.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REMOTE="${REMOTE:-xserver}"
REMOTE_PLATFORM="${REMOTE_PLATFORM:-auto}"
REMOTE_CARGO="${REMOTE_CARGO:-}"

fail() { echo "[deploy] error: $*" >&2; exit 1; }
quote_posix() { printf "'%s'" "${1//\'/\'\\\'\'}"; }
quote_powershell() { printf "'%s'" "${1//\'/\'\'}"; }
encode_powershell() { iconv -f UTF-8 -t UTF-16LE | base64 | tr -d '\r\n'; }

case "$REMOTE_PLATFORM" in
	auto)
		# Try a Windows-native command first: never run rsync/a Linux shell on a
		# Windows host. A failed SSH probe is not evidence that the host is POSIX.
		if probe="$(ssh "$REMOTE" 'powershell.exe -NoProfile -NonInteractive -Command "Write-Output AUDITUI_WINDOWS"' 2>/dev/null)" && [[ "$probe" == *AUDITUI_WINDOWS* ]]; then
			REMOTE_PLATFORM=windows
		elif probe="$(ssh "$REMOTE" 'uname -s' 2>/dev/null)" && [[ "$probe" == *Linux* || "$probe" == *Darwin* || "$probe" == *BSD* || "$probe" == *SunOS* ]]; then
			REMOTE_PLATFORM=posix
		else
			fail "cannot detect remote platform; check SSH connectivity or set REMOTE_PLATFORM=windows|posix"
		fi
		;;
	windows|posix) ;;
	*) fail "REMOTE_PLATFORM must be auto, windows, or posix" ;;
esac

if [[ "$REMOTE_PLATFORM" == windows ]]; then
	REMOTE_DIR="${REMOTE_DIR:-auditui-ledger-validation}"
else
	REMOTE_DIR="${REMOTE_DIR:-auditit-tui}"
fi

# Whitelist workspace inputs, not the checkout or the user's session trees.
# Whole crate directories retain build scripts, tests, and embedded fixtures.
inputs=(Cargo.toml Cargo.lock core tui)
for optional in README.md .cargo rust-toolchain rust-toolchain.toml; do
	[[ ! -e "$ROOT/$optional" ]] || inputs+=("$optional")
done
for input in Cargo.toml Cargo.lock core/Cargo.toml tui/Cargo.toml; do
	[[ -f "$ROOT/$input" ]] || fail "missing workspace input: $input"
done

work="$(mktemp -d "${TMPDIR:-/tmp}/auditui-deploy.XXXXXXXX")"
archive_name="${work##*/}.tar.gz"
archive="$work/$archive_name"
remote_cleanup=''
upload_started=false
cleanup() {
	status=$?
	trap - EXIT
	if [[ "$upload_started" == true && -n "$remote_cleanup" ]]; then
		ssh "$REMOTE" "$remote_cleanup" >/dev/null 2>&1 || echo "[deploy] warning: remote temporary archive may remain: $archive_name" >&2
	fi
	rm -rf -- "$work"
	exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

COPYFILE_DISABLE=1 tar -czf "$archive" --exclude=target --exclude=.git --exclude=.DS_Store -C "$ROOT" "${inputs[@]}"

if [[ "$REMOTE_PLATFORM" == windows ]]; then
	# Only an encoded script crosses the SSH default-shell boundary. Values are
	# PowerShell literals, so spaces, apostrophes and shell metacharacters survive.
	ps_dir="$(quote_powershell "$REMOTE_DIR")"
	ps_cargo="$(quote_powershell "$REMOTE_CARGO")"
	ps_archive="$(quote_powershell "$archive_name")"
	remote_cleanup="powershell.exe -NoProfile -NonInteractive -EncodedCommand $(printf '%s' "\$ErrorActionPreference = 'Stop'; \$p = Join-Path \$env:USERPROFILE $ps_archive; if (Test-Path -LiteralPath \$p) { Remove-Item -LiteralPath \$p -Force }" | encode_powershell)"
	remote_script="$(cat <<POWERSHELL
\$ErrorActionPreference = 'Stop'
\$archive = Join-Path \$env:USERPROFILE $ps_archive
\$destination = $ps_dir
\$cargo = $ps_cargo
try {
    if (\$destination -eq '~') {
        \$destination = \$env:USERPROFILE
    } elseif (\$destination.StartsWith('~/') -or \$destination.StartsWith('~\')) {
        \$destination = Join-Path \$env:USERPROFILE \$destination.Substring(2)
    } elseif (-not [IO.Path]::IsPathRooted(\$destination)) {
        \$destination = Join-Path \$env:USERPROFILE \$destination
    }
    if (-not \$cargo) {
        \$cargo = Join-Path \$env:USERPROFILE '.cargo\bin\cargo.exe'
    } elseif (\$cargo.StartsWith('~/') -or \$cargo.StartsWith('~\')) {
        \$cargo = Join-Path \$env:USERPROFILE \$cargo.Substring(2)
    }
    New-Item -ItemType Directory -Path \$destination -Force | Out-Null
    Write-Host "[deploy] extracting workspace to \$destination"
    & tar.exe -xzf \$archive -C \$destination
    if (\$LASTEXITCODE -ne 0) { throw "tar extraction failed (exit \$LASTEXITCODE)" }
    Set-Location -LiteralPath \$destination
    Write-Host '[deploy] cargo build --release'
    & \$cargo build --release
    if (\$LASTEXITCODE -ne 0) { throw "cargo release build failed (exit \$LASTEXITCODE)" }
    Write-Host '[deploy] smoke via --dry-run'
    & (Join-Path \$destination 'target\release\auditui.exe') --dry-run
    if (\$LASTEXITCODE -ne 0) { throw "auditui --dry-run failed (exit \$LASTEXITCODE)" }
} catch {
    [Console]::Error.WriteLine('[deploy] error: ' + \$_.Exception.Message)
    exit 1
} finally {
    Remove-Item -LiteralPath \$archive -Force -ErrorAction SilentlyContinue
}
POWERSHELL
)"
	remote_command="powershell.exe -NoProfile -NonInteractive -EncodedCommand $(printf '%s' "$remote_script" | encode_powershell)"
else
	sh_dir="$(quote_posix "$REMOTE_DIR")"
	sh_cargo="$(quote_posix "$REMOTE_CARGO")"
	sh_archive="$(quote_posix "$archive_name")"
	remote_cleanup="sh -c $(quote_posix "rm -f -- \"\$HOME\"/$sh_archive")"
	remote_script="$(cat <<POSIX
set -eu
archive="\$HOME"/$sh_archive
trap 'rm -f -- "\$archive"' 0
trap 'exit 130' 2
trap 'exit 143' 15
destination=$sh_dir
cargo=$sh_cargo
case "\$destination" in
    '~') destination="\$HOME" ;;
    '~/'*) destination="\$HOME/\${destination#\~/}" ;;
    /*) ;;
    *) destination="\$HOME/\$destination" ;;
esac
case "\$cargo" in
    '') cargo="\$HOME/.cargo/bin/cargo" ;;
    '~/'*) cargo="\$HOME/\${cargo#\~/}" ;;
esac
mkdir -p -- "\$destination"
printf '[deploy] extracting workspace to %s\n' "\$destination"
tar -xzf "\$archive" -C "\$destination"
cd "\$destination"
echo '[deploy] cargo build --release'
"\$cargo" build --release
echo '[deploy] smoke via --dry-run'
./target/release/auditui --dry-run
POSIX
)"
	remote_command="sh -c $(quote_posix "$remote_script")"
fi

echo "[deploy] $REMOTE_PLATFORM: scp workspace → $REMOTE:$REMOTE_DIR/"
upload_started=true
scp "$archive" "$REMOTE:./$archive_name" || fail "workspace upload failed"
ssh "$REMOTE" "$remote_command" || fail "remote extraction, release build, or dry-run failed"
echo "[deploy] done"
