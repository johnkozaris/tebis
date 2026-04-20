# Phase 2 — Agentic Session Awareness

Status: **shipped** · Owner: tebis core · Delivered as one PR, no interim/backward-compat modes.

This is a full-migration plan. The result: tebis distinguishes *agentic* tmux
sessions (running Claude Code, Copilot CLI) from *terminal* sessions, and
when an agentic session is spawned it auto-installs the agent's native hooks
so replies come via structured events — no pane-settle polling for agentic
sessions. Pane-settle stays the universal fallback for non-agentic TUIs.

---

## 1. Context — what we have today

After the v0.1.1 work the bridge has:

- **Pane-settle autoreply** — universal TUI-agnostic reply detection. Polls
  `capture-pane`, normalizes (strip Braille spinners + C0/C1 + collapse
  whitespace), hashes, settles after 3 s stable, sends the tail.
- **Diff-vs-baseline** tail extraction so the user doesn't see the agent's
  whole scrollback on every message.
- **Typing indicator** via `sendChatAction=typing` on a 4 s refresh loop.
- **UDS notify listener** bound by default (chat_id = allowed_user_id).
  Ready to receive hook-forwarded events from *any* script, no config.
- **Contrib hook script** at `contrib/claude/claude-hook.sh` — already
  battle-tested for Claude. Exits 0 on every path; fails open.

What's missing:

- **Hooks are not installed by tebis.** Users have to copy the script
  and merge `.claude/settings.local.json` by hand. tebis doesn't own the
  lifecycle.
- **No Copilot CLI support** in any form.
- **Double-reply risk.** If hooks were installed manually AND pane-settle
  was on, the user would get two messages per agent reply.

Phase 2 closes all three.

---

## 2. Research findings

### 2.1 Claude Code hooks (verified from official docs)

Source: <https://code.claude.com/docs/en/hooks>.

- **Event catalog (25+ in 2026)**. For our purposes four matter:
  `UserPromptSubmit`, `Stop`, `SubagentStop`, `Notification`. Others we
  can leverage later: `SessionStart`, `SessionEnd`, `PreCompact`,
  `Elicitation`.
- **Failure semantics.** Default: non-blocking. A hook that exits
  non-zero, isn't found, or hangs past `timeout` prints stderr to the
  user and the turn *continues*. **Exit code 2** is the only blocking
  signal, and only on `UserPromptSubmit`, `PreToolUse`, `Stop`,
  `SubagentStop`. Our script never emits 2 by design (guarded
  `|| true`). This is the critical safety property: **our hooks cannot
  break Claude.**
- **Timeout.** `timeout` (seconds) is enforced. Default 600 s. We set
  15 s for `Stop`/`SubagentStop`, 10 s for `Notification`, 5 s for
  `UserPromptSubmit`. Termination signal is not publicly documented;
  assume SIGKILL after grace → our script must not leave critical state
  uncommitted. It doesn't.
- **Merging.** All 4 settings levels (user, project, `.local`, plugin)
  merge into a **single array per event**. Duplicates are de-duped by
  command string. So we can safely add our entries without clobbering
  user-owned ones.
- **No schema-supported label field.** Claude Code's schema has no
  `name` / `description` / custom keys on a hook object. Our
  "is this tebis's?" query has to be **path-based**: the entry's
  `command` field starts with the tebis data dir prefix.
- **Reload.** `.claude/settings.local.json` is file-watched — edits are
  picked up within seconds, no session restart needed. Useful for
  install/upgrade paths.
- **Settings precedence for *writing* (least to most invasive):**
  - `.claude/settings.local.json` ← **we write here**. Lowest precedence,
    typically `.gitignore`d, doesn't touch user's shared project config.
  - `.claude/settings.json` ← shared with the repo; we never touch.
  - `~/.claude/settings.json` ← user-level; we never touch.
- **Uninstall.** Removing our entries from `.claude/settings.local.json`
  is complete cleanup. No caches / transcripts to prune.

### 2.2 Copilot CLI hooks (GA 2026-02-25, v1.0.32)

Source: <https://github.com/github/copilot-cli>, `docs.github.com`,
community blog posts. **Schema under background research** — see
companion investigation for exact JSON field names. Plan below assumes
the community-documented shape; the implementation of
`src/agent_hooks/copilot.rs` is the only place that depends on exact
schema, so a schema-miss is a localized fix.

What's confirmed:

- **Eight events**: `sessionStart`, `userPromptSubmitted`, `preToolUse`,
  `postToolUse`, `agentStop`, `subagentStop`, `errorOccurred`,
  `sessionEnd`. `agentStop` is the direct analogue of Claude's `Stop`.
- **Two config locations**:
  - `.github/hooks/*.json` (per-repo). ← **we write here.**
  - `~/.copilot/hooks/` (user-wide, added in 0.0.422).
- **Per-file** definitions (file per event / per hook) rather than one
  big settings object like Claude.
- **Structured stdin** with `hook_event_name`, `session_id`, ISO
  timestamps, snake_case field names (as of 0.0.421).
- **JSONL output mode** (`--output-format json`, added 0.0.420). Useful
  for one-shot piping but not relevant to our REPL-attached use case.

### 2.3 Industry patterns for "installer writes into user project" tools

Studied to pick the right idioms:

- **husky** — generates files in `.husky/` with a sentinel header
  `# Generated by husky — DO NOT EDIT BY HAND`. Owns whole files.
- **lefthook** — `.lefthook-local.yml` file format supports `extends:`
  and merge. Our case is simpler.
- **pre-commit** — owns `hooks` inside a YAML file. Installs each hook
  as a `.git/hooks/<name>` shim that calls back into pre-commit.
- **direnv** — doesn't touch user files; just reads `.envrc`. Inverse
  pattern from ours.
- **devcontainers** — owns whole `.devcontainer/` dir.

**Pattern we adopt**: husky's "owns specific paths" + a **sentinel path**
for identification (the command string in a hook entry points at our
materialized script in `$XDG_DATA_HOME/tebis/<agent>-hook.sh`). Any entry
whose command is under that prefix is ours; the installer never touches
anything else.

---

## 3. Design decisions

### D1. Sentinel format → **path-based**.

An entry is "ours" iff its command string equals our materialized hook
script path. The script lives at a known, stable location:

```
$XDG_DATA_HOME/tebis/claude-hook.sh     (Claude)
$XDG_DATA_HOME/tebis/copilot-hook.sh    (Copilot)
```

Fallback when `$XDG_DATA_HOME` is unset: `$HOME/.local/share/tebis/`.

Rationale:
- Robust: string compare, no parsing of shell invocations needed.
- Safe: won't false-positive on user's own hooks (unless they also
  point at our path, which is their business).
- Upgrade-safe: path never changes across tebis versions, so
  reinstall-dedupe works forever.
- Fail-visible: if user rm-rf's the data dir, Claude reports
  "file not found" as a non-blocking stderr, hook gracefully skips,
  turn continues. No user-visible breakage.

### D2. Hook script materialization → **embedded, lazy, versioned**.

- Scripts live in `contrib/{claude,copilot}/*.sh` in the repo,
  shellcheck'd in CI.
- `include_str!` into the binary at build time.
- At install/autostart-spawn, `materialize(agent) -> PathBuf`:
  1. Compute destination: `data_dir().join("<agent>-hook.sh")`.
  2. If file exists and content == embedded content, skip.
  3. Else atomic write (tmp + rename + chmod 0700).
- No in-binary version string needed — content-equality is the upgrade
  trigger.

### D3. Install target per agent → **project-local**, never user-level by default.

| Agent | Path | Rationale |
|---|---|---|
| Claude | `<dir>/.claude/settings.local.json` | Lowest-precedence, usually `.gitignore`d. |
| Copilot | `<dir>/.github/hooks/tebis-<event>.json` | Per-file = ownership is obvious. |

User-level install (`~/.claude/settings.json`, `~/.copilot/hooks/`) is
**out of scope** for v1. We'd risk silent cross-project effects.

### D4. Events we install (per agent).

Only events that feed our Telegram-reply flow:

| Claude event | Copilot analogue | Purpose |
|---|---|---|
| `Stop` | `agentStop` | Forward assistant's final message (primary reply). |
| `SubagentStop` | `subagentStop` | Forward subagent's message, tagged `[agent]`. |
| `Notification` | `notification` event (if exposed) | Forward permission / idle prompts, tagged `[ask]`/`[idle]`. |
| `UserPromptSubmit` | `userPromptSubmitted` | Inject "conclude with a summary" context so `Stop`'s tail is useful. |

No `PreToolUse`/`PostToolUse` installs — not in our contract; adds noise
and risk (PreToolUse can block tools on exit 2).

### D5. Session → hook linkage → `SessionState.hooked_sessions`.

`SessionState` gains `hooked_sessions: std::sync::Mutex<HashSet<String>>`.
- On `resolve_or_autostart` success with agentic command + `hooks_mode=auto`:
  install hooks, add session name to set.
- On autostart session kill (`/restart`, `/kill =name`): **do not**
  remove from the set — if the session is immediately re-spawned we want
  the hooks to still be there. Next `resolve_or_autostart` will re-install
  (idempotent) and re-add.
- On clean shutdown: nothing — hooks persist on disk for next run. User
  explicitly removes via `tebis hooks uninstall`.

### D6. Reply suppression → `bridge::handle_update` checks session set.

If `session_state.is_hooked(&session)` is true on a `Response::Sent`,
skip `autoreply::watch_and_forward` entirely. The hook will deliver.

Typing indicator (the `sendChatAction=typing` loop that was in the
autoreply task) moves out so hooked sessions still get typing feedback.
**New shared primitive**: `typing::indicate_for(tg, chat_id, until)` that
spins the refresh loop and cancels on a token.

### D7. Config surface → one env var, one subcommand family.

```
TELEGRAM_HOOKS_MODE=auto|off      (default: off)

tebis hooks install [<dir>]       (defaults to autostart dir)
tebis hooks uninstall [<dir>]
tebis hooks status [<dir>]
```

`off` = today's behavior, pane-settle only.
`auto` = at autostart, detect the agent, install hooks if supported,
mark session as hooked, skip autoreply for it.

The `tebis hooks` verbs let power users manage hooks independently of
the autostart flow (e.g. install in a dir they run `claude` in manually).

### D8. Failure modes — fail-open at every layer.

1. **Script materialize fails** (disk full, permission denied) → log
   warn, fall back to pane-settle for the session. Don't crash
   autostart.
2. **settings.local.json merge fails** (malformed JSON from user) →
   log error with path, fall back to pane-settle. **Do not rewrite the
   user's file.**
3. **Hook script runs but can't reach UDS socket** (bridge not
   running) → script exits 0, Claude continues. User sees no reply;
   when they message tebis it'll start and socket reappears.
4. **Hook installed but bridge changes path** (future) → stale entry
   points at a no-op path. Claude reports stderr once, continues.
   Solved by D1's stable path.
5. **Agent not detected** → log info ("command=zsh, no hooks installed"),
   pane-settle handles the session normally.

### D9. Migration → **clean cutover, no backward-compat layers**.

Per user's direction, not shipping intermediate flags:

- **Removed** (if still present): the pre-existing `NOTIFY_CHAT_ID` opt-in
  behavior became default-on in v0.1.1. Already landed.
- **No `TELEGRAM_AUTOREPLY=off` special-casing for hooks.** Autoreply
  continues to exist as a fallback; it's just **per-session suppressed**
  when hooks are installed.
- **`contrib/claude/claude-hook.sh`** → still embedded, still the
  canonical script. It moves conceptually (users no longer reference
  its path manually), but the file stays in the repo for CI + testing.

---

## 4. Architecture

### 4.1 Module tree (after)

```
src/
├── bridge/
│   ├── mod.rs          (existing: handle_update; checks hooked_sessions)
│   ├── autoreply.rs    (existing: pane-settle; typing loop extracted)
│   ├── typing.rs       (new: shared typing indicator loop)
│   ├── handler.rs      (existing: Response::Sent carries session + baseline)
│   ├── session.rs      (existing + hooked_sessions field + is_hooked / mark_hooked)
│   └── agent.rs        (new: AgentKind::detect)
├── agent_hooks/
│   ├── mod.rs          (new: HookManager trait + data_dir() + materialize())
│   ├── claude.rs       (new: ClaudeHooks impl)
│   └── copilot.rs      (new: CopilotHooks impl)
├── config.rs           (+ hooks_mode: HooksMode)
├── service.rs          (+ `tebis hooks` subcommand surface)
├── main.rs             (argv dispatch for `hooks` + wire hooks_mode into HandlerContext)
└── ... (everything else unchanged)

contrib/
├── claude/claude-hook.sh    (existing)
└── copilot/copilot-hook.sh  (new)
```

### 4.2 Key types

```rust
// src/bridge/agent.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind { Claude, Copilot }

impl AgentKind {
    pub fn detect(command: &str) -> Option<Self> { ... }
    pub fn display(&self) -> &'static str { ... }  // for logs/status
}

// src/agent_hooks/mod.rs
pub trait HookManager: Send + Sync {
    fn agent(&self) -> AgentKind;
    fn install(&self, project_dir: &Path, script_path: &Path) -> Result<InstallReport>;
    fn uninstall(&self, project_dir: &Path) -> Result<UninstallReport>;
    fn status(&self, project_dir: &Path) -> Result<StatusReport>;
}

pub struct InstallReport {
    pub files_written: Vec<PathBuf>,
    pub events: Vec<&'static str>,
    pub was_fresh: bool,  // true if we created the file, false if merged
}

pub struct UninstallReport {
    pub files_modified: Vec<PathBuf>,
    pub files_deleted: Vec<PathBuf>,
    pub events_removed: Vec<String>,
}

pub struct StatusReport {
    pub installed_events: Vec<String>,
    pub unexpected_entries: Vec<String>,  // tebis-like entries we don't own
}

pub fn for_kind(k: AgentKind) -> Box<dyn HookManager> { ... }
pub fn materialize(agent: AgentKind) -> Result<PathBuf> { ... }

// src/config.rs
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HooksMode {
    #[default] Off,
    Auto,
}

// src/bridge/session.rs
pub struct SessionState {
    // ... existing fields ...
    hooked_sessions: std::sync::Mutex<HashSet<String>>,
}
impl SessionState {
    pub fn mark_hooked(&self, session: &str) { ... }
    pub fn is_hooked(&self, session: &str) -> bool { ... }
}

// src/bridge/mod.rs
pub struct HandlerContext {
    // ... existing fields ...
    pub hooks_mode: HooksMode,  // just for logging; install decision is at autostart
}
```

### 4.3 Data flow

**Install path (autostart):**
```
main.rs: build Config { hooks_mode }
       ↓
SessionState::new(...)
       ↓
[first plain-text message]
       ↓
SessionState::resolve_or_autostart(tmux, hooks_mode)
       ├─ if command matches AgentKind::detect(...) && hooks_mode == Auto:
       │     materialize(agent) → script_path
       │     for_kind(agent).install(dir, script_path)
       │     mark_hooked(session)
       └─ spawn tmux session (existing)
```

**Reply path (per-message):**
```
handle_update(...)
       ↓
execute → Response::Sent { session, baseline }
       ↓
if session_state.is_hooked(&session):
    typing::indicate(tg, chat_id, ~60s cap)   # still show typing
    (hook will deliver the real reply via UDS; no autoreply task)
else:
    autoreply::watch_and_forward(...)         # pane-settle + typing combined
```

**Uninstall path (subcommand):**
```
main.rs: `tebis hooks uninstall [dir]`
       ↓
for_kind(detect_from_dir_or_prompt).uninstall(dir)
       ↓
report.print()
```

### 4.4 Atomic file operations

All mutations to `settings.local.json` / Copilot hook files use:
1. Read current content (or empty).
2. Modify in memory.
3. Serialize to `<path>.tebis.tmp`.
4. `fsync` the tmp file.
5. `rename` over the target.

A crash at any point leaves either the old file or the new file intact,
never partial.

---

## 5. Edge cases + gotchas

| # | Scenario | Handling |
|---|---|---|
| 1 | `$XDG_DATA_HOME` unset | Fall back to `$HOME/.local/share/tebis`. |
| 2 | `$HOME` unset | `materialize` returns `Err`; install fails; log + fall back to pane-settle. |
| 3 | User has existing tebis entries (upgrade) | `install` de-dupes by path match before inserting. |
| 4 | User has non-tebis entries in same event | Preserved: uninstall removes only entries with our script path. |
| 5 | `.claude/settings.local.json` is malformed JSON | Log error, abort install, do **not** overwrite. Pane-settle takes over. |
| 6 | `.claude/settings.local.json` root is not an object | Refuse to install; log. |
| 7 | User deletes `$XDG_DATA_HOME/tebis/claude-hook.sh` while installed | Next `claude` turn: hook command not found, Claude prints stderr once, continues. Autostart re-materializes on next session spawn. |
| 8 | Two tebis instances racing install | Single-instance lockfile (v0.1.1) prevents this. |
| 9 | Crash mid-install | Atomic rename keeps settings.local.json consistent. |
| 10 | Empty `hooks` block / empty settings object after uninstall | Remove the key; if file becomes `{}`, remove the file. |
| 11 | Project dir doesn't exist / not writable | `install` returns Err; autostart continues without hooks. |
| 12 | User renames autostart session but hooks installed | Hooks live in the *directory*, not the session. Unaffected. |
| 13 | User changes autostart dir | New dir has no hooks; auto-install fires again for new dir. Old dir keeps stale hooks; user cleans up via `tebis hooks uninstall <old>`. |
| 14 | Two different tebis setups on same host using same project | Last one's install wins (same path, same content). No conflict. |
| 15 | Hook script syntax error (shouldn't happen; shellcheck'd) | Claude reports non-zero exit, continues. Test in CI. |
| 16 | User has Claude Code plugin installed that also registers hooks | Merges fine; plugin hooks are at a different command path. |
| 17 | Hook fires before UDS socket is bound (bridge starting up) | Script's `[[ ! -S $SOCKET ]] && exit 0` already handles this. |
| 18 | `--output-format json` Copilot mode | Not a hooks path; orthogonal to this work. |

---

## 6. Implementation plan (ordered)

1. **`src/bridge/agent.rs`** — `AgentKind` + `detect()` (~40 lines; tests).
2. **`src/bridge/typing.rs`** — extract typing loop from autoreply into
   shared helper (~30 lines; no new logic).
3. **`src/agent_hooks/mod.rs`** — trait, data_dir, materialize (~80 lines).
4. **`src/agent_hooks/claude.rs`** — install/uninstall/status (~200 lines).
5. **`src/agent_hooks/copilot.rs`** — install/uninstall/status (~200 lines,
   pending background research).
6. **`contrib/copilot/copilot-hook.sh`** — adapted from claude-hook.sh
   (~80 lines).
7. **`src/config.rs`** — `HooksMode` enum + env parse.
8. **`src/bridge/session.rs`** — `hooked_sessions` HashSet + API.
9. **`src/bridge/session.rs::resolve_or_autostart`** — install hooks on
   agentic autostart when mode=auto.
10. **`src/bridge/mod.rs::handle_update`** — branch on `is_hooked`.
11. **`src/bridge/autoreply.rs`** — delegate typing to `typing.rs`.
12. **`src/service.rs`** — `hooks_install(dir)`, `hooks_uninstall(dir)`,
    `hooks_status(dir)` wrappers.
13. **`src/main.rs`** — argv dispatch: `hooks install|uninstall|status`;
    wire `HooksMode` through HandlerContext and autostart.
14. **Integration test harness** — unit tests for claude.rs's JSON
    merge/unmerge logic against representative inputs.
15. **Review cycles** — code review, UX/lifecycle review, dead-code
    review. Findings applied; see git history.

---

## 7. Code-quality guardrails (rule: do these a lot)

1. After each chunk of 200+ lines, run:
   - `cargo fmt --all && cargo clippy --all-targets -- -D warnings -W clippy::pedantic -W clippy::nursery`
   - `cargo test`
2. After all implementation: dispatch `coderabbit:code-reviewer` + a
   general code-review subagent; apply findings.
3. Also dispatch: dead-code / lifecycle / UX reviews (separate
   subagents). Apply findings.
4. DRY: the JSON-mutate + atomic-write pattern is shared; factor once
   (`agent_hooks/jsonfile.rs` or similar).
5. No dead code: remove the `Command::Send`-returning-`ReactSuccess`
   legacy branch if superseded. Remove `format_pane_reply` from `bridge/mod.rs`
   if only autoreply uses it (move into `autoreply.rs`).
6. No backwards-compat shims. Existing users who set
   `TELEGRAM_AUTOREPLY=off` still get that behavior; no silent migration.

---

## 8. Out of scope for Phase 2

- User-level hook install (`~/.claude/settings.json`,
  `~/.copilot/hooks/`).
- Non-tmux agents (Cursor CLI, Aider, etc.).
- Hook-event-specific routing (e.g. "PermissionPrompt → Telegram button
  keyboard to approve/deny"). Current script forwards as text only.
- Rich notification formatting per event (current header tags are
  plenty).
- Installing hooks on sessions created via `/new` (not autostart).
  Rationale: `/new` creates bare sessions; user launches agent manually;
  our install timing doesn't fit. Power users can `tebis hooks install <dir>`.
