<div align="center">

# tebis

**Chat with your AI coding agents from your phone.**

You're away from the laptop. Claude Code is deep in a refactor in a
multiplexer session. Tebis lets you open Telegram and:

- see what it last said,
- send a nudge,
- approve a permission prompt,
- kill it and spin up a fresh agent in a different project,
- send a voice note instead of typing.

That's the whole product. A small Rust daemon, one user, one bot, no cloud.

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 2024](https://img.shields.io/badge/rust-edition%202024-orange?logo=rust)](Cargo.toml)
[![MSRV 1.95](https://img.shields.io/badge/MSRV-1.95-blue?logo=rust)](Cargo.toml)
[![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-lightgrey)](#run-in-the-background)
[![No OpenSSL](https://img.shields.io/badge/deps-no%20OpenSSL-critical)](deny.toml)

</div>

---

## What it does

Tebis is a bridge between Telegram and a tmux/psmux session on your workstation.

- **You type on your phone →** the text lands as keystrokes in the pane.
- **The agent replies →** the tail of its reply lands in your chat.
- **You send a voice note →** transcribed in-process and sent as keystrokes.
- **Reply audio (optional) →** the agent's text is read back to you as a voice message.

It was built for Claude Code but works with `aider`, Copilot CLI, or
anything that runs in tmux/psmux. Replies come via native agent hooks when
available (Claude Code, Copilot CLI) and via pane-watching otherwise.

## Why it's different

- **One user, one bot.** Locked to your numeric Telegram ID — usernames
  are never trusted.
- **Nothing leaves your box.** Bot token stays local; no relay server;
  voice transcription runs in-process via whisper.cpp.
- **~4 MB binary.** Built on `hyper` + `rustls` directly; `deny.toml`
  bans OpenSSL, native-TLS, `reqwest`, and `aws-lc-rs`.
- **Boring by choice.** Single binary, plus an env file and one OS service entry.

## Get started

### macOS / Linux

```sh
cargo build --release
./target/release/tebis setup     # interactive wizard → ~/.config/tebis/env
./target/release/tebis install   # run as a background service (launchd / systemd user)
```

**You'll need:** Rust 1.95+, `tmux` 3.x, a C++ toolchain (Xcode CLT on
macOS, `build-essential cmake` on Linux).

### Windows

```powershell
cargo build --release
.\target\release\tebis.exe setup     # interactive wizard → %APPDATA%\tebis\env
.\target\release\tebis.exe install   # register as a Task Scheduler at-logon task
```

**You'll need:** Rust 1.95+, [psmux][psmux] (tmux-compatible multiplexer),
Visual Studio Build Tools (C++ workload), CMake for local STT builds, and
PowerShell 7+ recommended for psmux (PowerShell 5.1 is still enough for
tebis hook scripts). Claude Code and Copilot CLI both have native Windows
installers as of 2026; install whichever you want to drive from Telegram.

psmux v3.3+ is the current target. It ships `psmux.exe`, `pmux.exe`, and a
`tmux.exe` compatibility alias; tebis calls `psmux.exe` directly. Install
with one of:

```powershell
scoop bucket add psmux https://github.com/psmux/scoop-psmux
scoop install psmux
# or:
winget install marlocarlo.psmux
choco install psmux
cargo install psmux
```

Windows TTS uses the built-in WinRT `SpeechSynthesizer` backend
(`TELEGRAM_TTS_BACKEND=winrt`) with no extra install. Windows 11
Narrator “Natural” voices are not exposed to third-party apps, so WinRT
uses the installed OneCore voices (for example Zira/David/Mark).

The wizard walks through creating a bot on [`@BotFather`][botfather],
finding your numeric ID via [`@userinfobot`][userinfobot], and picking
which project Claude should start in when you first message the bot.

Tebis is a genuinely cross-platform daemon: same code, same security
invariants, per-OS primitives behind a single `platform::` abstraction
(UDS → Named Pipe, launchd/systemd → Task Scheduler, tmux → psmux, 0600
→ DACL). See [`CLAUDE.md`](CLAUDE.md) §Layout for the module map.

[botfather]: https://t.me/BotFather
[userinfobot]: https://t.me/userinfobot
[psmux]: https://github.com/psmux/psmux

## Using it

Once running, message your bot. Plain text is sent to the current multiplexer
target; slash commands control the bridge.

| Command | What it does |
|---|---|
| *plain text* | Sends to the current session (or spawns the autostart one) |
| `/list` | Lists multiplexer sessions (`✓` = allowlisted) |
| `/status` | Current target, autostart config, uptime |
| `/send <session> <text>` | Sends to a specific session |
| `/read [session] [lines]` | Grabs the last N lines of pane output |
| `/target <session>` | Sets the default target |
| `/new <session>` / `/kill <session>` | Create or kill a session |
| `/restart` | Kills the autostart session; the next message re-provisions |
| `/tts [off\|say\|winrt\|kokoro-local\|kokoro-remote]` | Switches voice-reply backend |
| `/help` | Usage |

Short commands (set target, new, kill) react with 👍 instead of replying
— less chat noise. Command output uses `<pre>` blocks.

## Configure

The wizard sets the required vars; everything else is optional. Full list
in `tebis --help`.

### Core

| Variable | Default | Notes |
|---|---|---|
| `TELEGRAM_BOT_TOKEN` | *required* | From `@BotFather` |
| `TELEGRAM_ALLOWED_USER` | *required* | Numeric user ID from `@userinfobot` |
| `TELEGRAM_ALLOWED_SESSIONS` | empty | Comma list. Empty = any valid name. |
| `TELEGRAM_POLL_TIMEOUT` | `30` | Long-poll seconds, 1..=900 |
| `TELEGRAM_MAX_OUTPUT_CHARS` | `4000` | `/read` truncation cap |

### Autostart — spawn your agent on first message

| Variable | Default | Notes |
|---|---|---|
| `TELEGRAM_AUTOSTART_SESSION` | — | Multiplexer session name |
| `TELEGRAM_AUTOSTART_DIR` | — | Working directory |
| `TELEGRAM_AUTOSTART_COMMAND` | — | e.g. `claude` |

Set all three or none.

### Voice

| Variable | Default | Notes |
|---|---|---|
| `TELEGRAM_STT` | `off` | `on` enables in-process voice-to-text (whisper.cpp) |
| `TELEGRAM_STT_MODEL` | `small.en` | whisper model (e.g. `base.en`) |
| `TELEGRAM_STT_LANGUAGE` | `en` | ISO 639-1 code |
| `TELEGRAM_STT_MAX_DURATION_SEC` | `120` | Per-clip cap |
| `TELEGRAM_TTS_BACKEND` | `off` | `say` (macOS), `winrt` (Windows), `kokoro-local`, `kokoro-remote` |
| `TELEGRAM_TTS_VOICE` | backend default | e.g. `Samantha` (say), `Zira` (winrt), `af_sarah` (Kokoro) |
| `TELEGRAM_TTS_RESPOND_TO_ALL` | `off` | `on` → voice-reply every message, not just voice-in |
| `TELEGRAM_TTS_REMOTE_URL` | — | OpenAI-compatible TTS endpoint (for `kokoro-remote`) |

### Outbound hooks & dashboard

| Variable | Default | Notes |
|---|---|---|
| `TELEGRAM_HOOKS_MODE` | `off` | `auto` → install Claude Code / Copilot CLI hooks at autostart |
| `TELEGRAM_AUTOREPLY` | `on` | `off` disables pane-watch fallback |
| `TELEGRAM_NOTIFY` | `on` | `off` disables the outbound UDS listener entirely |
| `NOTIFY_CHAT_ID` | your user ID | Override which chat receives hook pushes |
| `NOTIFY_SOCKET_PATH` | `$XDG_RUNTIME_DIR/tebis.sock` | UDS path |
| `INSPECT_PORT` | — | Bind local HTML dashboard on `127.0.0.1:<port>` |
| `BRIDGE_ENV_FILE` | — | Enables in-dashboard settings editor |

Session names must match `[A-Za-z0-9._-]{1,64}`.

## Run in the background

```sh
tebis install    # launchd user agent (macOS) / systemd user unit (Linux)
tebis start
tebis status
tebis stop
tebis uninstall [--purge]   # --purge also deletes binary, env, and model cache
```

On Linux, run `loginctl enable-linger "$USER"` if you want tebis to keep
running after you log out. Audit the sandbox with
`systemd-analyze --user security tebis`.

## Hooks (Claude Code, Copilot CLI)

With `TELEGRAM_HOOKS_MODE=auto`, tebis installs agent hooks in the
autostart project dir so replies arrive via the agent's own "stop" event
instead of pane-watching — faster and more reliable.

```sh
tebis hooks install   [<dir>]   # install in any project dir
tebis hooks uninstall [<dir>]   # remove only tebis-owned entries
tebis hooks status    [<dir>]
tebis hooks list                # every install host-wide
tebis hooks prune               # drop manifest entries for deleted dirs
```

| Agent event | Sent | Tag |
|---|---|---|
| Claude `Stop` | tail of agent's reply | *(none)* |
| Claude `SubagentStop` | subagent reply | `[agent]` |
| Claude `Notification` | permission / idle message | `[ask]` / `[idle]` |
| Copilot `notification` | completion / permission / idle message | notification kind |

Tebis only installs the supported events for each agent, and only entries
whose command path is under `$XDG_DATA_HOME/tebis/` — your other hooks are
never modified. Copilot CLI does not expose Claude-style `agentStop` or
`subagentStop` hooks.

**Runtime deps:** `jq` + `nc` (BSD netcat; `netcat-openbsd` on Linux).

## Dashboard

Set `INSPECT_PORT=51624` (any free port) and open
`http://127.0.0.1:51624`. Loopback-only, no auth, zero JavaScript.
Shows live sessions, handler metrics, uptime; kill / restart / edit
settings from the browser.

## Security model

- Auth by numeric Telegram ID — never username (recyclable).
- All multiplexer session names validated against `[A-Za-z0-9._-]{1,64}`.
- Keystrokes sent as literal bytes, not shell-interpolated key strings.
- All Telegram replies HTML-escaped.
- Bot token in `SecretString`, redacted from logs and error chains.
- Notify listener is local-user-only: UDS mode 0600 + peer creds on Unix,
  owner-only Named Pipe + SID check on Windows.
- Per-chat rate limit + global handler cap.

Full invariants in [CLAUDE.md](CLAUDE.md); vuln reports in
[SECURITY.md](SECURITY.md).

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets -- -D warnings -W clippy::pedantic -W clippy::nursery
cargo fmt --check
cargo audit
cargo deny check
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for PR expectations and
[CLAUDE.md](CLAUDE.md) for security invariants before touching
security-sensitive code.

## License

MIT — see [LICENSE](LICENSE).
