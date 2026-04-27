//! Background-service lifecycle: install/uninstall/start/stop/status for
//! launchd (macOS) or systemd user (Linux).

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use console::style;

use crate::{fsutil, lockfile};

#[cfg(target_os = "macos")]
const MACOS_PLIST_TEMPLATE: &str = include_str!("../../contrib/macos/local.tebis.plist");

#[cfg(target_os = "linux")]
const LINUX_SERVICE: &str = include_str!("../../contrib/linux/tebis.service");

#[cfg(target_os = "linux")]
const LINUX_CONTAINER_DROPIN: &str = include_str!("../../contrib/linux/tebis-container.conf");

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const LAUNCHD_LABEL: &str = "local.tebis";
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const SYSTEMD_UNIT_NAME: &str = "tebis";

pub fn install() -> Result<()> {
    refuse_if_foreground_running("install")?;

    // Preflight: assesses container, privileges, dependencies, and
    // writable paths up front. Aborts on blockers; on warnings, prompts
    // (TTY) or auto-continues (non-interactive). The detected container
    // kind threads through so install_linux doesn't re-shell.
    let report = crate::preflight::run_install_preflight();
    if !report.checks.is_empty() {
        println!();
        println!("{}  System assessment:", style("▶").cyan().bold());
        crate::preflight::render(&report, false);
    }
    if report.has_blockers() {
        bail!("preflight found blocking issues — fix the items above and re-run");
    }
    if report.has_warnings() && console::Term::stdout().is_term() {
        println!();
        let go = dialoguer::Confirm::with_theme(&dialoguer::theme::ColorfulTheme::default())
            .with_prompt("Continue with install?")
            .default(true)
            .interact()
            .unwrap_or(true);
        if !go {
            bail!("install cancelled");
        }
    }

    let bin_src = env::current_exe().context("locating current tebis binary")?;
    let bin_dst = home_dir()?.join(".local/bin/tebis");

    println!();
    println!(
        "{}  Installing tebis as a background service…",
        style("▶").cyan().bold()
    );
    println!("    binary  {} → {}", short(&bin_src), short(&bin_dst));
    install_binary(&bin_src, &bin_dst)?;

    #[cfg(target_os = "macos")]
    install_macos()?;
    #[cfg(target_os = "linux")]
    install_linux(report.container_kind.as_deref())?;
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = report; // silence unused-warning on unsupported builds
        bail!("unsupported platform — macOS and Linux only");
    }

    println!();
    println!(
        "{}  Installed. {}",
        style("✓").green().bold(),
        style("Auto-starts at login; respawns on crash.").dim(),
    );
    // PATH warning is now part of the preflight table; don't re-emit.
    println!();
    Ok(())
}

/// Tmp-then-rename 0644 write via [`fsutil::atomic_write`]. A torn plist would break `launchctl load`.
fn atomic_write_0644(path: &Path, bytes: &[u8]) -> Result<()> {
    fsutil::atomic_write(path, bytes, 0o644)
}

fn install_binary(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    if fs::canonicalize(src).ok() == fs::canonicalize(dst).ok() {
        return Ok(());
    }
    fs::copy(src, dst).with_context(|| format!("copying {} → {}", src.display(), dst.display()))?;
    fs::set_permissions(dst, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod 0755 {}", dst.display()))?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_macos() -> Result<()> {
    // Derive user from `$HOME`, not `$USER` — under `sudo -E` they disagree
    // and a mismatched username loads the plist into the wrong launchd domain.
    let home = env::var("HOME").context("HOME env var not set")?;
    let user = std::path::Path::new(&home)
        .file_name()
        .and_then(|n| n.to_str())
        .context("HOME has no final path component — is $HOME malformed?")?
        .to_string();
    let plist = MACOS_PLIST_TEMPLATE.replace("USERNAME", &user);
    let plist_path = plist_path()?;
    let plist_dir = plist_path
        .parent()
        .context("plist path has no parent — $HOME is malformed?")?;
    fs::create_dir_all(plist_dir).with_context(|| format!("creating {}", plist_dir.display()))?;
    atomic_write_0644(&plist_path, plist.as_bytes())
        .with_context(|| format!("writing {}", plist_path.display()))?;
    println!("    plist   {}", short(&plist_path));

    let _ = launchctl_quiet(&[OsStr::new("unload"), plist_path.as_os_str()]);
    run("launchctl", [OsStr::new("load"), plist_path.as_os_str()])?;
    println!("    launchd loaded (label: {LAUNCHD_LABEL})");
    println!("    logs    tail -f /tmp/tebis.log");
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_linux(container_kind: Option<&str>) -> Result<()> {
    let unit_path = systemd_unit_path()?;
    let unit_dir = unit_path
        .parent()
        .context("systemd unit path has no parent — $HOME is malformed?")?;
    fs::create_dir_all(unit_dir).with_context(|| format!("creating {}", unit_dir.display()))?;
    atomic_write_0644(&unit_path, LINUX_SERVICE.as_bytes())
        .with_context(|| format!("writing {}", unit_path.display()))?;
    println!("    unit    {}", short(&unit_path));

    // Detected by preflight. The unit's hardening (CapabilityBoundingSet,
    // Protect*, RestrictNamespaces, …) cannot be applied by user
    // systemd inside an unprivileged container — write a relaxed
    // drop-in. The container itself is the sandbox.
    if let Some(kind) = container_kind {
        let dropin = systemd_dropin_path()?;
        let install_dropin = if dropin.exists() {
            true
        } else if console::Term::stdout().is_term() {
            println!();
            dialoguer::Confirm::with_theme(&dialoguer::theme::ColorfulTheme::default())
                .with_prompt(format!(
                    "Container detected ({kind}). Install relaxed-hardening drop-in?"
                ))
                .default(true)
                .interact()
                .unwrap_or(true)
        } else {
            eprintln!(
                "    note    container ({kind}); installing relaxed drop-in (non-interactive)"
            );
            true
        };
        if install_dropin {
            let dropin_dir = dropin
                .parent()
                .context("drop-in path has no parent — malformed unit dir?")?;
            fs::create_dir_all(dropin_dir)
                .with_context(|| format!("creating {}", dropin_dir.display()))?;
            atomic_write_0644(&dropin, LINUX_CONTAINER_DROPIN.as_bytes())
                .with_context(|| format!("writing {}", dropin.display()))?;
            println!("    drop-in {}", short(&dropin));
        }
    }

    run("systemctl", ["--user", "daemon-reload"])?;
    run(
        "systemctl",
        ["--user", "enable", "--now", SYSTEMD_UNIT_NAME],
    )?;

    if !wait_for_active_linux() {
        explain_systemd_failure();
        bail!("tebis service failed to start — see hint above");
    }

    println!("    systemd enabled + started");
    println!(
        "    note    {} {}",
        style("to survive logout:").dim(),
        style("loginctl enable-linger $USER").bold(),
    );
    println!("    logs    journalctl --user -u tebis -f");
    Ok(())
}

#[cfg(target_os = "linux")]
fn systemd_dropin_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".config/systemd/user/tebis.service.d/container.conf"))
}

/// Polls `systemctl --user is-active` for up to ~3s. Distinguishes a
/// transient `activating` (normal during boot/reload) from a unit that
/// has already failed and is auto-restarting.
#[cfg(target_os = "linux")]
fn wait_for_active_linux() -> bool {
    use std::thread::sleep;
    use std::time::Duration;
    for _ in 0..6 {
        sleep(Duration::from_millis(500));
        if Command::new("systemctl")
            .args(["--user", "is-active", "--quiet", SYSTEMD_UNIT_NAME])
            .status()
            .is_ok_and(|s| s.success())
        {
            return true;
        }
    }
    false
}

/// Prints an actionable diagnostic when the unit isn't active. Tails
/// the journal looking for the well-known 218/CAPABILITIES marker so
/// the container case gets a tailored fix-it suggestion.
#[cfg(target_os = "linux")]
fn explain_systemd_failure() {
    let log = Command::new("journalctl")
        .args([
            "--user",
            "-u",
            SYSTEMD_UNIT_NAME,
            "-n",
            "30",
            "--no-pager",
        ])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();

    eprintln!();
    eprintln!(
        "{}  tebis is not active after install. Last journal lines:",
        style("✗").red().bold()
    );
    for line in log.lines().rev().take(15).collect::<Vec<_>>().into_iter().rev() {
        eprintln!("    {line}");
    }

    if log.contains("218/CAPABILITIES") || log.contains("Failed to drop capabilities") {
        eprintln!();
        eprintln!(
            "    {} systemd cannot drop capabilities here — typical of",
            style("hint").yellow().bold(),
        );
        eprintln!("    unprivileged containers. Install the relaxed drop-in:");
        eprintln!();
        eprintln!(
            "      {}",
            style("tebis uninstall && tebis install   # re-run; install detects container").dim()
        );
        eprintln!(
            "      {}",
            style("# or, manually:").dim()
        );
        eprintln!(
            "      {}",
            style("mkdir -p ~/.config/systemd/user/tebis.service.d").dim()
        );
        eprintln!(
            "      {}",
            style("# write the relaxed [Service] override there, then:").dim()
        );
        eprintln!(
            "      {}",
            style("systemctl --user daemon-reload && systemctl --user restart tebis").dim()
        );
    }
}

pub fn uninstall(purge_flag: bool) -> Result<()> {
    println!();
    println!(
        "{}  Removing tebis background service…",
        style("▶").cyan().bold()
    );
    #[cfg(target_os = "macos")]
    uninstall_macos()?;
    #[cfg(target_os = "linux")]
    uninstall_linux()?;
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    bail!("unsupported platform");
    let bin = home_dir()?.join(".local/bin/tebis");
    let env_dir = home_dir()?.join(".config/tebis");
    let data_dir = crate::agent_hooks::data_dir().ok();

    println!();
    println!("{}  Service removed.", style("✓").green().bold());

    // Show what's eligible for purge (binary, env, data cache). If
    // nothing, skip the prompt entirely.
    let purge_candidates: Vec<&Path> = {
        let mut v: Vec<&Path> = Vec::new();
        if bin.exists() {
            v.push(&bin);
        }
        if env_dir.exists() {
            v.push(&env_dir);
        }
        if let Some(d) = data_dir.as_deref()
            && d.exists()
        {
            v.push(d);
        }
        v
    };

    if purge_candidates.is_empty() {
        println!();
        return Ok(());
    }

    println!();
    println!("    {}", style("User-state paths still on disk:").dim());
    for p in &purge_candidates {
        println!("    {}", short(p));
    }

    // CLI flag wins. Otherwise prompt on a TTY; non-interactive
    // invocations (scripts, CI) default to no-purge — same as the old
    // conservative behavior before `--purge` existed.
    let should_purge = if purge_flag {
        true
    } else if console::Term::stdout().is_term() {
        println!();
        dialoguer::Confirm::with_theme(&dialoguer::theme::ColorfulTheme::default())
            .with_prompt("Also purge these (env, models, hook manifest)?")
            .default(false)
            .interact()
            .unwrap_or(false)
    } else {
        println!();
        println!(
            "    {}",
            style("(non-interactive — left in place. Pass `--purge` to remove.)").dim()
        );
        false
    };

    if should_purge {
        purge_user_state(&bin, &env_dir, data_dir.as_deref())?;
    }
    println!();
    Ok(())
}

/// Remove tebis-owned on-disk state. Per-project hook entries and system
/// packages (espeak-ng etc.) are preserved — uninstalling them is hostile.
fn purge_user_state(bin: &Path, env_dir: &Path, data_dir: Option<&Path>) -> Result<()> {
    println!();
    println!("{}  Purging user state…", style("▶").cyan().bold());

    let mut removed: Vec<PathBuf> = Vec::new();
    for p in [bin, env_dir] {
        if p.exists() {
            if p.is_dir() {
                fs::remove_dir_all(p).with_context(|| format!("removing {}", p.display()))?;
            } else {
                fs::remove_file(p).with_context(|| format!("removing {}", p.display()))?;
            }
            removed.push(p.to_path_buf());
        }
    }
    if let Some(d) = data_dir
        && d.exists()
    {
        fs::remove_dir_all(d).with_context(|| format!("removing {}", d.display()))?;
        removed.push(d.to_path_buf());
    }

    println!();
    if removed.is_empty() {
        println!(
            "{}  Nothing to purge — user state was already clean.",
            style("·").dim()
        );
    } else {
        println!(
            "{}  Purged {} path{}:",
            style("✓").green().bold(),
            removed.len(),
            if removed.len() == 1 { "" } else { "s" }
        );
        for p in &removed {
            println!("    {}", style(p.display()).dim());
        }
    }

    println!();
    println!(
        "    {}",
        style("Per-project agent hooks (if any) stay — remove with:").dim()
    );
    println!(
        "    {}",
        style("    tebis hooks list       # see which dirs have hooks").dim()
    );
    println!("    {}", style("    tebis hooks uninstall <dir>").dim());
    println!();
    println!(
        "    {}",
        style("System packages (espeak-ng) stay. Remove manually if unused:").dim()
    );
    #[cfg(target_os = "macos")]
    println!("    {}", style("    brew uninstall espeak-ng").dim());
    #[cfg(target_os = "linux")]
    {
        println!(
            "    {}",
            style("    sudo apt remove espeak-ng     # Debian/Ubuntu").dim()
        );
        println!(
            "    {}",
            style("    sudo dnf remove espeak-ng     # Fedora").dim()
        );
        println!(
            "    {}",
            style("    sudo pacman -R espeak-ng      # Arch").dim()
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn uninstall_macos() -> Result<()> {
    let plist = plist_path()?;
    if plist.exists() {
        let _ = launchctl_quiet(&[OsStr::new("unload"), plist.as_os_str()]);
        fs::remove_file(&plist).with_context(|| format!("removing {}", plist.display()))?;
        println!("    launchd unloaded");
        println!("    plist   removed {}", short(&plist));
    } else {
        println!("    {} no plist at {}", style("·").dim(), short(&plist));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn uninstall_linux() -> Result<()> {
    let _ = run_quiet(
        "systemctl",
        ["--user", "disable", "--now", SYSTEMD_UNIT_NAME],
    );
    let unit = systemd_unit_path()?;
    if unit.exists() {
        fs::remove_file(&unit).with_context(|| format!("removing {}", unit.display()))?;
        println!("    unit    removed {}", short(&unit));
    } else {
        println!("    {} no unit at {}", style("·").dim(), short(&unit));
    }
    // Clean up the container drop-in too. Best-effort — silently skip
    // if absent (most installs won't have one).
    if let Ok(dropin) = systemd_dropin_path()
        && dropin.exists()
    {
        let _ = fs::remove_file(&dropin);
        if let Some(d) = dropin.parent() {
            // rmdir succeeds only if empty; ignore if user added files.
            let _ = fs::remove_dir(d);
        }
        println!("    drop-in removed {}", short(&dropin));
    }
    let _ = run_quiet("systemctl", ["--user", "daemon-reload"]);
    Ok(())
}

pub fn start() -> Result<()> {
    ensure_installed()?;
    refuse_if_foreground_running("start")?;
    #[cfg(target_os = "macos")]
    run("launchctl", ["start", LAUNCHD_LABEL])?;
    #[cfg(target_os = "linux")]
    {
        run("systemctl", ["--user", "start", SYSTEMD_UNIT_NAME])?;
        if !wait_for_active_linux() {
            explain_systemd_failure();
            bail!("tebis service failed to start — see hint above");
        }
    }
    println!("{}  tebis started.", style("✓").green().bold());
    Ok(())
}

pub fn stop() -> Result<()> {
    ensure_installed()?;
    #[cfg(target_os = "macos")]
    run("launchctl", ["stop", LAUNCHD_LABEL])?;
    #[cfg(target_os = "linux")]
    run("systemctl", ["--user", "stop", SYSTEMD_UNIT_NAME])?;
    println!("{}  tebis stopped.", style("✓").green().bold());
    Ok(())
}

pub fn restart() -> Result<()> {
    ensure_installed()?;
    #[cfg(target_os = "macos")]
    {
        // SAFETY: `getuid(2)` is async-signal-safe and infallible.
        let user_domain = format!("gui/{}", unsafe { libc::getuid() });
        let target = format!("{user_domain}/{LAUNCHD_LABEL}");
        run("launchctl", ["kickstart", "-k", target.as_str()])?;
    }
    #[cfg(target_os = "linux")]
    {
        run("systemctl", ["--user", "restart", SYSTEMD_UNIT_NAME])?;
        if !wait_for_active_linux() {
            explain_systemd_failure();
            bail!("tebis service failed to start — see hint above");
        }
    }
    println!("{}  tebis restarted.", style("✓").green().bold());
    Ok(())
}

pub fn status() -> Result<()> {
    println!();
    #[cfg(target_os = "macos")]
    status_macos()?;
    #[cfg(target_os = "linux")]
    status_linux()?;
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    bail!("unsupported platform");

    let lock_path = lockfile::default_path();
    match lockfile::active_holder(&lock_path) {
        Some(pid) => println!("  Foreground  {} (pid {pid})", style("running").green()),
        None => println!("  Foreground  {}", style("not running").dim()),
    }
    println!();
    Ok(())
}

#[cfg(target_os = "macos")]
fn status_macos() -> Result<()> {
    let installed = plist_path()?.exists();
    println!(
        "  Service     {}",
        if installed {
            style("installed").green().to_string()
        } else {
            style("not installed").red().to_string()
        }
    );
    if installed {
        let loaded = Command::new("launchctl")
            .args(["list", LAUNCHD_LABEL])
            .output()
            .is_ok_and(|o| o.status.success());
        println!(
            "  Loaded      {}",
            if loaded {
                style("yes").green().to_string()
            } else {
                style("no").yellow().to_string()
            }
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn status_linux() -> Result<()> {
    let installed = systemd_unit_path()?.exists();
    println!(
        "  Service     {}",
        if installed {
            style("installed").green().to_string()
        } else {
            style("not installed").red().to_string()
        }
    );
    if installed {
        let active = Command::new("systemctl")
            .args(["--user", "is-active", "--quiet", SYSTEMD_UNIT_NAME])
            .status()
            .is_ok_and(|s| s.success());
        println!(
            "  Active      {}",
            if active {
                style("yes").green().to_string()
            } else {
                style("no").yellow().to_string()
            }
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn is_running() -> bool {
    Command::new("launchctl")
        .args(["list", LAUNCHD_LABEL])
        .output()
        .is_ok_and(|o| o.status.success())
}

#[cfg(target_os = "linux")]
pub fn is_running() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", SYSTEMD_UNIT_NAME])
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn is_running() -> bool {
    false
}

fn ensure_installed() -> Result<()> {
    #[cfg(target_os = "macos")]
    let path = plist_path()?;
    #[cfg(target_os = "linux")]
    let path = systemd_unit_path()?;
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    bail!("unsupported platform");
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if !path.exists() {
        bail!("not installed — run `tebis install` first");
    }
    Ok(())
}

fn home_dir() -> Result<PathBuf> {
    env::var("HOME")
        .map(PathBuf::from)
        .context("HOME env var not set")
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn plist_path() -> Result<PathBuf> {
    Ok(home_dir()?.join("Library/LaunchAgents/local.tebis.plist"))
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn systemd_unit_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".config/systemd/user/tebis.service"))
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

fn run<I, S>(cmd: &str, args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let st = Command::new(cmd)
        .args(args)
        .status()
        .with_context(|| format!("spawning {cmd}"))?;
    if !st.success() {
        bail!("{cmd} exited with {st}");
    }
    Ok(())
}

fn run_quiet<I, S>(cmd: &str, args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|_| ())
        .with_context(|| format!("spawning {cmd}"))
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn launchctl_quiet(args: &[&OsStr]) -> Result<()> {
    run_quiet("launchctl", args.iter().copied())
}

/// Avoid the two-poller 409 loop when a foreground tebis already holds the lock.
fn refuse_if_foreground_running(verb: &str) -> Result<()> {
    let lock_path = lockfile::default_path();
    if let Some(pid) = lockfile::active_holder(&lock_path) {
        bail!(
            "a foreground tebis is already running (pid {pid}). \
             Stop it first (`kill {pid}`) before `tebis {verb}`."
        );
    }
    Ok(())
}
