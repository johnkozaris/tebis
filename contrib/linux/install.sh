#!/bin/bash
# Thin wrapper — builds the release binary if needed, then delegates to
# `tebis install` (which handles all the systemd work).
#
# Usage:
#   ./contrib/linux/install.sh

set -eu

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"

BIN="./target/release/tebis"
if [ ! -x "$BIN" ]; then
    echo "Building release binary…"
    cargo build --release
fi

if [ ! -f "$HOME/.config/tebis/env" ]; then
    echo
    echo "  No config at ~/.config/tebis/env — run \`tebis setup\` first:"
    echo "      $BIN setup"
    echo
    exit 1
fi

exec "$BIN" install
