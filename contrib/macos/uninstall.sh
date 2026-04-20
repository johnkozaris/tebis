#!/bin/bash
# Thin wrapper around `tebis uninstall`. Removes the launchd agent.
#
# Usage:
#   ./contrib/macos/uninstall.sh

set -eu

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"

BIN="$HOME/.local/bin/tebis"
if [ ! -x "$BIN" ]; then
    BIN="./target/release/tebis"
fi
if [ ! -x "$BIN" ]; then
    echo "No tebis binary found at ~/.local/bin/tebis or ./target/release/tebis."
    echo "If the service is installed, you can still unload it manually:"
    echo "    launchctl unload ~/Library/LaunchAgents/local.tebis.plist"
    echo "    rm ~/Library/LaunchAgents/local.tebis.plist"
    exit 1
fi

exec "$BIN" uninstall
