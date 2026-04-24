//! Graceful-restart helper shared by the bridge HTTP command path and the
//! inspect dashboard config writer.

use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Cancel the root shutdown token after a short delay so the reply flushes
/// before exit. Bare `tokio::spawn` is fine: this is a shutdown trigger, not invariant-12 work.
pub fn schedule_graceful_restart(shutdown: &CancellationToken) {
    let shutdown = shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        shutdown.cancel();
    });
}
