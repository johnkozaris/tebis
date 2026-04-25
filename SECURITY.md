# Security policy

Tebis bridges Telegram to a local terminal session. Treat it like a tool that
can type into your shell.

## Supported versions

Tebis is pre-1.0. Security fixes are made on the current `main` branch.

## Security model

Tebis is designed for:

- one person
- one workstation
- one Telegram bot token
- local terminal sessions owned by that same OS user

It is not designed for shared team access or untrusted local users.

## In scope

Please report issues that could let someone:

- send commands without matching `TELEGRAM_ALLOWED_USER`
- bypass the terminal session name checks
- make Tebis leak a bot token or API key through logs, errors, or the dashboard
- inject Telegram HTML into replies
- write outside the intended config file through the dashboard
- send local agent notifications as another OS user

## Out of scope

These are outside the supported threat model:

- local root or an admin account on the machine
- someone who can already read your Tebis config file
- a malicious `tmux`, `psmux`, Claude Code, or Copilot CLI binary on `PATH`
- a compromised Telegram account, Telegram server, or BotFather
- denial of service caused by the authorized user

## Report a vulnerability

Please do not file a public issue for suspected security problems.

Preferred path: use GitHub private vulnerability reporting from the repository
Security tab.

If private reporting is not available, open a public issue asking for a private
contact method, but do not include vulnerability details.

Include:

- what can go wrong
- how to reproduce it
- your operating system
- relevant config shape with secrets removed
- whether you believe the issue is time-sensitive

You can expect acknowledgement within a week. Coordinated disclosure is
preferred so users can update before details are public.

## Hardening tips

- Disable "Allow Groups" in `@BotFather` so your bot cannot be added to group
  chats.
- Keep the Tebis config file private. `tebis setup` writes it with owner-only
  permissions.
- Use `TELEGRAM_ALLOWED_SESSIONS` when you know exactly which sessions Tebis
  should control.
- Enable the dashboard only when you need it, and never expose it outside
  `127.0.0.1`.
- Keep the bot token in a password manager or secret manager if you manage
  config outside `tebis setup`.
