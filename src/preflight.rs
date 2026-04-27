//! System + privilege assessment for `tebis install` and `tebis doctor`.
//!
//! Two consumers:
//! - [`run_install_preflight`] — called at the start of `service::install`
//!   to abort on blockers, surface warnings, and feed the container
//!   drop-in choice (so install doesn't re-run `systemd-detect-virt`).
//! - [`run_doctor`] — preflight plus runtime state (foreground lockfile,
//!   service active, env keys present), exposed as `tebis doctor`.
//!
//! Severity tiers are deliberately small. The rubber-duck rule is: a
//! Block must point to a real install failure with a fix the user can
//! act on; a Warn must be one the user can resolve in a minute. Kernel
//! / cgroup / userns / SELinux / AppArmor checks all failed that bar
//! and are excluded — they belong in `--verbose` mode if at all.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use console::style;

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum Severity {
    Info,
    Warn,
    Block,
}

#[derive(Debug)]
pub struct Check {
    pub severity: Severity,
    pub title: String,
    pub fix: Option<String>,
}

impl Check {
    fn info(title: impl Into<String>) -> Self {
        Self { severity: Severity::Info, title: title.into(), fix: None }
    }
    fn warn(title: impl Into<String>, fix: impl Into<String>) -> Self {
        Self { severity: Severity::Warn, title: title.into(), fix: Some(fix.into()) }
    }
    fn block(title: impl Into<String>, fix: impl Into<String>) -> Self {
        Self { severity: Severity::Block, title: title.into(), fix: Some(fix.into()) }
    }
}

#[derive(Debug, Default)]
pub struct Report {
    pub checks: Vec<Check>,
    /// Container runtime kind reported by `systemd-detect-virt --container`,
    /// or `None` on bare metal / non-Linux. Cached so `service::install`
    /// doesn't re-shell to detect.
    pub container_kind: Option<String>,
}

impl Report {
    pub fn has_blockers(&self) -> bool {
        self.checks.iter().any(|c| c.severity == Severity::Block)
    }
    pub fn has_warnings(&self) -> bool {
        self.checks.iter().any(|c| c.severity == Severity::Warn)
    }
}

/// Pre-install assessment. No service-state checks — those go in doctor.
pub fn run_install_preflight() -> Report {
    let mut checks = common_checks();
    let container_kind = platform_checks(&mut checks);
    Report { checks, container_kind }
}

/// Doctor mode: preflight + runtime/service state.
pub fn run_doctor() -> Report {
    let mut report = run_install_preflight();
    push_runtime_state(&mut report.checks);
    report
}

/// Render the report. One line per check; fix hint on a second indented
/// line. Info rows are dimmed and only shown when `verbose` is true.
pub fn render(report: &Report, verbose: bool) {
    for c in &report.checks {
        if c.severity == Severity::Info && !verbose {
            continue;
        }
        let glyph = match c.severity {
            Severity::Info => style("·").dim().to_string(),
            Severity::Warn => style("⚠").yellow().bold().to_string(),
            Severity::Block => style("✗").red().bold().to_string(),
        };
        println!("  {glyph}  {}", c.title);
        if let Some(fix) = &c.fix {
            println!("      {} {}", style("↳").dim(), style(fix).dim());
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Common (Linux + macOS)
// ─────────────────────────────────────────────────────────────────────

fn common_checks() -> Vec<Check> {
    let mut v = Vec::new();
    push_root_check(&mut v);
    push_env_file_check(&mut v);
    push_writable_dir_checks(&mut v);
    push_path_check(&mut v);
    push_tmux_check(&mut v);
    push_hook_tool_checks(&mut v);
    v
}

fn push_root_check(v: &mut Vec<Check>) {
    // Tebis is a single-user daemon. Running install as root would
    // write the unit/plist into root's home (or HOME=/root) and the
    // service would never reach the user's terminal multiplexer.
    // SAFETY: `geteuid(2)` is async-signal-safe and infallible.
    let euid = unsafe { libc::geteuid() };
    if euid == 0 {
        v.push(Check::block(
            "running as root (euid=0)",
            "exit the root shell and re-run as your normal user — tebis is per-user",
        ));
    }
}

fn push_env_file_check(v: &mut Vec<Check>) {
    let Ok(path) = crate::setup::env_file_path() else {
        v.push(Check::block(
            "$HOME not set — cannot resolve config path",
            "set $HOME to your user home directory",
        ));
        return;
    };
    if !path.exists() {
        v.push(Check::block(
            format!("no config at {}", short(&path)),
            "run `tebis setup` first",
        ));
    }
}

fn push_writable_dir_checks(v: &mut Vec<Check>) {
    let home = match env::var("HOME") {
        Ok(h) => PathBuf::from(h),
        Err(_) => return, // root_check / env_file_check already flagged this
    };
    // Where the binary copy lands.
    check_writable(v, &home.join(".local/bin"), "~/.local/bin", true);
    // Config dir (env file lives here; inspect dashboard edits it).
    if let Ok(cfg) = crate::setup::env_file_path()
        && let Some(parent) = cfg.parent()
    {
        check_writable(v, parent, "config dir", true);
    }
    // Data dir (model cache, hook scripts, manifest).
    if let Ok(data) = crate::agent_hooks::data_dir() {
        check_writable(v, &data, "data dir", true);
    }
}

fn check_writable(v: &mut Vec<Check>, dir: &Path, label: &str, blocking: bool) {
    if let Err(e) = fs::create_dir_all(dir) {
        let msg = format!("cannot create {label} ({}): {e}", short(dir));
        let fix = format!("ensure {} is writable by your user", short(dir));
        v.push(if blocking { Check::block(msg, fix) } else { Check::warn(msg, fix) });
        return;
    }
    // Probe write by creating a temp file. fs::create_dir_all alone
    // doesn't prove the *contents* are writable on read-only bind mounts.
    let probe = dir.join(".tebis-preflight-probe");
    match fs::write(&probe, b"") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
        }
        Err(e) => {
            let msg = format!("{label} not writable ({}): {e}", short(dir));
            let fix = format!("check mount/permissions on {}", short(dir));
            v.push(if blocking { Check::block(msg, fix) } else { Check::warn(msg, fix) });
        }
    }
}

fn push_path_check(v: &mut Vec<Check>) {
    let Ok(home) = env::var("HOME") else { return };
    let target = PathBuf::from(&home).join(".local/bin");
    let in_path = env::var("PATH").is_ok_and(|p| p.split(':').any(|s| Path::new(s) == target));
    if !in_path {
        v.push(Check::warn(
            "~/.local/bin is not in $PATH — `tebis` won't be on PATH after install",
            r#"add to your shell rc:  export PATH="$HOME/.local/bin:$PATH""#,
        ));
    }
}

fn push_tmux_check(v: &mut Vec<Check>) {
    let Some(out) = run_capture("tmux", &["-V"]) else {
        v.push(Check::block(
            "tmux not found — required for session control",
            "install tmux 3.x (apt: tmux · brew: tmux · pacman: tmux)",
        ));
        return;
    };
    // Output is "tmux 3.4" or "tmux next-3.5".
    if let Some(major) = out.split_whitespace().nth(1).and_then(parse_major)
        && major < 3
    {
        v.push(Check::warn(
            format!("tmux {} is old — tebis targets 3.x", out.trim()),
            "upgrade tmux to 3.0 or newer",
        ));
    }
}

fn parse_major(version: &str) -> Option<u32> {
    let trimmed = version.trim_start_matches(|c: char| !c.is_ascii_digit());
    let major = trimmed.split('.').next()?;
    major.parse().ok()
}

fn push_hook_tool_checks(v: &mut Vec<Check>) {
    if which("jq").is_none() {
        v.push(Check::warn(
            "jq not on PATH — agent hooks will silently no-op",
            "install jq (apt: jq · brew: jq · pacman: jq)",
        ));
    }
    // The embedded hook scripts use `nc -U` for the UDS path.
    if which("nc").is_none() {
        v.push(Check::warn(
            "nc not on PATH — agent hooks can't deliver replies",
            "install netcat (apt: netcat-openbsd · macOS: built-in)",
        ));
    }
}

// ─────────────────────────────────────────────────────────────────────
// Linux
// ─────────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn platform_checks(v: &mut Vec<Check>) -> Option<String> {
    push_user_systemd_check(v);
    let kind = detect_container();
    if let Some(k) = &kind {
        // Install will follow up by writing the relaxed drop-in
        // (interactive prompt or non-interactive auto). Surface it
        // here too so the preflight table is the full picture.
        v.push(Check::info(format!(
            "container detected ({k}) — relaxed-hardening drop-in will be installed"
        )));
    }
    push_xdg_runtime_check(v);
    kind
}

#[cfg(target_os = "linux")]
fn push_user_systemd_check(v: &mut Vec<Check>) {
    // Probe the actual interface we'll use. `$XDG_RUNTIME_DIR` and
    // `loginctl show-user` are weaker proxies — only `systemctl --user`
    // tells us the bus is reachable for the current invocation (sudo,
    // detached SSH sessions, and `su` all break this).
    let out = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .output();
    match out {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_owned();
            v.push(Check::block(
                format!("user systemd not reachable: {stderr}"),
                "log in via a real user session (not sudo/su); enable user lingering with \
                 `sudo loginctl enable-linger $USER`",
            ));
        }
        Err(_) => {
            v.push(Check::block(
                "systemctl not on PATH",
                "install systemd (this is unusual on a modern Linux distro)",
            ));
        }
    }
}

/// Detects whether we're running inside a container via
/// `systemd-detect-virt --container`. Returns `Some("lxc")` etc.
///
/// Falls back to filesystem markers for docker/podman in the (rare)
/// case where the helper isn't installed. We deliberately avoid
/// `/proc/1/environ` — under unprivileged user containers (LXC/Incus
/// default) PID 1 is root-owned and the env block is unreadable.
#[cfg(target_os = "linux")]
pub(crate) fn detect_container() -> Option<String> {
    if let Some(out) = run_capture("systemd-detect-virt", &["--container"]) {
        let kind = out.trim();
        if !kind.is_empty() && kind != "none" {
            return Some(kind.to_owned());
        }
    }
    if Path::new("/run/.containerenv").exists() {
        return Some("podman".to_owned());
    }
    if Path::new("/.dockerenv").exists() {
        return Some("docker".to_owned());
    }
    None
}

#[cfg(target_os = "linux")]
fn push_xdg_runtime_check(v: &mut Vec<Check>) {
    // The notify UDS prefers $XDG_RUNTIME_DIR/tebis.sock. Without it,
    // the daemon falls back to /tmp — functional, but world-readable
    // dir defeats the per-user mode-0600 invariant slightly less
    // cleanly than the per-uid runtime dir would.
    match env::var("XDG_RUNTIME_DIR") {
        Ok(s) if !s.is_empty() && Path::new(&s).is_dir() => {}
        _ => v.push(Check::warn(
            "$XDG_RUNTIME_DIR not set — notify socket falls back to /tmp",
            "log in via a desktop/login session, or set XDG_RUNTIME_DIR=/run/user/$(id -u)",
        )),
    }
}

// ─────────────────────────────────────────────────────────────────────
// macOS
// ─────────────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn platform_checks(v: &mut Vec<Check>) -> Option<String> {
    if which("launchctl").is_none() {
        v.push(Check::block(
            "launchctl not on PATH — required for LaunchAgent install",
            "install Xcode Command Line Tools: `xcode-select --install`",
        ));
    }
    // The plist must land under the user's LaunchAgents dir.
    if let Ok(home) = env::var("HOME") {
        let agents = PathBuf::from(home).join("Library/LaunchAgents");
        check_writable(v, &agents, "~/Library/LaunchAgents", true);
    }
    None
}

// ─────────────────────────────────────────────────────────────────────
// Other OSes (Windows is excluded — `service` module is Linux/macOS only)
// ─────────────────────────────────────────────────────────────────────

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_checks(_v: &mut Vec<Check>) -> Option<String> {
    None
}

// ─────────────────────────────────────────────────────────────────────
// Doctor: runtime/service state
// ─────────────────────────────────────────────────────────────────────

fn push_runtime_state(v: &mut Vec<Check>) {
    // Foreground lock: helpful when "why isn't my bot replying" turns
    // out to be "you have two tebis processes fighting over the bus".
    let lock = crate::lockfile::default_path();
    match crate::lockfile::active_holder(&lock) {
        Some(pid) => v.push(Check::info(format!("foreground tebis running (pid {pid})"))),
        None => v.push(Check::info("foreground tebis: not running")),
    }

    #[cfg(target_os = "linux")]
    push_linux_service_state(v);
    #[cfg(target_os = "macos")]
    push_macos_service_state(v);
}

#[cfg(target_os = "linux")]
fn push_linux_service_state(v: &mut Vec<Check>) {
    let unit = match env::var("HOME") {
        Ok(h) => PathBuf::from(h).join(".config/systemd/user/tebis.service"),
        Err(_) => return,
    };
    if !unit.exists() {
        v.push(Check::info("service not installed (run `tebis install`)"));
        return;
    }
    let active = Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "tebis"])
        .status()
        .is_ok_and(|s| s.success());
    if active {
        v.push(Check::info("service is active"));
    } else {
        v.push(Check::warn(
            "service installed but not active",
            "see `journalctl --user -u tebis -n 30 --no-pager`",
        ));
    }
}

#[cfg(target_os = "macos")]
fn push_macos_service_state(v: &mut Vec<Check>) {
    let plist = match env::var("HOME") {
        Ok(h) => PathBuf::from(h).join("Library/LaunchAgents/local.tebis.plist"),
        Err(_) => return,
    };
    if !plist.exists() {
        v.push(Check::info("LaunchAgent not installed (run `tebis install`)"));
        return;
    }
    let loaded = Command::new("launchctl")
        .args(["list", "local.tebis"])
        .output()
        .is_ok_and(|o| o.status.success());
    if loaded {
        v.push(Check::info("LaunchAgent loaded"));
    } else {
        v.push(Check::warn(
            "LaunchAgent installed but not loaded",
            "see `tail -n 30 /tmp/tebis.log` and try `tebis restart`",
        ));
    }
}

// ─────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────

fn which(prog: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(prog);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn run_capture(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn short(p: &Path) -> String {
    let s = p.display().to_string();
    if let Ok(home) = env::var("HOME")
        && let Some(rest) = s.strip_prefix(&home)
    {
        return format!("~{rest}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_major_handles_release_and_prerelease() {
        assert_eq!(parse_major("3.4"), Some(3));
        assert_eq!(parse_major("3.4a"), Some(3));
        assert_eq!(parse_major("next-3.5"), Some(3));
        assert_eq!(parse_major(""), None);
        assert_eq!(parse_major("abc"), None);
    }

    #[test]
    fn report_classifies_severities() {
        let mut r = Report::default();
        r.checks.push(Check::info("ok"));
        assert!(!r.has_blockers());
        assert!(!r.has_warnings());
        r.checks.push(Check::warn("a", "fix"));
        assert!(r.has_warnings());
        r.checks.push(Check::block("b", "fix"));
        assert!(r.has_blockers());
    }
}
