//! Stateful session concerns — the "behavior" half of the bridge.
//!
//! `tmux.rs` is a pure API wrapper (knows nothing about defaults, autostart,
//! or recovery). This module owns the mutable state a message handler needs
//! between invocations:
//!
//! - `default_target` — which session plain-text messages route to
//! - `autostart` — config for lazy-provisioning the default target on the
//!   first plain-text message
//! - `autostart_lock` — serializes provisioning so two concurrent messages
//!   don't race into `new_session` / TUI-boot-sleep
//!
//! Handlers should read state through [`SessionState`] rather than poking at
//! the fields directly so the lock discipline lives in one place.
//!
//! # Recovery
//!
//! [`SessionState::clear_target_if`] is how a handler tells the manager
//! "this session you handed me is gone, drop the cache". The next plain-text
//! message re-enters [`SessionState::resolve_or_autostart`] and provisions
//! a fresh session. This is the entire mechanism behind automatic recovery
//! from a dead autostart session (e.g., Claude exited in the tmux pane).

use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use crate::tmux::{self, Tmux, TmuxError};

/// Configuration for autostart-provisioning the default target on the first
/// plain-text message. Populated from env vars by [`crate::config`] but
/// owned here because it's consumed exclusively by [`SessionState`].
pub struct AutostartConfig {
    pub session: String,
    pub dir: String,
    pub command: String,
}

/// Post-spawn sleep for the autostart command. Claude Code's TUI (Ink/React)
/// takes ~1-2 s to boot and accept input; 3 s is conservative. Sending
/// keystrokes before the TUI is ready silently drops them. See the note on
/// [`SessionState::resolve_or_autostart`].
const TUI_BOOT_DELAY: Duration = Duration::from_secs(3);

/// Shared session state. Cheap to clone via `Arc` — construct once in `main`,
/// hand to every message handler.
pub struct SessionState {
    default_target: Mutex<Option<String>>,
    autostart: Option<AutostartConfig>,
    autostart_lock: tokio::sync::Mutex<()>,
}

impl SessionState {
    pub fn new(autostart: Option<AutostartConfig>) -> Self {
        Self {
            default_target: Mutex::new(None),
            autostart,
            autostart_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Snapshot of the current default target. The returned `Option` is
    /// owned so the caller can drop the lock before any `.await`.
    pub fn target(&self) -> Option<String> {
        self.lock_target().clone()
    }

    pub fn set_target(&self, session: String) {
        *self.lock_target() = Some(session);
    }

    /// Clear the default target *only if* it currently points at `session`.
    /// Prevents a handler clearing a target that a concurrent `/target`
    /// command just swapped to something else.
    pub fn clear_target_if(&self, session: &str) {
        let mut guard = self.lock_target();
        if guard.as_deref() == Some(session) {
            *guard = None;
        }
    }

    /// Autostart session name, if configured. Used by `/status` and
    /// `/restart` — both inspect the same config the plain-text path uses.
    pub fn autostart_session(&self) -> Option<&str> {
        self.autostart.as_ref().map(|a| a.session.as_str())
    }

    /// Resolve the session a plain-text message should route to, provisioning
    /// the autostart session if no target is set.
    ///
    /// Concurrency: `autostart_lock` serializes provisioning. A second
    /// plain-text message that arrives during `new_session` + TUI-boot sleep
    /// blocks here, then re-reads `default_target` on the other side (which
    /// the first caller has by then populated) and returns fast.
    ///
    /// If the autostart session exists already (e.g., user created it
    /// manually, or a prior bridge run left it behind), we skip
    /// `new_session` and just cache it as the default target. Users who
    /// want a fresh session should `/kill` it first.
    pub async fn resolve_or_autostart(&self, tmux: &Tmux) -> Result<String, ResolveError> {
        // Fast path — target already set. Bind the cloned Option so the
        // MutexGuard drops before any `.await`.
        if let Some(existing) = self.target() {
            return Ok(existing);
        }

        let Some(auto) = self.autostart.as_ref() else {
            return Err(ResolveError::NoTarget);
        };

        // Serialize the provisioning sequence. Second+ callers block here.
        let _guard = self.autostart_lock.lock().await;

        // Re-check under the lock — another task may have just finished
        // provisioning.
        if let Some(existing) = self.target() {
            return Ok(existing);
        }

        // `has_session` + `new_session` is a TOCTOU window: an external
        // process (or a tmux zombie-state flicker) can flip existence
        // between the two calls. Fold `AlreadyExists` into success so the
        // race surfaces as "target is ready" rather than an error the user
        // can't act on. Skip the TUI-boot sleep on AlreadyExists — the
        // session was already there, so the TUI is presumably up.
        let we_provisioned = if tmux.has_session(&auto.session).await? {
            false
        } else {
            match tmux
                .new_session(&auto.session, Some(&auto.dir), Some(&auto.command))
                .await
            {
                Ok(()) => {
                    tokio::time::sleep(TUI_BOOT_DELAY).await;
                    true
                }
                Err(TmuxError::AlreadyExists(_)) => {
                    tracing::debug!(
                        session = %auto.session,
                        "autostart: session appeared between has_session and new_session — assuming ready"
                    );
                    false
                }
                Err(e) => return Err(e.into()),
            }
        };

        // Post-provision verification: if WE created the session and it's
        // already gone, the configured command exited during the boot
        // window — typically a typo in `TELEGRAM_AUTOSTART_COMMAND` or a
        // startup error. Surface the cause loudly instead of letting
        // `send_keys` fail with a misleading `NotFound` a moment later.
        if we_provisioned && !tmux.has_session(&auto.session).await? {
            return Err(ResolveError::AutostartCommandDied(auto.command.clone()));
        }

        self.set_target(auto.session.clone());
        Ok(auto.session.clone())
    }

    /// Resolve the session for an explicit-or-default operation (e.g.,
    /// `/read [session]`). `explicit` wins; otherwise fall back to the
    /// cached default target. Never provisions — only plain-text messages
    /// trigger autostart.
    pub fn resolve_explicit(&self, explicit: Option<String>) -> Result<String, ResolveError> {
        if let Some(s) = explicit {
            return Ok(s);
        }
        self.target().ok_or(ResolveError::NoTarget)
    }

    fn lock_target(&self) -> MutexGuard<'_, Option<String>> {
        self.default_target.lock().expect("default_target poisoned")
    }
}

/// Things that can go wrong while picking a session. `Tmux` wraps tmux
/// errors from provisioning; `NoTarget` is the "nothing set, autostart not
/// configured" state a plain command like `/read` hits before the user has
/// chosen a target. `AutostartCommandDied` surfaces when the configured
/// autostart command exits before `send_keys` can reach it — almost always
/// a typo or missing binary in `TELEGRAM_AUTOSTART_COMMAND`.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("no target session set. Use /target <session> first.")]
    NoTarget,

    #[error(
        "autostart command {0:?} exited immediately — check TELEGRAM_AUTOSTART_COMMAND \
         (is the binary on PATH? does it need args?)"
    )]
    AutostartCommandDied(String),

    #[error("{0}")]
    Tmux(#[from] tmux::TmuxError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_state() -> SessionState {
        SessionState::new(None)
    }

    #[test]
    fn target_round_trips() {
        let s = empty_state();
        assert!(s.target().is_none());
        s.set_target("foo".into());
        assert_eq!(s.target().as_deref(), Some("foo"));
    }

    #[test]
    fn clear_target_if_matches() {
        let s = empty_state();
        s.set_target("foo".into());
        s.clear_target_if("foo");
        assert!(s.target().is_none());
    }

    #[test]
    fn clear_target_if_ignores_non_match() {
        let s = empty_state();
        s.set_target("foo".into());
        s.clear_target_if("bar");
        assert_eq!(s.target().as_deref(), Some("foo"));
    }

    #[test]
    fn clear_target_if_none_is_noop() {
        let s = empty_state();
        s.clear_target_if("foo");
        assert!(s.target().is_none());
    }

    #[test]
    fn resolve_explicit_prefers_arg() {
        let s = empty_state();
        s.set_target("default".into());
        assert_eq!(
            s.resolve_explicit(Some("arg".into())).unwrap(),
            "arg".to_string()
        );
    }

    #[test]
    fn resolve_explicit_falls_back_to_default() {
        let s = empty_state();
        s.set_target("default".into());
        assert_eq!(s.resolve_explicit(None).unwrap(), "default".to_string());
    }

    #[test]
    fn resolve_explicit_no_target_errors() {
        let s = empty_state();
        assert!(matches!(
            s.resolve_explicit(None).unwrap_err(),
            ResolveError::NoTarget
        ));
    }

    fn autostart(session: &str) -> AutostartConfig {
        AutostartConfig {
            session: session.into(),
            dir: "/tmp".into(),
            command: "echo".into(),
        }
    }

    #[test]
    fn constructs_with_autostart() {
        // Round-trip through SessionState::new to validate the now-local
        // AutostartConfig type composes with SessionState.
        let s = SessionState::new(Some(autostart("s")));
        assert_eq!(s.autostart_session(), Some("s"));
    }

    #[test]
    fn autostart_command_died_error_mentions_the_command() {
        // The Display impl must name the offending command so the user
        // knows which env var to fix.
        let err = ResolveError::AutostartCommandDied("clude".into());
        let rendered = err.to_string();
        assert!(rendered.contains("clude"));
        assert!(rendered.contains("TELEGRAM_AUTOSTART_COMMAND"));
    }

    #[test]
    fn no_target_error_includes_fix_hint() {
        // Users who hit `/read` before `/target` should see what to do next.
        let err = ResolveError::NoTarget;
        assert!(err.to_string().contains("/target"));
    }
}
