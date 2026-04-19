# Security policy

## Supported versions

tebis is pre-1.0. Only the current `master` / `main` branch is supported.
There are no back-ported fixes to earlier tags.

## Threat model

tebis is a single-user daemon. It is designed to be run by one person, on
their own host, against a Telegram bot whose token is treated as secret.

**In scope:**

- Remote command execution or tmux keystroke injection by anyone other
  than the configured numeric Telegram `user.id`.
- Leakage of the bot token through logs, error strings, or the inspect
  dashboard.
- Escape from the tmux session-name allowlist (shell metachars, path
  traversal, tmux exact-match syntax, prefix matching).
- Unauthorized access to the notify UDS from other local users.
- HTML injection in Telegram replies (`parse_mode=HTML`).
- Path-traversal or arbitrary file writes via the inspect dashboard's
  settings editor.

**Out of scope:**

- Anyone with local `root` or the ability to read `~/.config/tebis/env`.
- A malicious `tmux` binary on `PATH`.
- A compromised Telegram server or a hostile `@BotFather`.
- Denial-of-service from the authorized user themselves.

The `CLAUDE.md` file enumerates the specific invariants the code relies on.

## Reporting a vulnerability

Please do **not** file a public issue for suspected security problems.

If you found something, open a private advisory on the project's GitHub
repository ("Security" tab → "Report a vulnerability"), or email the
maintainer listed in `Cargo.toml` with:

- A short description of the impact.
- Repro steps (config / input / expected behavior).
- Whether you believe a fix is time-critical.

Expect an acknowledgement within a week. Coordinated disclosure is
preferred — we'll work with you on a timeline that lets deployed users
update before details go public.

## Hardening tips for deployments

- Run under a dedicated user account (the systemd unit in `contrib/linux/`
  is pre-sandboxed — audit with `systemd-analyze --user security tebis`).
- Keep `~/.config/tebis/env` at mode 0600.
- Use the strict allowlist (`TELEGRAM_ALLOWED_SESSIONS=…`) if you know
  ahead of time which session names you'll target.
- Disable "Allow Groups" in `@BotFather` so your bot cannot be added to a
  group chat it would read messages from.
- Only enable `INSPECT_PORT` if you need it; the dashboard is
  unauthenticated and relies on loopback-only binding + Origin-header CSRF
  protection.
