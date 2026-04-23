//! Test-only helpers. Env-mutating tests serialize on a process-wide `Mutex`.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use super::{AgentKind, HookManager};

/// Crate-wide test-only mutex for tests that mutate process-global
/// state (env vars via `with_scratch_data_home`, `libc::umask` in the
/// peer-listener bind tests, etc.). Must be held across the whole
/// side-effect window so parallel tests don't observe the mutation.
pub fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Point tebis's config + data dirs at a fresh scratch tree for `f`,
/// restore afterwards. Uses `TEBIS_SCRATCH_DIR` (honored by
/// `platform::paths` only in test builds) so the override works
/// uniformly on Unix and Windows — Windows's Known Folder API
/// wouldn't pick up an `XDG_*` override.
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

    let prior = std::env::var_os("TEBIS_SCRATCH_DIR");

    // SAFETY: We hold `env_lock`; no other thread in this process will
    // observe the intermediate state. We restore on every exit path.
    unsafe {
        std::env::set_var("TEBIS_SCRATCH_DIR", &scratch);
    }

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

    // SAFETY: see above.
    unsafe {
        match prior {
            Some(v) => std::env::set_var("TEBIS_SCRATCH_DIR", v),
            None => std::env::remove_var("TEBIS_SCRATCH_DIR"),
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);

    match result {
        Ok(r) => r,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// Temporary project dir, cleaned up on return or panic.
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

/// Fresh scratch env + project + `HookManager` + materialized script.
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
