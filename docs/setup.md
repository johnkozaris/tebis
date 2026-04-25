# Setup guide

This guide covers the full install path after you decide to try Tebis. For the
short version, run `tebis setup` and follow the prompts.

## 1. Install the platform requirements

| Platform | Required tools |
| --- | --- |
| macOS | Rust 1.95+, `tmux` 3.x, Xcode Command Line Tools |
| Linux | Rust 1.95+, `tmux` 3.x, C++ build tools, CMake |
| Windows | Rust 1.95+, `psmux`, Visual Studio Build Tools with C++, CMake |

On Windows, `tebis setup` can offer to install `psmux` if it detects Scoop,
WinGet, Chocolatey, or Cargo. Manual options:

```powershell
scoop bucket add psmux https://github.com/psmux/scoop-psmux
scoop install psmux

# or
winget install marlocarlo.psmux
choco install psmux
cargo install psmux
```

## 2. Build Tebis

```sh
git clone https://github.com/johnkozaris/tebis.git
cd tebis
cargo build --release
```

Windows PowerShell:

```powershell
git clone https://github.com/johnkozaris/tebis.git
cd tebis
cargo build --release
```

## 3. Run the setup wizard

macOS and Linux:

```sh
./target/release/tebis setup
```

Windows PowerShell:

```powershell
.\target\release\tebis.exe setup
```

The wizard asks for:

- a bot token from [`@BotFather`](https://t.me/BotFather)
- your numeric Telegram ID from [`@userinfobot`](https://t.me/userinfobot)
- optional session restrictions
- optional default agent command, such as `claude`
- optional agent hooks, dashboard, voice input, and voice replies

Tebis writes the config file here:

| Platform | Config file |
| --- | --- |
| macOS, Linux | `~/.config/tebis/env` |
| Windows | `%APPDATA%\tebis\env` |

## 4. Run Tebis

During setup, Tebis asks how you want to run it. You can start in the terminal
for a quick test or install it as a background service.

Service commands:

```sh
tebis install
tebis start
tebis status
tebis stop
tebis restart
tebis uninstall
```

`tebis install` registers a per-user background service:

| Platform | Background runner |
| --- | --- |
| macOS | launchd user agent |
| Linux | systemd user service |
| Windows | Task Scheduler at logon |

On Linux, run this once if you want Tebis to keep running after logout:

```sh
loginctl enable-linger "$USER"
```

## 5. Message your bot

Send plain text to the bot. If you configured a default agent, Tebis starts the
session on the first plain message and sends your text to it.

Useful commands:

| Command | What it does |
| --- | --- |
| `/list` | Lists running terminal sessions |
| `/status` | Shows current target and uptime |
| `/read [session] [lines]` | Reads recent output |
| `/target <session>` | Makes a session the default target |
| `/restart` | Restarts the default agent on the next message |
| `/help` | Shows all commands |

## Troubleshooting

| Symptom | What to check |
| --- | --- |
| Telegram says the bot token is unauthorized | Re-run `tebis setup` and paste a fresh token from `@BotFather` |
| The bot ignores your messages | Check `TELEGRAM_ALLOWED_USER`. It must be your numeric Telegram ID |
| Setup cannot find the terminal session tool | Install `tmux` on macOS/Linux or `psmux` on Windows |
| Config changes do not apply | Run `tebis restart` |
| Replies are late or missing | Use `/read` to inspect the session. For Claude Code or Copilot CLI, check `tebis hooks status` |
