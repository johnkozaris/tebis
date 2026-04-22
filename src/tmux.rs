//! tmux subprocess wrapper.
//!
//! Every public method validates the session name against
//! [`is_valid_session_name`], serializes operations on the same session
//! via a per-session lock, and classifies tmux stderr into [`TmuxError`]
//! so callers can recover from stale state without matching on free-form
//! strings. Two modes: strict (pre-declared allowlist) or permissive (any
//! regex-valid name, slot allocated on first use).

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::sanitize;

/// Gap between the text and the Enter keystroke. Ink/React TUIs
/// (Claude Code) treat Enter that arrives too close to text as a newline
/// inside the input rather than a submit. 300 ms is safe; 100 ms was not.
const SUBMIT_GAP: Duration = Duration::from_millis(300);

pub struct Tmux {
    strict: HashMap<String, Arc<SessionSlot>>,
    /// Lazy slots (permissive mode). Never shrinks — key is a bounded,
    /// regex-validated name, so growth is driven by real user traffic.
    dynamic: std::sync::Mutex<HashMap<String, Arc<SessionSlot>>>,
    permissive: bool,
    max_output_chars: usize,
}

/// `exact_target` is the precomputed `=NAME` argv value; `Mutex` serializes
/// per-session operations.
struct SessionSlot {
    exact_target: String,
    lock: Mutex<()>,
}

/// Display strings never leak the internal `=NAME` exact-match prefix —
/// that's tmux wire syntax, not something users should paste back.
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

    /// Sanitization stripped the entire message (pure control / bidi).
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
    #[must_use]
    pub const fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound(_))
    }
}

pub type Result<T> = std::result::Result<T, TmuxError>;

impl Tmux {
    /// Empty `allowed` enables **permissive mode** — any regex-valid name
    /// resolves, slots allocated on first reference.
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

    #[must_use]
    pub const fn is_permissive(&self) -> bool {
        self.permissive
    }

    /// Count of lazily-allocated slots in permissive mode. Used by the
    /// dashboard so operators can see the `dynamic` HashMap's size —
    /// it never shrinks by design (allocation is driven by bounded,
    /// regex-validated session names), but a runaway bot provisioning
    /// hundreds of ad-hoc sessions would surface here before it
    /// becomes a memory concern.
    #[must_use]
    pub fn dynamic_slot_count(&self) -> usize {
        self.dynamic
            .lock()
            .map_or(0, |g| g.len())
    }

    pub async fn list_sessions(&self) -> Result<Vec<String>> {
        let output = run_tmux("list-sessions", &["list-sessions", "-F", "#{session_name}"]).await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
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

    /// Sends text with `-l` (literal, bypasses key-name lookup), a
    /// `SUBMIT_GAP` pause, then Enter as raw hex `0d`. Per-session lock
    /// keeps the text+Enter pair atomic against concurrent calls.
    ///
    /// **Invariant**: the `text` send → sleep → Enter send triple must
    /// run under a single acquisition of the per-session mutex. Adding
    /// an `.await` between the three sub-steps that isn't on the lock
    /// itself is the bug — cancellation at an intermediate point can
    /// leave characters in the pane without the submit Enter, which
    /// then prepends onto the next command. Matches CLAUDE.md invariant
    /// 3 (don't cancel mid-`send_keys`).
    pub async fn send_keys(&self, session: &str, text: &str) -> Result<()> {
        let slot = self.slot(session)?;
        let _guard = slot.lock.lock().await;

        let sanitized = sanitize::sanitize_tmux_input(text);
        if sanitized.is_empty() {
            return Err(TmuxError::EmptyInput);
        }

        let out = run_tmux(
            "send-keys",
            &["send-keys", "-t", &slot.exact_target, "-l", &sanitized],
        )
        .await?;
        classify_status(&out, "send-keys", session)?;

        tokio::time::sleep(SUBMIT_GAP).await;

        let out = run_tmux(
            "send-keys",
            &["send-keys", "-t", &slot.exact_target, "-H", "0d"],
        )
        .await?;
        classify_status(&out, "send-keys", session)?;

        Ok(())
    }

    /// `has-session` is a pure query — no per-session lock taken.
    pub async fn has_session(&self, session: &str) -> Result<bool> {
        let slot = self.slot(session)?;
        let output = run_tmux("has-session", &["has-session", "-t", &slot.exact_target]).await?;
        Ok(output.status.success())
    }

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

    /// Idempotent: `NotFound` → `Ok`.
    pub async fn kill_session(&self, session: &str) -> Result<()> {
        let slot = self.slot(session)?;
        let _guard = slot.lock.lock().await;

        let out = run_tmux("kill-session", &["kill-session", "-t", &slot.exact_target]).await?;
        match classify_status(&out, "kill-session", session) {
            Ok(()) | Err(TmuxError::NotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// `-J` joins wrapped lines deterministically. Holds the per-session
    /// lock so a concurrent `send_keys` can't interleave.
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

    pub fn validate_session(&self, session: &str) -> Result<()> {
        self.slot(session).map(|_| ())
    }

    /// Strict allowlist snapshot. Empty in permissive mode.
    pub fn allowlisted_sessions(&self) -> Vec<String> {
        self.strict.keys().cloned().collect()
    }

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
        // Lazy alloc + cache; re-check under the lock so concurrent first
        // references share one slot (and one mutex).
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

/// Shell-metachar / path-traversal defense. Enforced at config load and
/// at every public `Tmux` method via `slot`.
#[must_use]
pub fn is_valid_session_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Classify tmux stderr into a typed error. `send-keys` reports "can't find
/// pane"; most other subcommands report "can't find session" — both fold
/// into `NotFound`. `session` is the bare name so user-facing strings never
/// leak the `=NAME` prefix.
fn classify_status(output: &std::process::Output, op: &'static str, session: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr_lc = stderr.to_ascii_lowercase();

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

/// Strip the `=` that `new-session` echoes back in "duplicate session: =NAME".
fn strip_equals_prefix(stderr: &str) -> std::borrow::Cow<'_, str> {
    if stderr.contains(": =") {
        std::borrow::Cow::Owned(stderr.replace(": =", ": "))
    } else {
        std::borrow::Cow::Borrowed(stderr)
    }
}

/// 5 s bound; on timeout the child is dropped and `kill_on_drop` sends SIGKILL.
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
