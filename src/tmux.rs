//! tmux subprocess wrapper. Validates session names, serializes per-session
//! ops via mutex, classifies errors. Strict or permissive allowlist.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::sanitize;

/// 300 ms is safe; Ink/React TUIs drop Enter that arrives too close to text.
const SUBMIT_GAP: Duration = Duration::from_millis(300);

pub struct Tmux {
    strict: HashMap<String, Arc<SessionSlot>>,
    dynamic: std::sync::Mutex<HashMap<String, Arc<SessionSlot>>>,
    permissive: bool,
    max_output_chars: usize,
}

/// Invariant 13: `=NAME:0` — `=` forces exact match (`-t foo` prefix-matches `foobar`);
/// `:0` is required so pane verbs (send-keys, capture-pane) resolve — bare `=NAME` fails.
struct SessionSlot {
    exact_target: String,
    lock: Mutex<()>,
}

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
    /// Empty `allowed` → permissive mode.
    pub fn new(allowed: Vec<String>, max_output_chars: usize) -> Self {
        let permissive = allowed.is_empty();
        let strict = allowed
            .into_iter()
            .map(|name| {
                let exact_target = format!("={name}:0");
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

    /// Lazily-allocated slot count (permissive mode). Exposed for the dashboard.
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

    /// Text → sleep → Enter, atomic under the per-session lock (CLAUDE.md
    /// invariant 3: cancellation mid-sequence strands chars without Enter).
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

        // `-s` takes a bare NAME, not a target — passing `=name` would
        // literally create a session named `=name`. Only `-t` gets the `=` prefix.
        let mut args: Vec<&str> = vec!["new-session", "-d", "-s", session];
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
        // Double-check under the lock so concurrent first references share one slot.
        let fresh = {
            let mut map = self.dynamic.lock().expect("dynamic slots poisoned");
            if let Some(slot) = map.get(session) {
                return Ok(slot.clone());
            }
            let fresh = Arc::new(SessionSlot {
                exact_target: format!("={session}:0"),
                lock: Mutex::new(()),
            });
            map.insert(session.to_string(), fresh.clone());
            fresh
        };
        Ok(fresh)
    }
}

/// Shell-metachar / path-traversal defense. Invariant 2.
#[must_use]
pub fn is_valid_session_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// `send-keys` says "can't find pane"; others say "can't find session".
/// Both fold into `NotFound`.
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

fn strip_equals_prefix(stderr: &str) -> std::borrow::Cow<'_, str> {
    if stderr.contains(": =") {
        std::borrow::Cow::Owned(stderr.replace(": =", ": "))
    } else {
        std::borrow::Cow::Borrowed(stderr)
    }
}

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
