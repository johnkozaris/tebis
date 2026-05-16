#!/bin/sh
# install.sh — one-shot installer for tebis on macOS and Linux.
#
# Usage:
#   curl -fsSL https://github.com/johnkozaris/tebis/releases/latest/download/install.sh | sh
#   curl -fsSL https://github.com/johnkozaris/tebis/releases/latest/download/install.sh | sh -s -- --version v0.1.0
#
# What it does:
#   1. Detect OS + arch, map to Rust target triple
#   2. Resolve the requested release tag (latest by default)
#   3. Download tebis-<triple> + tebis-<triple>.sha256
#   4. Verify SHA-256
#   5. Install to ~/.local/bin/tebis (chmod 0755)
#   6. macOS: strip the quarantine xattr so Gatekeeper allows execution
#   7. Print PATH hint if ~/.local/bin isn't on $PATH
#
# What it does NOT do:
#   - Run `tebis setup` (separate, interactive step)
#   - Install tmux / jq / nc (`tebis setup` offers that per-PM)
#   - Modify your shell rc (we print the line; you decide)
#   - Use sudo or write outside $HOME

set -eu

REPO="johnkozaris/tebis"
INSTALL_DIR="${TEBIS_INSTALL_DIR:-${HOME}/.local/bin}"
BIN_NAME="tebis"
TAG="${TEBIS_VERSION:-latest}"

# ─── ANSI helpers (skip when stdout isn't a TTY) ─────────────────────
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    BOLD='\033[1m'; DIM='\033[2m'; RED='\033[31m'
    GREEN='\033[32m'; YELLOW='\033[33m'; CYAN='\033[36m'; RESET='\033[0m'
else
    BOLD=''; DIM=''; RED=''; GREEN=''; YELLOW=''; CYAN=''; RESET=''
fi

say()  { printf '%b▶%b  %s\n' "$CYAN$BOLD" "$RESET" "$1"; }
ok()   { printf '%b✓%b  %s\n' "$GREEN$BOLD" "$RESET" "$1"; }
warn() { printf '%b⚠%b  %s\n' "$YELLOW$BOLD" "$RESET" "$1"; }
die()  { printf '%b✗%b  %s\n' "$RED$BOLD"   "$RESET" "$1" >&2; exit 1; }

# ─── Argument parsing ────────────────────────────────────────────────
print_help() {
    # Heredoc avoids reading $0 — when piped via `curl | sh`, $0 is
    # `sh` and any self-scrape would fail.
    cat <<EOF
install.sh — one-shot installer for tebis on macOS and Linux.

Usage:
  curl -fsSL https://github.com/${REPO}/releases/latest/download/install.sh | sh
  curl -fsSL https://.../install.sh | sh -s -- --version v0.1.0
  curl -fsSL https://.../install.sh | TEBIS_INSTALL_DIR=/opt/tebis/bin sh

Options:
  --version, -v <tag>   release tag (e.g. v0.1.0); default: latest
  --dir, -d <path>      install directory; default: \${HOME}/.local/bin

Env:
  TEBIS_VERSION         same as --version
  TEBIS_INSTALL_DIR     same as --dir
  NO_COLOR              disable ANSI escapes

The installer writes a single file (\${INSTALL_DIR}/tebis) and never
touches sudo, shell rc files, or system package managers.
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version|-v) TAG="${2:-}"; shift 2 ;;
        --dir|-d)     INSTALL_DIR="${2:-}"; shift 2 ;;
        --help|-h)    print_help; exit 0 ;;
        *) die "unknown argument: $1 (try --help)" ;;
    esac
done

# ─── Platform detection ──────────────────────────────────────────────
detect_target() {
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os" in
        Linux)
            case "$arch" in
                x86_64|amd64) echo "x86_64-unknown-linux-gnu" ;;
                aarch64|arm64) echo "aarch64-unknown-linux-gnu" ;;
                *) die "unsupported Linux arch: $arch" ;;
            esac
            ;;
        Darwin)
            case "$arch" in
                x86_64) echo "x86_64-apple-darwin" ;;
                arm64) echo "aarch64-apple-darwin" ;;
                *) die "unsupported macOS arch: $arch" ;;
            esac
            ;;
        *) die "unsupported OS: $os (use install.ps1 on Windows)" ;;
    esac
}

# ─── Dependency probes ───────────────────────────────────────────────
require_cmd() {
    command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

require_cmd uname
require_cmd mkdir
require_cmd chmod
require_cmd mv
require_cmd rm

# At least one of curl or wget — we prefer curl since it's standard
# on both macOS and most Linux distros.
if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL "$1" -o "$2"; }
    fetch_str() { curl -fsSL "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -qO "$2" "$1"; }
    fetch_str() { wget -qO- "$1"; }
else
    die "need curl or wget (neither found on PATH)"
fi

# At least one of shasum (macOS / many Linux) or sha256sum (most Linux).
if command -v shasum >/dev/null 2>&1; then
    sha256_of() { shasum -a 256 "$1" | awk '{print $1}'; }
elif command -v sha256sum >/dev/null 2>&1; then
    sha256_of() { sha256sum "$1" | awk '{print $1}'; }
else
    die "need shasum or sha256sum to verify the download"
fi

# ─── Resolve target + tag ────────────────────────────────────────────
TARGET="$(detect_target)"
ASSET="tebis-${TARGET}"

if [ "$TAG" = "latest" ]; then
    BASE_URL="https://github.com/${REPO}/releases/latest/download"
else
    BASE_URL="https://github.com/${REPO}/releases/download/${TAG}"
fi

BIN_URL="${BASE_URL}/${ASSET}"
SHA_URL="${BASE_URL}/${ASSET}.sha256"

say "Installing tebis"
printf '    %btarget:%b      %s\n' "$DIM" "$RESET" "$TARGET"
printf '    %btag:%b         %s\n' "$DIM" "$RESET" "$TAG"
printf '    %binstall dir:%b %s\n' "$DIM" "$RESET" "$INSTALL_DIR"
printf '    %burl:%b         %s\n' "$DIM" "$RESET" "$BIN_URL"

# ─── Download to tmp ─────────────────────────────────────────────────
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT INT TERM

TMP_BIN="${TMPDIR}/${ASSET}"
TMP_SHA="${TMPDIR}/${ASSET}.sha256"

say "Downloading binary…"
fetch "$BIN_URL" "$TMP_BIN" || die "download failed: $BIN_URL"

say "Downloading checksum…"
fetch "$SHA_URL" "$TMP_SHA" || die "download failed: $SHA_URL"

# ─── Verify checksum ─────────────────────────────────────────────────
EXPECTED="$(awk '{print $1}' "$TMP_SHA" | tr '[:upper:]' '[:lower:]')"
GOT="$(sha256_of "$TMP_BIN")"
if [ "$EXPECTED" != "$GOT" ]; then
    die "checksum mismatch
    expected: $EXPECTED
    got:      $GOT"
fi
ok "Checksum verified."

# ─── Install ─────────────────────────────────────────────────────────
mkdir -p "$INSTALL_DIR"
INSTALL_PATH="${INSTALL_DIR}/${BIN_NAME}"

# Use `mv` rather than `cp` so the resulting file inode is fresh —
# avoids issues if INSTALL_PATH was a symlink or hardlinked elsewhere.
mv "$TMP_BIN" "$INSTALL_PATH"
chmod 0755 "$INSTALL_PATH"

# macOS quarantine: Gatekeeper attaches `com.apple.quarantine` to
# anything curl/wget downloaded. Strip it now so the first
# invocation doesn't pop the Gatekeeper dialog. We OWN this download
# (we just verified its SHA-256), so removing the attribute is safe.
if [ "$(uname -s)" = "Darwin" ] && command -v xattr >/dev/null 2>&1; then
    xattr -d com.apple.quarantine "$INSTALL_PATH" 2>/dev/null || true
fi

ok "Installed tebis to ${INSTALL_PATH}"

# ─── PATH hint ───────────────────────────────────────────────────────
case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
        warn "${INSTALL_DIR} is not on your \$PATH"
        printf '    %bAdd to your shell rc (~/.zshrc, ~/.bashrc, etc):%b\n' "$DIM" "$RESET"
        # shellcheck disable=SC2016
        # The literal `$PATH` is the point — user pastes this verbatim
        # into their shell rc where the shell expands it at init time.
        printf '\n        export PATH="%s:$PATH"\n\n' "$INSTALL_DIR"
        ;;
esac

# ─── Next steps ──────────────────────────────────────────────────────
printf '\n%bNext steps%b\n' "$BOLD" "$RESET"
printf '    %s setup              %brun the interactive config wizard%b\n' \
    "$BIN_NAME" "$DIM" "$RESET"
printf '    %s install            %binstall as a background service%b\n' \
    "$BIN_NAME" "$DIM" "$RESET"
printf '    %s --help             %bsee all commands%b\n\n' \
    "$BIN_NAME" "$DIM" "$RESET"
