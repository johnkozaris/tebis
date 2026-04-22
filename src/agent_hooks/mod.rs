//! Per-agent hook installation (Claude Code, GitHub Copilot CLI).
//!
//! Each agent exposes a different config surface; the [`HookManager`]
//! trait is the common shape. Materialization of the hook script is
//! shared — both agents shell out to the same "read stdin JSON, push
//! over UDS" script, we just drop a version per agent for clarity.
//!
//! Sentinel: entries tebis owns live at a stable path
//! (`$XDG_DATA_HOME/tebis/<agent>-hook.sh`). An entry is ours iff its
//! command / file name matches. Users never collide because the path
//! is vendor-specific.

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
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const CLAUDE_HOOK_FILE: &str = "claude-hook.sh";
const COPILOT_HOOK_FILE: &str = "copilot-hook.sh";

/// Embedded hook scripts. Materialized on disk the first time they're
/// referenced so the agent can `fork+exec` them.
const CLAUDE_HOOK_SCRIPT: &str = include_str!("../../contrib/claude/claude-hook.sh");
const COPILOT_HOOK_SCRIPT: &str = include_str!("../../contrib/copilot/copilot-hook.sh");

/// Per-agent install / uninstall / inspect operations.
pub trait HookManager: Send + Sync {
    fn agent(&self) -> AgentKind;

    /// Write tebis-owned hook entries into `project_dir`. Idempotent:
    /// re-installing replaces our entries (by sentinel path) and leaves
    /// user-owned entries untouched.
    fn install(&self, project_dir: &Path, script_path: &Path) -> Result<InstallReport>;

    /// Remove tebis-owned hook entries from `project_dir`. Leaves
    /// user-owned entries alone. Idempotent.
    fn uninstall(&self, project_dir: &Path) -> Result<UninstallReport>;

    /// Report what tebis has installed (or would install) in `project_dir`.
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

/// Factory. Returns the right `HookManager` for the agent.
#[must_use]
pub fn for_kind(kind: AgentKind) -> Box<dyn HookManager> {
    match kind {
        AgentKind::Claude => Box::new(claude::ClaudeHooks),
        AgentKind::Copilot => Box::new(copilot::CopilotHooks),
    }
}

/// Base dir for tebis-owned data: `$XDG_DATA_HOME/tebis` or
/// `$HOME/.local/share/tebis`. Neither is a config path — this is for
/// binaries we ship (hook scripts), not user preferences.
pub(crate) fn data_dir() -> Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        return Ok(PathBuf::from(xdg).join("tebis"));
    }
    let home = std::env::var("HOME").context("$HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/share/tebis"))
}

/// Materialize the hook script for `agent` at its stable path.
/// Content-addressable: rewrites only when the embedded script doesn't
/// match what's on disk. Permissions are re-applied on every call so
/// an out-of-band `chmod 0777` gets retightened even when the content
/// hasn't changed.
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
    // Unconditional re-chmod: cheap (one syscall) and guarantees mode
    // 0700 even when content was already correct. Without this, a user
    // who loosened the script via `chmod 0777` for debugging would
    // stay loose until the next tebis upgrade.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod 0700 {}", path.display()))?;
    Ok(path)
}

/// True when `candidate`'s parent dir is the tebis data dir. Used to
/// classify arbitrary entries' command strings as "tebis-owned".
/// Canonicalizes both sides so symlinks / case-fold filesystems don't
/// falsely exclude an entry we did install. Logs at warn if
/// `data_dir()` can't be resolved — we can only return `false` in that
/// case but the user deserves to know why classification is lossy.
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

/// Path equality that tolerates symlinks / case-fold filesystems.
/// Canonicalizes both sides; falls back to raw `==` if either side
/// can't be resolved (e.g. still-being-created path).
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
            assert!(!is_our_script(Path::new("claude-hook.sh"))); // no parent
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

    /// Both embedded hook scripts must parse as valid bash. Catches
    /// syntax errors at test time so a broken script never ships to
    /// users. (Doesn't catch semantic errors — that's shellcheck's
    /// job; add it in CI later.)
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
