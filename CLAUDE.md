# tebis — project notes

Personal Rust daemon that bridges Telegram ↔ tmux so a phone can drive AI
coding agents (Claude Code) running in tmux sessions.

## Layout

Split into two layers so plumbing and policy are testable independently:

**Plumbing** (pure API wrappers — no knowledge of commands or autostart):
- `src/main.rs` — poll loop, spawn-per-update, `CancellationToken` + `TaskTracker` shutdown
- `src/telegram.rs` — Bot API client (`thiserror` errors, 429/5xx retry, 409 bubbles up)
- `src/tmux.rs` — `send-keys` / `capture-pane` with per-session `tokio::Mutex`; returns typed `TmuxError` (`NotFound` / `AlreadyExists` / `EmptyInput` / …); owns `is_valid_session_name` (name-allowlist regex lives with the thing that enforces it)
- `src/notify/mod.rs` — optional UDS listener for hook-pushed summaries: `Forwarder` trait + `TelegramForwarder` + `Payload` + `NotifyConfig` + `spawn` entrypoint
- `src/notify/format.rs` — pure HTML body formatter (kind tag + header + `<pre>`)
- `src/notify/listener.rs` — UDS bind, accept loop, per-connection protocol. Parameterized over `Forwarder` for testability.
- `src/types.rs` — Telegram DTOs
- `src/config.rs` — env-var parsing only. Returns `Config`, `AutostartConfig` (defined in `session.rs`), `NotifyConfig` (defined in `notify/mod.rs`). Each consumer owns the shape of its own config.

**Behavior** (what happens on a new message):
- `src/bridge.rs` — per-message behavior: rate-limit → parse → execute → reply. The `HandlerContext` each spawned task gets. Instruments `Metrics` at each stage.
- `src/metrics.rs` — lock-free atomic counters + last-event timestamps. Shared via `Arc<Metrics>`; read by the inspect dashboard.
- `src/inspect.rs` — opt-in local HTML dashboard (`INSPECT_PORT=<n>` → `127.0.0.1:<n>`). Server-rendered via `format!`, zero JS, CSRF-safe POST actions, loopback-only enforced.
- `src/handler.rs` — command parse + execute. Clears stale `default_target` and retries provisioning once on `TmuxError::NotFound` for the plain-text path (with an explicit `kill_session` drain to break zombie-state loops).
- `src/session.rs` — `SessionState` owns `default_target` + `autostart` + its serialization lock; `resolve_or_autostart`, `resolve_explicit`, `clear_target_if`. Defines `AutostartConfig` and `ResolveError` (incl. `AutostartCommandDied` when the configured command exits during TUI-boot sleep).
- `src/security.rs` — numeric-ID auth + per-chat GCRA rate limiter (access-control primitives only)
- `src/sanitize.rs` — input/output sanitizers (C0/C1/bidi), `escape_html`, `wrap_and_truncate`

## Security invariants — do not weaken

These have been reviewed and justified. Relax any of them only with explicit
discussion.

1. **Auth by numeric `user.id` only.** Never by username (CVE-2026-28480).
2. **Session-name regex `[A-Za-z0-9._-]{1,64}` is always enforced** — at
   config load *and* at every `send_keys`/`capture_pane`/`kill_session`
   via `Tmux::slot()`. The regex is non-negotiable (shell metachar /
   path-traversal defense). The *allowlist* itself is opt-in: empty
   `TELEGRAM_ALLOWED_SESSIONS` puts `Tmux` in permissive mode, where
   slots are lazily created per regex-valid name. Permissive is the
   default for fresh installs; existing deployments that set a non-empty
   list keep strict behavior.
3. **`send-keys` uses `-l` then separate `-H 0d`.** No single-call key-name
   interpolation. The sequence must not be cancelled mid-way — do NOT wrap
   handlers in a cancel `select!`.
4. **All Telegram text replies go through `sanitize::escape_html` before
   `parse_mode=HTML`.** Error paths too. Use `wrap_and_truncate` for anything
   wrapped in `<pre>`/`<code>`; naive chunking splits tags and entities.
5. **Never log `message.text`.** `tracing::debug!` may emit to the journal;
   pasted secrets must not leak. Log `chat_id` and `bytes = text.len()` only.
6. **Network errors go through `redact_network_error` before reaching
   `TelegramError::Network`.** The bot token lives in the URL path
   (`/bot<TOKEN>/method`); hyper's `Display` chain could conceivably
   include the URI. We walk to the root cause, emit only that, and never
   format the `Request` or `Uri` into a log line. `TelegramClient::Debug`
   prints `base_url: "<redacted>"`.
7. **Low-level HTTP/TLS crates at `warn` in the tracing filter**
   (`hyper`, `hyper_util`, `rustls`). We disabled `http2` and `aws-lc-rs`
   features so `h2` and `aws-lc-*` don't ship in the binary at all.
8. **Per-session `tokio::Mutex`** serializes `send_keys` + `capture_pane` on
   the same session. `capture_pane` must actually acquire the guard (`let
   _guard = lock.lock().await`, not `let _ = ...`).
9. **Notify socket is UDS-only, mode 0600, explicit chmod after bind.**
   No TCP fallback — the listener must not be reachable over the network.
   `fs::set_permissions` on the path *after* `UnixListener::bind` because
   umask alone can't be trusted. Unlink any stale socket before bind.
10. **Notify payload max 16 KiB, per-connection read timeout 5 s.** Do not
    trust hook scripts to be well-behaved just because they're local.
11. **Notify protocol is newline-terminated JSON, not EOF-framed.** macOS's
    stock `nc` doesn't support `-N` (UDS half-close). Newline framing works
    with `nc -U -w 2` on every platform. Bridge's reader uses
    `read_until(b'\n')` with the 16 KiB cap.
12. **Per-connection notify tasks spawn on the shared `TaskTracker`.** So
    `tracker.wait()` drains in-flight deliveries at shutdown. Don't use
    bare `tokio::spawn` — that leaves the future orphaned on cancel.
13. **Every tmux `-t` target goes through `exact_target(session)`** which
    prepends `=`. Bare `-t name` does *prefix matching* — `/send foo`
    would land in an allowlisted `foobar` session. Allowlist prevents
    boundary escape, but cross-session drift is a real correctness bug.
14. **Autostart provisioning is serialized by a shared `tokio::Mutex<()>`.**
    Without it, concurrent plain-text messages race the TUI-boot sleep:
    the first spawns Claude, the second sees `has_session == true` and
    skips the wait, sending keystrokes before the TUI is ready.
15. **Stale-target recovery uses `TmuxError::NotFound`, not string
    matching.** tmux error wording differs by subcommand (`send-keys` says
    "can't find pane", others say "can't find session"); `classify_status`
    folds both into `NotFound` so `handler.rs` can safely match on the
    variant. `kill_session` is idempotent — `NotFound` folds into `Ok(())`.
    Plain-text auto-retries once via `resolve_or_autostart` on `NotFound`.
16. **Global handler concurrency cap via `tokio::Semaphore`**
    (`MAX_CONCURRENT_HANDLERS`). Acquired *after* the rate-limit check so
    rate-limited replies don't consume a work slot. Bounds subprocess
    fan-out when Telegram delivers a burst (offline-phone reconnect).
17. **UDS uses three-layer peer defense.** (a) `umask(0o177)` around
    `bind(2)` so the socket file is `0600` from creation — Linux honors
    socket-file perms for `connect`. (b) explicit `chmod 0600` as
    belt-and-suspenders against weird init umasks. (c) `peer_cred()` check
    on every accepted connection, rejecting any uid ≠ our euid. The cred
    check is the only authenticated gate — (a) and (b) close the TOCTOU
    window between bind and chmod. Do not remove any layer independently.

## Architectural rules

- **Hand-rolled Telegram client on `hyper` directly** (not `reqwest`).
  Build stack: `hyper` 1.x + `hyper-util` legacy `Client` (connection pool)
  + `hyper-rustls` `HttpsConnector` + `rustls` with **`ring`** crypto
  backend + `webpki-roots` for CAs. No `reqwest`, no `aws-lc-rs`, no
  `native-tls`, no native cert store. Enforced in `deny.toml`. Migrating
  to `reqwest` or switching rustls to `aws-lc-rs` requires removing the
  corresponding deny rules.
- **HTTP/1.1 only.** The Telegram Bot API is HTTP/1.1. We do not enable
  hyper's `http2` feature — that would pull `h2` back in for no gain.
- **Do not pull in `teloxide` / `telers`.** They add MB of unused
  dispatcher machinery and trail the Bot API version. If you want typed
  schemas without the framework, copy types from `frankenstein` (don't
  take the dep).
- **`std::sync::Mutex` at the application edge; `tokio::sync::Mutex` only
  where locks cross `.await`.** Current uses: `tokio` for per-session tmux
  locks; `std` for `default_target` and rate limiter.
- **`anyhow` at binary edge, `thiserror` inside modules** that need
  pattern-matching on error shape (e.g. `TelegramError::is_conflict`).
- **Single-message Telegram replies.** Handlers produce bodies guaranteed
  ≤ 4000 chars via `wrap_and_truncate`. No multi-chunk send path — it was
  removed because HTML-aware chunking is harder than truncation.
- **Notify delivery goes through the `Forwarder` trait.** Production wires
  `TelegramForwarder`; tests inject a recording fake. Keep the trait thin
  (one method) and don't leak `TelegramError` shape through it — collapse
  to `ForwardError::Delivery(String)` so the listener doesn't pattern-match
  on specifics it shouldn't act on.
- **For long assistant messages, tail-truncate, don't head-truncate.** The
  conclusion lives at the end. The hook script pairs this with a
  `UserPromptSubmit` wrap that asks Claude to end with a summary, so the
  tail *is* the summary in the common case.

## Build / test / audit

```sh
cargo test                                   # unit tests
cargo clippy --all-targets -- -D warnings    # base lints
cargo clippy --all-targets -- -D warnings -W clippy::pedantic -W clippy::nursery  # full
cargo audit                                  # RUSTSEC advisories
cargo deny check                             # licenses + bans + sources
cargo build --release                        # ~4.25 MB binary
```

CI runs audit + deny daily on a cron (`.github/workflows/audit.yml`).

## Secrets

Bot token belongs in OpenBao at `secret/telegram/bot-token/bridge` (see global
rules). For local/systemd/launchd, put env in `~/.config/tebis/env` with
mode 0600 (or use `tebis setup`). **Never commit a filled `.env`.**
`.env` is gitignored.

## Don't-dos

- Don't add `parse_mode=HTML` to an error without escaping the error content.
- Don't cancel the handler future mid-`send_keys` — text would stick in the
  pane without Enter and be prepended to the next command.
- Don't use `native-tls` / `openssl` — `deny.toml` forbids them.
- Don't narrow the tracing filter below `warn` for `hyper`/`reqwest`/`h2`/`rustls`.
- Don't set `limit > 100` on `getUpdates` — Telegram silently clamps to 100.
- Don't reuse `tokio::sync::Mutex` on short critical sections with no
  `.await` inside — use `std::sync::Mutex` and drop the guard before any
  await.
- Don't head-truncate Claude's last message when forwarding to Telegram.
  Tail-truncate instead; the conclusion is what a phone notification wants.
- Don't use a Stop-hook `{"decision":"block","reason":"summarize"}` pattern
  to force Claude to self-summarize — it burns a Sonnet turn and has
  documented loop bugs (claude-mem #987, #1460). Use the
  `UserPromptSubmit` + tail-truncate pair instead.
