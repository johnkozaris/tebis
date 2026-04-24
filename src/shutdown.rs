//! Graceful-restart helper shared by the bridge HTTP command path and the
//! inspect dashboard config writer.

use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Cancel the root shutdown token after a short delay so the current reply
/// or HTTP redirect has time to flush before the process exits; the service
/// manager (launchd / systemd) respawns per its keep-alive policy.
///
/// Bare `tokio::spawn` is intentional: this task *triggers* shutdown, so
/// invariant 12 (drain in-flight work via `TaskTracker`) does not apply —
/// draining the trigger would deadlock shutdown.
pub fn schedule_graceful_restart(shutdown: &CancellationToken) {
    let shutdown = shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        shutdown.cancel();
    });
}
