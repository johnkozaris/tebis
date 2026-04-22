//! Agent identity + hook-install policy.

use anyhow::Result;

use crate::env_file;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    Claude,
    Copilot,
}

/// Policy for tebis-installed agent hooks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HooksMode {
    #[default]
    Off,
    Auto,
}

impl HooksMode {
    /// Parse via `env_file::parse_toggle` — unknown values error (fail loud on typos).
    pub fn from_env_str(value: &str) -> Result<Self> {
        match env_file::parse_toggle(value)? {
            Some(true) => Ok(Self::Auto),
            Some(false) | None => Ok(Self::Off),
        }
    }
}

impl AgentKind {
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
