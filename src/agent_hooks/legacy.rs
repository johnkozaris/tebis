//! Detect hand-installed hook entries that would otherwise fire alongside ours.

use std::path::Path;

/// Substring match on purpose — false positives are benign (warnings only)
/// and a JSON walk would miss invalid-shape user edits. Matches both the
/// Unix `.sh` form and the Windows `.ps1` form so upgrades from an older
/// install on either platform surface in the warning.
///
/// Windows quirk: `.claude/settings.local.json` stores paths with JSON-escaped
/// backslashes (`C:\\Users\\…`), while `data_dir().to_string_lossy()` returns
/// the raw path (`C:\Users\…`). We check both forms so our own install isn't
/// flagged as legacy.
pub fn scan_claude(project_dir: &Path) -> Vec<String> {
    let settings = project_dir.join(".claude/settings.local.json");
    let Ok(content) = std::fs::read_to_string(&settings) else {
        return Vec::new();
    };
    let Ok(data_dir) = super::data_dir() else {
        return Vec::new();
    };
    let our_prefix = data_dir.to_string_lossy().into_owned();
    #[cfg(windows)]
    let our_prefix_escaped = our_prefix.replace('\\', "\\\\");
    content
        .lines()
        .filter(|line| {
            let hits_hook = line.contains("claude-hook.sh") || line.contains("claude-hook.ps1");
            if !hits_hook {
                return false;
            }
            let is_ours = line.contains(&our_prefix);
            #[cfg(windows)]
            let is_ours = is_ours || line.contains(&our_prefix_escaped);
            !is_ours
        })
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
