//! Local IPC listener restricted to connections from the current user.
//!
//! Two backends share the same public surface (`Listener`, `Conn`,
//! `bind`, `accept`, `is_trusted_peer`):
//!
//! **Unix** — Abstract-free filesystem UDS at `<runtime>/tebis.sock`,
//! mode `0600`, umask-bypass during bind, explicit chmod, and a
//! `peer_cred` uid check on every accepted stream. Three-layer defense
//! per invariant 17.
//!
//! **Windows** — Named Pipe at `\\.\pipe\tebis-<user>-notify`, DACL
//! protected (`SE_DACL_PROTECTED`) and granting `GENERIC_ALL` only to
//! the current user's SID, plus `ImpersonateNamedPipeClient` +
//! `GetTokenInformation(TokenUser)` SID-equality check on every
//! accept. Never `GetNamedPipeClientProcessId` — PID is spoofable
//! (Project Zero 2019). Invariant 20.
//!
//! The two `Conn` types are different concrete streams (Unix Tokio
//! `UnixStream` vs Windows `NamedPipeServer`), but both implement
//! `AsyncRead + AsyncWrite`, which is all `notify/listener.rs`
//! touches.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{Conn, Listener};
#[cfg(windows)]
pub use windows::{Conn, Listener};
