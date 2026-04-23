//! Terminal-multiplexer subprocess wrapper. Validates session names,
//! serializes per-session ops via mutex, classifies errors. Strict or
//! permissive allowlist.
//!
//! Both backends — `tmux` on Unix and `psmux` on Windows — share the
//! tmux-compatible CLI (`new-session`, `send-keys -l` / `-H 0d`,
//! `capture-pane`, etc.), so this module is one implementation with a
//! single `BINARY` name that switches per-OS. Any behavioral quirks
//! unique to psmux get handled inside `classify_status`, which is
//! already fuzzy-tolerant ("no such session" / "session not found"
//! already fold into `NotFound`).

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::sanitize;

/// 300 ms is safe; Ink/React TUIs drop Enter that arrives too close to text.
const SUBMIT_GAP: Duration = Duration::from_millis(300);

/// Binary to invoke. tmux on Unix, psmux on Windows (tmux-compatible
/// Rust port — reads `~/.tmux.conf`, same command language). psmux
/// needs to be on PATH for the Windows build to actually drive
/// sessions; `has_on_path` probes will surface a setup-time warning.
#[cfg(unix)]
pub const BINARY: &str = "tmux";
#[cfg(windows)]
pub const BINARY: &str = "psmux";

/// `BINARY -V` dashboard probe. Returns `"(unknown)"` if the
/// multiplexer isn't installed — no hard failure, just signals the
/// inspect UI.
pub async fn version() -> String {
    match Command::new(BINARY).arg("-V").output().await {
        Ok(out) if out.status.success() => {
            let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
            // tmux prints "tmux 3.5a"; psmux prints similar. Strip the
            // leading binary name if present.
            line.strip_prefix(&format!("{BINARY} "))
                .map_or_else(|| line.clone(), ToString::to_string)
        }
        _ => "(unknown)".to_string(),
    }
}

pub struct Mux {
    strict: HashMap<String, Arc<SessionSlot>>,
    dynamic: std::sync::Mutex<HashMap<String, Arc<SessionSlot>>>,
    permissive: bool,
    max_output_chars: usize,
}

/// Invariant 13: `exact_target` is `=NAME`. The `=` forces tmux exact-match;
/// bare `-t foo` prefix-matches and would land in `foobar`.
struct SessionSlot {
    exact_target: String,
    lock: Mutex<()>,
}

#[derive(Debug, thiserror::Error)]
pub enum MuxError {
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

    #[error("multiplexer {op} failed: {stderr}")]
    CommandFailed { op: &'static str, stderr: String },

    #[error("multiplexer {op} timed out (5s)")]
    Timeout { op: &'static str },

    #[error("multiplexer spawn error: {0}")]
    Spawn(String),
}

impl MuxError {
    #[must_use]
    pub const fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound(_))
    }
}

pub type Result<T> = std::result::Result<T, MuxError>;

impl Mux {
    /// Empty `allowed` → permissive mode.
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

    /// Lazily-allocated slot count (permissive mode). Exposed for the dashboard.
    #[must_use]
    pub fn dynamic_slot_count(&self) -> usize {
        self.dynamic
            .lock()
            .map_or(0, |g| g.len())
    }

    pub async fn list_sessions(&self) -> Result<Vec<String>> {
        let output = run_mux("list-sessions", &["list-sessions", "-F", "#{session_name}"]).await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("no server running") || stderr.contains("no sessions") {
                return Ok(Vec::new());
            }
            return Err(MuxError::CommandFailed {
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
            return Err(MuxError::EmptyInput);
        }

        let out = run_mux(
            "send-keys",
            &["send-keys", "-t", &slot.exact_target, "-l", &sanitized],
        )
        .await?;
        classify_status(&out, "send-keys", session)?;

        tokio::time::sleep(SUBMIT_GAP).await;

        let out = run_mux(
            "send-keys",
            &["send-keys", "-t", &slot.exact_target, "-H", "0d"],
        )
        .await?;
        classify_status(&out, "send-keys", session)?;

        Ok(())
    }

    pub async fn has_session(&self, session: &str) -> Result<bool> {
        let slot = self.slot(session)?;
        let output = run_mux("has-session", &["has-session", "-t", &slot.exact_target]).await?;
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

        let out = run_mux("new-session", &args).await?;
        classify_status(&out, "new-session", session)?;
        Ok(())
    }

    /// Idempotent: `NotFound` → `Ok`.
    pub async fn kill_session(&self, session: &str) -> Result<()> {
        let slot = self.slot(session)?;
        let _guard = slot.lock.lock().await;

        let out = run_mux("kill-session", &["kill-session", "-t", &slot.exact_target]).await?;
        match classify_status(&out, "kill-session", session) {
            Ok(()) | Err(MuxError::NotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub async fn capture_pane(&self, session: &str, lines: usize) -> Result<String> {
        let slot = self.slot(session)?;
        let _guard = slot.lock.lock().await;

        let start_line = format!("-{lines}");
        let output = run_mux(
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
            return Err(MuxError::InvalidName);
        }
        if let Some(slot) = self.strict.get(session) {
            return Ok(slot.clone());
        }
        if !self.permissive {
            let allowed = self.strict.keys().cloned().collect::<Vec<_>>().join(", ");
            return Err(MuxError::NotAllowed {
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
                exact_target: format!("={session}"),
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
        return Err(MuxError::NotFound(session.to_string()));
    }

    if stderr_lc.contains("duplicate session") {
        return Err(MuxError::AlreadyExists(session.to_string()));
    }

    Err(MuxError::CommandFailed {
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

async fn run_mux(op: &'static str, args: &[&str]) -> Result<std::process::Output> {
    let child = Command::new(BINARY)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| MuxError::Spawn(e.to_string()))?;

    match tokio::time::timeout(Duration::from_secs(5), child.wait_with_output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(e)) => Err(MuxError::Spawn(format!("{BINARY} io error: {e}"))),
        Err(_) => {
            tracing::warn!(?args, bin = BINARY, "multiplexer command timed out (5s); killed on drop");
            Err(MuxError::Timeout { op })
        }
    }
}

// These tests cover tmux's stderr wording for error classification —
// inherently Unix (tmux doesn't run natively on Windows; psmux gets
// its own classifier in Phase 5) and uses `ExitStatus::from_raw` for
// synthetic outputs, which is itself Unix-only.
#[cfg(all(test, unix))]
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
        assert!(matches!(err, MuxError::NotFound(ref s) if s == "demo"));
        assert!(err.is_not_found());
    }

    #[test]
    fn classify_recognizes_not_found_pane() {
        // send-keys reports pane, not session
        let out = fake_output(1, "can't find pane: demo\n");
        let err = classify_status(&out, "send-keys", "demo").unwrap_err();
        assert!(matches!(err, MuxError::NotFound(_)));
    }

    #[test]
    fn classify_recognizes_duplicate() {
        let out = fake_output(1, "duplicate session: =demo\n");
        let err = classify_status(&out, "new-session", "demo").unwrap_err();
        assert!(matches!(err, MuxError::AlreadyExists(ref s) if s == "demo"));
    }

    #[test]
    fn classify_falls_through_to_command_failed() {
        let out = fake_output(1, "some unexpected tmux error\n");
        let err = classify_status(&out, "list-sessions", "x").unwrap_err();
        assert!(matches!(err, MuxError::CommandFailed { .. }));
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
        let err = MuxError::NotFound("demo".into());
        assert_eq!(err.to_string(), "session 'demo' not found");
    }

    #[test]
    fn display_of_already_exists_omits_equals_prefix() {
        let err = MuxError::AlreadyExists("demo".into());
        assert_eq!(err.to_string(), "session 'demo' already exists");
    }

    #[test]
    fn display_of_empty_input_is_human_readable() {
        let err = MuxError::EmptyInput;
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
        let t = Mux::new(vec!["allowed".into()], 4000);
        assert!(!t.is_permissive());
        assert!(matches!(t.validate_session("allowed"), Ok(()),));
        assert!(matches!(
            t.validate_session("not-listed"),
            Err(MuxError::NotAllowed { .. }),
        ));
    }

    #[test]
    fn permissive_mode_accepts_any_valid_name() {
        let t = Mux::new(Vec::new(), 4000);
        assert!(t.is_permissive());
        // Regex still enforced.
        assert!(matches!(
            t.validate_session(""),
            Err(MuxError::InvalidName),
        ));
        assert!(matches!(
            t.validate_session("session name"),
            Err(MuxError::InvalidName),
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
        let t = Mux::new(Vec::new(), 4000);
        let a = t.slot("sess").unwrap();
        let b = t.slot("sess").unwrap();
        assert!(Arc::ptr_eq(&a, &b));
    }
}
