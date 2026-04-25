# Configuration

The setup wizard writes the config file for you. Edit it by hand only when you
want to tune Tebis after setup.

| Platform | Config file |
| --- | --- |
| macOS, Linux | `~/.config/tebis/env` |
| Windows | `%APPDATA%\tebis\env` |

After changing the file, restart Tebis:

```sh
tebis restart
```

See [`.env.example`](../.env.example) for a copy-paste starting point.

## Required settings

| Variable | Meaning |
| --- | --- |
| `TELEGRAM_BOT_TOKEN` | Token from `@BotFather` |
| `TELEGRAM_ALLOWED_USER` | Your numeric Telegram user ID |

## Session access

| Variable | Default | Meaning |
| --- | --- | --- |
| `TELEGRAM_ALLOWED_SESSIONS` | empty | Comma-separated session names Tebis may control. Empty means any valid name |

Session names may contain only letters, numbers, `.`, `_`, and `-`, up to 64
characters.

## Default agent

Set all three values together:

| Variable | Meaning |
| --- | --- |
| `TELEGRAM_AUTOSTART_SESSION` | Session name for the default agent |
| `TELEGRAM_AUTOSTART_DIR` | Project directory for the default agent |
| `TELEGRAM_AUTOSTART_COMMAND` | Command to run in that project, such as `claude` |

When no target is selected, the first plain Telegram message starts this
session and sends the message to it.

## Telegram polling and output size

| Variable | Default | Meaning |
| --- | --- | --- |
| `TELEGRAM_POLL_TIMEOUT` | `30` | Long-poll seconds, from 1 to 900 |
| `TELEGRAM_MAX_OUTPUT_CHARS` | `4000` | Maximum size for command output sent to Telegram |

## Replies

| Variable | Default | Meaning |
| --- | --- | --- |
| `TELEGRAM_AUTOREPLY` | `on` | Set `off` to stop watching terminal output for replies |
| `TELEGRAM_HOOKS_MODE` | `off` | Set `auto` to install supported Claude Code or Copilot CLI hooks for the default project |
| `TELEGRAM_NOTIFY` | `on` | Set `off` to disable local hook notifications |
| `NOTIFY_CHAT_ID` | your user ID | Override which chat receives hook replies |
| `NOTIFY_SOCKET_PATH` | platform default | Unix-only local socket path for hook replies |

For details, see [Agent hooks](hooks.md).

## Dashboard

| Variable | Meaning |
| --- | --- |
| `INSPECT_PORT` | Enables the dashboard on `127.0.0.1:<port>` |
| `BRIDGE_ENV_FILE` | Enables settings edits from the dashboard |

For details, see [Dashboard](dashboard.md).

## Voice

| Variable | Default | Meaning |
| --- | --- | --- |
| `TELEGRAM_STT` | `off` | Set `on` to transcribe Telegram voice notes locally |
| `TELEGRAM_STT_MODEL` | `small.en` | Whisper model key |
| `TELEGRAM_STT_LANGUAGE` | `en` | Spoken language |
| `TELEGRAM_STT_MAX_DURATION_SEC` | `120` | Maximum voice note length |
| `TELEGRAM_TTS_BACKEND` | `off` | `say`, `winrt`, `kokoro-local`, `kokoro-remote`, or `off` |
| `TELEGRAM_TTS_VOICE` | backend default | Voice name or backend-specific voice key |
| `TELEGRAM_TTS_RESPOND_TO_ALL` | `off` | Set `on` to send voice replies for every response |
| `TELEGRAM_TTS_REMOTE_URL` | unset | OpenAI-compatible speech endpoint for `kokoro-remote` |
| `TELEGRAM_TTS_REMOTE_API_KEY` | unset | Optional API key for remote speech |
| `TELEGRAM_TTS_REMOTE_MODEL` | `kokoro` | Remote speech model name |
| `TELEGRAM_TTS_REMOTE_TIMEOUT_SEC` | `10` | Remote speech request timeout |
| `TELEGRAM_TTS_REMOTE_ALLOW_HTTP` | `off` | Set `on` only for trusted LAN HTTP endpoints |

For details, see [Voice](voice.md).
