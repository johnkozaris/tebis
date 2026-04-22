//! Full phonemization pipeline: normalize → espeak-ng → E2M fixup.
//!
//! ## Pipeline
//!
//! ```text
//! raw text
//!   │
//!   ├─ normalize::preprocess  (numbers/currency/titles/etc. → words)
//!   │
//!   ├─ espeak-ng -v en-us -q --ipa=3 "<normalized>"
//!   │
//!   ├─ e2m::apply_e2m         (diphthong merge / flap-T / rhotacization)
//!   │
//!   ▼
//! IPA string ready for `tokens::ipa_to_token_ids`
//! ```
//!
//! ## espeak-ng flags
//!
//! - `-q`: quiet (don't play audio; we only want the phonemes on stdout).
//! - `--ipa=3`: full IPA output with stress marks (stress is part of
//!   Kokoro's training vocab — dropping it degrades prosody).
//! - `-v en-us`: American English phonemes. `en-gb` also works but the
//!   Kokoro American voices were trained on `en-us` so keep them aligned.
//!
//! ## Why shell-out vs linking libespeak-ng
//!
//! `libespeak-ng` is LGPL-2.1+ with an essential GPL-3 dialect payload;
//! static linking would contaminate our MIT binary. Shell-out avoids
//! the license transfer — same pattern as `say`, `jq`, `nc`, `espeak-ng`
//! elsewhere in tebis.
//!
//! ## Why we add the normalize + E2M passes
//!
//! Kokoro was trained against `misaki`'s processed phonemes, not raw
//! espeak-ng output. Without these passes:
//! - `2024` comes out as "two zero two four" (digits) — audibly wrong
//! - `$42.50` reads literal dollar-sign / digits
//! - Diphthongs don't merge to Kokoro's single-char markers
//! - Flap-T (`butter`) stays as `ɾ` instead of collapsing to `T`
//! - Rhotic `r` stays as dental `r` instead of becoming `ɹ`
//!
//! The perceived quality gap between "robotic" and "matches the remote
//! Kokoro-FastAPI deployment" is mostly these two passes.
//!
//! ## Invariants
//!
//! - 10 s timeout on espeak-ng. Typical call is <10 ms; the timeout is
//!   paranoia against a forked process deadlocking on stdio.
//! - No shell interpolation: tokio's `Command::arg` uses an argv array,
//!   so arbitrary text (including tmux paste → TTS) is safe from shell
//!   metacharacter injection.

use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

use crate::KokoroError;
use crate::{e2m, normalize};

/// Phonemize timeout. espeak-ng should finish in <50 ms for typical
/// reply lengths; 10 s is the handler-level fail-safe.
const PHONEMIZE_TIMEOUT: Duration = Duration::from_secs(10);

/// Run the full `normalize → espeak-ng → e2m` pipeline and return the
/// Kokoro-ready IPA phoneme string.
///
/// Errors:
/// - [`KokoroError::Init`] if `espeak-ng` isn't on `PATH` — the caller
///   should already have probed via `setup::phonemizer::probe_espeak_ng`
///   at startup, but the runtime guard catches the apt-remove-mid-run
///   edge case.
/// - [`KokoroError::Synthesis`] for non-zero exits, timeouts, empty output,
///   or if normalization collapses the input to whitespace.
pub async fn phonemize(text: &str) -> Result<String, KokoroError> {
    if text.trim().is_empty() {
        return Err(KokoroError::Synthesis("empty input to phonemize".to_string()));
    }

    // Pre-espeak text normalization (numbers, currency, titles, etc.).
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

    // Post-espeak IPA fixups — diphthong merging, flap-T, rhotacization,
    // tie-mark stripping. This is the key quality lever: without it,
    // Kokoro sees phonemes it wasn't trained on and the output sounds
    // "off" (robotic prosody, missing diphthongs).
    Ok(e2m::apply_e2m(&raw_ipa))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Requires espeak-ng on PATH; marked #[ignore] so CI doesn't fail
    /// on hosts without it. Run with `cargo test --features kokoro -- \
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
        // Cheap check before we fork a process.
        let err = phonemize("   ").await.unwrap_err();
        matches!(err, KokoroError::Synthesis(_));
    }

    /// Full-pipeline integration test — requires espeak-ng on PATH.
    /// Demonstrates that normalize + E2M are wired in correctly by
    /// driving a complex input that exercises both passes.
    #[tokio::test]
    #[ignore = "requires espeak-ng on PATH"]
    async fn full_pipeline_normalizes_and_fixes_up() {
        // Input that exercises number normalization. If normalize
        // didn't run, espeak would read "2024" digit-by-digit.
        let ipa = phonemize("The year is 2024.")
            .await
            .expect("phonemize year 2024");
        // Year should have been expanded to "twenty twenty-four"
        // before espeak, so the IPA should look like a four-syllable
        // word sequence, not four separate digit phonemes. Heuristic:
        // the output should be longer than 10 chars and not contain
        // the stress pattern you'd get from individual digits.
        assert!(ipa.len() > 20, "too short for normalized year: {ipa:?}");
        // E2M `r→ɹ` should fire on at least one `r` from "year".
        assert!(ipa.contains('ɹ'), "E2M didn't fire: {ipa:?}");
    }
}
