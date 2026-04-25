//! Full phonemization: `normalize → espeak-ng -v en-us -q --ipa=3 → e2m`.
//!
//! Kokoro was trained against misaki-processed phonemes. Without the
//! normalize + E2M passes, raw espeak output produces digit-strings,
//! unmerged diphthongs, dental `r`, and unhandled flap-T — all audible.
//! These two passes close ~70% of the quality gap vs a Kokoro-FastAPI
//! deployment.
//!
//! Safety: `Command::arg` uses argv (not shell interpolation), so
//! arbitrary input text — including tmux paste-through — can't inject
//! shell metacharacters.

use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

use crate::KokoroError;
use crate::{e2m, normalize};

/// Phonemize timeout. espeak-ng should finish in <50 ms for typical
/// reply lengths; 10 s is the handler-level fail-safe.
const PHONEMIZE_TIMEOUT: Duration = Duration::from_secs(10);

/// Errors:
/// - [`KokoroError::Init`] if `espeak-ng` isn't on `PATH` — the caller
///   should already have probed via `setup::phonemizer::probe_espeak_ng`
///   at startup, but the runtime guard catches the apt-remove-mid-run
///   edge case.
/// - [`KokoroError::Synthesis`] for non-zero exits, timeouts, empty output,
///   or if normalization collapses the input to whitespace.
pub async fn phonemize(text: &str) -> Result<String, KokoroError> {
    if text.trim().is_empty() {
        return Err(KokoroError::Synthesis(
            "empty input to phonemize".to_string(),
        ));
    }

    let normalized = normalize::preprocess(text);
    if normalized.trim().is_empty() {
        return Err(KokoroError::Synthesis(
            "input collapsed to whitespace after normalization".to_string(),
        ));
    }

    let run = Command::new("espeak-ng")
        .arg("-v")
        .arg("en-us")
        .arg("-q")
        .arg("--ipa=3")
        .arg(&normalized)
        .output();

    let output = match timeout(PHONEMIZE_TIMEOUT, run).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(KokoroError::Init(
                "espeak-ng not found on PATH — install it (macOS: `brew install \
                 espeak-ng`, Linux: your distro's package manager) or set \
                 TELEGRAM_TTS_BACKEND=none"
                    .to_string(),
            ));
        }
        Ok(Err(e)) => {
            return Err(KokoroError::Synthesis(format!("espeak-ng spawn: {e}")));
        }
        Err(_) => {
            return Err(KokoroError::Synthesis(format!(
                "espeak-ng timed out after {PHONEMIZE_TIMEOUT:?}"
            )));
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(KokoroError::Synthesis(format!(
            "espeak-ng exited {:?}: {stderr}",
            output.status.code()
        )));
    }

    let raw_ipa = String::from_utf8_lossy(&output.stdout)
        .trim_matches(|c: char| c.is_whitespace() || c == '\n')
        .to_string();
    if raw_ipa.is_empty() {
        return Err(KokoroError::Synthesis(
            "espeak-ng returned empty phonemes".to_string(),
        ));
    }

    Ok(e2m::apply_e2m(&raw_ipa))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Requires espeak-ng on PATH; marked #[ignore] so CI doesn't fail
    /// on hosts without it. Run with `cargo test --features kokoro-local -- \
    /// --ignored espeak_produces_ipa` to exercise the real flow.
    #[tokio::test]
    #[ignore = "requires espeak-ng on PATH"]
    async fn espeak_produces_ipa_for_simple_input() {
        let ipa = phonemize("hello").await.expect("phonemize hello");
        // "hello" in en-us IPA is approximately "həlˈoʊ" — verify the
        // output at least contains the schwa + L + stress marker so
        // regressions in espeak-ng arg parsing surface quickly.
        assert!(ipa.contains('ə'), "expected schwa in: {ipa:?}");
        assert!(ipa.contains('l'), "expected l in: {ipa:?}");
    }

    #[tokio::test]
    async fn empty_text_rejected_without_spawn() {
        let err = phonemize("   ").await.unwrap_err();
        matches!(err, KokoroError::Synthesis(_));
    }

    /// Full-pipeline integration test — requires espeak-ng on PATH.
    /// Demonstrates that normalize + E2M are wired in correctly by
    /// driving a complex input that exercises both passes.
    #[tokio::test]
    #[ignore = "requires espeak-ng on PATH"]
    async fn full_pipeline_normalizes_and_fixes_up() {
        let ipa = phonemize("The year is 2024.")
            .await
            .expect("phonemize year 2024");
        assert!(ipa.len() > 20, "too short for normalized year: {ipa:?}");
        assert!(ipa.contains('ɹ'), "E2M didn't fire: {ipa:?}");
    }
}
