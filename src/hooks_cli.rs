//! `tebis hooks {install,uninstall,status,list,prune}` — manual hook lifecycle.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use console::style;

use crate::agent_hooks;
use crate::agent_hooks::AgentKind;
use crate::env_file;
use crate::setup;

const USAGE: &str = "\
Usage:
  tebis hooks install   [<dir>] [--agent claude|copilot]
  tebis hooks uninstall [<dir>]
  tebis hooks status    [<dir>]
  tebis hooks list
  tebis hooks prune

<dir> defaults to the autostart working directory from ~/.config/tebis/env.
--agent is required for `install` when not detectable from autostart.
`list` shows every project dir where tebis has installed hooks.
`prune` drops manifest entries whose project dir has been deleted.
";

pub fn run(args: &[String]) -> Result<()> {
    let verb = args.first().map(String::as_str);
    match verb {
        Some("install") => install(&args[1..]),
        Some("uninstall") => uninstall(&args[1..]),
        Some("status") => status(&args[1..]),
        Some("list") => {
            list();
            Ok(())
        }
        Some("prune") => prune(),
        _ => {
            eprint!("{USAGE}");
            bail!("expected `install`, `uninstall`, `status`, `list`, or `prune`");
        }
    }
}

fn install(args: &[String]) -> Result<()> {
    let parsed = parse_args(args)?;
    let dir = resolve_dir(parsed.dir.as_deref())?;

    // Fail loud on typos so we don't write hooks into a phantom dir.
    if !dir.is_dir() {
        anyhow::bail!(
            "directory does not exist: {} — pass a valid project path or \
             `cd` into the project and re-run without args",
            dir.display()
        );
    }

    let agent = match parsed.agent {
        Some(a) => a,
        None => detect_agent_from_config()?.with_context(|| {
            "cannot detect agent — pass --agent claude|copilot, \
             or set TELEGRAM_AUTOSTART_COMMAND to a supported agent"
        })?,
    };

    warn_if_hook_deps_missing();
    warn_if_legacy_hooks_present(&dir);

    let script = agent_hooks::materialize(agent)?;
    let mgr = agent_hooks::for_kind(agent);
    let report = mgr.install(&dir, &script)?;

    println!();
    println!(
        "{} Installed {} hooks in {}",
        style("✓").green().bold(),
        style(agent.display()).bold(),
        style(dir.display()).bold(),
    );
    println!("    events: {}", style(report.events.join(", ")).dim());
    for f in &report.files_written {
        println!(
            "    wrote:  {}  {}",
            style(f.display()).dim(),
            style("(lowest-precedence; normally gitignored)")
                .italic()
                .dim(),
        );
    }
    println!(
        "    script: {}  {}",
        style(script.display()).dim(),
        style("(tebis owns this; `tebis hooks uninstall` to remove)")
            .italic()
            .dim(),
    );
    println!();
    Ok(())
}

fn uninstall(args: &[String]) -> Result<()> {
    let parsed = parse_args(args)?;
    let dir = resolve_dir(parsed.dir.as_deref())?;

    if !dir.is_dir() {
        anyhow::bail!(
            "directory does not exist: {} — pass a valid project path or \
             `cd` into the project and re-run without args",
            dir.display()
        );
    }

    // Only run per-agent uninstallers when something is actually installed —
    // avoids pruning empty user dirs like `.github/`.
    let mut total_modified = Vec::new();
    let mut total_deleted = Vec::new();
    let mut total_events = Vec::new();
    for agent in [AgentKind::Claude, AgentKind::Copilot] {
        let mgr = agent_hooks::for_kind(agent);
        let status = mgr
            .status(&dir)
            .with_context(|| format!("status for {}", agent.display()))?;
        if status.installed_events.is_empty() {
            continue;
        }
        let r = mgr
            .uninstall(&dir)
            .with_context(|| format!("uninstalling {} hooks", agent.display()))?;
        total_modified.extend(r.files_modified);
        total_deleted.extend(r.files_deleted);
        total_events.extend(r.events_removed);
    }

    if total_modified.is_empty() && total_deleted.is_empty() && total_events.is_empty() {
        println!(
            "\n{} No tebis hooks found in {}\n",
            style("·").dim(),
            style(dir.display()).dim()
        );
        return Ok(());
    }

    println!();
    println!(
        "{} Removed tebis hooks from {}",
        style("✓").green().bold(),
        style(dir.display()).bold(),
    );
    for f in &total_modified {
        println!("    modified: {}", style(f.display()).dim());
    }
    for f in &total_deleted {
        println!("    deleted:  {}", style(f.display()).dim());
    }
    println!();
    Ok(())
}

fn list() {
    let entries = agent_hooks::manifest::load_entries();
    println!();
    if entries.is_empty() {
        println!("  {}", style("no hooks installed").dim());
        println!(
            "  {}",
            style("(run `tebis hooks install <dir>` to add some)").dim()
        );
        println!();
        return;
    }
    println!("  {}", style("Installed hooks").bold());
    let mut any_missing = false;
    for e in entries {
        let missing = !e.dir.exists();
        let dir_style = if missing {
            style(e.dir.display()).red().dim()
        } else {
            style(e.dir.display()).bold()
        };
        let suffix = if missing {
            any_missing = true;
            style(" (missing)").red().dim().to_string()
        } else {
            String::new()
        };
        println!(
            "    {:<8} {}{}  {}",
            style(&e.agent).green(),
            dir_style,
            suffix,
            style(format!("installed {}", e.installed_at)).dim(),
        );
    }
    if any_missing {
        println!(
            "\n  {} {}",
            style("›").dim(),
            style("run `tebis hooks prune` to drop entries for deleted dirs").dim(),
        );
    }
    println!();
}

fn prune() -> Result<()> {
    let removed = agent_hooks::manifest::prune_missing_dirs()
        .context("pruning manifest of missing project dirs")?;
    println!();
    if removed.is_empty() {
        println!(
            "  {}",
            style("nothing to prune — every manifest entry exists").dim()
        );
    } else {
        println!(
            "  {} Pruned {} dangling {}:",
            style("✓").green().bold(),
            removed.len(),
            if removed.len() == 1 {
                "entry"
            } else {
                "entries"
            },
        );
        for e in &removed {
            println!(
                "    {:<8} {}",
                style(&e.agent).green(),
                style(e.dir.display()).dim()
            );
        }
    }
    println!();
    Ok(())
}

fn status(args: &[String]) -> Result<()> {
    let parsed = parse_args(args)?;
    let dir = resolve_dir(parsed.dir.as_deref())?;

    println!();
    println!("  {} {}", style("dir:").dim(), style(dir.display()).bold());
    for agent in [AgentKind::Claude, AgentKind::Copilot] {
        let mgr = agent_hooks::for_kind(agent);
        let r = mgr
            .status(&dir)
            .with_context(|| format!("status for {}", agent.display()))?;
        let summary = if r.installed_events.is_empty() {
            style("not installed").dim().to_string()
        } else {
            style(r.installed_events.join(", ")).green().to_string()
        };
        println!("  {:<16} {}", format!("{}:", agent.display()), summary);
    }
    println!();
    Ok(())
}

struct Args {
    dir: Option<PathBuf>,
    agent: Option<AgentKind>,
}

fn parse_args(args: &[String]) -> Result<Args> {
    let mut dir = None;
    let mut agent = None;
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--agent" => {
                let v = iter
                    .next()
                    .context("--agent requires a value (claude|copilot)")?;
                agent = Some(match v.as_str() {
                    "claude" | "claude-code" => AgentKind::Claude,
                    "copilot" | "copilot-cli" => AgentKind::Copilot,
                    other => bail!("unknown --agent value {other:?} (expected claude|copilot)"),
                });
            }
            other if other.starts_with('-') => bail!("unknown flag: {other}"),
            other => {
                if dir.is_some() {
                    bail!("unexpected extra argument: {other}");
                }
                dir = Some(PathBuf::from(other));
            }
        }
    }
    Ok(Args { dir, agent })
}

fn resolve_dir(cli: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = cli {
        return expand_tilde(p);
    }
    let env_path = setup::env_file_path()?;
    let dir = read_autostart_dir(&env_path)?.with_context(|| {
        format!(
            "no dir argument and no TELEGRAM_AUTOSTART_DIR in {}",
            env_path.display()
        )
    })?;
    expand_tilde(Path::new(&dir))
}

fn expand_tilde(p: &Path) -> Result<PathBuf> {
    let s = p.to_string_lossy();
    if s == "~" {
        return std::env::var("HOME")
            .map(PathBuf::from)
            .context("$HOME unset");
    }
    if let Some(rest) = s.strip_prefix("~/") {
        let home = std::env::var("HOME").context("$HOME unset")?;
        return Ok(PathBuf::from(home).join(rest));
    }
    Ok(p.to_path_buf())
}

fn read_autostart_dir(env_path: &Path) -> Result<Option<String>> {
    Ok(env_file::read_key(env_path, "TELEGRAM_AUTOSTART_DIR")?)
}

fn detect_agent_from_config() -> Result<Option<AgentKind>> {
    let env_path = setup::env_file_path()?;
    Ok(env_file::read_key(&env_path, "TELEGRAM_AUTOSTART_COMMAND")?
        .as_deref()
        .and_then(AgentKind::detect))
}

/// Warn when `jq`/`nc` aren't on PATH — hook scripts silently no-op without them.
fn warn_if_hook_deps_missing() {
    for tool in ["jq", "nc"] {
        if !has_on_path(tool) {
            eprintln!(
                "  {}  `{tool}` not on PATH — hook scripts need it, \
                 and will silently do nothing when missing.",
                style("⚠").yellow().bold(),
            );
            eprintln!(
                "     install: {}",
                style(match tool {
                    "jq" => "`brew install jq` (macOS) / `apt install jq` (Debian)",
                    "nc" => "`brew install netcat` / `apt install netcat-openbsd`",
                    _ => "see your OS package manager",
                })
                .dim(),
            );
        }
    }
}

fn has_on_path(tool: &str) -> bool {
    let Ok(path_var) = std::env::var("PATH") else {
        return false;
    };
    for dir in path_var.split(':') {
        if dir.is_empty() {
            continue;
        }
        let candidate = Path::new(dir).join(tool);
        if let Ok(meta) = std::fs::metadata(&candidate)
            && meta.is_file()
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o111 != 0 {
                return true;
            }
        }
    }
    false
}

/// Warn about pre-Phase-2 hooks (repo-checkout paths) so the user can
/// remove them before install adds a duplicate entry.
fn warn_if_legacy_hooks_present(project_dir: &Path) {
    let lines = agent_hooks::legacy::scan_claude(project_dir);
    if lines.is_empty() {
        return;
    }
    let settings = project_dir.join(".claude/settings.local.json");
    eprintln!();
    eprintln!(
        "  {}  legacy hook entry found in {}:",
        style("⚠").yellow().bold(),
        style(settings.display()).bold(),
    );
    for line in &lines {
        eprintln!("     {}", style(line).dim());
    }
    eprintln!(
        "     {} Installing will add a second entry — remove the old one first",
        style("→").yellow(),
    );
    eprintln!("     to avoid double-delivery.");
    eprintln!();
}
