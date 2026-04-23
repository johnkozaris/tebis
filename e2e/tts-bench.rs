//! `say` + Opus + OGG latency microbench. `cargo run --release --example tts-bench`

use std::time::Instant;

use anyhow::Result;
use tebis::audio::{AudioConfig, AudioSubsystem, tts::{BackendConfig, TtsConfig}};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

#[tokio::main]
async fn main() -> Result<()> {
    tebis::telegram::install_crypto_provider();

    let cfg = AudioConfig {
        stt: None,
        tts: Some(TtsConfig {
            backend: BackendConfig::Say {
                voice: "Samantha".to_string(),
            },
            respond_to_all: false,
        }),
    };
    let tracker = TaskTracker::new();
    let shutdown = CancellationToken::new();
    let audio = AudioSubsystem::new(&cfg, &tracker, shutdown).await?;

    let short = "Hello from tebis.";
    let medium = "I've updated the config and restarted the service. \
                  The dashboard should show the new voice settings on refresh.";
    let long = concat!(
        "I pulled main, rebased your branch, and ran the full test suite. ",
        "There were three failing tests in the autoreply module — looks like ",
        "the recent `capture-pane` refactor changed the normalization path in ",
        "a way that broke our baseline hash. I pushed a fix to `bridge::autoreply` ",
        "that re-normalizes before hashing, and all 227 tests pass now. ",
        "I also noticed the CI config still references the old test name for ",
        "`decode_round_trip_silence` — renamed it to match the new `encode_decode_round_trip_silence` ",
        "in the codec module. That's pushed too. Your PR description should ",
        "probably mention the CI rename since someone merging without rebasing ",
        "first might hit a stale test name. Let me know if you want me to open ",
        "a follow-up PR for the CI config, or if you'd rather roll it into ",
        "this branch. Otherwise I think we're green to merge once you've ",
        "had a chance to look at the autoreply diff — it's three lines, ",
        "self-contained, with a regression test included. I can also land ",
        "the CHANGELOG entry if you want, but I wasn't sure which version ",
        "line to add it under. Last thing: the release binary size went up ",
        "from 4.25 MB to 4.98 MB, all whisper-rs + Core ML + libopus. Let me know."
    );

    for (label, text) in [
        ("short (17 chars)", short),
        ("medium (~130 chars)", medium),
        ("long (~1500 chars)", long),
    ] {
        let char_count = text.chars().count();
        let t0 = Instant::now();
        let (oga, duration_sec) = audio.synthesize(text).await?;
        let elapsed = t0.elapsed();
        #[allow(clippy::cast_precision_loss)]
        let us_per_char = (elapsed.as_micros() as f64) / (char_count as f64);
        println!(
            "{label}: {elapsed:.2?} · {oga_kb} KB · {duration_sec} s · {us_per_char:.1} µs/char",
            elapsed = elapsed,
            oga_kb = oga.len() / 1024,
        );
    }

    Ok(())
}
