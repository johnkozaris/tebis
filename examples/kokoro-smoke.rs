//! End-to-end smoke test for the Kokoro local TTS backend.
//!
//! Build with `--features kokoro` — the example is a no-op without it.
//!
//! Steps: probe espeak-ng + libonnxruntime; build AudioSubsystem with
//! `TELEGRAM_TTS_BACKEND=kokoro-local` and the manifest's default voice; synthesize
//! three increasingly-hard inputs (simple, numeric, mixed) that
//! exercise the normalize → espeak → E2M pipeline; write each output
//! to `/tmp/tebis-kokoro-smoke-<idx>.oga` for aural verification.
//!
//! A pass = the program exits 0 AND the output OGA files are non-empty
//! AND their duration is roughly proportional to input length. The
//! interesting signal is actually hearing the output — open the files
//! with QuickTime / `afplay` / `mpv` and compare prosody to a bare
//! espeak-ng baseline if you have one.
//!
//! Run:
//! ```bash
//! # macOS
//! brew install espeak-ng onnxruntime
//! cargo run --release --features kokoro --example kokoro-smoke
//!
//! # Linux (Debian/Ubuntu)
//! sudo apt install espeak-ng libonnxruntime-dev
//! cargo run --release --features kokoro --example kokoro-smoke
//!
//! # Play one of the outputs
//! afplay /tmp/tebis-kokoro-smoke-3.oga         # macOS
//! mpv    /tmp/tebis-kokoro-smoke-3.oga         # Linux
//! ```

#[cfg(not(feature = "kokoro"))]
fn main() {
    eprintln!(
        "This smoke test requires the `kokoro` cargo feature. Rebuild with:"
    );
    eprintln!();
    eprintln!("  cargo run --release --features kokoro --example kokoro-smoke");
    std::process::exit(2);
}

#[cfg(feature = "kokoro")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use std::time::Instant;

    use anyhow::Context;

    use tebis::audio::tts::{BackendConfig, TtsConfig};
    use tebis::audio::{AudioConfig, AudioSubsystem};
    use tokio_util::sync::CancellationToken;
    use tokio_util::task::TaskTracker;

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(
            |_| {
                tracing_subscriber::EnvFilter::new(
                    "info,hyper=warn,hyper_util=warn,rustls=warn,tebis=debug",
                )
            },
        ))
        .with_target(false)
        .init();

    tebis::telegram::install_crypto_provider();

    // ---- 1. Prerequisite probes ----
    println!();
    println!("Probing prerequisites…");
    match tebis::setup::phonemizer::probe_espeak_ng() {
        Some(info) => println!("  ✓ espeak-ng at {}", info.path.display()),
        None => {
            anyhow::bail!(
                "espeak-ng not found on PATH. Install it:\n  \
                 macOS: brew install espeak-ng\n  \
                 Linux: apt install espeak-ng"
            );
        }
    }

    // ---- 2. Build AudioSubsystem with Kokoro backend ----
    let manifest = tebis::audio::manifest::get();
    let default_tts_model = manifest
        .default_tts_model()
        .context("manifest has no default TTS model")?
        .to_string();
    // First CLI arg overrides the voice — lets you run the smoke test
    // against any voice in the manifest without editing code.
    // Example: `cargo run --release --features kokoro --example kokoro-smoke -- am_adam`
    let default_voice = std::env::args()
        .nth(1)
        .unwrap_or_else(|| manifest.tts_model(&default_tts_model).ok()
            .map(|m| m.default_voice.clone())
            .unwrap_or_else(|| "af_sarah".to_string()));

    let cfg = AudioConfig {
        stt: None,
        tts: Some(TtsConfig {
            backend: BackendConfig::KokoroLocal {
                model: default_tts_model.clone(),
                voice: default_voice.clone(),
            },
            respond_to_all: false,
        }),
    };
    let tracker = TaskTracker::new();
    let shutdown = CancellationToken::new();

    println!();
    println!(
        "Loading Kokoro backend (model={default_tts_model}, voice={default_voice}).",
    );
    println!("  First run downloads ~346 MB model + 510 KB voice file.");
    let t0 = Instant::now();
    let audio = AudioSubsystem::new(&cfg, &tracker, shutdown)
        .await
        .context("AudioSubsystem::new failed")?;
    println!("  ✓ Ready in {:.2}s.", t0.elapsed().as_secs_f64());

    if audio.tts_backend_kind() != "kokoro-local" {
        anyhow::bail!(
            "TTS backend is `{}`, expected `kokoro-local`. \
             Is libonnxruntime installed? \
             (macOS: brew install onnxruntime; Linux: apt install libonnxruntime-dev)",
            audio.tts_backend_kind()
        );
    }
    println!("  ✓ Backend kind: {}", audio.tts_backend_kind());

    // ---- 3. Synthesize increasingly-hard inputs ----
    let inputs = [
        ("simple", "Hello from tebis."),
        (
            "numeric",
            "The 2024 report shows 42.5% improvement on the 3rd test.",
        ),
        (
            "mixed",
            "Dr. Smith's invoice for $3.50 was paid on the 1st of June.",
        ),
    ];

    for (idx, (label, text)) in inputs.iter().enumerate() {
        let idx = idx + 1;
        println!();
        println!("[{idx}] {label}: {text:?}");
        let t = Instant::now();
        let (oga, duration_sec) = audio.synthesize(text).await.context("synthesize")?;
        let elapsed = t.elapsed();
        let out_path = std::env::temp_dir().join(format!("tebis-kokoro-smoke-{idx}.oga"));
        std::fs::write(&out_path, &oga).context("write OGA")?;
        println!(
            "    ✓ {elapsed:.2?} · {kb} KB · {duration_sec} s · {path}",
            kb = oga.len() / 1024,
            path = out_path.display(),
        );
        if duration_sec == 0 {
            anyhow::bail!("synthesis for {label} returned zero-second audio");
        }
    }

    println!();
    println!("✓ Smoke test passed. Open the OGA files to listen:");
    println!("    afplay /tmp/tebis-kokoro-smoke-3.oga   (macOS)");
    println!("    mpv    /tmp/tebis-kokoro-smoke-3.oga   (Linux)");
    println!();
    Ok(())
}
