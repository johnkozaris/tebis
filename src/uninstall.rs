//! Host-wide cleanup helpers shared by `service::{unix,windows}::uninstall`.
//!
//! The service-uninstall flow stops/removes the daemon's unit file. This
//! module covers the rest of the "zero-trace" sequence the `--purge` flag
//! promises: per-project hook removal driven by the manifest, killing any
//! standalone daemon, scrubbing the lockfile + notify socket (which live
//! OUTSIDE `data_dir` on Unix and would otherwise be missed), and the
//! Windows self-delete trampoline.
//!
//! Everything here is best-effort — a stale lockfile or unreadable
//! manifest must never block the rest of the cleanup. Each helper logs
//! warn on failure but never returns `Err`.

use std::path::PathBuf;

use console::style;

use crate::agent_hooks::{self, AgentKind};

/// Result of iterating the manifest and uninstalling per-project hooks.
#[derive(Debug, Default)]
pub struct HookCleanupReport {
    pub files_modified: Vec<PathBuf>,
    pub files_deleted: Vec<PathBuf>,
    pub projects_skipped_missing: Vec<PathBuf>,
    pub projects_failed: Vec<(PathBuf, String)>,
    /// Manifest entries whose agent string this build doesn't recognize.
    /// Likely a future tebis version installed them; we don't know how to
    /// clean them so we leave both the project entries AND the manifest
    /// in place for a newer tebis to handle.
    pub unknown_agents: Vec<(PathBuf, String)>,
}

impl HookCleanupReport {
    /// True when purge cannot safely delete the manifest — some entries
    /// either failed to clean or were unrecognized. Caller should
    /// preserve `data_dir` so the manifest survives for a retry.
    #[must_use]
    pub fn is_partial(&self) -> bool {
        !self.projects_failed.is_empty() || !self.unknown_agents.is_empty()
    }
}

/// Iterate the installed-hooks manifest and run the per-agent uninstaller
/// against every project directory. Idempotent: missing project dirs and
/// already-clean projects are no-ops.
///
/// Call this BEFORE removing `data_dir`, since the manifest lives there.
pub fn uninstall_all_project_hooks() -> HookCleanupReport {
    let mut report = HookCleanupReport::default();
    let entries = agent_hooks::manifest::load_entries();
    for entry in entries {
        if !entry.dir.is_dir() {
            report.projects_skipped_missing.push(entry.dir.clone());
            continue;
        }
        let kind = match entry.agent.as_str() {
            "claude" => AgentKind::Claude,
            "copilot" => AgentKind::Copilot,
            other => {
                // Future tebis versions may grow new agent kinds. We
                // skip cleanup but record so purge stays partial — the
                // manifest is preserved and a newer tebis can finish.
                tracing::warn!(agent = %other, "uninstall: unknown agent kind in manifest");
                report
                    .unknown_agents
                    .push((entry.dir.clone(), other.to_string()));
                continue;
            }
        };
        let mgr = agent_hooks::for_kind(kind);
        match mgr.uninstall(&entry.dir) {
            Ok(r) => {
                report.files_modified.extend(r.files_modified);
                report.files_deleted.extend(r.files_deleted);
            }
            Err(e) => {
                report
                    .projects_failed
                    .push((entry.dir.clone(), e.to_string()));
            }
        }
    }
    report
}

/// Stop a foreground `tebis` process if one is still running outside the
/// service. Reads the PID from the advisory lockfile and sends SIGTERM →
/// SIGKILL (Unix) / `taskkill /T` → `/F` (Windows).
///
/// Returns `Some(pid)` when something was killed, `None` otherwise.
pub fn kill_standalone_daemon() -> Option<u32> {
    let path = crate::lockfile::default_path();
    let pid = crate::lockfile::active_holder(&path)?;
    crate::platform::process::kill_and_wait(pid);
    Some(pid)
}

/// Remove the daemon's transient runtime files: advisory lockfile and
/// (on Unix) the notify UDS. Returns the paths that were actually
/// removed for reporting.
///
/// On Windows the notify endpoint is a named pipe with no FS entry, and
/// the lockfile lives under `data_dir` so `--purge`'s `remove_dir_all`
/// covers it — this call only needs to clean the Unix paths that escape
/// `data_dir` (`/tmp/...` or `$XDG_RUNTIME_DIR/...`).
pub fn remove_runtime_files() -> Vec<PathBuf> {
    let mut removed = Vec::new();
    let candidates = [
        crate::lockfile::default_path(),
        // notify socket / pipe — only the Unix path is on the FS.
        match crate::platform::paths::notify_address() {
            Ok(p) => p,
            Err(_) => return removed,
        },
    ];
    for path in candidates {
        if !path.exists() {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => removed.push(path),
            Err(e) => {
                tracing::warn!(path = %path.display(), err = %e, "uninstall: remove failed");
            }
        }
    }
    removed
}

/// Print a hook-cleanup summary using the standard `tebis` glyph palette.
/// Called from both Unix and Windows service-uninstall flows.
pub fn print_hook_cleanup_summary(report: &HookCleanupReport) {
    if report.files_modified.is_empty()
        && report.files_deleted.is_empty()
        && report.projects_skipped_missing.is_empty()
        && report.projects_failed.is_empty()
        && report.unknown_agents.is_empty()
    {
        return;
    }
    println!();
    println!(
        "{}  Removed project hooks:",
        style("✓").green().bold()
    );
    for p in &report.files_modified {
        println!("    {} {}", style("modified").dim(), style(p.display()).dim());
    }
    for p in &report.files_deleted {
        println!("    {} {}", style("deleted ").dim(), style(p.display()).dim());
    }
    for p in &report.projects_skipped_missing {
        println!(
            "    {} {} (project dir gone)",
            style("skipped ").dim(),
            style(p.display()).dim()
        );
    }
    for (p, err) in &report.projects_failed {
        println!(
            "    {} {}: {}",
            style("FAILED  ").red(),
            p.display(),
            style(err).dim()
        );
    }
    for (p, agent) in &report.unknown_agents {
        println!(
            "    {} {} (unknown agent {agent:?} — needs newer tebis)",
            style("SKIPPED ").yellow(),
            p.display()
        );
    }
}

/// Windows-only: spawn a detached PowerShell trampoline that waits 2s
/// (long enough for our process to exit and release the .exe handle)
/// then removes the install directory recursively.
///
/// We can't simply `fs::remove_file(current_exe())` on Windows like Unix
/// allows — the running .exe is mapped + locked by the loader, so the
/// remove fails with sharing-violation. The trampoline is the standard
/// Windows pattern: a sibling short-lived process holds the cleanup,
/// the doomed process exits first.
#[cfg(windows)]
pub fn spawn_self_delete_trampoline(install_dir: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    // CREATE_NO_WINDOW (0x0800_0000) | DETACHED_PROCESS (0x0000_0008).
    const FLAGS: u32 = 0x0800_0000 | 0x0000_0008;

    // Single-quoted PowerShell string; embedded single quotes are
    // escaped by doubling them per PS lexical rules.
    let path = install_dir.display().to_string().replace('\'', "''");
    let script = format!(
        "Start-Sleep -Milliseconds 2000; \
         Remove-Item -Recurse -Force '{path}' -ErrorAction SilentlyContinue"
    );
    Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            &script,
        ])
        .creation_flags(FLAGS)
        .spawn()
        .context("spawning self-delete trampoline")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_cleanup_report_empty_by_default() {
        let r = HookCleanupReport::default();
        assert!(r.files_modified.is_empty());
        assert!(r.files_deleted.is_empty());
        assert!(r.projects_skipped_missing.is_empty());
        assert!(r.projects_failed.is_empty());
    }

    #[test]
    fn print_summary_silent_on_empty() {
        // Just asserts the call doesn't panic — captured output isn't
        // checked since both paths are valid (print or no-op).
        print_hook_cleanup_summary(&HookCleanupReport::default());
    }
}
