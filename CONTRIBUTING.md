# Contributing

Thanks for looking. Tebis is a small single-user daemon that drives real
processes through `tmux send-keys`, so the bar for security-sensitive
changes is intentionally high. Worth a read before your first PR.

## Ground rules

1. **Read [`CLAUDE.md`](CLAUDE.md).** It lists the security invariants
   (numbered 1–18) and architectural rules. Most were added after a
   concrete bug or CVE; don't weaken them without discussion.
2. **No new runtime dependencies without justification.** We build on
   `hyper` + `rustls` directly to keep the binary small and the audit
   surface tight. `deny.toml` bans `reqwest`, `openssl`, `native-tls`,
   and `aws-lc-rs`.
3. **Never log message content.** `message.text`, notify payloads, and
   pane output can carry secrets. Log metadata only (`chat_id`, `bytes`,
   `kind`).
4. **Escape every Telegram reply.** Route through
   `sanitize::escape_html`, and wrap `<pre>`/`<code>` content with
   `sanitize::wrap_and_truncate`.

## Where things live

```
src/main.rs              # argv dispatch, run loop
src/config.rs            # env → Config
src/bridge/              # per-message behavior
src/telegram/            # hand-rolled hyper/rustls Bot API client
src/tmux.rs              # send-keys / capture-pane wrapper
src/sanitize.rs          # C0/C1/bidi + HTML escape
src/notify/              # UDS listener for agent hooks
src/inspect/             # opt-in loopback HTML dashboard
src/agent_hooks/         # Claude Code + Copilot CLI hook install
src/audio/               # STT (whisper.cpp) + TTS backends
src/setup/               # first-run wizard
```

## Local checks

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets -- -D warnings -W clippy::pedantic -W clippy::nursery
cargo fmt --check
cargo audit
cargo deny check
```

CI runs the same on every push/PR and a daily audit cron.

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

Short imperative subject; body explains the *why* when non-obvious.

**Do not** add `Co-authored-by: Claude` / `Co-authored-by: Copilot` or
any AI-authored trailer.

## Scope

**Welcome:** new `/command`s, recovery paths, smaller binary, sharper docs,
additional agent hook integrations, new TTS backends.

**Probably rejected:** multi-tenant dispatch, username-based auth,
non-loopback web UI, TCP notify sockets, anything that widens the
single-user threat model. Open an issue to discuss before coding.

## Reporting bugs

Normal bugs → the issue tracker. Anything that looks like auth bypass,
token leak, or injection → follow [SECURITY.md](SECURITY.md) instead of
filing a public issue.
