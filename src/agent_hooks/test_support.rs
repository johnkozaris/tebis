//! Shared test helpers. Exists only at `cfg(test)`; production code
//! never sees it.
//!
//! The `agent_hooks` tests mutate `XDG_DATA_HOME` + `HOME` to redirect
//! `data_dir()` away from the developer's real home. Rust runs `#[test]`
//! functions in parallel, so two tests racing on env vars would leak
//! one's scratch dir into the other's view. We serialize all
//! env-touching tests through a process-wide `Mutex`.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use super::{AgentKind, HookManager};

/// Exclusive guard held for the duration of an env-mutating test body.
/// Acquire via [`with_scratch_data_home`].
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Run `f` with `XDG_DATA_HOME` pointing at a fresh scratch dir and
/// `HOME` pointing somewhere innocuous (so `data_dir` never escapes into
/// the developer's actual `~/.local/share/tebis`). Restores both on
/// return, including panics.
pub fn with_scratch_data_home<R>(tag: &str, f: impl FnOnce() -> R) -> R {
    let _guard = env_lock();
    let scratch = std::env::temp_dir().join(format!(
        "tebis-scratch-{tag}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).expect("scratch mkdir");

    let prior_xdg = std::env::var_os("XDG_DATA_HOME");
    let prior_home = std::env::var_os("HOME");

    // SAFETY: We hold `env_lock`; no other thread in this process will
    // observe the intermediate state. We restore on every exit path.
    unsafe {
        std::env::set_var("XDG_DATA_HOME", &scratch);
        // Neutralise HOME too, so the fallback branch of `data_dir`
        // can't ever point at the real filesystem.
        std::env::set_var("HOME", &scratch);
    }

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

    // SAFETY: see above.
    unsafe {
        match prior_xdg {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        match prior_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);

    match result {
        Ok(r) => r,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// Temporary directory to write project-side fixtures into. Cleaned up
/// after `f` returns (or panics).
pub fn with_scratch_project<R>(tag: &str, f: impl FnOnce(&Path) -> R) -> R {
    let dir = std::env::temp_dir().join(format!(
        "tebis-proj-{tag}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("proj mkdir");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&dir)));
    let _ = std::fs::remove_dir_all(&dir);
    match result {
        Ok(r) => r,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// Full harness: fresh `XDG_DATA_HOME`, fresh project dir, a
/// `HookManager` for `kind`, and the materialized script path.
pub fn with_hook_fixtures<R>(
    tag: &str,
    kind: AgentKind,
    f: impl FnOnce(&dyn HookManager, &Path, &PathBuf) -> R,
) -> R {
    with_scratch_data_home(tag, || {
        with_scratch_project(tag, |proj| {
            let script = super::materialize(kind).expect("materialize");
            let mgr = super::for_kind(kind);
            f(&*mgr, proj, &script)
        })
    })
}
