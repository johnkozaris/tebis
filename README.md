<div align="center">

# tebis

**Drive AI coding agents from your phone.**

A hardened Rust daemon that bridges **Telegram ↔ tmux** so `claude`,
`aider`, or any long-running TUI on your workstation becomes a chat
conversation on your phone.

[![CI](https://img.shields.io/badge/CI-passing-brightgreen)](.github/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 2024](https://img.shields.io/badge/rust-edition%202024-orange?logo=rust)](Cargo.toml)
[![MSRV 1.95](https://img.shields.io/badge/MSRV-1.95-blue?logo=rust)](Cargo.toml)
[![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey)](#run-persistently)
[![Binary ~4 MB](https://img.shields.io/badge/binary-~4%20MB-success)](#build-from-source)
[![Deps: rustls](https://img.shields.io/badge/TLS-rustls%2Fring-informational)](deny.toml)
[![No OpenSSL](https://img.shields.io/badge/deps-no%20OpenSSL-critical)](deny.toml)

[Quickstart](#quickstart) ·
[Features](#features) ·
[How it works](#how-it-works) ·
[Configure](#configure) ·
[Security](#security) ·
[Contributing](CONTRIBUTING.md)

</div>

---

## Why tebis

You're away from the laptop. `claude` is in a tmux session doing a long
refactor. You want to:

- check what it last said,
- nudge it with a quick instruction,
- or kill it and spin up a fresh agent on a different project.

tebis is the ~4 MB daemon that makes that a Telegram chat. No cloud, no
web dashboard exposed to the internet, no tunnel. Your bot token locked to
your numeric user id is the only entry point.

## Features

- **Single purpose.** A Telegram message becomes a `tmux send-keys`; a
  `/read` becomes a `tmux capture-pane`. Nothing more.
- **Locked to one user.** Access control is a numeric `user.id` match —
  usernames are never trusted (recyclable / mutable).
- **Auto-provisioning.** Configure an autostart session and the first
  message spawns `claude` in the project dir, waits for the TUI, and
  delivers the keystrokes. If the agent dies, the next message re-spawns.
- **Opt-in outbound.** A local Unix-domain socket lets a Claude Code hook
  push `Stop` / `SubagentStop` / permission / idle events to Telegram.
  No TCP, `chmod 0600`, peer-cred check on every connection.
- **Opt-in voice input.** Set `TELEGRAM_STT=on` and voice notes from
  your phone become typed commands via in-process `whisper.cpp`. No
  cloud STT, no extra service — the model (default `base.en`, 148 MB)
  downloads on first run to `$XDG_DATA_HOME/tebis/models/` with SHA-256
  verification, then loads into the daemon. Metal on Apple Silicon;
  OpenBLAS optional on Linux.
- **Opt-in voice replies, cross-platform.** Three backends, picked via
  `TELEGRAM_TTS_BACKEND`: (a) `say` on macOS — zero install, built-in;
  (b) `kokoro-local` — neural ONNX via `ort` + espeak-ng (cross-platform,
  needs `--features kokoro` at build time plus `brew/apt install
  espeak-ng onnxruntime` at runtime); (c) `kokoro-remote` — HTTP POST
  to any OpenAI-compatible TTS endpoint you've deployed. Defaults to
  "voice-in → voice-out only"; flip `TELEGRAM_TTS_RESPOND_TO_ALL=on`
  for every reply. See [`PLAN-TTS-V2.md`](PLAN-TTS-V2.md) for the
  platform matrix.
- **Auto-wired hooks.** Set `TELEGRAM_HOOKS_MODE=auto` and tebis writes
  Claude Code / Copilot CLI hook configs into your project dir at
  autostart time so replies arrive via the agent's native `Stop` event
  — no pane-settle polling, no per-project setup. Full lifecycle via
  `tebis hooks {install,uninstall,status}`; removal is clean.
- **Opt-in dashboard.** Set `INSPECT_PORT` and get a zero-JS HTML control
  panel on `127.0.0.1` — live tmux sessions, activity metrics,
  kill / restart buttons, in-place settings editing.
- **Hardened by default.** All replies HTML-escaped. All tmux argv
  regex-validated. Bot token in `SecretString`, redacted from logs and
  `Debug`. Panic hook with no payload. systemd unit with sandboxing
  pre-configured.
- **Reproducible deps.** `deny.toml` bans OpenSSL, native-TLS, `reqwest`,
  and `aws-lc-rs`. CI audits on every push plus a daily cron.

## Quickstart

```sh
cargo build --release
./target/release/tebis setup     # interactive first-run wizard
./target/release/tebis           # run
```

The wizard walks through creating a bot on [`@BotFather`][botfather],
finding your numeric user id via [`@userinfobot`][userinfobot], picking
session names, and (optionally) enabling autostart and the control
dashboard. It writes `~/.config/tebis/env` (mode 0600).

Start manually after setup:

```sh
set -a; source ~/.config/tebis/env; set +a
./target/release/tebis
```

[botfather]: https://t.me/BotFather
[userinfobot]: https://t.me/userinfobot

### Build from source

```sh
git clone https://github.com/<your-fork>/tebis.git
cd tebis
cargo build --release
```

**Requirements:** Rust 1.95+ (edition 2024), `tmux` 3.x, a C++ toolchain
for whisper.cpp (Xcode CLT on macOS, `build-essential cmake` on Linux
— standard on any dev box). No OpenSSL, no native TLS — rustls/ring is
bundled. Release binary ~5 MB (LTO + strip; 4.25 MB without voice input,
4.98 MB with).

## How it works

```
┌──────────┐                   ┌──────────┐                   ┌──────────┐
│ Phone    │   Bot API long    │  tebis   │   tmux send-keys  │  tmux    │
│ Telegram │ ───── poll ─────▶ │  daemon  │ ────────────────▶ │  pane    │
│          │ ◀──── reply ───── │          │ ◀─ capture-pane ─ │ (claude) │
└──────────┘                   └────▲─────┘                   └──────────┘
                                    │
                                    │ UDS (opt-in)
                                    │ mode 0600
                                 ┌──┴───────┐
                                 │ hook.sh  │
                                 │ (Claude) │
                                 └──────────┘
```

Inbound: long-poll → filter by user id → rate-limit → parse command →
`send-keys` with a 300 ms submit gap → react 👍 or reply with `<pre>`
block.

Outbound: Claude Code's `Stop` hook writes a JSON line to the local UDS;
tebis forwards the tail of the agent's final message to your chat.

Voice input (opt-in, `TELEGRAM_STT=on`): a Telegram voice note → OGG/Opus
download via `getFile` → in-process decode (`ogg` + `opus` crates) →
16 kHz mono PCM → `whisper-rs` (linked `whisper.cpp`, Metal-accelerated
on Apple Silicon) → transcript fed into the same command path as typed
text. No subprocess, no external server, no round-trip to a cloud STT.

## CLI

```
tebis              Run the bridge (reads env)
tebis setup        First-run wizard (creates ~/.config/tebis/env)
tebis --help       Env-var reference + options
tebis --version    Print version
```

## Commands

| Command | Effect |
|---|---|
| `/list` | List active tmux sessions (`✓` = allowlisted, `✗` = visible but not targetable) |
| `/status` | Show default target, autostart session, allowlist, uptime |
| `/send <session> <text>` | Send text + Enter to session |
| `/read [session] [lines]` | Capture pane output (default: current target, 50 lines) |
| `/target <session>` | Set the default target session |
| `/new <session>` | Create an empty detached tmux session |
| `/kill <session>` | Kill a tmux session (idempotent) |
| `/restart` | Kill the autostart session and drop the cached target; next plain-text re-provisions |
| `/help` | Show help |
| *plain text* | Send to the default target (or autostart on first message) |

Ack-only commands react with 👍 rather than replying. Commands with
output (`/list`, `/read`, `/status`, `/help`) reply with a formatted
`<pre>` block; all text-returning paths are HTML-escaped before
`parse_mode=HTML`.

## Configure

The setup wizard writes these; `tebis --help` lists them all.

| Variable | Required | Default | Notes |
|---|---|---|---|
| `TELEGRAM_BOT_TOKEN` | yes | — | From `@BotFather` |
| `TELEGRAM_ALLOWED_USER` | yes | — | Numeric user id (from `@userinfobot`) |
| `TELEGRAM_ALLOWED_SESSIONS` | no | — | Comma-separated allowlist. Unset = any valid tmux name is accepted. |
| `TELEGRAM_POLL_TIMEOUT` | no | `30` | Long-poll seconds (1..=900) |
| `TELEGRAM_MAX_OUTPUT_CHARS` | no | `4000` | `capture-pane` truncation cap |
| `TELEGRAM_AUTOSTART_SESSION` | no | — | Autostart session name (must be allowlisted if allowlist set) |
| `TELEGRAM_AUTOSTART_DIR` | no | — | Autostart working directory |
| `TELEGRAM_AUTOSTART_COMMAND` | no | — | Autostart command (e.g. `claude`) |
| `NOTIFY_CHAT_ID` | no | — | Enables outbound-notify listener |
| `NOTIFY_SOCKET_PATH` | no | `$XDG_RUNTIME_DIR/tebis.sock` or `/tmp/tebis-$USER.sock` | UDS path for hook pushes |
| `INSPECT_PORT` | no | — | Local HTML control dashboard on `127.0.0.1:<port>` |
| `BRIDGE_ENV_FILE` | no | — | Env file path (enables in-dashboard Settings edits) |
| `TELEGRAM_HOOKS_MODE` | no | `off` | `auto` → install Claude Code / Copilot CLI hooks into the autostart dir automatically |
| `TELEGRAM_AUTOREPLY` | no | `on` | `off` disables pane-settle auto-reply (useful if you only want hook-driven replies) |
| `TELEGRAM_NOTIFY` | no | `on` | `off` disables the outbound-notify UDS listener |

Session names must match `[A-Za-z0-9._-]{1,64}`. Invalid names fail startup.

## Run persistently

### macOS (launchd user agent)

```sh
cargo build --release
./contrib/macos/install.sh       # first run creates env file from template
# (edit ~/.config/tebis/env; or run `tebis setup`)
./contrib/macos/install.sh       # second run loads the agent
tail -f /tmp/tebis.log
```

Auto-starts at login (`RunAtLoad`), respawns on crash (`KeepAlive`).

### Linux (systemd user unit)

```sh
cp target/release/tebis ~/.local/bin/
mkdir -p ~/.config/tebis ~/.config/systemd/user
./target/release/tebis setup     # or copy .env.example and edit manually
cp contrib/linux/tebis.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now tebis
loginctl enable-linger "$USER"   # survive logout
```

Audit the sandbox: `systemd-analyze --user security tebis`.

## Autostart

Set all three of these so the first plain-text message auto-provisions a
detached tmux session running a TUI:

```
TELEGRAM_AUTOSTART_SESSION=claude-code
TELEGRAM_AUTOSTART_DIR=/path/to/your/project
TELEGRAM_AUTOSTART_COMMAND=claude
```

Behaviors:

- First plain-text message → provisions the session, waits 3 s for the
  TUI to boot, sends the message.
- If the autostart session later exits (agent quit, pane died) and you
  send another plain-text message, the bridge clears the stale target,
  re-provisions, and retries once — transient agent death self-heals.
- `/restart` gives explicit control: kills the session + clears the
  target without sending anything.

## Claude Code notifications

The bridge can forward four kinds of Claude Code events to Telegram over
a local UDS (mode 0600, unreachable from the network):

| Hook event | What gets sent | Header tag |
|---|---|---|
| `Stop` | tail of Claude's final message for the turn | *(none)* |
| `SubagentStop` | subagent's final message (pre-extracted by Claude Code) | `[agent]` |
| `Notification` / `permission_prompt` | "Claude needs permission to …" | `[ask]` |
| `Notification` / `idle_prompt` | "Claude is idle waiting for input" | `[idle]` |

### Summarization strategy

Rather than head-truncate a long reply (which would lose the
conclusion), the hook pair:

1. **`UserPromptSubmit`** injects an `additionalContext` instruction
   asking Claude to end non-trivial replies with a concise ≤1500-char
   summary.
2. **`Stop`** / **`SubagentStop`** take the **tail** of the final
   assistant message (last 1500 chars, not first).

If Claude complied, the tail *is* the summary. If it didn't, the tail
still has the conclusion — usually what a phone notification wants. No
extra LLM calls, no Stop-block loops.

### Setup

The UDS listener is on by default (chat_id = your `TELEGRAM_ALLOWED_USER`).
To get Claude Code / Copilot CLI replies via their native `Stop` events
instead of pane-settle polling, enable hook auto-install and the rest is
automatic:

```sh
echo 'TELEGRAM_HOOKS_MODE=auto' >> ~/.config/tebis/env
```

On the next autostart, tebis writes the agent's hook config into the
autostart directory and the embedded hook script into
`~/.local/share/tebis/<agent>-hook.sh`. You'll see a banner line like:

```
  ▶  installed Claude Code hooks in /path/to/your/project
```

**Where tebis writes** (and what it owns):

| Agent | Config file | Script |
|---|---|---|
| Claude Code | `<dir>/.claude/settings.local.json` (lowest-precedence; normally `.gitignore`d) | `~/.local/share/tebis/claude-hook.sh` |
| Copilot CLI | `<dir>/.github/hooks/tebis.json` (file owned outright by tebis) | `~/.local/share/tebis/copilot-hook.sh` |

**tebis only touches the four events it installs** (`UserPromptSubmit`,
`Stop`, `SubagentStop`, `Notification`) and only entries whose `command`
path is the tebis data dir. Your other hooks are never modified.

**Manual lifecycle** — for dirs you run an agent in that aren't the
autostart dir:

```sh
tebis hooks install [<dir>]      # defaults to autostart dir
tebis hooks uninstall [<dir>]    # removes only tebis-owned entries
tebis hooks status [<dir>]       # lists tebis events installed
```

**Dependencies**: `jq` and `nc` (BSD netcat — `netcat-openbsd` on Linux).

**Failure-open**: the hook exits 0 on every path, so even a missing
script or broken socket never blocks an agent's turn.

## Inspect dashboard

Set `INSPECT_PORT=51624` (or any port — the wizard defaults to `51624`
in the IANA dynamic range to avoid common collisions like Prometheus
`9090`) and the bridge binds a local HTML control panel at
`http://127.0.0.1:51624/`. **Loopback-only**; no authentication — do not
try to expose it beyond the host.

**Shows:** non-secret config, live tmux sessions with ✓/✗ markers,
activity metrics (last message / response, counts, poll health),
uptime, default target, handler slot availability.

**Actions:** kill any session, kill-all-allowlisted, restart bridge
(graceful — launchd/systemd respawns), edit poll timeout / output cap /
autostart dir and save-restart in one click.

`GET /status` returns the same info as JSON. Auto-refresh every 5 s via
`<meta http-equiv="refresh">`. Zero JavaScript, zero bundler, zero
external assets.

## Security

tebis executes keystrokes into another process. The security model is
worth understanding before you deploy it.

- **Auth by numeric Telegram `user.id` only.** Usernames are recyclable
  and never used for access control.
- **Session-name regex `[A-Za-z0-9._-]{1,64}`** enforced at every tmux
  call (shell-metachar / path-traversal defense). Optional strict
  allowlist on top.
- **`send-keys -l` + separate `-H 0d`** so message text is sent as
  literal keystrokes and can never be interpreted as a tmux key-name.
- **All Telegram replies HTML-escaped** before `parse_mode=HTML`. Output
  from tmux also passes through ANSI / C0 / C1 / bidi-codepoint
  stripping.
- **Bot token in `SecretString`**, never logged. Network errors walk the
  source chain and redact anything that looks like URL / token data.
- **Outbound-notify UDS-only**, mode 0600 with `peer_cred` check on
  every accepted connection.
- **Per-chat GCRA rate limit + global handler semaphore** bound
  subprocess fan-out during bursts.
- **Reproducible dependency policy** in `deny.toml`: no OpenSSL, no
  native TLS, no `reqwest`, no `aws-lc-rs`.

Reporting a vulnerability: see [SECURITY.md](SECURITY.md).

## Development

```sh
cargo test                                           # unit tests
cargo clippy --all-targets -- -D warnings \
    -W clippy::pedantic -W clippy::nursery           # full lints
cargo fmt --check                                    # style
cargo audit                                          # RUSTSEC advisories
cargo deny check                                     # licenses + bans + sources
cargo build --release                                # ~4 MB binary (LTO + strip)
```

See [CLAUDE.md](CLAUDE.md) for security invariants before touching
security-sensitive code, and [CONTRIBUTING.md](CONTRIBUTING.md) for PR
expectations.

## Project layout

```
src/
  main.rs                       # runtime, signals, poll loop, argv dispatch
  config.rs                     # env → Config + HooksMode
  env_file.rs                   # shared 0600 atomic write + KEY=VAL parse + toggles
  bridge/                       # per-message behavior
    mod.rs                      #   entry pipeline + reply routing
    handler.rs                  #   command parser + executor
    session.rs                  #   default-target state + autostart + hooked_sessions
    autoreply.rs                #   pane-settle fallback reply path
    typing.rs                   #   shared "typing…" refresher (TypingGuard RAII)
  agent_hooks/                  # hook install / uninstall per agent
    mod.rs                      #   HookManager trait, materialize()
    agent.rs                    #   AgentKind enum + detect() + HooksMode
    claude.rs                   #   Claude Code: .claude/settings.local.json
    copilot.rs                  #   Copilot CLI: .github/hooks/tebis.json
    jsonfile.rs                 #   atomic JSON write
    manifest.rs                 #   host-wide installed-hooks manifest + flock
    legacy.rs                   #   pre-Phase-2 hook detection
  hooks_cli.rs                  # `tebis hooks install|uninstall|status|list|prune`
  telegram/                     # Bot API client
    mod.rs                      #   hyper + rustls, token redaction
    types.rs                    #   DTOs
  tmux.rs                       # send-keys / capture / allowlist
  sanitize.rs                   # C0/C1/bidi + HTML escape
  security.rs                   # user-id auth + GCRA rate limit
  metrics.rs                    # lock-free atomic counters
  lockfile.rs                   # single-instance flock
  service.rs                    # launchd/systemd install + status
  inspect/                      # opt-in local HTML dashboard
  notify/                       # UDS listener + Forwarder trait
    mod.rs                      #   Forwarder trait + TelegramForwarder + Payload
    listener.rs                 #   UDS bind / accept / per-connection protocol
    format.rs                   #   Payload → Telegram HTML body (tag + text)
    markdown.rs                 #   Markdown → Telegram HTML (bold, code, fences)
  setup/                        # `tebis setup` wizard
    mod.rs, steps.rs,
    discover.rs, ui.rs
contrib/
  macos/                        # launchd user agent + installer
  linux/                        # sandboxed systemd user unit
  claude/claude-hook.sh         # embedded into binary via include_str!
  copilot/copilot-hook.sh       # embedded into binary via include_str!
examples/
  inspect-demo.rs               # spin up the dashboard with synthetic metrics
```

## License

MIT — see [LICENSE](LICENSE).
