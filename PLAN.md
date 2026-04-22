# Active Plan — Kokoro TTS (cross-platform)

**Branch**: `voice-bridge` · **Last updated**: 2026-04-22

Supersedes the TTS sections of `PLAN-VOICE.md` (historical) and folds in
the design discipline from `PLAN-KOKORO-TTS.md`. This file is the live
execution checklist — each `[ ]` becomes a `[x]` as it lands.

---

## Status snapshot

- [x] Phase 0: audio plumbing (manifest / fetch / cache / codec)
- [x] Phase 1: local STT via `whisper-rs`
- [x] Phase 3: voice → STT → tmux end-to-end
- [x] Phase 4a: TTS via macOS `say` (kept as opt-in fallback)
- [x] STT default → `small.en` q5_1 (accent-friendlier)
- [ ] **Phase 4b: Kokoro TTS cross-platform** ← this plan

## Goal

Replace the default TTS backend with Kokoro-82M via `kokoroxide`:

1. Linux gets TTS (currently returns `UnsupportedPlatform`)
2. Mac + Linux share one code path + one quality bar
3. `say` stays as opt-in macOS-only fallback for Siri-voice users

---

## Blockers — resolved by pre-implementation research

| Question | Answer |
|---|---|
| Where do voice files live on HF? | `onnx-community/Kokoro-82M-v1.0-ONNX/resolve/main/voices/{name}.bin` — per-voice 510 KB files, not a bundle. Confirmed 200 OK for `af_bella`, `am_adam`, `bf_emma`, `bm_george`, etc. |
| Model URL? | `onnx-community/Kokoro-82M-v1.0-ONNX/resolve/main/onnx/model.onnx` — fp16 ~175 MB. |
| Tokenizer URL? | `onnx-community/Kokoro-82M-v1.0-ONNX/resolve/main/tokenizer.json` |
| Which crate? | `kokoroxide` — purpose-built for Kokoro, accepts onnx-community's layout directly, MIT/Apache-2.0. |
| System deps? | `onnxruntime` (brew/apt) — required by `ort` in `load-dynamic` mode. `espeak-ng` (brew/apt) — phonemizer. User has already OK'd both. |
| Why not `any-tts` pure Rust? | Its Kokoro path uses Candle + hexgrad safetensors, no q4/quantized variant, only 11% of the crate documented. Risk of finding surprises mid-implementation. |

---

## Cleanup (do as we go, not upfront)

- [ ] Top-banner `PLAN-VOICE.md` noting it's historical (Phase 0–4a context).
- [ ] Top-banner `PLAN-KOKORO-TTS.md` noting it's the design reference (PLAN.md is execution).
- [ ] `src/audio/tts/mod.rs` module docs reference "macOS-only Phase 4" — update.
- [ ] `README.md` TTS bullet says "macOS" only — update to cross-platform.
- [ ] `examples/tts-bench.rs` hard-codes `TtsConfig` without a `backend` field — update once the field lands.

---

## Implementation checklist

Each `[ ]` is a discrete commit unless noted.

### 4b-1 · Manifest + asset pinning

- [ ] `src/audio/manifest.json`:
  - Keep `onnx_url` → correct `onnx/model.onnx` (already pinned SHA: `8fbea51e...`).
  - Add `tokenizer_url` + `tokenizer_sha256` + `tokenizer_size_bytes`.
  - Replace `voices_url` / `voices_sha256` / `voices_size_bytes` (single-bundle keys) with `voices: BTreeMap<String, VoiceAsset>` where each `VoiceAsset = { url, sha256, size_bytes }`.
  - Shipped voice set (small by default; `TELEGRAM_STT_MODEL=<any>` works if user wants more): `af_bella`, `am_adam`, `bf_emma`, `bm_george`.
- [ ] `src/audio/manifest.rs` — schema update to match.
- [ ] `scripts/pin-model-shas.sh` — extend to iterate voices array, fetch each, pin SHA.
- [ ] Run the pinning script.
- [ ] `cargo test --lib audio::manifest::` — passes.

### 4b-2 · Cargo deps

- [ ] `Cargo.toml` add:
  - `kokoroxide = { version = "0.1" }` (pin current version once verified)
  - `ort = { version = "2", default-features = false, features = ["load-dynamic"] }`
- [ ] `cargo deny check` — no `reqwest`, no new GPL.
- [ ] `cargo build` — succeeds.

### 4b-3 · System-dep probe

- [ ] `src/audio/tts/kokoro.rs::probe_runtime_deps()`:
  - `which espeak-ng` → return `TtsError::Init` with install hint if missing.
  - `ort::Environment::builder().build()` via load-dynamic; on failure, surface clear error with `ORT_DYLIB_PATH` env hint.
- [ ] Platform-specific install hints:
  - macOS: `brew install espeak-ng onnxruntime`
  - Ubuntu/Debian: `apt install espeak-ng libonnxruntime-dev`
- [ ] Probe runs at `AudioSubsystem::new` time, before downloading any assets.

### 4b-4 · `src/audio/tts/kokoro.rs` core

- [ ] `KokoroTts` struct holds `kokoroxide::KokoroTTS` + a voice-style registry (`HashMap<String, VoiceStyle>`).
- [ ] `KokoroTts::load(model_path, tokenizer_path, voices_dir, default_voice) -> Result<Self, TtsError>`:
  - probe deps
  - build `TTSConfig::new(&model_path, &tokenizer_path)` + `KokoroTTS::with_config`
  - lazy-load the default voice style (others loaded on first use)
- [ ] `impl Tts for KokoroTts::synthesize(text, voice) -> Synthesis`:
  - look up or lazy-load voice style
  - `spawn_blocking(move || tts.speak(text, &style))` — inference is CPU/GPU-bound
  - return `Synthesis { pcm, sample_rate: 24_000, duration_ms }` (Kokoro native rate)
- [ ] Keep `kokoroxide::*` types internal — no leak through public API.
- [ ] Unit tests: voice-lookup-miss returns clean error; pure-function voice-name validation.

### 4b-5 · Codec resampling (pure function)

- [ ] `src/audio/codec.rs::resample_24k_to_16k(pcm: &[f32]) -> Vec<f32>`:
  - Linear interpolation, stride 3:2.
  - Output length = `pcm.len() * 2 / 3` (±1 for fractional endpoint).
- [ ] Unit tests: output length, silence stays silent, sine-wave amplitude preserved within 5%.

### 4b-6 · Backend enum dispatch

- [ ] `src/audio/tts/mod.rs`:
  ```rust
  pub enum Backend {
      Kokoro(kokoro::KokoroTts),
      #[cfg(target_os = "macos")]
      Say(say::SayTts),
  }
  ```
- [ ] `Backend::synthesize`:
  - calls the backend's `Tts::synthesize`
  - resamples to 16 kHz at the boundary if `sr != 16_000` via `codec::resample_24k_to_16k`
- [ ] `Backend::display_name` for logs + dashboard.

### 4b-7 · AudioSubsystem wiring

- [ ] `src/audio/mod.rs::build_tts` dispatches on `cfg.backend`:
  - `TtsBackend::Kokoro`: download model + tokenizer + default voice; call `KokoroTts::load`
  - `TtsBackend::Say` (macOS): existing path unchanged
- [ ] Voice-file lazy-fetch helper: `ensure_voice_cached(name)` in `kokoro.rs`, called from `synthesize` if voice not yet on disk.

### 4b-8 · Config

- [ ] `src/audio/tts/mod.rs::TtsConfig`:
  ```rust
  pub struct TtsConfig {
      pub backend: TtsBackend,    // Kokoro (default) | Say
      pub voice: String,          // "af_bella" for Kokoro, "Samantha" for Say
      pub respond_to_all: bool,
  }
  ```
- [ ] `src/config.rs::load_tts_config`:
  - Parse `TELEGRAM_TTS_BACKEND` (default `kokoro`).
  - Reject `backend=say` on non-macOS with clear error.
  - Pick sensible voice default per backend.

### 4b-9 · Wizard

- [ ] `src/setup/steps.rs::step_tts`:
  - No longer cfg-gated macOS.
  - On macOS: ask backend (Kokoro | say) — default Kokoro.
  - Voice picker: show 4 shipped Kokoro voices + "any voice name from `tebis tts voices`".
- [ ] `WIZARD_MANAGED_KEYS`: add `TELEGRAM_TTS_BACKEND`.
- [ ] `src/setup/discover.rs`: parse TTS_BACKEND.
- [ ] `src/setup/ui.rs`: Voice-out summary row mentions backend.

### 4b-10 · CLI

- [ ] `src/tts_cli.rs` new module, mirrors `hooks_cli`:
  - `tebis tts voices` — list manifest voices grouped by language / gender.
  - `tebis tts test [voice]` — synthesize "Hello from tebis, I am {voice}", play via `afplay`/`aplay` if present, else print written tempfile path.
  - `tebis tts status` — show backend, voice, cached files, last-synth metrics.
- [ ] `src/main.rs` dispatches `tebis tts <verb>`.
- [ ] `HELP` text in `src/main.rs` updated.

### 4b-11 · Dashboard

- [ ] `src/inspect/mod.rs::VoiceInfo::tts_backend: &'static str`.
- [ ] `src/inspect/render.rs::build_voice_rows` displays the backend.
- [ ] `examples/inspect-demo.rs` updated.

### 4b-12 · Tests

- [ ] Unit: voice-name validation, resample helper (length + silence + amplitude), manifest voice-map parse.
- [ ] Integration `#[ignore]`: load cached model + tokenizer + voice, synthesize "test", assert > 100 samples.
- [ ] `examples/audio-smoke` — exercise Kokoro; drop `say` by default.
- [ ] `examples/tts-bench` — extend to A/B both backends on macOS.

### 4b-13 · Verification gates

- [ ] `cargo build --release` on macOS (primary dev box).
- [ ] `cargo test --lib` — all pass, 230+ count.
- [ ] `cargo clippy --all-targets -- -D warnings -W clippy::pedantic -W clippy::nursery`.
- [ ] `cargo deny check` — advisories / bans / licenses / sources ok.
- [ ] `./target/release/examples/audio-smoke` — full round-trip OK.
- [ ] `./target/release/tebis tts test af_bella` — produces audible, correct audio.
- [ ] README + docs cleanup from the top-of-file list done.

### 4b-14 · Wrap

- [ ] Squash / rebase into tidy commits per phase-step group.
- [ ] Final `git log --oneline master..voice-bridge` reviewed for narrative.

---

## Design non-negotiables (carried from PLAN-KOKORO-TTS.md §17)

- **Layer decomposition**: bridge → AudioSubsystem → Backend enum → Tts trait → codec → telegram. No cross-layer imports.
- **No leaky abstractions**: no `kokoroxide::*` or `ort::*` in public API.
- **`spawn_blocking` for inference**. Model loaded once; inference calls share.
- **Thiserror for module errors**; HTML-escape at the bridge boundary.
- **Functions ≤ 50 lines**. No `unwrap`/`expect` outside known-impossible paths.
- **RecordingTts** test fake for bridge tests — no real model in CI.

## Success criteria

1. `TELEGRAM_TTS=on` works on both macOS and Linux after two one-time installs (`espeak-ng`, `onnxruntime`).
2. End-to-end voice-in → voice-out round-trip ≤ 3 s on M4 Pro for a 1 k char reply.
3. Binary size delta ≤ 15 MB.
4. First-run TTS download ≤ 180 MB.
5. Tests + clippy + deny all green.

## Accepted risks

- `kokoroxide` 0.1.5 is young. Mitigation: pin version, test smoke path end-to-end.
- `ort` `load-dynamic` may miss `libonnxruntime` on non-standard paths. Mitigation: probe + `ORT_DYLIB_PATH` hint.
- Linear 24k → 16k resample loses content > 8 kHz. For speech this is inaudible; swap to `rubato` later if anyone complains.

## Non-goals (this phase)

- Streaming synthesis.
- Voice cloning.
- GPU acceleration on Linux (leave `ort`'s `cuda` feature off).
- Windows support.
- Removing `say` on macOS.
