//! Detect pre-Phase-2 hook entries in a project's settings file.
//!
//! Users who hand-installed the hook script from a tebis repo checkout
//! (path like `/Users/me/Repos/tebis/contrib/claude/claude-hook.sh`)
//! before the Phase-2 auto-install landed will, on upgrade + `HOOKS_MODE=auto`,
//! end up with a SECOND entry pointing at `$XDG_DATA_HOME/tebis/claude-hook.sh`.
//! Both fire → double delivery. This module returns the offending lines so
//! every install site (CLI + autostart) can warn consistently.

use std::path::Path;

/// Returns the verbatim lines in `.claude/settings.local.json` that
/// reference `claude-hook.sh` under a path NOT inside the current
/// tebis data dir. Empty vec when the settings file is missing, when
/// the data dir is unresolvable, or when no legacy lines are present.
///
/// Crude substring match on purpose — false positives are benign
/// (we're only warning), and a proper JSON walk would miss the "user
/// hand-edited their file into an invalid shape" case.
pub fn scan_claude(project_dir: &Path) -> Vec<String> {
    let settings = project_dir.join(".claude/settings.local.json");
    let Ok(content) = std::fs::read_to_string(&settings) else {
        return Vec::new();
    };
    let Ok(data_dir) = super::data_dir() else {
        return Vec::new();
    };
    let our_prefix = data_dir.to_string_lossy().into_owned();
    content
        .lines()
        .filter(|line| line.contains("claude-hook.sh") && !line.contains(&our_prefix))
        .map(|line| line.trim().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_hooks::AgentKind;
    use crate::agent_hooks::test_support::{with_hook_fixtures, with_scratch_data_home};
    use std::fs;

    #[test]
    fn scan_returns_empty_when_no_settings_file() {
        with_scratch_data_home("legacy-nosettings", || {
            let tmp = std::env::temp_dir().join(format!("tebis-no-claude-{}", std::process::id()));
            let _ = fs::remove_dir_all(&tmp);
            fs::create_dir_all(&tmp).unwrap();
            assert!(scan_claude(&tmp).is_empty());
            let _ = fs::remove_dir_all(&tmp);
        });
    }

    #[test]
    fn scan_ignores_our_own_install() {
        with_hook_fixtures(
            "legacy-our-install",
            AgentKind::Claude,
            |mgr, proj, script| {
                mgr.install(proj, script).unwrap();
                let lines = scan_claude(proj);
                assert!(
                    lines.is_empty(),
                    "our install should not look legacy: {lines:?}"
                );
            },
        );
    }

    #[test]
    fn scan_flags_repo_path_entries() {
        with_scratch_data_home("legacy-repo-path", || {
            let proj = std::env::temp_dir().join(format!("tebis-legacy-{}", std::process::id()));
            let _ = fs::remove_dir_all(&proj);
            fs::create_dir_all(proj.join(".claude")).unwrap();
            fs::write(
                proj.join(".claude/settings.local.json"),
                r#"{
  "hooks": {
    "Stop": [
      {"hooks": [{"type": "command", "command": "/Users/me/Repos/tebis/contrib/claude/claude-hook.sh", "timeout": 15}]}
    ]
  }
}
"#,
            )
            .unwrap();
            let lines = scan_claude(&proj);
            assert_eq!(lines.len(), 1);
            assert!(lines[0].contains("/Users/me/Repos/tebis/contrib/claude/claude-hook.sh"));
            let _ = fs::remove_dir_all(&proj);
        });
    }
}
