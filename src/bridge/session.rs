//! Default-target + autostart + provisioning lock + hooked-session tracking.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use crate::agent_hooks;
use crate::agent_hooks::{AgentKind, HooksMode};
use crate::platform::multiplexer::{self as mux, Mux, MuxError};

pub struct AutostartConfig {
    pub session: String,
    pub dir: String,
    pub command: String,
}

/// Claude Code's Ink/React TUI drops input for ~1–2 s after spawn.
const TUI_BOOT_DELAY: Duration = Duration::from_secs(3);

pub struct SessionState {
    default_target: Mutex<Option<String>>,
    autostart: Option<AutostartConfig>,
    autostart_lock: tokio::sync::Mutex<()>,
    hooks_mode: HooksMode,
    hooked_sessions: Mutex<HashSet<String>>,
}

impl SessionState {
    pub fn new(autostart: Option<AutostartConfig>, hooks_mode: HooksMode) -> Self {
        Self {
            default_target: Mutex::new(None),
            autostart,
            autostart_lock: tokio::sync::Mutex::new(()),
            hooks_mode,
            hooked_sessions: Mutex::new(HashSet::new()),
        }
    }

    pub fn mark_hooked(&self, session: &str) {
        self.hooked_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session.to_string());
    }

    pub fn unmark_hooked(&self, session: &str) {
        self.hooked_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session);
    }

    #[must_use]
    pub fn is_hooked(&self, session: &str) -> bool {
        self.hooked_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(session)
    }

    pub fn target(&self) -> Option<String> {
        self.lock_target().clone()
    }

    pub fn set_target(&self, session: String) {
        *self.lock_target() = Some(session);
    }

    /// Compare-and-clear so a `NotFound` handler doesn't stomp a concurrent `/target`.
    pub fn clear_target_if(&self, session: &str) {
        let mut guard = self.lock_target();
        if guard.as_deref() == Some(session) {
            *guard = None;
        }
    }

    pub fn autostart_session(&self) -> Option<&str> {
        self.autostart.as_ref().map(|a| a.session.as_str())
    }

    /// Invariant 14: `autostart_lock` serializes provisioning so concurrent messages don't
    /// race the TUI-boot sleep. Hook install runs outside — idempotent, no ordering dep.
    pub async fn resolve_or_autostart(&self, tmux: &Mux) -> Result<String, ResolveError> {
        if let Some(existing) = self.target() {
            return Ok(existing);
        }

        let Some(auto) = self.autostart.as_ref() else {
            return Err(ResolveError::NoTarget);
        };

        let hooked = self.try_install_hooks(auto).await;

        let _guard = self.autostart_lock.lock().await;

        if let Some(existing) = self.target() {
            return Ok(existing);
        }

        // `has_session`/`new_session` is a TOCTOU window; fold `AlreadyExists` into success.
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
                Err(MuxError::AlreadyExists(_)) => {
                    tracing::debug!(
                        session = %auto.session,
                        "autostart: session appeared between has_session and new_session"
                    );
                    false
                }
                Err(e) => return Err(e.into()),
            }
        };

        // If the session died during the boot window, say so explicitly.
        if we_provisioned && !tmux.has_session(&auto.session).await? {
            return Err(ResolveError::AutostartCommandDied(auto.command.clone()));
        }

        // `else` branch is load-bearing: a re-provision after hook install
        // failure must clear a stale `is_hooked` or pane-settle stays suppressed.
        if hooked {
            self.mark_hooked(&auto.session);
        } else {
            self.unmark_hooked(&auto.session);
        }
        self.set_target(auto.session.clone());
        Ok(auto.session.clone())
    }

    /// Returns `true` when hooks were installed. Any error falls through to
    /// pane-settle via `false`. Filesystem I/O runs under `spawn_blocking`.
    async fn try_install_hooks(&self, auto: &AutostartConfig) -> bool {
        if self.hooks_mode != HooksMode::Auto {
            return false;
        }
        let Some(kind) = AgentKind::detect(&auto.command) else {
            tracing::info!(
                command = %auto.command,
                "TELEGRAM_HOOKS_MODE=auto but autostart command is not a recognized agent \
                 — skipping hook install, pane-settle will handle replies"
            );
            return false;
        };
        // Warn about pre-Phase-2 repo-path entries that would double-deliver.
        if matches!(kind, AgentKind::Claude) {
            let legacy = agent_hooks::legacy::scan_claude(Path::new(&auto.dir));
            if !legacy.is_empty() {
                tracing::warn!(
                    dir = %auto.dir,
                    count = legacy.len(),
                    "legacy Claude hook entry detected — may cause double delivery; \
                     remove pre-Phase-2 entries from .claude/settings.local.json"
                );
            }
        }
        let dir = auto.dir.clone();
        let join = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let script = agent_hooks::materialize(kind)?;
            let report = agent_hooks::for_kind(kind).install(Path::new(&dir), &script)?;
            Ok((script, report))
        })
        .await;
        let result = match join {
            Ok(r) => r,
            Err(e) => Err(anyhow::anyhow!("hook install task panicked: {e}")),
        };
        match result {
            Ok((script, report)) => {
                tracing::info!(
                    agent = %kind.display(),
                    dir = %auto.dir,
                    script = %script.display(),
                    events = ?report.events,
                    "hooks installed for autostart session"
                );
                if console::Term::stdout().is_term() {
                    eprintln!(
                        "  {}  installed {} hooks in {}",
                        console::style("▶").cyan().bold(),
                        kind.display(),
                        auto.dir
                    );
                }
                true
            }
            Err(e) => {
                tracing::warn!(
                    err = %e, agent = %kind.display(), dir = %auto.dir,
                    "hook install failed; falling back to pane-settle"
                );
                false
            }
        }
    }

    /// Explicit arg wins, otherwise default target. Never provisions.
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
    Mux(#[from] mux::MuxError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_state() -> SessionState {
        SessionState::new(None, HooksMode::Off)
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
        let s = SessionState::new(Some(autostart("s")), HooksMode::Off);
        assert_eq!(s.autostart_session(), Some("s"));
    }

    #[test]
    fn autostart_command_died_error_mentions_the_command() {
        let err = ResolveError::AutostartCommandDied("clude".into());
        let rendered = err.to_string();
        assert!(rendered.contains("clude"));
        assert!(rendered.contains("TELEGRAM_AUTOSTART_COMMAND"));
    }

    #[test]
    fn no_target_error_includes_fix_hint() {
        let err = ResolveError::NoTarget;
        assert!(err.to_string().contains("/target"));
    }
}
