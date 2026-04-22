# Contributing to tebis

Thanks for the interest. tebis is a small, single-user daemon that drives
another process over `tmux send-keys`, so the bar for changes — especially
to security-sensitive paths — is intentionally high. Please read this
before filing a PR.

## Ground rules

1. **Read `CLAUDE.md`.** It enumerates security invariants (1–19) and
   architectural rules that must not be weakened without explicit
   discussion. Most of them have been reached after a concrete bug, CVE,
   or misbehaving client.
2. **No new runtime dependencies without a justification.** We run on
   `hyper` + `rustls` (ring) directly to keep the binary and audit surface
   small. `deny.toml` bans `reqwest`, `openssl`, `native-tls`, and
   `aws-lc-rs`. If your PR needs one of those, expect a hard conversation.
3. **Don't log message content.** `message.text`, notify payload `text`,
   and tmux pane output can carry secrets. Log metadata only
   (`chat_id`, `bytes`, `kind`).
4. **Don't relax HTML-escaping.** Every Telegram reply must go through
   `sanitize::escape_html` and, for `<pre>`/`<code>`-wrapped content,
   `sanitize::wrap_and_truncate`.

## Local development

```sh
cargo test                                               # unit tests
cargo clippy --all-targets -- -D warnings                # base lints
cargo clippy --all-targets -- -D warnings \
    -W clippy::pedantic -W clippy::nursery               # pedantic pass
cargo fmt --check                                        # style
cargo audit                                              # RUSTSEC advisories
cargo deny check                                         # licenses + bans + sources
```

CI runs the equivalent on every push / PR. The audit workflow also runs
daily on a cron so the main branch reports drift independently of pushes.

## PR checklist

Before opening a PR:

- [ ] `cargo test` passes locally.
- [ ] `cargo clippy --all-targets -- -D warnings -W clippy::pedantic -W clippy::nursery` is clean.
- [ ] `cargo fmt --check` is clean.
- [ ] `cargo deny check` is clean (if you touched deps).
- [ ] No `unwrap()` on non-test code paths unless the invariant is
      documented inline (comment or `.expect("why this cannot fail")`).
- [ ] `unsafe` blocks have a `// SAFETY:` comment explaining why the
      preconditions hold.
- [ ] No new `.env` values or paths hard-coded to a specific machine or user.
- [ ] Tests accompany behavioral changes (parsing, sanitization, tmux error
      classification, etc.).
- [ ] If you changed security-sensitive code, the `CLAUDE.md` invariants
      section reflects the new reality.

## Commit style

Short, imperative summary on the subject line. Body explains the *why* when
non-obvious — git history is the first place a future reader will look.

Do **not** sign commits with `Co-authored-by: Claude` / `Co-authored-by:
Copilot` / any AI-authored-by trailer.

## Reporting bugs

Use the project's issue tracker. For anything that looks like a security
issue (auth bypass, token leak, shell / tmux injection, path traversal),
please follow [SECURITY.md](SECURITY.md) instead of filing a public issue.

## Scope

tebis is intentionally single-user, single-binary, and opinionated. Nice
additions: a new `/command`, more robust recovery paths, smaller binary,
better docs. Probably-rejected additions: multi-tenant dispatch,
auth-via-usernames, a web UI that isn't loopback-only, TCP-reachable notify
sockets. If you're unsure, open an issue to discuss before coding.
