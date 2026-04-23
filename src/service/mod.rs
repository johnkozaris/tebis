//! `tebis service {install,uninstall,start,stop,restart,status}` —
//! per-OS system service integration.
//!
//! # Backends
//!
//! - **Unix** (`unix`): launchd on macOS, systemd user service on
//!   Linux. See `src/service/unix.rs`.
//! - **Windows** (`windows`): Task Scheduler at-logon via
//!   `schtasks.exe /Create /SC ONLOGON /RL LIMITED`. Runs in the
//!   logged-in user's session so the notify SID gate, `%APPDATA%`
//!   env file, and Claude Code autostart all see the right
//!   principal (an SCM service running as LocalSystem wouldn't).
//!   True SCM integration with explicit user credentials is a future
//!   follow-up if anyone needs proper service-lifecycle events;
//!   Task Scheduler is the sane v1.
//!
//! All backends share the same public surface: `install`, `uninstall`,
//! `start`, `stop`, `restart`, `status`, `is_running`.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{install, is_running, restart, start, status, stop, uninstall};
#[cfg(windows)]
pub use windows::{install, is_running, restart, start, status, stop, uninstall};
