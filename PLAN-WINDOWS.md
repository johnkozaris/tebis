# Plan: Windows port of tebis

Personal Rust daemon currently Unix-only (macOS + Linux). Target: native
Windows 10 / 11 support, preserving every security invariant, without
regressing Unix.

## Principles

- **Preserve every security invariant.** Each has a Windows counterpart in
  the mapping below; none are weakened.
- **Phased, behavior-preserving.** Each phase compiles and passes tests on
  both platforms before the next starts.
- **Unix branch stays first-class.** Windows gets a parallel implementation
  behind traits / `#[cfg]`, not a rewrite.
- **CI-gated per phase.** Add `windows-latest` to GH Actions in Phase 0 with
  a `cargo check` gate that tightens as we go.

## Context: April 2026 state of the world

Three things changed in the last year that make a native-Windows port
realistic instead of a stunt:

1. **Zellij 0.44.0** (2026-03-23) ships native Windows support — full
   parity with Linux/macOS via a community ConPTY port.
2. **psmux** — native Rust, tmux-compatible multiplexer for Windows. Reads
   `~/.tmux.conf`, speaks tmux command language, installs `tmux`/`pmux`/
   `psmux` aliases. Ships first-class Claude Code integration.
3. **Claude Code** now has a native Windows installer (`irm
   https://claude.ai/install.ps1 | iex`), and **GitHub Copilot CLI** GA'd
   2026-02-25 with Authenticode-signed Windows native prebuilds.

ConPTY (Win10 1809+) is the primitive underneath. We don't integrate with
it directly — psmux / Zellij do, and we drive them.

## File-by-file porting surface

| File | Lines | Unix coupling | Windows plan |
|---|---|---|---|
| `src/lockfile.rs` | 182 | `#![cfg(unix)]`, raw `libc::flock` | Replace with `fs4` crate. One impl for both OSes. |
| `src/env_file.rs` | 296 | `OpenOptionsExt::mode(0o600)`, `set_permissions(0o600)` | Keep Unix path; add Windows path using DACL at file creation (inheritance off, current-user SID only). |
| `src/notify/listener.rs` | 355 | `UnixListener`, `libc::umask`, `peer_cred` | New `PeerListener` trait. UDS backend (unchanged). Named Pipe backend: tokio `NamedPipeServer`, DACL on pipe, `ImpersonateNamedPipeClient` + `TokenUser` SID check. |
| `src/service.rs` | 594 | launchd + systemd branches | Add Windows SCM backend using `windows-service` (Mullvad). `install/start/stop/status/restart/uninstall` on SCM. |
| `src/tmux.rs` | 470 | Subprocess calls to `tmux` binary | Rename `Tmux` → `Mux`, extract `Multiplexer` trait. `TmuxBackend` (Unix). `PsmuxBackend` (Windows) — tmux-compatible CLI so the `-l`/`-H 0d` sequence and `=name` exact-targeting carry over. Verify invariants 3, 13, 15 against psmux empirically. |
| `src/agent_hooks/mod.rs` | ~160 | `PermissionsExt::from_mode(0o700)` on hook-script dir | `#[cfg(unix)]` for chmod; Windows relies on `%LOCALAPPDATA%` being already user-private. |
| `src/agent_hooks/manifest.rs` | ~120 | Raw `libc::flock` | Switch to `fs4`. |
| `src/agent_hooks/script_e2e_tests.rs` | ~100 | `UnixListener` fake bridge | Mirror as `NamedPipeServer` fake for Windows test runs. |
| `src/hooks_cli.rs` | ~380 | Probes `jq` / `nc` on PATH | Windows path probes PowerShell ≥ 5.1 (or `pwsh` ≥ 7). No jq — `ConvertFrom-Json` is native. No nc — `System.IO.Pipes.NamedPipeClientStream`. |
| Hook scripts (bash) | — | bash + jq + nc | Ship `.ps1` siblings. Both call the same wire format (newline-terminated JSON). `agent_hooks/claude.rs` + `copilot.rs` write the OS-appropriate script. |
| `src/main.rs` | L700 area | `signal::unix::SignalKind::terminate()` | `#[cfg(unix)]` SIGTERM path; `#[cfg(windows)]` `signal::windows::ctrl_c` + in service mode, SCM Stop control code fires the same `CancellationToken`. |
| `src/inspect/mod.rs` | L264 area | `libc::kill(SIGTERM/SIGKILL/0)`, `libc::gethostname` | Windows: `OpenProcess` + `TerminateProcess` (collapses to single path — no signal distinction); hostname via `GetComputerNameExW`. |
| `src/audio/cache.rs` | ~140 | `from_mode(0o700/0o644)` on cache dir + files | `#[cfg(unix)]` chmod calls; Windows relies on NTFS default-inherit from `%LOCALAPPDATA%`. |
| `src/audio/tts/say.rs` | — | macOS-only (`say` command) | Already `#[cfg(target_os = "macos")]` — no Windows work. |
| `Cargo.toml` | — | `whisper-rs` only for macOS + linux | Add `cfg(windows)` target block: CPU-only Whisper. |
| `src/config.rs` | L130, L161 | `#[cfg(target_os = "macos")]` paths for Whisper models | Already branched — extend with Windows path via `directories::ProjectDirs::data_dir()`. |

## Dependency additions (`Cargo.toml`)

```toml
# Cross-platform (unify Unix + Windows paths)
fs4 = "0.13"                     # replaces raw libc::flock in lockfile.rs + manifest.rs
directories = "6"                # replaces hand-rolled XDG logic

# Windows-only
[target.'cfg(windows)'.dependencies]
windows-service = "0.8"
windows = { version = "0.60", features = [
  "Win32_Security",
  "Win32_Security_Authorization",
  "Win32_System_Pipes",
  "Win32_System_Threading",
  "Win32_Foundation",
  "Win32_System_SystemInformation",  # GetComputerNameExW
]}

# Whisper (Windows CPU-only)
[target.'cfg(windows)'.dependencies]
whisper-rs = { version = "0.16", default-features = false }

# Unix-only going forward
[target.'cfg(unix)'.dependencies]
libc = "0.2.185"
```

## Security invariant mapping

| # | Invariant | Windows equivalent |
|---|---|---|
| 1 | numeric `user.id` auth | **Unchanged.** |
| 2 | session-name regex | **Unchanged** — applied at config load + every multiplexer call. psmux session names use same charset. |
| 3 | `send_keys -l` → sleep → `-H 0d` under one mutex | **Unchanged** if psmux honors `-l` / `-H 0d`. **Verify empirically in Phase 0 spike.** If psmux differs, fall back to ConPTY direct-drive (scope change). |
| 4 | HTML escape all replies | **Unchanged.** |
| 5 | never log `message.text` | **Unchanged.** |
| 6 | redact network errors | **Unchanged.** |
| 7 | HTTP/TLS crates at `warn` | **Unchanged.** |
| 8 | per-session `tokio::Mutex` | **Unchanged.** |
| 9 | UDS-only, 0600, chmod after bind | **Rewritten for Windows:** Named Pipe with explicit DACL at `CreateNamedPipe` time (SDDL `D:P(A;;GA;;;%OWNER_SID%)` — owner-only, inheritance denied). No TCP fallback either platform. |
| 10 | 16 KiB cap, 5 s read timeout | **Unchanged.** |
| 11 | newline-framed JSON, not EOF | **Unchanged** — PowerShell's `StreamWriter.WriteLine` matches. |
| 12 | shared `TaskTracker` for notify spawns | **Unchanged.** |
| 13 | `=name` exact-target prefix | **Unchanged** on psmux (tmux-compatible). Verify in Phase 0. |
| 14 | autostart mutex | **Unchanged.** |
| 15 | `TmuxError::NotFound` recovery | **Unchanged** shape; may need psmux-specific stderr classification in `classify_status`. |
| 16 | global handler semaphore | **Unchanged.** |
| 17 | three-layer UDS peer defense (umask + chmod + peer_cred) | **Rewritten as three-layer Named Pipe peer defense:** (a) DACL at pipe creation (owner-only via `SECURITY_ATTRIBUTES`); (b) deny inheritance (`SE_DACL_PROTECTED`); (c) `ImpersonateNamedPipeClient` + `GetTokenInformation(TokenUser)` SID-equality check against current process's user SID. PID-based `GetNamedPipeClientProcessId` is **explicitly rejected** (Project Zero 2019: spoofable). |
| 18 | `send_keys` atomic under one mutex | **Unchanged** (same mutex pattern; multiplexer-abstracted). |
| 19 | STT transcript byte cap | **Unchanged.** |

**New invariant to add when Phase 2 lands:**

> **20. Named Pipe peer auth uses `ImpersonateNamedPipeClient` +
> `TokenUser` SID check, never `GetNamedPipeClientProcessId`.** PID is
> spoofable (Project Zero 2019). The impersonation token is kernel-verified.

## Phases

### Phase 0 — Foundation + psmux spike (1–2 days)

- **1-hour psmux spike first** — install psmux, `new-session -d -s t`,
  `send-keys -t "=t" -l "echo hi"` + `send-keys -t "=t" -H 0d`,
  `capture-pane -t "=t" -p`. If any of the four fail or behave
  differently from tmux, re-plan toward ConPTY direct-drive.
- Add `windows-latest` to `.github/workflows/ci.yml`: runs
  `cargo check --target x86_64-pc-windows-msvc` only, initially.
- Move `libc` into `[target.'cfg(unix)'.dependencies]`.
- Introduce `src/platform/mod.rs` with Unix + Windows submodules for the
  three primitives that cross boundaries: `secure_file_write`,
  `peer_listener`, `multiplexer`. Each starts as a re-export of the
  current Unix impl so nothing changes on Unix.
- `#[cfg(unix)]` gates added to existing code so Windows compiles with
  `todo!()` stubs in `platform/windows/` subdirs. CI enforces "Windows
  compiles" from here forward.

**Exit criteria:** `cargo check --target x86_64-pc-windows-msvc` passes.
All Unix tests still green. psmux spike green.

### Phase 1 — Cross-platform plumbing (2–3 days)

- `src/lockfile.rs` — replace raw `libc::flock` with
  `fs4::FileExt::try_lock_exclusive`. Delete `#![cfg(unix)]`. Keep
  pidfile write logic (portable). One path for both.
- `src/agent_hooks/manifest.rs` — same migration. Drop the `FlockGuard`
  unsafe block.
- `src/config.rs` — route XDG-ish paths through
  `directories::ProjectDirs::from("", "", "tebis")`. Windows:
  `%APPDATA%\tebis\`, `%LOCALAPPDATA%\tebis\`,
  `%LOCALAPPDATA%\tebis\cache\`. Runtime dir (Unix only) → fall back to
  `%LOCALAPPDATA%\tebis\run\` on Windows.
- `src/main.rs` — SIGTERM path becomes Unix-only; Windows gets
  `tokio::signal::windows::ctrl_c()` only (service stop handled in
  Phase 4).

**Exit criteria:** `cargo test` passes on Unix. Windows compiles and the
four cross-platform primitives (lockfile, manifest, dirs, ctrl_c) have
tests running on `windows-latest`.

### Phase 2 — Notify IPC rewrite (~1 week)

- Introduce `PeerListener` trait in `src/notify/listener.rs`:
  ```rust
  #[async_trait]
  trait PeerListener {
      type Conn: AsyncRead + AsyncWrite + Unpin + Send;
      async fn accept(&self) -> io::Result<Self::Conn>;
  }
  ```
- Unix backend: current `UnixListener` + `umask(0o177)` + `chmod(0o600)`
  + `peer_cred` (unchanged).
- Windows backend: `NamedPipeServer` with SDDL
  `D:P(A;;GA;;;%OWNER_SID%)` at `CreateNamedPipeW`
  (`SE_DACL_PROTECTED` flag + explicit owner ACE). On each accept:
  `ImpersonateNamedPipeClient`, `GetTokenInformation(TokenUser)`,
  compare SID to process's token SID, `RevertToSelf`. Reject ≠.
- Pipe name: `\\.\pipe\tebis-<username>-notify` (username prevents
  collision on multi-user Windows hosts; SID in DACL is the auth gate).
- Rewrite the two embedded hook scripts as `.ps1` siblings using
  `System.IO.Pipes.NamedPipeClientStream` + `StreamWriter`. Keep bash
  versions for Unix.
- `agent_hooks/claude.rs` + `copilot.rs` write the OS-appropriate script
  (`.sh` vs `.ps1`) based on `cfg!(windows)`.
- `src/hooks_cli.rs` install-time probes: Unix probes `jq`/`nc`
  (existing); Windows probes `powershell.exe` ≥ 5.1 (or `pwsh` ≥ 7).
- Mirror `script_e2e_tests.rs` for Windows: fake pipe server instead of
  fake UDS.

**Exit criteria:** End-to-end notify test passes on Windows — hook
PowerShell script connects to pipe, sends JSON payload, listener verifies
peer SID, parses, forwards to fake Telegram.

### Phase 3 — Secret file permissions (2–3 days)

- Extract `write_secure` from `env_file.rs` into
  `platform::secure_file_write`.
- Windows impl:
  1. Create tmp file with `CreateFileW` using a `SECURITY_DESCRIPTOR`
     that has DACL protected + single ACE granting
     `FILE_GENERIC_READ | FILE_GENERIC_WRITE` to current user's SID.
  2. Write + flush + rename (same atomic pattern as Unix).
  3. No secondary `chmod` — permissions are set at creation.
- Use the `windows` crate directly rather than `windows-acl` if that
  crate is dormant — check crate activity first.
- Apply to any other secret-bearing file write (only `env_file` today;
  audit agent_hooks for hook script writes — those are non-secret but
  still user-scoped).

**Exit criteria:** Test that a secondary user account on the same
Windows host cannot read the env file written by tebis.

### Phase 4 — Service backend (~1 week)

- `src/service.rs` gains `#[cfg(windows)]` block using `windows-service`:
  - `define_windows_service!` macro.
  - SCM event handler dispatches `ServiceControl::Stop` → fires the main
    `CancellationToken`.
  - Log target: Windows Event Log via `tracing-subscriber` or write to
    `%LOCALAPPDATA%\tebis\logs\tebis.log` with rotation.
  - Install path: `%LOCALAPPDATA%\Programs\tebis\tebis.exe`.
  - `tebis service install` registers with SCM as auto-start user
    service (not LocalSystem — need per-user token for correct SID).
- In service mode, the binary re-enters a special `service_main` fn.
  CLI modes (setup, hooks, inspect) bypass.

**Install mechanism — decision point:** true SCM user-service install
requires the user's password or an interactive SYSTEM token, which is
fiddly. Recommend v1 ships **Task Scheduler "at logon"** as the default
and keeps full SCM install behind a flag for v2.

**Exit criteria:** Install as Task Scheduler task → starts on login →
serves Telegram → clean stop via `tebis service stop` → uninstall leaves
no registry/filesystem residue.

### Phase 5 — Multiplexer backend (~1–2 weeks)

**Judgment call:** psmux (tmux-compatible, lowest port cost) vs Zellij
(native Rust lib, no subprocess marshalling).

- **Recommend psmux** for v1 because:
  - Same CLI surface as tmux → `Tmux` subprocess code mostly reuses its
    arg-building.
  - Same `-l` / `-H 0d` sequence honored (tmux-compatible means the
    three-call atomic sequence carries over as-is).
  - `=name` exact-target prefix honored (invariant 13 holds).
  - Explicitly ships first-class Claude Code integration (nice
    tailwind, not required).
- Refactor: rename `src/tmux.rs` → `src/multiplexer/mod.rs`, extract
  `Multiplexer` trait; existing impl becomes
  `multiplexer::tmux::TmuxBackend`.
- New `multiplexer::psmux::PsmuxBackend` (Windows): same command
  structure, just different binary name. Verify empirically in Phase 0
  spike:
  - `psmux new-session -d -s <name>` creates detached session
  - `psmux send-keys -t "=<name>" -l "<text>"` literal text
  - `psmux send-keys -t "=<name>" -H 0d` sends Enter as hex
  - `psmux capture-pane -t "=<name>" -p` dumps pane
  - `psmux has-session -t "=<name>"` / `kill-session` error codes —
    classify for `NotFound`.
- `TmuxError` → `MuxError` (rename, same variants). Per-session
  `tokio::Mutex` unchanged.
- If psmux falls short on any of the three core sequences, fall back to
  driving ConPTY directly via the `conpty` crate — bigger rewrite, park
  as Phase 5b.
- Setup wizard step for multiplexer choice: Unix defaults tmux; Windows
  defaults psmux with Zellij as opt-in.

**Exit criteria:** `/send foo`, `/switch bar`, autostart, hooks all
work end-to-end with psmux on Windows. Invariants 2, 3, 13, 15, 18 all
verified with integration tests.

### Phase 6 — Inspect dashboard + misc syscalls (1–2 days)

- `src/inspect/mod.rs`:
  - `libc::kill(pid, SIGTERM/SIGKILL/0)` → Windows:
    `OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_INFORMATION)` +
    `TerminateProcess` (no graceful-vs-forceful split — Windows has no
    signal equivalent, so `SIGTERM` and `SIGKILL` paths collapse, with
    a brief wait before `TerminateProcess` for "graceful").
  - `libc::gethostname` →
    `GetComputerNameExW(ComputerNameDnsHostname, ...)`.
- `src/audio/cache.rs`: gate `set_permissions(0o7xx)` calls on
  `#[cfg(unix)]`. Windows relies on `%LOCALAPPDATA%` inheritance.

**Exit criteria:** Inspect dashboard renders hostname, PID list, kill
buttons work on Windows.

### Phase 7 — Audio (STT on Windows) (1 day)

- Add `[target.'cfg(windows)'.dependencies] whisper-rs = { version =
  "0.16", default-features = false }` (CPU-only).
- TTS via `say` command is already macOS-only (no change).
- Kokoro TTS is already a feature flag; works on any platform if
  `onnxruntime` is available.

**Decision:** ship STT on Windows. Voice messages are a core tebis
feature; shipping without them on Windows is a downgrade. Small code
cost, ~5 MB binary increase.

**Exit criteria:** Send voice message from Telegram to Windows tebis
→ transcribes → enters pane.

### Phase 8 — CI hardening + integration tests (~1 week)

- Windows GHA runner:
  - `windows-latest` step: install psmux via Chocolatey, install
    PowerShell 7, install Claude Code.
  - Run full test suite including notify-pipe integration test.
- Add a `cross-platform` tag to tests that should run on both; skip
  tmux-specific tests on Windows.
- Binary size check: release build ≤ ~6 MB on Windows (vs ~4.25 MB on
  Linux).
- `cargo audit` + `cargo deny` on Windows target (may surface new
  transitive deps).

### Phase 9 — Docs + setup wizard + release (~3 days)

- `README.md`: Windows install section (psmux + Claude Code native +
  tebis installer).
- `CLAUDE.md`: add invariant 20 (Named Pipe peer auth). Revise 9 and 11
  to "UDS on Unix / Named Pipe on Windows." Revise 13/17 similarly.
- `src/setup/steps/` new step: multiplexer selection (skipped on Unix,
  shown on Windows).
- Windows installer: single-file MSI or Scoop manifest for `tebis`.

## Total estimate

- **Sequential, single-dev, focused:** 5–7 weeks.
- **With parallel reviews + buffer:** 8–10 weeks.

## Biggest risks / open questions

1. **psmux `send-keys -l` / `-H 0d` fidelity.** Everything rests on
   this. If psmux is only 95% tmux-compatible, we fall back to ConPTY
   direct-drive (bigger scope). **Mitigate:** 1-hour spike in Phase 0
   before committing.
2. **psmux maturity.** Released 2026, smaller ecosystem than tmux.
   Bug-fix velocity may be lower. **Mitigate:** keep Zellij as Plan B —
   its API is stable and well-funded.
3. **Windows user service vs machine service.** tebis needs per-user
   context (SID for auth, per-user `%APPDATA%`, per-user Claude Code
   config). Running as LocalSystem breaks this; need to run as the
   logged-in user. `windows-service` supports this but the install
   ceremony is fiddly. **Mitigate:** ship with "run from Task Scheduler
   at logon" as v1 default, add true SCM install later.
4. **Claude Code on Windows requires Git for Windows.** Our autostart
   command (`claude code`) assumes it's on PATH. Setup wizard needs a
   Windows probe. **Mitigate:** new wizard step.
5. **`windows-acl` crate activity.** If dormant, drop it and use
   `windows` crate raw. No blocker, just scope.

## First action

Run the **Phase 0 psmux spike** — 1 hour to confirm the three
tmux-compatible primitives work as advertised. If green, Phase 0 + 1
can complete in the same week.
