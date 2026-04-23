# Contributing

tebis is a single-user daemon that drives other processes via `tmux send-keys`.
The bar for security-sensitive changes is high — read this before filing a PR.

## Ground rules

1. **Read `CLAUDE.md`.** It lists the security invariants. Most were added
   after a concrete bug or CVE. Don't weaken them without explicit discussion.
2. **No new runtime dependencies without justification.** `deny.toml` bans
   `reqwest`, `openssl`, `native-tls`, `aws-lc-rs`.
3. **Don't log message content.** `message.text`, notify payloads, and pane
   output can carry secrets. Log metadata only (`chat_id`, `bytes`, `kind`).
4. **Every Telegram reply goes through `sanitize::escape_html`**, and
   `<pre>`/`<code>`-wrapped content through `sanitize::wrap_and_truncate`.

## Local checks

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets -- -D warnings -W clippy::pedantic -W clippy::nursery
cargo fmt --check
cargo audit
cargo deny check
```

CI runs the same on every push/PR plus a daily audit cron.

## PR checklist

- [ ] Tests pass.
- [ ] Clippy pedantic/nursery clean.
- [ ] `cargo fmt --check` clean.
- [ ] `cargo deny check` clean (if deps changed).
- [ ] No `unwrap()` outside tests unless the invariant is documented.
- [ ] Every `unsafe` block has a `// SAFETY:` comment.
- [ ] Behavioral changes have tests.
- [ ] `CLAUDE.md` invariants reflect any security-relevant change.

## Commits

Short imperative subject; body explains the *why*.

Do **not** add `Co-authored-by: Claude` / `Co-authored-by: Copilot` /
any AI trailer.

## Bugs and security

Public issues for bugs; private advisory or `SECURITY.md` for anything
that looks like auth bypass, token leak, or injection.

## Scope

Nice: new `/command`s, recovery paths, smaller binary, better docs.
Probably rejected: multi-tenant dispatch, username-based auth, non-loopback
web UI, TCP notify sockets. Open an issue first if unsure.
