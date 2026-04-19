#!/bin/bash
# Install (or reinstall) the tebis launchd agent on macOS.
#
#   ./contrib/macos/install.sh
#
# After this runs, the bridge auto-starts at login and respawns on
# crash. Edit ~/.config/tebis/env and click "Restart bridge"
# in the dashboard (or `launchctl kickstart -k gui/$(id -u)/local.tebis`)
# to apply config changes.

set -eu

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BIN_SRC="$REPO_ROOT/target/release/tebis"
BIN_DST="$HOME/.local/bin/tebis"
ENV_DIR="$HOME/.config/tebis"
ENV_FILE="$ENV_DIR/env"
ENV_EXAMPLE="$REPO_ROOT/.env.example"
PLIST_SRC="$REPO_ROOT/contrib/macos/local.tebis.plist"
PLIST_DST="$HOME/Library/LaunchAgents/local.tebis.plist"

if [ ! -x "$BIN_SRC" ]; then
    echo "Release binary missing. Build first:"
    echo "  cargo build --release"
    exit 1
fi

mkdir -p "$(dirname "$BIN_DST")" "$ENV_DIR" "$(dirname "$PLIST_DST")"
install -m 755 "$BIN_SRC" "$BIN_DST"

if [ ! -f "$ENV_FILE" ]; then
    install -m 600 "$ENV_EXAMPLE" "$ENV_FILE"
    echo
    echo "  Created $ENV_FILE from template."
    echo "  Edit it with your real TELEGRAM_BOT_TOKEN / ALLOWED_USER / SESSIONS,"
    echo "  then re-run this script (or just: launchctl load $PLIST_DST)."
    echo
    exit 0
fi

# Substitute $USER into the plist so plist literal paths expand for the
# current user.
sed "s|USERNAME|$USER|g" "$PLIST_SRC" > "$PLIST_DST"

# Unload any prior load (ignore errors on first install) then load fresh.
launchctl unload "$PLIST_DST" 2>/dev/null || true
launchctl load "$PLIST_DST"

echo
echo "  tebis launchd agent loaded."
echo "  Logs:  tail -f /tmp/tebis.log"
echo "  Stop:  launchctl unload $PLIST_DST"
echo "  Dash:  open http://127.0.0.1:\$INSPECT_PORT  (if INSPECT_PORT is set)"
echo
