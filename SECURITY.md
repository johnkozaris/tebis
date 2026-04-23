# Security policy

Tebis runs keystrokes into another process. The security model is worth
understanding before you deploy it.

## Supported versions

Pre-1.0. Only the current `master` branch is supported — no back-ported
fixes to earlier tags.

## Threat model

Tebis is a single-user daemon: one person, one host, one bot token
treated as secret.

**In scope**

- Remote command execution or tmux keystroke injection by anyone other
  than the configured numeric Telegram user id.
- Bot-token leakage through logs, error chains, or the inspect dashboard.
- Escape from the session-name allowlist (shell metachars, path
  traversal, tmux prefix matching).
- Unauthorized access to the notify UDS from other local users.
- HTML injection in Telegram replies (`parse_mode=HTML`).
- Path traversal or arbitrary writes via the dashboard settings editor.

**Out of scope**

- Local root, or anyone who can already read `~/.config/tebis/env`.
- A malicious `tmux` binary on `PATH`.
- A compromised Telegram server or `@BotFather`.
- Denial-of-service from the authorized user themselves.

See [CLAUDE.md](CLAUDE.md) for the numbered invariants the code relies on.

## Reporting a vulnerability

Please **do not** file a public issue for suspected security problems.

- Preferred: open a private advisory on GitHub ("Security" tab →
  "Report a vulnerability").
- Alternative: email the maintainer listed in `Cargo.toml`.

Include a short impact description, repro steps (config / input /
expected behavior), and whether you believe a fix is time-critical.

**Response:** acknowledgement within a week. Coordinated disclosure
preferred — we'll work with you on a timeline that lets deployed users
update before details go public.

## Hardening tips

- Run under a dedicated user account. The systemd unit in
  `contrib/linux/` is pre-sandboxed — audit with
  `systemd-analyze --user security tebis`.
- Keep `~/.config/tebis/env` at mode 0600 (the setup wizard does this
  for you; confirm after manual edits).
- Use a strict allowlist: set `TELEGRAM_ALLOWED_SESSIONS=…` when you
  know the session names you want to target.
- Disable "Allow Groups" in `@BotFather` so the bot can't be added to a
  group chat it would read from.
- Only enable `INSPECT_PORT` if you need it. The dashboard is
  unauthenticated and relies on loopback-only binding plus an
  Origin-header CSRF check.
- Keep the bot token in a secret manager (OpenBao, 1Password, etc.)
  rather than a shell rc file.
