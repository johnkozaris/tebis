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
- `src/config.rs` — env-var parsing only. Populates `Config`; every subsystem owns the shape of its own config type (`AutostartConfig` in `bridge::session`, `NotifyConfig` in `notify`, `HooksMode` in `agent_hooks`).
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
- `platform/paths.rs` — `config_dir`, `data_dir`, `env_file_path`, `lock_file_path`, `notify_address`, `models_dir`, `home_dir`. XDG on Linux, Apple-ish on macOS, Known Folder API on Windows. Tests override via `TEBIS_SCRATCH_DIR`.
- `platform/secure_file.rs` — `atomic_write_private` (0600 on Unix; owner-only DACL + MoveFileExW-replace on Windows), `ensure_private_dir`, `set_owner_executable`
- `platform/peer_listener/{mod,unix,windows}.rs` — local IPC listener restricted to same-user peers. Unix UDS + umask/chmod/peer_cred; Windows Named Pipe + SDDL `D:P(A;;GA;;;<SID>)` + `ImpersonateNamedPipeClient` + `TokenUser` SID equality.
- `platform/multiplexer.rs` — `Mux` struct driving the tmux-compatible CLI; `BINARY` cfg-gated to `tmux` on Unix, `psmux` on Windows.
- `platform/windows_auth.rs` — shared SID/SDDL/SECURITY_DESCRIPTOR helpers + `HandleGuard`, consumed by both `peer_listener::windows` and `secure_file::windows`.

**Behavior** — what happens per message:
- `src/bridge/mod.rs` — auth → parse → execute → reply routing. After `send_keys`, fires a capped typing indicator and lets the independent notify path (`notify::listener`) deliver hook events. Owns `HandlerContext` (includes the shared `TaskTracker`). Instruments `Metrics` at each stage.
- `src/bridge/handler.rs` — command parse + execute. Clears stale `default_target` and retries provisioning once on `MuxError::NotFound` for the plain-text path (with an explicit `kill_session` drain to break zombie-state loops).
- `src/bridge/session.rs` — `SessionState` owns `default_target` + `autostart` + its serialization lock; `resolve_or_autostart`, `resolve_explicit`, `clear_target_if`. Defines `AutostartConfig` and `ResolveError` (incl. `AutostartCommandDied`). Hook install runs OUTSIDE the autostart lock — it's idempotent atomic writes with no ordering dep on provisioning.
- `src/bridge/typing.rs` — `TypingGuard` RAII handle + `spawn_with_cap` free fn. Every typing-indicator spawn goes on the shared `TaskTracker` so shutdown drains them.

**Shared utilities:**
- `src/sanitize.rs` — input/output sanitizers (C0/C1/bidi), `escape_html`, `wrap_and_truncate`
- `src/security.rs` — numeric-ID auth gate
- `src/metrics.rs` — lock-free atomic counters, shared via `Arc<Metrics>`

**Subsystems:**
- `src/inspect/{mod,server,render}.rs` — opt-in local HTML dashboard. `INSPECT_PORT=<n>` → `127.0.0.1:<n>`. Loopback-only, CSRF-checked, zero JS. `server.rs` handles HTTP + routing + env-file I/O via `env_file::atomic_write_0600`; `render.rs` handles HTML + JSON + inline CSS. `HooksInfo` row shows mode + every project dir from the manifest.
- `src/notify/{mod,listener,format}.rs` — opt-in listener for hook-pushed summaries. Transport is `platform::peer_listener` (UDS on Unix, Named Pipe on Windows — both owner-only, peer-authed). `mod.rs` owns `Forwarder` trait + `TelegramForwarder` + `Payload`. `listener.rs` is pure protocol (newline-framed JSON, 16 KiB cap). `format.rs` is HTML body formatting.
- `src/setup/{mod,steps,discover,ui}.rs` — six-step first-run wizard. `mod.rs` runs steps + preserves user-added env keys across re-runs. `steps.rs` has each step fn + validators (step 5 is hook-mode, defaulting Auto when the autostart command resolves to a known agent). `discover.rs` parses existing env via `env_file::parse_kv_line`. `ui.rs` is the terminal rendering primitives.
- `src/agent_hooks/{mod,agent,claude,copilot,manifest,jsonfile,test_support}.rs` — native-hook installation for Claude Code + Copilot CLI. `agent.rs` owns `AgentKind` + `HooksMode`. `claude.rs` merges into `.claude/settings.local.json` (lowest-precedence project layer); `copilot.rs` writes a single sentinel `.github/hooks/tebis.json`. Hook scripts embedded via `include_str!` — `.sh` on Unix, `.ps1` on Windows (per-OS cfg-gated constants). `script_command(script_path)` produces the per-OS command string (raw path on Unix, `powershell.exe -NoProfile -ExecutionPolicy Bypass -File "<path>"` on Windows). `manifest.rs` tracks every project-dir/agent pair at `$XDG_DATA_HOME/tebis/installed.json`.
- `src/hooks_cli.rs` — `tebis hooks {install,uninstall,status,list}`. Unix: probes `jq` + `nc` on `$PATH`. Windows: probes `powershell.exe` / `pwsh.exe`. Both scan for legacy (pre-Phase-2) hook entries.
- `src/service/{mod,unix,windows}.rs` — per-OS service install. `unix.rs`: launchd on macOS, systemd user on Linux. `windows.rs`: Task Scheduler via `schtasks.exe /Create /SC ONLOGON /RL LIMITED` — runs in the user's session so per-user paths + Git Bash + Claude Code autostart all work (SCM services default to LocalSystem, which would break all of that).

## Security guarantees — do not weaken

This is a single-user personal daemon. The list below is the small set
of properties that actually matter; relax any of them only with
explicit discussion.

1. **Auth by numeric `user.id` only.** Never by username (CVE-2026-28480).
   `is_authorized` returns false for any update without a `from.id` in
   the configured allowlist; the polling loop drops the update.

2. **Session-name regex `[A-Za-z0-9._-]{1,64}` is always enforced** at
   every `send_keys` / `kill_session` via
   `Mux::slot()`. Shell-metachar / path-traversal defense — non
   negotiable. The optional allowlist (`TELEGRAM_ALLOWED_SESSIONS`) is
   layered on top: empty list = permissive (any regex-valid name is
   lazily slotted), non-empty list = strict.

3. **`send_keys` is a single atomic sequence under one per-session
   `tokio::Mutex`.** `-l` text → Ink-render sleep → `-H 0d` Enter, all
   three calls under the same guard. Cancellation mid-sequence would
   strand chars without the trailing Enter and they'd prepend to the
   next command. Do NOT wrap `send_keys` in a cancel `select!`.

4. **Per-connection notify tasks spawn on the shared `TaskTracker`,
   and the TTS/voice send paths use it too.** `tracker.wait()` drains
   in-flight work at shutdown. Bare `tokio::spawn` would orphan
   futures on cancel.

5. **Autostart provisioning is serialized by a shared `tokio::Mutex<()>`.**
   Without it, concurrent plain-text messages race the TUI-boot sleep:
   the first spawns Claude, the second sees `has_session == true`,
   skips the wait, and sends keystrokes before the TUI is ready.

Other useful but less load-bearing properties (still enforced; just
less likely to silently break):

- All Telegram replies go through `sanitize::escape_html` before
  `parse_mode=HTML`. Use `wrap_and_truncate` for `<pre>`/`<code>` —
  naive chunking splits tags and entities.
- Never log `message.text`. `tracing::debug!` may emit to the journal;
  pasted secrets must not leak. Log `chat_id` and `bytes = text.len()`.
- Network errors flow through `redact_hyper_error_string` with a
  per-endpoint predicate before reaching `TelegramError::Network`. The
  bot token lives in the URL path (`/bot<TOKEN>/method`); reqwest's
  `Display` chain could conceivably include the URI. `TelegramClient`'s
  manual `Debug` prints `base_url: "<redacted>"`.
- Notify payload max 16 KiB, per-connection read timeout 5 s. Hook
  scripts are local but not implicitly trustworthy.
- Notify protocol is newline-terminated JSON, not EOF-framed (macOS
  stock `nc` lacks `-N`). `read_until(b'\n')` with the 16 KiB cap.
- Every tmux `-t` target goes through `exact_target(session)` which
  prepends `=`. Bare `-t name` does prefix matching — `/send foo`
  could land in an allowlisted `foobar` session.
- Stale-target recovery uses `MuxError::NotFound`, not string matching
  on stderr. `classify_status` folds the various phrasings (tmux's
  "can't find pane" / "can't find session", psmux's "no such session"
  / "no server running on") into the variant.
- STT transcripts are byte-capped at `MAX_TRANSCRIPT_BYTES` (4000)
  before entering `handler::parse`. Without this, a long noisy voice
  recording could paste 100+ KiB of transcribed text into tmux and
  bypass every text-message size limit.
- Secure file writes go through
  `platform::secure_file::atomic_write_private`. Unix:
  `O_CREAT | O_WRONLY | O_TRUNC` + `mode(0o600)` + post-write `chmod`
  + atomic rename + best-effort parent fsync. Windows: `CreateFileW`
  with `SECURITY_ATTRIBUTES` holding a DACL of
  `D:P(A;;FA;;;<OUR_SID>)` (set at creation, no TOCTOU window) plus
  `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`. Don't roll your own
  write-then-chmod or remove-then-rename — both have windows the
  primitive closes.
- UDS three-layer defense: (a) `umask(0o177)` around `bind`, (b)
  explicit `chmod 0600` after, (c) `peer_cred()` uid check on every
  accept. Don't remove a layer in isolation. Windows analogue is
  `ImpersonateNamedPipeClient` + `OpenThreadToken(TOKEN_QUERY)` +
  `GetTokenInformation(TokenUser)` + `EqualSid` — never
  `GetNamedPipeClientProcessId` (PID is spoofable; Project Zero
  2019-09). `RevertToSelf` must fire on every exit path —
  `RevertGuard` Drop covers panics and early returns.

## Architectural rules

- **Telegram client on `reqwest`** (rustls + `webpki-roots` + ring),
  HTTP/1.1 only, json + multipart features. The hand-rolled hyper
  client lived here through 2026-04 and was dropped — `reqwest` covers
  the actual surface (POST JSON, GET file, multipart `sendVoice`,
  retry on 429/5xx) without adding meaningful weight. Hyper is still
  in the dep graph for the inspect dashboard's local HTTP server.
- **Do not pull in `teloxide` / `telers`.** They add MB of unused
  dispatcher machinery and trail the Bot API version. If you want typed
  schemas without the framework, copy types from `frankenstein` (don't
  take the dep).
- **`std::sync::Mutex` at the application edge; `tokio::sync::Mutex` only
  where locks cross `.await`.** Current uses: `tokio` for per-session
  mux locks and the autostart-provisioning serialization; `std` for
  the slot map and `default_target`.
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

## Agent hooks — May 2026 reality

**Claude Code** ships 25+ events. We install the four that matter for a
chat-forwarding bridge: `UserPromptSubmit` (inject "conclude with a
summary" context), `Stop` (forward the final assistant message),
`SubagentStop` (tagged `[agent]`), `Notification` (permission prompts;
the hook script drops idle/"waiting for input" pings at source — they
duplicate the Stop signal). Claude hot-reloads
`.claude/settings.local.json` — no session restart needed after
install. Non-zero exit is non-blocking on every event we install;
exit 2 is the only blocking signal and we never emit it. Default
timeout is 600 s; we set 5–15 s which is plenty.

**Copilot CLI** (verified against @github/copilot 1.0.48 app.js, May
2026): `agentStop` was added in v1.0.45 (2026-05-11) and fires
reliably on `task_complete`. We install four events:
`userPromptSubmitted`, `agentStop` (forward final reply), `subagentStop`
(rubber-duck / research / explore / task sub-agents), and
`notification` (permission prompts; idle dropped at source same as
Claude). Copilot loads every `*.json` in `.github/hooks/` and merges
them, so our sentinel file (`tebis.json`) co-exists cleanly with user
files. Both `_vsCodeCompat` (snake_case `hook_event_name`,
`session_id`, `transcript_path`) and native (camelCase `eventName`,
`sessionId`, `transcriptPath`) payload shapes are accepted by the
hook scripts.

**Transcript shape**: Copilot writes one JSON object per line to
`events.jsonl`. Assistant text lives in events with
`type == "assistant.message"` and `data.content` as a string. Sub-agent
events carry an `agentId`; main-agent events omit it — we use that to
route `agentStop` (main only) vs. `subagentStop` (sub only) tail
extraction. Claude's transcript shape is different (`assistant` events
with structured content blocks); see `claude-hook.sh` for that variant.

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

## Distribution lifecycle

End-to-end install/upgrade/uninstall story. Source of truth lives in:

- `scripts/install.sh` — POSIX sh installer for macOS + Linux.
- `scripts/install.ps1` — PowerShell 5.1+ installer for Windows.
- `.github/workflows/release.yml` — matrix build for the 5 supported
  targets on `v*` tag push.
- `src/upgrade.rs` — `tebis upgrade` (GitHub Releases client, SHA-256
  streaming verify, 64 MiB cap).
- `src/platform/binary_replace.rs` — atomic-replace abstraction
  (Unix `rename(2)`, Windows `MoveFileExW` with `.old` staging).
- `src/uninstall.rs` — zero-trace `tebis uninstall --purge`.

Supported release targets (must match `upgrade::current_target()`
exactly; adding a target requires changes in both places):

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `aarch64-apple-darwin`
- `x86_64-apple-darwin`
- `x86_64-pc-windows-msvc`

Each release ships the binary plus a sidecar `<asset>.sha256` whose
first whitespace-delimited token is the hex hash (matches
`shasum -a 256` output). Both `install.sh` and `tebis upgrade` parse
just the first token — do not change the format unless you update
both readers.

Distribution invariants:

- **`tebis upgrade` does not auto-restart** by default. The new binary
  only takes effect after a manual restart (Unix) or on the next
  `MoveFileExW` cycle (Windows). Pass `--restart` to re-exec the
  freshly-installed image's `restart` subcommand — that ensures the
  *new* image runs `restart`, not the still-loaded old image.
- **Unix binary replacement uses `rename(2)`.** The loader keeps the
  old inode mapped while the new binary lives at the same path. Safe
  to run while the daemon is up.
- **Windows binary replacement** moves `tebis.exe` → `tebis.exe.old`
  (allowed while running because we only need the directory entry, not
  the file handle), then moves the new image into place. The `.old`
  is best-effort unlinked on the next upgrade.
- **macOS installer strips `com.apple.quarantine`** after download so
  Gatekeeper does not prompt on first run. Safe because we own the
  verified download. The Windows installer cannot do the equivalent
  for SmartScreen — document the "More info → Run anyway" workaround.
  install.ps1 does call `Unblock-File` to clear Zone.Identifier on the
  downloaded binary; this skips MOTW-driven prompts on first run when
  AppLocker policy permits it.
- **PATH cleanup is asymmetric.** Both installers append PATH
  idempotently. Windows `--purge` removes the User PATH entry
  surgically via `[Environment]::SetEnvironmentVariable('Path', ..., 'User')`;
  never `setx` — it silently truncates User PATH at 1024 chars. Unix
  installers print the `export PATH=…` line for the user to add to
  their rc file and we never edit dotfiles on uninstall — too risky
  after the fact.
- **`tebis install` is idempotent w.r.t. the binary copy.** Both
  `service::unix::install_binary` and `service::windows::copy_binary`
  short-circuit when `src == dst` (canonicalized). This matters
  because the primary Windows flow is `install.ps1` (writes to
  `installed_binary_path()`) followed by `tebis install` (would
  otherwise `fs::copy` the running .exe to itself).
- **Default `tebis uninstall` removes only the service.** Binary,
  env, data, and per-project hook entries persist so a re-install
  is a quick `tebis install` away. `--purge` is the zero-trace
  path. Past PRs landed code that surprised users who expected
  `uninstall` alone to be terminal.
- **`--purge` ordering** (verified against `service::{unix,windows}::uninstall`):
  service-unit stop+delete → kill any standalone daemon still holding
  the lockfile → iterate the manifest and uninstall per-project hooks
  → remove config dir and data dir (data dir held back if hook
  cleanup was partial — the manifest must survive for a retry) →
  remove the lockfile + notify socket that live outside data_dir on
  Unix → remove the binary (immediate `fs::remove_file` on Unix;
  deferred PowerShell trampoline with a 30 s retry budget on Windows
  because the running `.exe` is loader-locked). On Windows, the
  surgical User-PATH cleanup runs BEFORE the trampoline so a new
  shell opened immediately after uninstall sees the corrected PATH.
- **`tebis uninstall` never removes** `tmux`, `jq`, `nc`, `psmux`,
  the user's project repos, running multiplexer sessions, or
  unrelated entries in `.claude/settings.local.json`. We identify
  our hook entries by matching script paths under `<data_dir>/scripts/`.
- **Custom install locations are not auto-tracked.** Both Unix
  (`~/.local/bin/tebis`) and Windows
  (`%LOCALAPPDATA%\Programs\tebis\tebis.exe`) hard-code the
  service-binary path. A user who installed via `install.sh
  --dir=/opt/...` and then ran `tebis install` will end up with two
  copies. `--purge` only cleans the service path. docs/install.md
  troubleshooting calls this out.

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

## Code hygiene — rules for AI-authored changes

These exist because past AI passes drifted in predictable ways. Hold the line.

1. **Rationale comments: 1–2 lines, hard cap.** Preserve the *why* when
   it ties to a CVE, a platform quirk, or a non-obvious correctness
   reason. Delete prose that restates the code below it. Three+ lines
   are reserved for `# Safety` blocks on `unsafe` and genuinely
   multi-step protocol docs — not for prose.

2. **One logical change per commit.** A feature commit may not silently
   refactor or strip comments from unrelated modules. Cleanup passes get
   their own commit with an honest subject line. If you notice unrelated
   drift while touching a file, write it down and handle it in a separate
   commit — do not fold it in.

3. **No split-brain.** Before copying a function to avoid a dep arrow,
   propose extracting a shared helper. If you truly must duplicate, the
   duplicate's header comment must name the canonical source AND the
   specific reason the arrow is forbidden (module-layering, orphan-rule,
   cyclic-feature-gate — not "to avoid inbound coupling", which is not a
   reason).

4. **Network-error redaction lives in `src/sanitize.rs`.**
   `contains_bot_token_shape` and `redact_hyper_error_string` are the
   shared primitives. Per-destination predicates (Telegram:
   `/bot<digit>|api.telegram.org`; remote-TTS: `://|Bearer|Authorization`)
   live at the call site because redaction triggers differ by endpoint
   shape.

## Secrets

Bot token belongs in the private secret manager. For local/systemd/launchd,
put env in `~/.config/tebis/env` with mode 0600 (or use `tebis setup`).
**Never commit a filled `.env`.** `.env` is gitignored.

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
