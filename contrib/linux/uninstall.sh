#!/bin/bash
# Thin wrapper around `tebis uninstall`. Disables and removes the systemd
# user unit.
#
# Usage:
#   ./contrib/linux/uninstall.sh

set -eu

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"

BIN="$HOME/.local/bin/tebis"
if [ ! -x "$BIN" ]; then
    BIN="./target/release/tebis"
fi
if [ ! -x "$BIN" ]; then
    echo "No tebis binary found at ~/.local/bin/tebis or ./target/release/tebis."
    echo "If the service is installed, you can still remove it manually:"
    echo "    systemctl --user disable --now tebis"
    echo "    rm ~/.config/systemd/user/tebis.service"
    echo "    systemctl --user daemon-reload"
    exit 1
fi

exec "$BIN" uninstall
