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
#[cfg(target_os = "macos")]
use tebis::audio::tts::TtsConfig;
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

    #[cfg(target_os = "macos")]
    let tts_cfg = Some(TtsConfig {
        voice: "Samantha".to_string(),
        respond_to_all: false,
    });
    #[cfg(not(target_os = "macos"))]
    let tts_cfg = None;

    // Honor the manifest's own default so the smoke test tracks the
    // current recommended model without code changes when the default
    // shifts.
    let default_model = tebis::audio::manifest::get()
        .default_stt_model()
        .context("manifest has no default STT model")?
        .to_string();
    let cfg = AudioConfig {
        stt: Some(SttConfig {
            model: default_model,
            language: "en".to_string(),
            max_duration_sec: 120,
            max_bytes: 20 * 1024 * 1024,
            threads: 4,
        }),
        tts: tts_cfg,
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

    #[cfg(target_os = "macos")]
    {
        println!();
        println!("Synthesizing 'hello from tebis' via macOS `say`…");
        let t3 = Instant::now();
        let oga = audio
            .synthesize("hello from tebis")
            .await
            .context("synthesize failed")?;
        let out_path = std::env::temp_dir().join("tebis-smoke.oga");
        std::fs::write(&out_path, &oga).context("write OGG")?;
        println!(
            "  ✓ Done in {:.2}s · {} bytes written to {}",
            t3.elapsed().as_secs_f64(),
            oga.len(),
            out_path.display()
        );
        println!("  (open with QuickTime / afplay to hear it)");
    }

    println!();
    println!("✓ Smoke test passed.");
    Ok(())
}
