//! Windows service backend — Phase-4 stub.
//!
//! Every operation currently returns an "unsupported" error. The real
//! implementation is tracked in PLAN-WINDOWS.md Phase 4 and will use
//! one of:
//!
//! - `windows-service` crate (Mullvad) — true SCM integration with a
//!   `define_windows_service!` entry point and a Stop-control handler
//!   that fires the main `CancellationToken`. Cleaner runtime behavior
//!   (proper service lifecycle, Event Log integration) but the install
//!   ceremony needs the user's password or an elevated token.
//! - Task Scheduler "at logon" — a scheduled task is installed via
//!   `schtasks.exe` that starts `tebis.exe` on every login. Simpler
//!   install UX, no SCM interaction; downside is we don't get proper
//!   service-lifecycle events.
//!
//! The plan recommends Task Scheduler for v1 and SCM for v2; this stub
//! fails loudly so any caller that tries `tebis install` etc. before
//! Phase 4 lands gets a clear "not yet" rather than silent wrong
//! behavior.

use anyhow::{Result, bail};

fn unsupported(op: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "`tebis {op}` is not yet supported on Windows. \
         Phase-4 work in progress — see PLAN-WINDOWS.md. \
         Meanwhile, run `tebis` in the foreground from a terminal."
    )
}

pub fn install() -> Result<()> {
    Err(unsupported("install"))
}

pub fn uninstall(_purge_flag: bool) -> Result<()> {
    Err(unsupported("uninstall"))
}

pub fn start() -> Result<()> {
    Err(unsupported("start"))
}

pub fn stop() -> Result<()> {
    Err(unsupported("stop"))
}

pub fn restart() -> Result<()> {
    Err(unsupported("restart"))
}

pub fn status() -> Result<()> {
    bail!("`tebis status` not yet supported on Windows (Phase-4 work in progress)")
}

/// Whether a background tebis service/task is currently running.
/// Always `false` in the stub — there's nothing to detect yet.
#[must_use]
pub fn is_running() -> bool {
    false
}
