# Security policy

## Supported versions

Pre-1.0. Only the current `master` branch is supported.

## Threat model

tebis is a single-user daemon. One person, one host, one bot token.

**In scope:**

- RCE / tmux keystroke injection by anyone other than the configured user id.
- Bot-token leakage through logs, errors, or the dashboard.
- Escape from the session-name allowlist (shell metachars, path traversal,
  tmux prefix matching).
- Unauthorized notify UDS access from other local users.
- HTML injection into Telegram replies.
- Path traversal via the dashboard settings editor.

**Out of scope:**

- Local root or anyone who can read `~/.config/tebis/env`.
- A malicious `tmux` on `PATH`.
- A compromised Telegram server.
- DoS by the authorized user.

See `CLAUDE.md` for the invariants the code relies on.

## Reporting a vulnerability

Do **not** file a public issue.

Open a private advisory on the repo ("Security" → "Report a vulnerability"),
or email the maintainer in `Cargo.toml` with: impact, repro steps, and
whether a fix is time-critical.

Acknowledgement within a week. Coordinated disclosure preferred.

## Hardening tips

- Run under a dedicated user (systemd unit in `contrib/linux/` is
  pre-sandboxed — `systemd-analyze --user security tebis`).
- Keep `~/.config/tebis/env` at mode 0600.
- Set `TELEGRAM_ALLOWED_SESSIONS=…` if you know your session names ahead
  of time.
- Disable "Allow Groups" in `@BotFather` so the bot can't be added to a
  group.
- Only enable `INSPECT_PORT` if you need it — the dashboard is
  unauthenticated and relies on loopback + Origin-header CSRF.
