#!/bin/sh
# Glass quick support: https://glassdesk.dev/help
#
#   curl -fsSL glassdesk.dev/help | sh
#
# Runs Glass just for this session, from a temporary folder: it shows a code to read to
# the person helping you, asks before letting them in, and when you stop it (Ctrl+C, or
# Stop sharing), it's gone. Nothing is installed and nothing starts with the computer.
set -eu

BASE="${GLASS_BASE:-https://glassdesk.dev}"

bold() { printf '\033[1m%s\033[0m\n' "$*"; }
info() { printf '  \033[36m›\033[0m %s\n' "$*"; }
fail() { printf '  \033[31m✗\033[0m %s\n' "$*" >&2; exit 1; }

echo
bold "Glass quick support"
echo

case "$(uname -s)-$(uname -m)" in
  Darwin-*)
    major=$(sw_vers -productVersion | cut -d. -f1)
    [ "$major" -ge 13 ] || fail "Glass needs macOS 13 (Ventura) or newer; this Mac has $(sw_vers -productVersion)."
    TARBALL="glass-latest-darwin-universal.tar.gz"
    ;;
  Linux-x86_64) TARBALL="glass-latest-linux-x86_64.tar.gz" ;;
  Linux-aarch64|Linux-arm64) TARBALL="glass-latest-linux-aarch64.tar.gz" ;;
  *) fail "Quick support runs on macOS and on Linux (x86_64 or ARM64)." ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM HUP

info "Downloading Glass for this session (nothing is installed)"
curl -fsSL "$BASE/releases/$TARBALL" -o "$tmp/glass.tar.gz" || fail "Download failed. Check your connection and try again."
curl -fsSL "$BASE/releases/$TARBALL.sha256" -o "$tmp/glass.sha256" || fail "Couldn't fetch the checksum."
expected=$(cut -d' ' -f1 < "$tmp/glass.sha256")
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$tmp/glass.tar.gz" | cut -d' ' -f1)
else
  actual=$(shasum -a 256 "$tmp/glass.tar.gz" | cut -d' ' -f1)
fi
[ "$expected" = "$actual" ] || fail "Checksum mismatch: the download is corrupt or was tampered with."
tar -xzf "$tmp/glass.tar.gz" -C "$tmp"

# Piped from curl, stdin is this script: questions are read from the terminal.
if (exec < /dev/tty) 2>/dev/null; then
  GLASS_BASE="$BASE" "$tmp/glass/bin/glass" support < /dev/tty || true
else
  GLASS_BASE="$BASE" "$tmp/glass/bin/glass" support || true
fi
echo
info "Done. Glass is gone from this computer."
