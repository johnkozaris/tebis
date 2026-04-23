# tebis — project notes

Personal Rust daemon that bridges Telegram ↔ terminal multiplexer
(tmux on Unix, psmux on Windows) so a phone can drive AI coding
agents (Claude Code, Copilot CLI) running in a multiplexer session.

## Layout

Split into three layers so plumbing, per-OS primitives, and policy
are testable independently.

**Plumbing** — pure I/O wrappers, no knowledge of commands or autostart:
- `src/main.rs` — poll loop, spawn-per-update, `CancellationToken` + `TaskTracker` shutdown, 401 dead-end
- `src/telegram/mod.rs` — Bot API client (`thiserror` errors, 429/5xx retry, 409/401 expose via `is_conflict`/`is_unauthorized`)
- `src/telegram/types.rs` — Telegram DTOs
- `src/config.rs` — env-var parsing only. Populates `Config`; every subsystem owns the shape of its own config type (`AutostartConfig` in `bridge::session`, `NotifyConfig` in `notify`, `AutoreplyConfig` in `bridge::autoreply`, `HooksMode` in `agent_hooks`).
- `src/env_file.rs` — shared env-file utilities. `atomic_write_0600` (thin wrapper over `platform::secure_file::atomic_write_private`), `parse_kv_line`, `parse_toggle`.
- `src/lockfile.rs` — single-instance advisory lock via `std::fs::File::try_lock` (stable 1.89 — `flock(2)` on Unix, `LockFileEx` on Windows). Path resolved through `platform::paths::lock_file_path`.

**Per-OS primitives** (`src/platform/`) — where Unix and Windows diverge.
Each submodule exposes one cross-platform API; backends live side-by-
side inside the module so callers never need `#[cfg]` inline. See
[`feedback_platform_separation.md`](memory) for the "large vs small
divergence" rule.
- `platform/signal.rs` — `shutdown_signal()` (SIGINT+SIGTERM on Unix, Ctrl+C on Windows)
- `platform/hostname.rs` — `current()` (`gethostname(2)` / `%COMPUTERNAME%`)
- `platform/process.rs` — `kill_and_wait(pid)` (SIGTERM→SIGKILL on Unix, `taskkill /T` → `/F` on Windows)
- `platform/paths.rs` — `config_dir`, `data_dir`, `env_file_path`, `lock_file_path`, `notify_address`, `models_dir`, `hook_manifest_path`. XDG on Linux, Apple-ish on macOS, Known Folder API on Windows. Tests override via `TEBIS_SCRATCH_DIR`.
- `platform/secure_file.rs` — `atomic_write_private` (0600 on Unix; owner-only DACL + MoveFileExW-replace on Windows), `ensure_private_dir`, `set_owner_executable`
- `platform/peer_listener/{mod,unix,windows}.rs` — local IPC listener restricted to same-user peers. Unix UDS + umask/chmod/peer_cred; Windows Named Pipe + SDDL `D:P(A;;GA;;;<SID>)` + `ImpersonateNamedPipeClient` + `TokenUser` SID equality (invariant 20).
- `platform/multiplexer.rs` — `Mux` struct driving the tmux-compatible CLI; `BINARY` cfg-gated to `tmux` on Unix, `psmux` on Windows.
- `platform/windows_auth.rs` — shared SID/SDDL/SECURITY_DESCRIPTOR helpers + `HandleGuard`, consumed by both `peer_listener::windows` and `secure_file::windows`.

**Behavior** — what happens per message:
- `src/bridge/mod.rs` — rate-limit → parse → execute → reply routing (hook-driven / pane-settle / bare 👍). Owns `HandlerContext` (includes the shared `TaskTracker`). Instruments `Metrics` at each stage.
- `src/bridge/handler.rs` — command parse + execute. Clears stale `default_target` and retries provisioning once on `MuxError::NotFound` for the plain-text path (with an explicit `kill_session` drain to break zombie-state loops).
- `src/bridge/session.rs` — `SessionState` owns `default_target` + `autostart` + its serialization lock + `hooked_sessions` set; `resolve_or_autostart`, `resolve_explicit`, `clear_target_if`, `mark_hooked`/`unmark_hooked`/`is_hooked`. Defines `AutostartConfig` and `ResolveError` (incl. `AutostartCommandDied`). Hook install runs OUTSIDE the autostart lock — it's idempotent atomic writes with no ordering dep on provisioning.
- `src/bridge/autoreply.rs` — TUI-agnostic pane-settle reply detection (Braille-spinner-tolerant hash + diff-against-baseline). Owns `AutoreplyConfig` (tunings live with the consumer).
- `src/bridge/typing.rs` — `TypingGuard` RAII handle + `spawn_with_cap` free fn. Every typing-indicator spawn goes on the shared `TaskTracker` (invariant 12).

**Shared utilities:**
- `src/sanitize.rs` — input/output sanitizers (C0/C1/bidi), `escape_html`, `wrap_and_truncate`
- `src/security.rs` — numeric-ID auth + per-chat GCRA rate limiter
- `src/metrics.rs` — lock-free atomic counters, shared via `Arc<Metrics>`

**Subsystems:**
- `src/inspect/{mod,server,render}.rs` — opt-in local HTML dashboard. `INSPECT_PORT=<n>` → `127.0.0.1:<n>`. Loopback-only, CSRF-checked, zero JS. `server.rs` handles HTTP + routing + env-file I/O via `env_file::atomic_write_0600`; `render.rs` handles HTML + JSON + inline CSS. `HooksInfo` row shows mode + every project dir from the manifest.
- `src/notify/{mod,listener,format}.rs` — opt-in listener for hook-pushed summaries. Transport is `platform::peer_listener` (UDS on Unix, Named Pipe on Windows — both owner-only, peer-authed). `mod.rs` owns `Forwarder` trait + `TelegramForwarder` + `Payload`. `listener.rs` is pure protocol (newline-framed JSON, 16 KiB cap). `format.rs` is HTML body formatting.
- `src/setup/{mod,steps,discover,ui}.rs` — six-step first-run wizard. `mod.rs` runs steps + preserves user-added env keys across re-runs. `steps.rs` has each step fn + validators (step 5 is hook-mode, defaulting Auto when the autostart command resolves to a known agent). `discover.rs` parses existing env via `env_file::parse_kv_line`. `ui.rs` is the terminal rendering primitives.
- `src/agent_hooks/{mod,agent,claude,copilot,manifest,jsonfile,test_support}.rs` — native-hook installation for Claude Code + Copilot CLI. `agent.rs` owns `AgentKind` + `HooksMode`. `claude.rs` merges into `.claude/settings.local.json` (lowest-precedence project layer); `copilot.rs` writes a single sentinel `.github/hooks/tebis.json`. Hook scripts embedded via `include_str!` — `.sh` on Unix, `.ps1` on Windows (per-OS cfg-gated constants). `script_command(script_path)` produces the per-OS command string (raw path on Unix, `powershell.exe -NoProfile -ExecutionPolicy Bypass -File "<path>"` on Windows). `manifest.rs` tracks every project-dir/agent pair at `platform::paths::hook_manifest_path()`.
- `src/hooks_cli.rs` — `tebis hooks {install,uninstall,status,list}`. Unix: probes `jq` + `nc` on `$PATH`. Windows: probes `powershell.exe` / `pwsh.exe`. Both scan for legacy (pre-Phase-2) hook entries.
- `src/service/{mod,unix,windows}.rs` — per-OS service install. `unix.rs`: launchd on macOS, systemd user on Linux. `windows.rs`: Task Scheduler via `schtasks.exe /Create /SC ONLOGON /RL LIMITED` — runs in the user's session so per-user paths + Git Bash + Claude Code autostart all work (SCM services default to LocalSystem, which would break all of that).

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
3. **`send_keys` is a single atomic sequence: `-l` text → Ink-render
   sleep → `-H 0d` Enter, under one per-session mutex acquisition.**
   The text-then-Enter pair must land on the same agent turn. No
   single-call key-name interpolation (`send-keys -l 'foo Enter'`
   would shell-parse the space). The tokio mutex is held across all
   three calls so a concurrent `/send` can't interleave its text
   before our Enter. Do NOT wrap `send_keys` in a cancel `select!` —
   mid-sequence cancellation strands chars without Enter and they
   prepend to the next command.
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
9. **Notify listener is local-user-only, per-OS owner-restricted.**
   No TCP fallback on any platform — the listener must not be reachable
   over the network. Unix: UDS with mode 0600 + explicit chmod after
   bind (`fs::set_permissions` on the path *after* `UnixListener::bind`
   because umask alone can't be trusted; unlink any stale socket first).
   Windows: Named Pipe with an owner-only DACL set at `CreateNamedPipeW`
   via SDDL `D:P(A;;GA;;;<OUR_SID>)` — no inheritance, no other
   principals. See invariants 17 + 20 for the peer-auth gate.
10. **Notify payload max 16 KiB, per-connection read timeout 5 s.** Do not
    trust hook scripts to be well-behaved just because they're local.
11. **Notify protocol is newline-terminated JSON, not EOF-framed.** macOS's
    stock `nc` doesn't support `-N` (UDS half-close). Newline framing
    works with `nc -U -w 2` on Unix and with
    `System.IO.StreamWriter.WriteLine` over
    `NamedPipeClientStream` on Windows (see
    `contrib/claude/claude-hook.ps1`). Bridge's reader uses
    `read_until(b'\n')` with the 16 KiB cap on both transports.
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
15. **Stale-target recovery uses `MuxError::NotFound`, not string
    matching.** Multiplexer error wording differs by subcommand
    (`send-keys` says "can't find pane", others say "can't find
    session"; psmux uses "no such session" / "session not found");
    `classify_status` folds them all into `NotFound` so `handler.rs`
    can safely match on the variant. `kill_session` is idempotent —
    `NotFound` folds into `Ok(())`. Plain-text auto-retries once via
    `resolve_or_autostart` on `NotFound`.
16. **Global handler concurrency cap via `tokio::Semaphore`**
    (`MAX_CONCURRENT_HANDLERS`). Acquired *after* the rate-limit check so
    rate-limited replies don't consume a work slot. Bounds subprocess
    fan-out when Telegram delivers a burst (offline-phone reconnect).
17. **UDS (Unix) uses three-layer peer defense.** (a) `umask(0o177)`
    around `bind(2)` so the socket file is `0600` from creation —
    Linux honors socket-file perms for `connect`. (b) explicit
    `chmod 0600` as belt-and-suspenders against weird init umasks.
    (c) `peer_cred()` check on every accepted connection, rejecting
    any uid ≠ our euid. The cred check is the only authenticated
    gate — (a) and (b) close the TOCTOU window between bind and
    chmod. Do not remove any layer independently. **Windows
    equivalent is invariant 20.**
18. **STT transcripts are byte-capped at `MAX_TRANSCRIPT_BYTES` (4000)
    before entering `handler::parse`.** Matches
    `TELEGRAM_MAX_OUTPUT_CHARS`'s upper bound. Without this, a long
    noisy voice recording could paste 100+ KiB of transcribed text
    into tmux and bypass every text-message size limit. The cap is in
    bytes (not chars) because `text.len()` is bytes; the config key
    uses "CHARS" for historical reasons.
19. **Named-Pipe peer auth (Windows) uses `ImpersonateNamedPipeClient` +
    `OpenThreadToken(TOKEN_QUERY)` + `GetTokenInformation(TokenUser)` +
    `EqualSid` — never `GetNamedPipeClientProcessId`.** PID is spoofable
    (Project Zero 2019-09). The impersonation token is kernel-verified.
    `RevertToSelf` must fire on every exit path — `RevertGuard` Drop
    handles panic + early-return. This is the Windows analogue of
    invariant 17's `peer_cred` gate.
20. **Secure file writes go through `platform::secure_file::atomic_write_private`.**
    Unix: `O_CREAT | O_WRONLY | O_TRUNC` with `mode(0o600)` + post-write
    `chmod 0o600` + atomic rename + best-effort parent fsync.
    Windows: `CreateFileW` with `SECURITY_ATTRIBUTES` holding a DACL of
    `D:P(A;;FA;;;<OUR_SID>)` (set at creation, no TOCTOU window) plus
    `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` for atomic replace. Do
    not roll your own write-then-chmod or remove-then-rename — both
    have windows the primitive closes.

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

## Agent hooks — April 2026 reality

**Claude Code** ships 25+ events. We install the four that matter for a
chat-forwarding bridge: `UserPromptSubmit` (inject "conclude with a
summary" context), `Stop` (forward the final assistant message),
`SubagentStop` (tagged `[agent]`), `Notification` (permission /
idle prompts). Claude hot-reloads `.claude/settings.local.json` — no
session restart needed after install. Non-zero exit is non-blocking on
every event we install; exit 2 is the only blocking signal and we
never emit it. Default timeout is 600 s; we set 5–15 s which is plenty.

**Copilot CLI** (v1.0.32, GA 2026-02-25) does **not** ship an
`agentStop` event — that's Claude-Code-only. The closest signal is
`notification` (async on agent completion, permission prompts, idle).
We install just `userPromptSubmitted` + `notification`. Copilot loads
every `*.json` in `.github/hooks/` and merges them, so our sentinel
file (`tebis.json`) co-exists cleanly with user files. Per-turn reply
delivery via hooks is less precise on Copilot than on Claude; pane-settle
is the universal fallback.

**Dependencies**: the embedded hook scripts shell out to `jq` and `nc`
(BSD netcat on macOS; `netcat-openbsd` on Debian/Ubuntu). `tebis hooks
install` probes PATH and warns up front when either is missing —
otherwise the hook fires silently and nothing arrives at the bridge.

**Ownership**: neither agent's schema has a provenance field. We
identify tebis-owned entries by matching `command` / `bash` fields to
scripts whose parent dir is `$XDG_DATA_HOME/tebis/`. On uninstall,
both installers probe `data_dir()` up front so an unresolvable `$HOME`
fails loudly instead of silently leaving hooks in place (symptom of
a bug we fixed: `is_our_script` returned `false` on error, making
every entry look user-owned).

**Manifest**: `$XDG_DATA_HOME/tebis/installed.json` records every
`(agent, project_dir, timestamp)` tuple. Updated on every install /
uninstall. Read by `tebis hooks list` and the inspect dashboard so
users can enumerate installs across the whole host.

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
