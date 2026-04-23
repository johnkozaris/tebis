//! Per-agent hook installation. Ownership sentinel: `command` points at
//! `$XDG_DATA_HOME/tebis/<agent>-hook.sh`.

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

const CLAUDE_HOOK_FILE: &str = "claude-hook.sh";
const COPILOT_HOOK_FILE: &str = "copilot-hook.sh";

const CLAUDE_HOOK_SCRIPT: &str = include_str!("../../contrib/claude/claude-hook.sh");
const COPILOT_HOOK_SCRIPT: &str = include_str!("../../contrib/copilot/copilot-hook.sh");

/// Per-agent install / uninstall / status. All methods are idempotent; ours
/// are identified by the sentinel script path.
pub trait HookManager: Send + Sync {
    fn agent(&self) -> AgentKind;
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

/// Host-wide tebis data dir — tebis-owned binaries + hook manifest.
/// Thin re-export of [`crate::platform::paths::data_dir`].
pub(crate) fn data_dir() -> Result<PathBuf> {
    crate::platform::paths::data_dir()
}

/// Write hook script to its stable path. Rewrites only on content drift;
/// re-chmods unconditionally so a manual `chmod 0777` gets retightened.
pub fn materialize(agent: AgentKind) -> Result<PathBuf> {
    let dir = data_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
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

/// True when `candidate`'s parent is our data dir. Symlink-tolerant;
/// logs on `data_dir()` failure since the result then under-reports.
pub(super) fn is_our_script(candidate: &Path) -> bool {
    let our_dir = match data_dir() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(err = %e, "is_our_script: data_dir unavailable");
            return false;
        }
    };
    let cand_parent = candidate.parent().unwrap_or(candidate);
    paths_eq(cand_parent, our_dir.as_path())
}

/// Canonicalize-first path equality; `==` fallback for nonexistent paths.
pub(super) fn paths_eq(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_support::with_scratch_data_home;

    #[test]
    fn is_our_script_matches_exact_parent() {
        with_scratch_data_home("is_our_script", || {
            let our = data_dir().expect("data_dir with HOME override");
            assert!(is_our_script(&our.join("claude-hook.sh")));
            assert!(!is_our_script(Path::new("/usr/local/bin/other-hook.sh")));
            assert!(!is_our_script(Path::new("claude-hook.sh")));
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

    /// `bash -n` on the embedded scripts so a broken hook can't ship.
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
            assert!(
                updated.contains("#!/usr/bin/env bash"),
                "rewrote with embedded content"
            );
        });
    }
}
