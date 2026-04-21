//! End-to-end smoke test for the audio subsystem.
//!
//! Exercises the full STT path without needing a Telegram bot token:
//! 1. Builds an `AudioSubsystem` with STT enabled.
//! 2. Downloads the default model (~148 MB for base.en) if not cached.
//! 3. Synthesizes 1 s of silence as f32 PCM.
//! 4. Runs `transcribe()` against the silence and prints the result.
//!
//! A pass = it returns cleanly (text can be empty or a filler like
//! "(silence)"). The interesting measurements are the wall-clock times
//! — first-run download ~53 s, subsequent start ~300 ms, inference
//! ~100 ms on M4 with Metal.
//!
//! Run: `cargo run --release --example audio-smoke`

use std::time::Instant;

use anyhow::{Context, Result};
use tebis::audio::{AudioConfig, AudioSubsystem, stt::SttConfig};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new(
                    "info,hyper=warn,hyper_util=warn,rustls=warn,tebis=debug",
                )
            }),
        )
        .with_target(false)
        .init();

    tebis::telegram::install_crypto_provider();

    let cfg = AudioConfig {
        stt: Some(SttConfig {
            model: "base.en".to_string(),
            language: "en".to_string(),
            max_duration_sec: 120,
            max_bytes: 20 * 1024 * 1024,
            threads: 4,
        }),
    };
    let tracker = TaskTracker::new();
    let shutdown = CancellationToken::new();

    println!();
    println!("Loading AudioSubsystem (will download ~148 MB on first run)…");
    let t0 = Instant::now();
    let audio = AudioSubsystem::new(&cfg, &tracker, shutdown)
        .await
        .context("AudioSubsystem::new failed")?;
    println!(
        "  ✓ Ready in {:.2}s. Model: {:?}",
        t0.elapsed().as_secs_f64(),
        audio.stt_model_name()
    );

    println!();
    println!("Transcribing 1 s of silence…");
    let silence = vec![0.0_f32; 16_000];
    let t1 = Instant::now();
    let result = audio
        .transcribe(&silence, "en")
        .await
        .context("transcribe failed")?;
    println!(
        "  ✓ Done in {:.2}s (whisper.cpp reports {} ms)",
        t1.elapsed().as_secs_f64(),
        result.duration_ms
    );
    println!("  text: {:?}", result.text);
    println!("  language: {}", result.language);

    println!();
    println!("Running a second inference (warm cache)…");
    let t2 = Instant::now();
    let result2 = audio.transcribe(&silence, "en").await?;
    println!(
        "  ✓ Done in {:.2}s (whisper.cpp {} ms)",
        t2.elapsed().as_secs_f64(),
        result2.duration_ms
    );

    println!();
    println!("✓ Smoke test passed.");
    Ok(())
}
