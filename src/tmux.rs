use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::sanitize;

/// Delay between sending the text and sending Enter. See `send_keys`.
const SUBMIT_GAP: Duration = Duration::from_millis(300);

/// Pure tmux API surface. Every public method validates the session name
/// via the regex [`is_valid_session_name`], serializes operations on the
/// same session via a per-session lock, and classifies stderr into
/// [`TmuxError`] so callers can recover from stale state without matching
/// on free-form strings.
///
/// Two allowlist modes:
///
/// - **Strict** (`allowed` non-empty at construction): only the pre-declared
///   names resolve. Any other name returns [`TmuxError::NotAllowed`]. Slots
///   are allocated up front and their `=NAME` argv prefix is cached — zero
///   allocation on the hot path.
/// - **Permissive** (`allowed` empty at construction): any name matching
///   the regex resolves. Slots are allocated on first reference and cached
///   in `dynamic` so the per-session lock is stable across calls.
///
/// In either mode the name regex is enforced, the per-session lock is
/// honored, and `send_keys` / `capture_pane` / `kill_session` all go
/// through the same `slot()` entry point.
pub struct Tmux {
    /// Pre-populated slots (strict mode). Empty in permissive mode.
    strict: HashMap<String, Arc<SessionSlot>>,
    /// Lazily-allocated slots (permissive mode). Never shrinks — the map
    /// is keyed by a bounded, regex-validated name, so growth is driven by
    /// real user commands, not attacker input.
    dynamic: std::sync::Mutex<HashMap<String, Arc<SessionSlot>>>,
    /// Cached `strict.is_empty()` so hot-path lookups don't re-read the
    /// map length.
    permissive: bool,
    max_output_chars: usize,
}

/// Per-session cached state. The `exact_target` string (`=NAME`) is the
/// argv value passed to every `tmux -t` call; precomputing it here saves
/// one `format!` allocation per `send_keys` / `capture_pane` / `kill_session`.
/// The inner `Mutex` serializes operations on the same session; `Arc` lets
/// `slot()` hand out ownership that survives the map lock being dropped.
struct SessionSlot {
    exact_target: String,
    lock: Mutex<()>,
}

/// Structured tmux error. `NotFound` / `AlreadyExists` are the two shapes
/// callers act on — everything else is opaque `CommandFailed`.
///
/// Kept `thiserror` so handlers can pattern-match on shape (the project's
/// "thiserror inside modules" rule); `Display` impls are the user-facing
/// strings shown in Telegram replies and intentionally omit the internal
/// `=SESSION` exact-match prefix — that prefix is tmux wire syntax, not
/// something we want leaking into error messages and tempting users to
/// type it back into `/kill =…`.
#[derive(Debug, thiserror::Error)]
pub enum TmuxError {
    #[error("session '{0}' not found")]
    NotFound(String),

    #[error("session '{0}' already exists")]
    AlreadyExists(String),

    #[error("invalid session name: only [A-Za-z0-9._-] allowed, 1..=64 chars")]
    InvalidName,

    #[error("session '{session}' is not in the allowlist. Allowed: {allowed}")]
    NotAllowed { session: String, allowed: String },

    /// Sanitization stripped the entire message (pure control/bidi/zero-width).
    /// Separate from `CommandFailed` so the user sees a clear reason instead
    /// of a `tmux send-keys failed: …` wrapper around internal detail.
    #[error("message is empty (nothing to send after sanitization)")]
    EmptyInput,

    #[error("tmux {op} failed: {stderr}")]
    CommandFailed { op: &'static str, stderr: String },

    #[error("tmux {op} timed out (5s)")]
    Timeout { op: &'static str },

    #[error("tmux spawn error: {0}")]
    Spawn(String),
}

impl TmuxError {
    /// True when the underlying session doesn't exist (from tmux's POV).
    /// Callers use this to detect a stale default-target and clear it
    /// before re-provisioning on the next message.
    #[must_use]
    pub const fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound(_))
    }
}

pub type Result<T> = std::result::Result<T, TmuxError>;

impl Tmux {
    /// Construct a tmux wrapper.
    ///
    /// `allowed` is the strict allowlist. Pass an empty `Vec` to enable
    /// **permissive mode**, where any name matching the regex resolves
    /// and slots are allocated on demand.
    pub fn new(allowed: Vec<String>, max_output_chars: usize) -> Self {
        let permissive = allowed.is_empty();
        let strict = allowed
            .into_iter()
            .map(|name| {
                let exact_target = format!("={name}");
                (
                    name,
                    Arc::new(SessionSlot {
                        exact_target,
                        lock: Mutex::new(()),
                    }),
                )
            })
            .collect();
        Self {
            strict,
            dynamic: std::sync::Mutex::new(HashMap::new()),
            permissive,
            max_output_chars,
        }
    }

    /// True when the wrapper was constructed with an empty allowlist —
    /// any valid session name resolves. The dashboard and `/list` use
    /// this to render "any" instead of a bounded set.
    #[must_use]
    pub const fn is_permissive(&self) -> bool {
        self.permissive
    }

    /// List all active tmux sessions.
    pub async fn list_sessions(&self) -> Result<Vec<String>> {
        let output = run_tmux("list-sessions", &["list-sessions", "-F", "#{session_name}"]).await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // "no server running" / "no sessions" is not an error — just
            // means no sessions.
            if stderr.contains("no server running") || stderr.contains("no sessions") {
                return Ok(Vec::new());
            }
            return Err(TmuxError::CommandFailed {
                op: "list-sessions",
                stderr: stderr.into_owned(),
            });
        }

        let sessions: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        Ok(sessions)
    }

    /// Send keystrokes to a tmux session.
    /// Serializes per-session so text + Enter can't interleave with a concurrent call.
    pub async fn send_keys(&self, session: &str, text: &str) -> Result<()> {
        // Resolve the slot (strict lookup or permissive-mode lazy alloc)
        // and hold the per-session lock for the whole text + Enter pair.
        let slot = self.slot(session)?;
        let _guard = slot.lock.lock().await;

        let sanitized = sanitize::sanitize_tmux_input(text);
        if sanitized.is_empty() {
            return Err(TmuxError::EmptyInput);
        }

        // Send the text as literal keystrokes (-l bypasses key-name lookup).
        let out = run_tmux(
            "send-keys",
            &["send-keys", "-t", &slot.exact_target, "-l", &sanitized],
        )
        .await?;
        classify_status(&out, "send-keys", session)?;

        // Gap before Enter. Claude Code's TUI (Ink/React) sometimes batches
        // Enter that arrives too close to the text and treats it as a newline
        // inside the input instead of submit. 300 ms is the safe middle;
        // 100 ms proved too short in practice for cold TUI boots.
        tokio::time::sleep(SUBMIT_GAP).await;

        // Send Enter as raw hex byte (CR = 0x0d).
        let out = run_tmux(
            "send-keys",
            &["send-keys", "-t", &slot.exact_target, "-H", "0d"],
        )
        .await?;
        classify_status(&out, "send-keys", session)?;

        Ok(())
    }

    /// Check whether a session currently exists in tmux. Uses `tmux
    /// has-session`, whose exit code is the single source of truth
    /// (0 = exists, non-zero = doesn't). Name regex + allowlist gate
    /// apply; no per-session lock is held because this is a pure query.
    pub async fn has_session(&self, session: &str) -> Result<bool> {
        let slot = self.slot(session)?;
        let output = run_tmux("has-session", &["has-session", "-t", &slot.exact_target]).await?;
        Ok(output.status.success())
    }

    /// Create a detached tmux session with the given name, optional working
    /// directory, and optional command to run inside. Holds the per-session
    /// lock for the full create so a concurrent `send_keys` / `capture_pane`
    /// can't race the new-session.
    pub async fn new_session(
        &self,
        session: &str,
        dir: Option<&str>,
        command: Option<&str>,
    ) -> Result<()> {
        let slot = self.slot(session)?;
        let _guard = slot.lock.lock().await;

        let mut args: Vec<&str> = vec!["new-session", "-d", "-s", &slot.exact_target];
        if let Some(d) = dir {
            args.push("-c");
            args.push(d);
        }
        if let Some(c) = command {
            args.push(c);
        }

        let out = run_tmux("new-session", &args).await?;
        classify_status(&out, "new-session", session)?;
        Ok(())
    }

    /// Kill a tmux session by name. **Idempotent**: if the session is
    /// already gone, returns `Ok(())` — a caller's intent ("make sure this
    /// is dead") is satisfied either way. Holds the per-session lock so
    /// any in-flight `send_keys` / `capture_pane` completes before the
    /// session disappears.
    pub async fn kill_session(&self, session: &str) -> Result<()> {
        let slot = self.slot(session)?;
        let _guard = slot.lock.lock().await;

        let out = run_tmux("kill-session", &["kill-session", "-t", &slot.exact_target]).await?;
        // Idempotent: NotFound folds into success — caller's intent
        // ("make sure this is dead") is satisfied either way.
        match classify_status(&out, "kill-session", session) {
            Ok(()) | Err(TmuxError::NotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Capture the visible pane content. `-J` joins wrapped lines deterministically.
    /// Holds the per-session lock so a concurrent `send_keys` can't interleave.
    pub async fn capture_pane(&self, session: &str, lines: usize) -> Result<String> {
        let slot = self.slot(session)?;
        let _guard = slot.lock.lock().await;

        let start_line = format!("-{lines}");
        let output = run_tmux(
            "capture-pane",
            &[
                "capture-pane",
                "-t",
                &slot.exact_target,
                "-p",
                "-J",
                "-S",
                &start_line,
            ],
        )
        .await?;
        classify_status(&output, "capture-pane", session)?;

        let raw = String::from_utf8_lossy(&output.stdout).to_string();
        Ok(sanitize::sanitize_tmux_output(&raw, self.max_output_chars))
    }

    /// Check that a session name is valid and (in strict mode) allowlisted.
    /// Cheap — no subprocess.
    pub fn validate_session(&self, session: &str) -> Result<()> {
        self.slot(session).map(|_| ())
    }

    /// Snapshot of the configured strict allowlist. In permissive mode
    /// this returns an empty `Vec` — callers that need a list of touchable
    /// sessions should combine this with `list_sessions()` and treat
    /// [`Self::is_permissive`] as "all live sessions are touchable".
    pub fn allowlisted_sessions(&self) -> Vec<String> {
        self.strict.keys().cloned().collect()
    }

    /// Resolve a session name to its [`SessionSlot`] (exact-target string
    /// + per-session lock).
    ///
    /// Enforces the name regex in both modes and the strict allowlist in
    /// strict mode; in permissive mode, allocates a fresh slot on first
    /// reference and caches it.
    fn slot(&self, session: &str) -> Result<Arc<SessionSlot>> {
        if !is_valid_session_name(session) {
            return Err(TmuxError::InvalidName);
        }
        if let Some(slot) = self.strict.get(session) {
            return Ok(slot.clone());
        }
        if !self.permissive {
            let allowed = self.strict.keys().cloned().collect::<Vec<_>>().join(", ");
            return Err(TmuxError::NotAllowed {
                session: session.to_string(),
                allowed,
            });
        }
        // Permissive mode: lazy-alloc-and-cache. Re-check under the lock
        // so two concurrent first-references share one slot (and thus one
        // per-session mutex). Released at the end of the `{}` block so the
        // Arc clone happens outside the critical section.
        let fresh = {
            let mut map = self.dynamic.lock().expect("dynamic slots poisoned");
            if let Some(slot) = map.get(session) {
                return Ok(slot.clone());
            }
            let fresh = Arc::new(SessionSlot {
                exact_target: format!("={session}"),
                lock: Mutex::new(()),
            });
            map.insert(session.to_string(), fresh.clone());
            fresh
        };
        Ok(fresh)
    }
}

/// Validate a tmux session name. Strict allowlist — only alphanumeric,
/// hyphen, underscore, and dot. Max 64 chars. No path separators or shell
/// metacharacters.
///
/// Enforced at two points: every public `Tmux` method via `slot`, and at
/// config load in `config.rs` so a bad `TELEGRAM_ALLOWED_SESSIONS` or
/// `TELEGRAM_AUTOSTART_SESSION` fails loudly at startup.
#[must_use]
pub fn is_valid_session_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Map a non-zero tmux exit into the right `TmuxError` variant by looking
/// at stderr. tmux's error wording is stable (hasn't changed in years), and
/// we only need to recognize two shapes — "not found" (session/pane gone)
/// and "already exists" (duplicate). Everything else falls through to
/// `CommandFailed` with the stderr preserved.
///
/// `session` is the bare name (not `=name`) — so user-facing error strings
/// never leak tmux's exact-match prefix.
fn classify_status(output: &std::process::Output, op: &'static str, session: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr_lc = stderr.to_ascii_lowercase();

    // `send-keys` on a missing target reports "can't find pane"; most
    // other subcommands report "can't find session". Treat both as NotFound.
    if stderr_lc.contains("can't find session")
        || stderr_lc.contains("can't find pane")
        || stderr_lc.contains("session not found")
        || stderr_lc.contains("no such session")
    {
        return Err(TmuxError::NotFound(session.to_string()));
    }

    if stderr_lc.contains("duplicate session") {
        return Err(TmuxError::AlreadyExists(session.to_string()));
    }

    Err(TmuxError::CommandFailed {
        op,
        stderr: strip_equals_prefix(&stderr).into_owned(),
    })
}

/// Remove the leading `=` that tmux echoes back in some error messages
/// (notably `new-session`'s "duplicate session: =NAME"). That prefix is
/// our internal exact-match syntax, not meaningful to the user.
fn strip_equals_prefix(stderr: &str) -> std::borrow::Cow<'_, str> {
    if stderr.contains(": =") {
        std::borrow::Cow::Owned(stderr.replace(": =", ": "))
    } else {
        std::borrow::Cow::Borrowed(stderr)
    }
}

/// Run a tmux subprocess with a 5-second bound. On timeout, the child future
/// is dropped — `kill_on_drop(true)` sends SIGKILL — and a warn-level log
/// fires so operators can see the hang.
async fn run_tmux(op: &'static str, args: &[&str]) -> Result<std::process::Output> {
    let child = Command::new("tmux")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| TmuxError::Spawn(e.to_string()))?;

    match tokio::time::timeout(Duration::from_secs(5), child.wait_with_output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(e)) => Err(TmuxError::Spawn(format!("tmux io error: {e}"))),
        Err(_) => {
            tracing::warn!(?args, "tmux command timed out (5s); killed on drop");
            Err(TmuxError::Timeout { op })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_output(status: i32, stderr: &str) -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(status << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn classify_recognizes_not_found_session() {
        let out = fake_output(1, "can't find session: demo\n");
        let err = classify_status(&out, "kill-session", "demo").unwrap_err();
        assert!(matches!(err, TmuxError::NotFound(ref s) if s == "demo"));
        assert!(err.is_not_found());
    }

    #[test]
    fn classify_recognizes_not_found_pane() {
        // send-keys reports pane, not session
        let out = fake_output(1, "can't find pane: demo\n");
        let err = classify_status(&out, "send-keys", "demo").unwrap_err();
        assert!(matches!(err, TmuxError::NotFound(_)));
    }

    #[test]
    fn classify_recognizes_duplicate() {
        let out = fake_output(1, "duplicate session: =demo\n");
        let err = classify_status(&out, "new-session", "demo").unwrap_err();
        assert!(matches!(err, TmuxError::AlreadyExists(ref s) if s == "demo"));
    }

    #[test]
    fn classify_falls_through_to_command_failed() {
        let out = fake_output(1, "some unexpected tmux error\n");
        let err = classify_status(&out, "list-sessions", "x").unwrap_err();
        assert!(matches!(err, TmuxError::CommandFailed { .. }));
    }

    #[test]
    fn classify_passes_success_through() {
        let out = fake_output(0, "");
        assert!(classify_status(&out, "send-keys", "x").is_ok());
    }

    #[test]
    fn strip_equals_prefix_removes_from_error() {
        assert_eq!(
            strip_equals_prefix("duplicate session: =demo").as_ref(),
            "duplicate session: demo"
        );
    }

    #[test]
    fn strip_equals_prefix_leaves_clean_strings_alone() {
        assert_eq!(
            strip_equals_prefix("can't find session: demo").as_ref(),
            "can't find session: demo"
        );
    }

    #[test]
    fn display_of_not_found_omits_equals_prefix() {
        let err = TmuxError::NotFound("demo".into());
        assert_eq!(err.to_string(), "session 'demo' not found");
    }

    #[test]
    fn display_of_already_exists_omits_equals_prefix() {
        let err = TmuxError::AlreadyExists("demo".into());
        assert_eq!(err.to_string(), "session 'demo' already exists");
    }

    #[test]
    fn display_of_empty_input_is_human_readable() {
        let err = TmuxError::EmptyInput;
        assert_eq!(
            err.to_string(),
            "message is empty (nothing to send after sanitization)"
        );
    }

    #[test]
    fn valid_session_names() {
        assert!(is_valid_session_name("claude-code"));
        assert!(is_valid_session_name("session_1"));
        assert!(is_valid_session_name("my.session"));
        assert!(is_valid_session_name("a"));
    }

    #[test]
    fn invalid_session_names() {
        assert!(!is_valid_session_name(""));
        assert!(!is_valid_session_name("a".repeat(65).as_str()));
        assert!(!is_valid_session_name("session name"));
        assert!(!is_valid_session_name("session;cmd"));
        assert!(!is_valid_session_name("../etc/passwd"));
        assert!(!is_valid_session_name("$(whoami)"));
        assert!(!is_valid_session_name("session\ttab"));
    }

    #[test]
    fn strict_mode_rejects_unknown_names() {
        let t = Tmux::new(vec!["allowed".into()], 4000);
        assert!(!t.is_permissive());
        assert!(matches!(t.validate_session("allowed"), Ok(()),));
        assert!(matches!(
            t.validate_session("not-listed"),
            Err(TmuxError::NotAllowed { .. }),
        ));
    }

    #[test]
    fn permissive_mode_accepts_any_valid_name() {
        let t = Tmux::new(Vec::new(), 4000);
        assert!(t.is_permissive());
        // Regex still enforced.
        assert!(matches!(
            t.validate_session(""),
            Err(TmuxError::InvalidName),
        ));
        assert!(matches!(
            t.validate_session("session name"),
            Err(TmuxError::InvalidName),
        ));
        // Any regex-valid name resolves.
        assert!(t.validate_session("arbitrary-name").is_ok());
        assert!(t.validate_session("another.one_2").is_ok());
    }

    #[test]
    fn permissive_mode_caches_slots() {
        // Same name → same `Arc<SessionSlot>`, so the per-session mutex
        // is stable across calls (two concurrent sends on the same
        // session can actually serialize).
        let t = Tmux::new(Vec::new(), 4000);
        let a = t.slot("sess").unwrap();
        let b = t.slot("sess").unwrap();
        assert!(Arc::ptr_eq(&a, &b));
    }
}
