<div align="center">

# tebis

**Control your local AI coding agent from Telegram.**

Tebis is a small Rust daemon for one person and one workstation. It connects a
private Telegram bot to a local terminal session, so you can check progress,
send a prompt, approve a permission question, restart an agent, or send a voice
note from your phone.

No hosted relay. No team dashboard. Your bot token is stored locally and used
only for direct Telegram Bot API calls.

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 2024](https://img.shields.io/badge/rust-edition%202024-orange?logo=rust)](Cargo.toml)
[![MSRV 1.95](https://img.shields.io/badge/MSRV-1.95-blue?logo=rust)](Cargo.toml)
[![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-lightgrey)](#requirements)

</div>

## What Tebis does

Tebis keeps a local command-line agent reachable while you are away from the
laptop.

1. Run Claude Code, Copilot CLI, aider, or another terminal app in a persistent
   terminal session.
2. Message your private Telegram bot.
3. Tebis sends your text to that local session as keystrokes.
4. Tebis sends useful output back to Telegram.

On macOS and Linux, Tebis uses `tmux`. On Windows, it uses [`psmux`][psmux].
Both keep terminal apps running even when no terminal window is open.

## Who it is for

Tebis is for solo developers who:

- run local AI coding agents for long tasks
- want to check progress from a phone
- need to answer permission prompts without walking back to the desk
- prefer a local binary over a hosted control panel

It is not built for team access, shared bots, or remote shell hosting. The
safety model is intentionally simple: one Telegram user, one local machine, one
bot token.

## Requirements

| Platform | You need |
| --- | --- |
| macOS | Rust 1.95+, `tmux` 3.x, Xcode Command Line Tools |
| Linux | Rust 1.95+, `tmux` 3.x, C++ build tools, CMake |
| Windows | Rust 1.95+, [psmux][psmux], Visual Studio Build Tools with C++, CMake |

Optional voice features may download local speech models during setup.

## Quick start

```sh
git clone https://github.com/johnkozaris/tebis.git
cd tebis
cargo build --release
./target/release/tebis setup
```

Windows PowerShell:

```powershell
git clone https://github.com/johnkozaris/tebis.git
cd tebis
cargo build --release
.\target\release\tebis.exe setup
```

The setup wizard walks you through:

- creating a Telegram bot with [`@BotFather`][botfather]
- finding your numeric Telegram ID with [`@userinfobot`][userinfobot]
- choosing which terminal sessions Tebis may control
- choosing a default project and agent command, such as `claude`
- optionally enabling faster replies, voice features, and the local dashboard

At the end, choose foreground mode for a quick test or background mode so Tebis
starts when you log in.

See [Setup guide](docs/setup.md) for platform notes and background service
commands.

## Use it from Telegram

Plain text goes to your current terminal session. Slash commands control Tebis:

| Command | What it does |
| --- | --- |
| `/list` | Lists running terminal sessions |
| `/status` | Shows the current target and uptime |
| `/send <session> <text>` | Sends text to a specific session |
| `/read [session] [lines]` | Reads recent output |
| `/target <session>` | Makes a session the default target |
| `/new <session>` | Creates an empty background session |
| `/kill <session>` | Stops a session |
| `/restart` | Restarts the default agent on the next message |
| `/tts ...` | Changes voice replies |
| `/help` | Shows the Telegram command list |

Short control commands respond with a thumbs-up reaction instead of adding
noise to the chat.

## Safety at a glance

Tebis is careful because it sends keystrokes to a real local process.

- It accepts messages only from your numeric Telegram user ID.
- Usernames are not trusted.
- Your bot token is stored in your local config file.
- There is no Tebis cloud service or relay server.
- The dashboard binds to `127.0.0.1` only.
- Local agent notifications are accepted only from your operating-system user.
- Telegram replies are escaped before being sent.

For security details or vulnerability reports, see [SECURITY.md](SECURITY.md).

## More guides

| Guide | When to read it |
| --- | --- |
| [Setup](docs/setup.md) | Install, run, and manage Tebis as a background service |
| [Configuration](docs/configuration.md) | Edit the env file by hand |
| [Agent hooks](docs/hooks.md) | Get faster replies from Claude Code or Copilot CLI |
| [Voice](docs/voice.md) | Use Telegram voice notes and voice replies |
| [Dashboard](docs/dashboard.md) | Enable the local browser dashboard |
| [Contributing](CONTRIBUTING.md) | Open issues and pull requests |
| [Security policy](SECURITY.md) | Report vulnerabilities privately |

## Project status

Tebis is pre-1.0 and built for a focused personal workflow. It has CI coverage
for macOS, Linux, and Windows, but the command surface and config names may
still change before 1.0.

## License

MIT. See [LICENSE](LICENSE).

[botfather]: https://t.me/BotFather
[userinfobot]: https://t.me/userinfobot
[psmux]: https://github.com/psmux/psmux
