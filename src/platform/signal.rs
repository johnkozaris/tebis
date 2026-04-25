//! Cross-platform shutdown signal. Unix awaits SIGINT or SIGTERM;
//! Windows awaits Ctrl+C (SIGTERM has no Windows equivalent — the SCM
//! Stop control code is handled separately by `service::windows` when
//! running as a service).

#[cfg(unix)]
mod unix {
    pub async fn shutdown_signal() {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }
}

#[cfg(windows)]
mod windows {
    pub async fn shutdown_signal() {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(unix)]
pub use unix::shutdown_signal;
#[cfg(windows)]
pub use windows::shutdown_signal;
