//! Windows service backend via Task Scheduler (`schtasks.exe`).
//!
//! # Why Task Scheduler, not SCM
//!
//! tebis is a per-user daemon — it needs the logged-in user's SID for
//! the notify peer-auth check, their `%APPDATA%` for the env file,
//! their `%LOCALAPPDATA%` for the lockfile, and their Git Bash for
//! Claude Code autostart. SCM services default to running as
//! `LocalSystem`, which isolates them from all of that.
//!
//! A Task Scheduler job registered with `/SC ONLOGON /RL LIMITED`
//! starts the binary in the logged-in user's session, as the user,
//! with the user's full environment. This is the closest Windows
//! analogue of a systemd `--user` service or a launchd `LaunchAgent`.
//!
//! SCM with explicit user credentials is tracked for a future Phase-4
//! follow-up if users need it; v1 ships Task Scheduler.
//!
//! # Install layout
//!
//! - **Binary:** `%LOCALAPPDATA%\Programs\tebis\tebis.exe` — stable
//!   path the Task Scheduler entry references. Copied from
//!   `env::current_exe()` at install time.
//! - **Task name:** `tebis` — one per-user task; re-install
//!   (`schtasks /Create ... /F`) replaces atomically.
//! - **Working directory:** `%USERPROFILE%` — so any relative path
//!   the autostart command produces resolves predictably.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use console::style;

use crate::setup;

const TASK_NAME: &str = "tebis";

/// Install: copy the binary to a stable location, then register a
/// logon-triggered scheduled task pointing there.
pub fn install() -> Result<()> {
    let env_path = setup::env_file_path()?;
    if !env_path.exists() {
        bail!(
            "no config at {} — run `tebis setup` first",
            env_path.display()
        );
    }

    refuse_if_foreground_running("install")?;

    let bin_src = env::current_exe().context("locating current tebis binary")?;
    let bin_dst = installed_binary_path()?;

    println!();
    println!(
        "{}  Installing tebis as a background task (Task Scheduler)…",
        style("▶").cyan().bold()
    );
    println!("    binary  {} → {}", bin_src.display(), bin_dst.display());
    copy_binary(&bin_src, &bin_dst)?;

    register_task(&bin_dst)?;

    println!();
    println!(
        "{}  Installed. {}",
        style("✓").green().bold(),
        style("Auto-starts at next logon; `tebis start` runs it now.").dim()
    );
    println!();
    Ok(())
}

pub fn uninstall(purge_flag: bool) -> Result<()> {
    // End the task first if running — `schtasks /End` then `/Delete`.
    let _ = run_schtasks(&["/End", "/TN", TASK_NAME]);
    let del = Command::new("schtasks")
        .args(["/Delete", "/TN", TASK_NAME, "/F"])
        .output()
        .context("spawning schtasks /Delete")?;
    if !del.status.success() {
        let stderr = String::from_utf8_lossy(&del.stderr);
        // A missing task is fine — we're idempotent.
        if !stderr.to_ascii_lowercase().contains("does not exist")
            && !stderr.to_ascii_lowercase().contains("cannot find")
        {
            bail!("schtasks /Delete /TN {TASK_NAME} failed: {}", stderr.trim());
        }
    }

    println!();
    println!("{}  Service removed.", style("✓").green().bold());

    if purge_flag {
        let bin = installed_binary_path().ok();
        let env_file = setup::env_file_path().ok();
        let data = crate::platform::paths::data_dir().ok();
        for entry in [bin.as_deref(), env_file.as_deref(), data.as_deref()]
            .into_iter()
            .flatten()
        {
            if entry.exists() {
                let kind = if entry.is_dir() { "dir " } else { "file" };
                match if entry.is_dir() {
                    fs::remove_dir_all(entry)
                } else {
                    fs::remove_file(entry)
                } {
                    Ok(()) => println!("    {}  {} {}", style("✓").green(), kind, entry.display()),
                    Err(e) => {
                        tracing::warn!(path = %entry.display(), err = %e, "purge: remove failed");
                    }
                }
            }
        }
    }
    println!();
    Ok(())
}

pub fn start() -> Result<()> {
    let out = Command::new("schtasks")
        .args(["/Run", "/TN", TASK_NAME])
        .output()
        .context("spawning schtasks /Run")?;
    if !out.status.success() {
        bail!(
            "schtasks /Run /TN {TASK_NAME} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    println!("{}  Task started.", style("✓").green().bold());
    Ok(())
}

pub fn stop() -> Result<()> {
    let out = Command::new("schtasks")
        .args(["/End", "/TN", TASK_NAME])
        .output()
        .context("spawning schtasks /End")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr
            .to_ascii_lowercase()
            .contains("not currently running")
        {
            println!("{}  Task was not running.", style("ℹ").dim());
            return Ok(());
        }
        bail!("schtasks /End /TN {TASK_NAME} failed: {}", stderr.trim());
    }
    println!("{}  Task stopped.", style("✓").green().bold());
    Ok(())
}

pub fn restart() -> Result<()> {
    // Best-effort stop, then start — a not-running task is fine.
    let _ = stop();
    start()
}

pub fn status() -> Result<()> {
    let out = Command::new("schtasks")
        .args(["/Query", "/TN", TASK_NAME, "/V", "/FO", "LIST"])
        .output()
        .context("spawning schtasks /Query")?;
    if !out.status.success() {
        bail!(
            "schtasks /Query /TN {TASK_NAME} failed (not installed?): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    print!("{}", String::from_utf8_lossy(&out.stdout));
    Ok(())
}

/// Reports `true` when the Task Scheduler entry exists and is running.
#[must_use]
pub fn is_running() -> bool {
    let Ok(out) = Command::new("schtasks")
        .args(["/Query", "/TN", TASK_NAME, "/V", "/FO", "LIST"])
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
    stdout
        .lines()
        .filter_map(|line| line.split_once(':'))
        .any(|(key, value)| key.trim().eq_ignore_ascii_case("status") && value.contains("running"))
}

/// Reports `true` when any foreground `tebis.exe` process is running for
/// the current user. Used only to avoid installing over an active dev run.
fn tebis_process_running() -> bool {
    let current_pid = std::process::id();
    let Ok(out) = Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq tebis.exe", "/FO", "CSV", "/NH"])
        .output()
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    tasklist_contains_other_tebis(&text, current_pid)
}

// ---- Helpers ----

fn installed_binary_path() -> Result<PathBuf> {
    // `%LOCALAPPDATA%\Programs\tebis\tebis.exe`. Mirrors the Microsoft
    // Store / WinGet convention of installing per-user apps under
    // `%LOCALAPPDATA%\Programs\<app>\`.
    let base = env::var_os("LOCALAPPDATA").context("LOCALAPPDATA env var not set")?;
    Ok(PathBuf::from(base)
        .join("Programs")
        .join("tebis")
        .join("tebis.exe"))
}

fn copy_binary(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::copy(src, dst).with_context(|| format!("copying {} → {}", src.display(), dst.display()))?;
    Ok(())
}

fn register_task(bin_path: &Path) -> Result<()> {
    let task_command = quote_task_command(bin_path);
    let out = run_schtasks(&[
        "/Create",
        "/TN",
        TASK_NAME,
        "/TR",
        task_command.as_str(),
        "/SC",
        "ONLOGON",
        "/RL",
        "LIMITED",
        "/F",
    ])?;
    if !out.status.success() {
        bail!(
            "schtasks /Create failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn quote_task_command(bin_path: &Path) -> String {
    format!("\"{}\"", bin_path.display())
}

fn tasklist_contains_other_tebis(text: &str, current_pid: u32) -> bool {
    text.lines()
        .filter_map(parse_tasklist_csv_line)
        .any(|(image, pid)| image.eq_ignore_ascii_case("tebis.exe") && pid != current_pid)
}

fn parse_tasklist_csv_line(line: &str) -> Option<(String, u32)> {
    let fields = csv_fields(line);
    if fields.len() < 2 {
        return None;
    }
    let pid = fields[1].trim().parse().ok()?;
    Some((fields[0].trim().to_string(), pid))
}

fn csv_fields(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut chars = line.chars().peekable();
    let mut in_quotes = false;

    while let Some(ch) = chars.next() {
        match ch {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                field.push('"');
                let _ = chars.next();
            }
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                fields.push(field.trim().to_string());
                field.clear();
            }
            _ => field.push(ch),
        }
    }
    fields.push(field.trim().to_string());
    fields
}

fn run_schtasks(args: &[&str]) -> Result<std::process::Output> {
    Command::new("schtasks")
        .args(args)
        .output()
        .context("spawning schtasks.exe")
}

fn refuse_if_foreground_running(op: &str) -> Result<()> {
    if tebis_process_running() {
        bail!(
            "a tebis process is currently running; stop it before `tebis {op}`. \
             Use `tebis stop` (if installed) or close the foreground terminal."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn tasklist_parser_ignores_current_process() {
        let current = std::process::id();
        let text = format!("\"tebis.exe\",\"{current}\",\"Console\",\"1\",\"12,345 K\"\r\n");
        assert!(!tasklist_contains_other_tebis(&text, current));
    }

    #[test]
    fn tasklist_parser_detects_other_tebis_process() {
        let current = 100_u32;
        let text = "\"tebis.exe\",\"4242\",\"Console\",\"1\",\"12,345 K\"\r\n";
        assert!(tasklist_contains_other_tebis(text, current));
    }

    #[test]
    fn task_command_quotes_paths_with_spaces() {
        let path = PathBuf::from(r"C:\Users\Jane Doe\AppData\Local\Programs\tebis\tebis.exe");
        assert_eq!(
            quote_task_command(&path),
            r#""C:\Users\Jane Doe\AppData\Local\Programs\tebis\tebis.exe""#
        );
    }
}
