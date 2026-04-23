//! Per-platform primitives. Each submodule owns one OS-level concern
//! (signal handling, secure file writes, IPC listener, multiplexer, …)
//! and exposes a single cross-platform API; the Unix and Windows
//! backends live side-by-side inside the submodule so callers never
//! need `#[cfg]` inline.
//!
//! Adding a new primitive:
//! 1. Create `src/platform/<name>.rs` (or `src/platform/<name>/mod.rs`
//!    if the backends are large enough to justify separate files).
//! 2. Inside, put `#[cfg(unix)] mod unix;` + `#[cfg(windows)] mod windows;`
//!    modules with a shared function/trait shape.
//! 3. Re-export the backend-specific items at the primitive's module
//!    root via `#[cfg(unix)] pub use unix::*;` / windows equivalent.
//! 4. Callers only reach in via `crate::platform::<name>::…`.

pub mod hostname;
pub mod process;
pub mod signal;
