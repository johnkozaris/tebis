//! Agent identity + hook-install policy.
//!
//! [`AgentKind`] is the canonical "which agent?" enum used by every
//! hook installer and by the autostart path. [`HooksMode`] is the
//! matching "should we install?" policy loaded from
//! `TELEGRAM_HOOKS_MODE`.
//!
//! `AgentKind::detect` string-matches on the leaf of the command so
//! both `claude` and `/opt/homebrew/bin/claude` resolve to
//! `AgentKind::Claude`. Only the first whitespace-delimited token
//! matters — users who put a wrapper script in front
//! (e.g. `my-env-setup claude`) will see detection fail and fall back
//! to pane-settle. That's an acceptable default; they can install
//! hooks manually via `tebis hooks install`.

use anyhow::Result;

use crate::env_file;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    Claude,
    Copilot,
}

/// Policy for whether tebis should install agent-native hooks
/// (Claude Code / Copilot CLI) into the autostart project.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HooksMode {
    /// Don't install anything. Pane-settle auto-reply is the only path.
    #[default]
    Off,
    /// At autostart, if the command is a known agent, materialize the
    /// hook script and install hooks in the project dir. Replies come
    /// via the agent's native event system; pane-settle is suppressed
    /// for that session.
    Auto,
}

impl HooksMode {
    /// Parse from an env-var string. Accepts the documented `auto`
    /// plus the synonyms [`env_file::parse_toggle`] accepts. A
    /// non-empty unrecognized value is an error — silently collapsing
    /// a typo like `on` to `Off` was a real footgun.
    pub fn from_env_str(value: &str) -> Result<Self> {
        match env_file::parse_toggle(value)? {
            Some(true) => Ok(Self::Auto),
            Some(false) | None => Ok(Self::Off),
        }
    }
}

impl AgentKind {
    /// Return `Some` if `command` starts with a known agent binary.
    #[must_use]
    pub fn detect(command: &str) -> Option<Self> {
        let leaf = command.split_whitespace().next()?.rsplit('/').next()?;
        match leaf {
            "claude" | "claude-code" => Some(Self::Claude),
            "copilot" | "copilot-cli" | "gh-copilot" => Some(Self::Copilot),
            _ => None,
        }
    }

    #[must_use]
    pub const fn display(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Copilot => "GitHub Copilot CLI",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_binary_names() {
        assert_eq!(AgentKind::detect("claude"), Some(AgentKind::Claude));
        assert_eq!(AgentKind::detect("copilot"), Some(AgentKind::Copilot));
    }

    #[test]
    fn absolute_paths() {
        assert_eq!(
            AgentKind::detect("/opt/homebrew/bin/claude"),
            Some(AgentKind::Claude)
        );
        assert_eq!(
            AgentKind::detect("/usr/local/bin/copilot"),
            Some(AgentKind::Copilot)
        );
    }

    #[test]
    fn ignores_trailing_args() {
        assert_eq!(
            AgentKind::detect("claude --no-interactive"),
            Some(AgentKind::Claude)
        );
        assert_eq!(
            AgentKind::detect("copilot --continue"),
            Some(AgentKind::Copilot)
        );
    }

    #[test]
    fn rejects_non_agents() {
        assert_eq!(AgentKind::detect("bash"), None);
        assert_eq!(AgentKind::detect("zsh -l"), None);
        assert_eq!(AgentKind::detect("aider"), None);
    }

    #[test]
    fn rejects_wrapper_scripts() {
        // A wrapper like `env-setup claude` doesn't match because the
        // first token is the wrapper. Power users install manually.
        assert_eq!(AgentKind::detect("env-setup claude"), None);
    }

    #[test]
    fn rejects_empty_command() {
        assert_eq!(AgentKind::detect(""), None);
        assert_eq!(AgentKind::detect("   "), None);
    }

    #[test]
    fn claude_code_alias() {
        assert_eq!(AgentKind::detect("claude-code"), Some(AgentKind::Claude));
    }
}
