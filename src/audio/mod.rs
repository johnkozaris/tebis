//! Voice bridge subsystem: STT (inbound) and TTS (outbound, Phase 4).
//!
//! Current status: **Phase 0 plumbing**. Models/codec/providers are
//! stubbed or deferred; this module establishes the layout and ships
//! the infrastructure everything else sits on.
//!
//! - `manifest.rs` — embedded JSON of pinned asset URLs + SHAs.
//! - `cache.rs` — `$XDG_DATA_HOME/tebis/models/` filesystem layout,
//!   atomic model install, stale-tmp reaping.
//! - `fetch.rs` — HTTPS streaming download with SHA-256 verification.
//! - `codec.rs` — OGG/Opus ↔ PCM for Telegram voice (stub, Phase 3).
//! - `stt/` — Phase 1: `whisper-rs` in-process + remote providers.
//! - `tts/` — Phase 4: `any-tts` in-process + remote providers.
//!
//! See `/PLAN-VOICE.md` for the end-to-end design, including invariant
//! compliance (CLAUDE.md 4, 5, 6, 9, 10, 12) and the rollout phases.

pub mod cache;
pub mod codec;
pub mod fetch;
pub mod manifest;

/// Unified error surface for the audio subsystem. Sub-modules keep their
/// own typed errors (`FetchError`, `CodecError`, …) for pattern-matching;
/// this enum is the one we expose to `bridge`, which flattens to an
/// HTML-escaped reply string.
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error(transparent)]
    Fetch(#[from] fetch::FetchError),

    #[error(transparent)]
    Codec(#[from] codec::CodecError),

    #[error("audio subsystem config: {0}")]
    Config(String),

    #[error("audio subsystem not initialized for provider `{provider}`")]
    NotEnabled { provider: &'static str },
}
