//! Per-agent hook installation. Ownership sentinel: `command` references
//! the tebis-owned hook script in the tebis data dir (`<data_dir>/<agent>-hook.{sh,ps1}`).

pub mod agent;
pub mod claude;
pub mod copilot;
mod jsonfile;
pub mod legacy;
pub mod manifest;
#[cfg(test)]
mod script_e2e_tests;
#[cfg(test)]
pub mod test_support;

pub use agent::{AgentKind, HooksMode};

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::platform::secure_file;

// Hook scripts: bash `.sh` on Unix, PowerShell `.ps1` on Windows.
// Both are embedded via include_str! so the release binary carries them
// and `materialize()` writes the OS-appropriate one to disk.
#[cfg(unix)]
const CLAUDE_HOOK_FILE: &str = "claude-hook.sh";
#[cfg(windows)]
const CLAUDE_HOOK_FILE: &str = "claude-hook.ps1";

#[cfg(unix)]
const COPILOT_HOOK_FILE: &str = "copilot-hook.sh";
#[cfg(windows)]
const COPILOT_HOOK_FILE: &str = "copilot-hook.ps1";

#[cfg(unix)]
const CLAUDE_HOOK_SCRIPT: &str = include_str!("../../contrib/claude/claude-hook.sh");
#[cfg(windows)]
const CLAUDE_HOOK_SCRIPT: &str = include_str!("../../contrib/claude/claude-hook.ps1");

#[cfg(unix)]
const COPILOT_HOOK_SCRIPT: &str = include_str!("../../contrib/copilot/copilot-hook.sh");
#[cfg(windows)]
const COPILOT_HOOK_SCRIPT: &str = include_str!("../../contrib/copilot/copilot-hook.ps1");

/// Per-agent install / uninstall / status. All methods are idempotent; ours
/// are identified by the sentinel script path.
pub trait HookManager: Send + Sync {
    fn install(&self, project_dir: &Path, script_path: &Path) -> Result<InstallReport>;
    fn uninstall(&self, project_dir: &Path) -> Result<UninstallReport>;
    fn status(&self, project_dir: &Path) -> Result<StatusReport>;
}

#[derive(Debug, Default)]
pub struct InstallReport {
    pub files_written: Vec<PathBuf>,
    pub events: Vec<&'static str>,
}

#[derive(Debug, Default)]
pub struct UninstallReport {
    pub files_modified: Vec<PathBuf>,
    pub files_deleted: Vec<PathBuf>,
    pub events_removed: Vec<String>,
}

#[derive(Debug, Default)]
pub struct StatusReport {
    pub installed_events: Vec<String>,
}

#[must_use]
pub fn for_kind(kind: AgentKind) -> Box<dyn HookManager> {
    match kind {
        AgentKind::Claude => Box::new(claude::ClaudeHooks),
        AgentKind::Copilot => Box::new(copilot::CopilotHooks),
    }
}

pub(crate) fn data_dir() -> Result<PathBuf> {
    crate::platform::paths::data_dir()
}

/// Write hook script to its stable path. Rewrites only on content drift;
/// re-chmods unconditionally so a manual `chmod 0777` gets retightened.
pub fn materialize(agent: AgentKind) -> Result<PathBuf> {
    let dir = data_dir()?;
    crate::platform::secure_file::ensure_private_dir(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let (name, content) = match agent {
        AgentKind::Claude => (CLAUDE_HOOK_FILE, CLAUDE_HOOK_SCRIPT),
        AgentKind::Copilot => (COPILOT_HOOK_FILE, COPILOT_HOOK_SCRIPT),
    };
    let path = dir.join(name);
    let needs_write = !fs::read_to_string(&path).is_ok_and(|cur| cur == content);
    if needs_write {
        jsonfile::atomic_write_bytes(&path, content.as_bytes())?;
    }
    secure_file::set_owner_executable(&path)
        .with_context(|| format!("set owner-executable on {}", path.display()))?;
    Ok(path)
}

/// Shell command the agent should run to invoke the hook. On Unix the
/// script is directly executable (chmod 0700) and the command is just
/// the path. On Windows the script is `.ps1`, so the command wraps it
/// in a `powershell.exe` invocation that disables the profile (speed)
/// and Bypasses the execution policy (for unsigned user-local scripts).
#[must_use]
pub fn script_command(script_path: &Path) -> String {
    #[cfg(unix)]
    {
        script_path.to_string_lossy().into_owned()
    }
    #[cfg(windows)]
    {
        format!(
            "powershell.exe -NoProfile -ExecutionPolicy Bypass -File \"{}\"",
            script_path.display()
        )
    }
}

/// True iff `command_str` (the raw `command` field from an agent's
/// hook config) references a path inside our data dir. Handles both
/// the Unix shape (`<data_dir>/claude-hook.sh`) and the Windows shape
/// (`powershell.exe ... -File "<data_dir>\claude-hook.ps1"`).
pub(super) fn command_references_our_script(command_str: &str) -> bool {
    let Ok(our_dir) = data_dir() else {
        return false;
    };
    let dir_str = our_dir.to_string_lossy();
    command_str.contains(dir_str.as_ref())
}

/// True iff `command_str` references exactly `script_path` (not just
/// "somewhere in our data dir"). Used for dedup on install.
pub(super) fn command_references_script(command_str: &str, script_path: &Path) -> bool {
    let s = script_path.to_string_lossy();
    command_str.contains(s.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_support::with_scratch_data_home;

    #[test]
    fn command_references_our_script_matches_data_dir() {
        with_scratch_data_home("cmd_ref_our", || {
            let our = data_dir().expect("data_dir with HOME override");
            let inside = our.join("claude-hook.sh");
            assert!(command_references_our_script(&inside.to_string_lossy()));
            assert!(!command_references_our_script(
                "/usr/local/bin/other-hook.sh"
            ));
            assert!(!command_references_our_script("claude-hook.sh"));
            // Windows-shape invocation (raw string so we can use it on
            // Unix tests too; the helper does a plain substring check).
            let wrapped = format!(r#"powershell.exe -NoProfile -File "{}""#, inside.display());
            assert!(command_references_our_script(&wrapped));
        });
    }

    #[test]
    fn materialize_writes_then_is_idempotent() {
        with_scratch_data_home("materialize_idem", || {
            let p1 = materialize(AgentKind::Claude).unwrap();
            assert!(p1.exists());
            let mtime1 = fs::metadata(&p1).unwrap().modified().unwrap();

            std::thread::sleep(std::time::Duration::from_millis(10));
            let p2 = materialize(AgentKind::Claude).unwrap();
            assert_eq!(p1, p2);
            let mtime2 = fs::metadata(&p2).unwrap().modified().unwrap();
            assert_eq!(mtime1, mtime2, "expected no rewrite when content matches");
        });
    }

    /// `bash -n` on the embedded Unix scripts so a broken `.sh` hook
    /// can't ship. The Windows `.ps1` equivalents are validated in CI
    /// on windows-latest via `pwsh -NoProfile -Command "..."`; we can't
    /// run that on macOS dev boxes.
    #[cfg(unix)]
    #[test]
    fn embedded_hook_scripts_parse_as_bash() {
        for (name, content) in [
            ("claude", CLAUDE_HOOK_SCRIPT),
            ("copilot", COPILOT_HOOK_SCRIPT),
        ] {
            let path = std::env::temp_dir().join(format!(
                "tebis-shellcheck-{name}-{}-{:x}.sh",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos())
            ));
            fs::write(&path, content).unwrap();
            let out = std::process::Command::new("bash")
                .args(["-n", path.to_str().unwrap()])
                .output()
                .expect("bash available");
            let _ = fs::remove_file(&path);
            assert!(
                out.status.success(),
                "{name}-hook.sh syntax error:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    #[test]
    fn materialize_rewrites_when_content_differs() {
        with_scratch_data_home("materialize_rewrite", || {
            let path = materialize(AgentKind::Claude).unwrap();
            std::fs::write(&path, "outdated content").unwrap();
            let p2 = materialize(AgentKind::Claude).unwrap();
            assert_eq!(p2, path);
            let updated = fs::read_to_string(&p2).unwrap();
            #[cfg(unix)]
            assert!(
                updated.contains("#!/usr/bin/env bash"),
                "rewrote with embedded Unix content"
            );
            #[cfg(windows)]
            assert!(
                updated.contains("tebis") && updated.contains("pipe"),
                "rewrote with embedded Windows content"
            );
        });
    }

    #[cfg(windows)]
    #[test]
    fn embedded_windows_hook_clients_expose_identity_to_pipe_server() {
        for (name, content) in [
            ("claude", CLAUDE_HOOK_SCRIPT),
            ("copilot", COPILOT_HOOK_SCRIPT),
        ] {
            assert!(
                content.contains("TokenImpersonationLevel]::Identification"),
                "{name} hook must let the pipe server identify the same-user client"
            );
            assert!(
                !content.contains("TokenImpersonationLevel]::Anonymous"),
                "{name} hook would be rejected by impersonation-based peer auth"
            );
        }
    }
}
