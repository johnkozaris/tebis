//! `tebis service {install,uninstall,start,stop,restart,status}` —
//! per-OS system service integration.
//!
//! # Backends
//!
//! - **Unix** (`unix`): launchd on macOS, systemd user service on Linux.
//!   See `src/service/unix.rs` for the full-fidelity implementation.
//! - **Windows** (`windows`): **Phase-4 stub.** All operations return
//!   a "not yet supported" error so the binary builds and the other
//!   CLI surfaces work on Windows. The real implementation will use
//!   either `windows-service` (true SCM integration) or Task Scheduler
//!   at-logon registration — the plan defers that decision until the
//!   install ergonomics are tested in the wizard.
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
