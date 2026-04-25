//! Kokoro local TTS smoke. Writes `/tmp/tebis-kokoro-smoke-{1,2,3}.oga` for
//! aural check. Needs espeak-ng + onnxruntime + `--features kokoro-local`.
//! First CLI arg overrides the voice, e.g. `… -- am_adam`.

#[cfg(not(feature = "kokoro-local"))]
fn main() {
    eprintln!("This smoke test requires the `kokoro-local` cargo feature. Rebuild with:");
    eprintln!();
    eprintln!("  cargo run --release --features kokoro-local --example kokoro-smoke");
    std::process::exit(2);
}

#[cfg(feature = "kokoro-local")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use std::time::Instant;

    use anyhow::Context;

    use tebis::audio::tts::{BackendConfig, TtsConfig};
    use tebis::audio::{AudioConfig, AudioSubsystem};
    use tokio_util::sync::CancellationToken;
    use tokio_util::task::TaskTracker;

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

    let manifest = tebis::audio::manifest::get();
    let default_tts_model = manifest
        .default_tts_model()
        .context("manifest has no default TTS model")?
        .to_string();
    let default_voice = std::env::args().nth(1).unwrap_or_else(|| {
        manifest
            .tts_model(&default_tts_model)
            .ok()
            .map(|m| m.default_voice.clone())
            .unwrap_or_else(|| "af_sarah".to_string())
    });

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
    println!("Loading Kokoro backend (model={default_tts_model}, voice={default_voice}).",);
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
