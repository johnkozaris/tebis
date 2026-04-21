//! OGG/Opus ⇄ PCM codec for the Telegram voice format.
//!
//! **Status: stub — implementation deferred to Phase 3 (bridge integration).**
//!
//! Phase 0 intentionally does not lock in a codec crate because the
//! obvious candidate, [`ogg-opus`](https://crates.io/crates/ogg-opus),
//! is at v0.1.2 (2021) with no clear license file, and the pure-Rust
//! alternatives ([`hasenbanck/opus-native`](https://github.com/hasenbanck/opus-native),
//! [`lu-zero/opus`](https://github.com/lu-zero/opus)) are explicitly
//! unfinished. The decision gets made at Phase 3 when we actually need
//! the decode path: audit license, evaluate alternatives that have
//! shipped in the meantime, possibly settle on `audiopus` + `ogg` directly.
//!
//! The module exists now so:
//! 1. The API shape is documented and visible to reviewers.
//! 2. `audio::mod` can declare it without cfg-gating.
//! 3. Tests that don't exercise decode/encode still compile.
//!
//! Calling either function at runtime in Phase 0–2 will panic via
//! `todo!`; STT in Phase 1 runs against already-decoded PCM provided by
//! the test harness / integration wiring, not through this codec.

use bytes::Bytes;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("opus decode failed: {0}")]
    Decode(String),

    #[error("opus encode failed: {0}")]
    Encode(String),

    #[error("unsupported sample rate {0} Hz (expected 16000 for Whisper input / 48000 for Telegram voice)")]
    UnsupportedRate(u32),
}

/// Decode OGG/Opus bytes (as delivered by Telegram `voice` messages) to
/// 16 kHz mono PCM samples normalized to `[-1.0, 1.0]` — the exact input
/// shape `whisper-rs` wants.
///
/// Telegram voice is OGG container, Opus codec, 16 kHz, mono. Music-file
/// uploads (`message.audio`) may differ; callers are expected to limit
/// this to the voice path or pre-resample.
pub fn decode_opus_to_pcm16k(_oga_bytes: &[u8]) -> Result<Vec<f32>, CodecError> {
    todo!("Phase 3: wire opus decode crate (see module docs)")
}

/// Encode 48 kHz mono PCM `f32` samples to an OGG/Opus byte blob suitable
/// for `POST /sendVoice`. Kokoro (via `any-tts`) emits 24 kHz — callers
/// resample to 48 kHz before this (or we can support 24 kHz input here
/// in Phase 4 when we know the exact shape).
pub fn encode_pcm_to_opus48k(_pcm: &[f32]) -> Result<Bytes, CodecError> {
    todo!("Phase 4: wire opus encode crate (see module docs)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_variants_render() {
        // Keep the Display impls covered so renaming them doesn't silently
        // regress the error messages users see.
        assert!(CodecError::Decode("x".into()).to_string().contains("opus decode"));
        assert!(CodecError::Encode("x".into()).to_string().contains("opus encode"));
        assert!(CodecError::UnsupportedRate(22050).to_string().contains("22050"));
    }
}
