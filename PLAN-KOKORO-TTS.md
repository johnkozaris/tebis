# Phase 4b — Kokoro q4 TTS Implementation Plan

> **📜 DESIGN REFERENCE** — This document is the design rationale for
> cross-platform Kokoro TTS. Execution was attempted and is currently
> **blocked** on Rust ecosystem issues (see `Cargo.toml` comment block
> and `PLAN.md`). All four candidate Kokoro crates fail to build in
> this workspace as of 2026-04-22. The live execution status is in
> `PLAN.md`; this file stays as the go-to reference for when the
> ecosystem stabilizes and implementation can resume.

**Status:** Design · 2026-04-22 · supersedes the `say`-based Phase 4 in `PLAN-VOICE.md`
**Scope:** Replace macOS `say` with cross-platform Kokoro q4 TTS. Close the Linux TTS gap. Keep `say` as an optional Mac-only fallback.

---

## 0. Executive summary

The current TTS backend is macOS `say` shelled out as a subprocess. It works but has three problems:

1. **Linux has no TTS** — fails with `UnsupportedPlatform`.
2. **Quality inconsistent** — Mac Premium voices > Linux `espeak-ng` > basic `say` voices.
3. **Dead-end** — `say` is Apple-proprietary; same code can't grow to better quality.

Kokoro-82M q4 solves all three in one move:
- Same quality on Mac and Linux
- Neural TTS (MOS 4.2 — near-commercial) vs `say`'s concatenative quality
- Cross-platform binary, shared code path

**Net delta**: ~400–500 LoC net (after dropping `say.rs` — can keep as opt-in), +~25 MB binary (Candle + phonemizer), +~130 MB disk on first run (Kokoro q4 + voices), zero new user system installs.

---

## 1. Crate choice: `any-tts` with `default-features = false`

### Why `any-tts` over `kokoro-tiny`

| Axis | `any-tts 0.1.1` | `kokoro-tiny 0.1.0` |
|---|---|---|
| Phonemizer | **Pure-Rust in-tree** (kana2phone + lindera + pinyin) | `espeak-ng` system dep |
| ONNX backend | Candle (pure Rust, Metal-native) | `ort` (FFI to C++) |
| `reqwest` required? | Only if `download` feature enabled — we disable it | No |
| System installs on user box | **None** | `brew install espeak-ng` + `brew install onnxruntime` |
| Binary size delta | +~25 MB | +~5 MB (with ort load-dynamic) |
| Cross-platform | Identical story | Same story but depends on user actions |
| Apache/MIT | MIT OR Apache-2.0 ✓ | Apache-2.0 ✓ |

**Decision**: `any-tts`. The +20 MB binary cost buys zero new system installs on both macOS and Linux — aligned with the "no extra install friction" goal that drove the prior `say` choice. Pure-Rust end-to-end also keeps the audit surface homogeneous.

### Cargo.toml additions

```toml
[target.'cfg(target_os = "macos")'.dependencies]
any-tts = { version = "0.1", default-features = false, features = ["kokoro", "metal"] }

[target.'cfg(target_os = "linux")'.dependencies]
any-tts = { version = "0.1", default-features = false, features = ["kokoro"] }
# "accelerate" feature available on Mac too — test if it speeds synthesis.
# "cuda" available on Linux if a GPU is present — skip for broad-deploy default.
```

### Why disabling `download` is safe

`any-tts`'s `download` feature pulls `reqwest` to fetch models from Hugging Face. We already have `src/audio/fetch.rs` — SHA-verified, cancel-safe, redirect-following, with manifest pinning. We hand the loaded model bytes into `any-tts` directly. No reqwest pulled. `deny.toml` stays unchanged.

### Transitive concerns to validate

- `candle-core` / `candle-nn` / `candle-transformers` — Apache-2.0. Binary growth ~15–20 MB.
- `lindera` + IPA dict (Japanese morphology) — pulled unconditionally with `kokoro` feature. ~5 MB data + code. Compiled in even though we default to English. Acceptable.
- `pinyin` (Chinese) — small (<1 MB).
- `kana2phone` (Japanese kana → phoneme) — small.
- ORT not pulled when Candle backend selected (verify at build time).

Blocking check: `cargo tree --edges normal | grep reqwest` must be empty after the dep addition.

---

## 2. System impact summary

### New deps

| | Cargo | System install |
|---|---|---|
| macOS | `any-tts 0.1` | **None** (Metal via Candle is pure Rust) |
| Ubuntu | `any-tts 0.1` | **None** |
| RPi / ARM | `any-tts 0.1` | **None** |

### What gets fetched on first run

- `model.onnx` (Kokoro-82M q4 quantized) — ~90 MB, SHA-pinned in manifest
- `voices.bin` — ~26 MB, SHA-pinned in manifest
- Cached to `$XDG_DATA_HOME/tebis/models/kokoro/` (new subdir)

### What stays the same

- `src/audio/fetch.rs` — re-used verbatim
- `src/audio/cache.rs` — new `kokoro/` subdir under `models/`
- `src/audio/codec.rs` — PCM → Opus → OGG path unchanged
- `src/telegram/send_voice` — unchanged
- `src/bridge/synthesize_and_send_voice` — unchanged
- Dashboard + wizard structure — small updates only

### What's removed

- Nothing — `say.rs` stays as an opt-in macOS-only backend, gated behind `TELEGRAM_TTS_BACKEND=say`.

---

## 3. Architecture

### Module layout

```
src/audio/tts/
├── mod.rs          # Tts trait, Backend enum, TtsError, TtsConfig
├── kokoro.rs       # NEW: KokoroTts wrapping any-tts's SynthesisRequest
└── say.rs          # Existing; now opt-in via config
```

### Backend enum

```rust
pub enum Backend {
    /// Kokoro-82M q4 via any-tts, Metal on macOS / CPU elsewhere.
    /// Default on both platforms.
    Kokoro(kokoro::KokoroTts),

    /// macOS `say` shell-out. Opt-in via `TELEGRAM_TTS_BACKEND=say`.
    /// Kept for users who want Siri-grade Premium voices.
    #[cfg(target_os = "macos")]
    Say(say::SayTts),
}

impl Backend {
    pub async fn synthesize(&self, text: &str, voice: &str) -> Result<Synthesis, TtsError> {
        match self {
            Self::Kokoro(b) => b.synthesize(text, voice).await,
            #[cfg(target_os = "macos")]
            Self::Say(b) => b.synthesize(text, voice).await,
        }
    }

    pub const fn display_name(&self) -> &'static str {
        match self {
            Self::Kokoro(_) => "kokoro",
            #[cfg(target_os = "macos")]
            Self::Say(_) => "say",
        }
    }
}
```

### KokoroTts shape

```rust
pub struct KokoroTts {
    // any-tts loads the model once at construction; inference shares the
    // loaded context across calls via its own thread-safe abstraction.
    engine: any_tts::kokoro::KokoroBackend,
    sample_rate: u32,  // 24000 typical — we'll resample to 16000 for Telegram
}

impl KokoroTts {
    pub async fn load(
        model_path: &Path,
        voices_path: &Path,
    ) -> Result<Self, TtsError> { ... }
}

impl Tts for KokoroTts {
    async fn synthesize(&self, text: &str, voice: &str)
        -> Result<Synthesis, TtsError>
    {
        // 1. Build any-tts SynthesisRequest with language="en"
        // 2. Dispatch on spawn_blocking (Candle inference is CPU-bound or GPU-bound)
        // 3. Resample output from 24000 Hz → 16000 Hz mono for telegram
        // 4. Return Synthesis { pcm, duration_ms }
    }
}
```

### Resampling

Kokoro outputs at **24 kHz mono** by default (per any-tts docs). Telegram voice is **16 kHz mono**. Options:

1. **Hand-rolled linear interpolation** in `audio/codec.rs::resample_24k_to_16k` — ~20 LoC, negligible quality loss for speech. Speech-band info is well below Nyquist.
2. Use a dedicated resampler crate (`rubato`, `samplerate`) — ~1 MB binary growth, better quality, overkill for speech.
3. Change Opus encoder to accept 24 kHz — no, Telegram wants 16 kHz for the voice bubble.

**Pick**: hand-rolled linear. ~20 LoC, testable, zero new deps.

### Ownership model

- `KokoroTts` loads the model once in `AudioSubsystem::new` (blocking, ~1 s cold).
- Shared across all synthesis calls via owned struct inside `Backend::Kokoro`.
- `any-tts`'s own internals handle thread-safety of inference.
- Handler semaphore (8 permits) already caps concurrent syntheses.

---

## 4. Config surface

### New env var

`TELEGRAM_TTS_BACKEND` — `kokoro` (default) / `say` (macOS only).

### Updated voice list

Kokoro has 54 voices across 8 languages. For English we expose:

**Female**:
- `af_bella` — default; neutral US accent
- `af_sky` — brighter tone
- `af_nicole` — warmer
- `af_sarah` — professional
- `af_alloy` — calm
- `af_nova` — energetic

**Male**:
- `am_adam` — default male; neutral US accent
- `am_michael` — deeper tone
- `am_onyx` — serious

**British**:
- `bf_emma` — British female
- `bm_george` — British male

(Users can set any of the 54 — `TELEGRAM_TTS_VOICE=am_adam`.)

Voice blending: Kokoro supports `TELEGRAM_TTS_VOICE=af_sky+af_nicole.3` syntax for voice style mixing. Leave this as power-user territory; don't surface in wizard.

### Wizard changes

**Step 8 (Voice replies)** — now cross-platform:

```
┌─ Voice replies (optional) ───────────────────────────────┐
│                                                          │
│ Synthesize Telegram text replies as voice notes using    │
│ Kokoro TTS (local, no cloud, no extra install).          │
│                                                          │
│ ? Enable voice replies? (y/N)                            │
│                                                          │
│ [if yes:]                                                │
│ ? Voice:                                                 │
│   > 1. af_bella (US female, default)                     │
│     2. am_adam (US male, default)                        │
│     3. bf_emma (British female)                          │
│     4. bm_george (British male)                          │
│     5. af_sky (US female, brighter)                      │
│     6. ... (see docs for all 54)                         │
│                                                          │
│ ? Also voice-reply to typed messages? (y/N)              │
│                                                          │
│ [macOS only:]                                            │
│ ? Use macOS `say` instead of Kokoro? (y/N)               │
│   Note: say has higher quality with Siri voices but      │
│   Mac-only. Kokoro is cross-platform and default.        │
└──────────────────────────────────────────────────────────┘
```

`WIZARD_MANAGED_KEYS` gains `TELEGRAM_TTS_BACKEND`.

---

## 5. CLI commands (new — optional but high-value)

```
tebis tts voices          List available Kokoro voices (54 options).
tebis tts test [voice]    Synthesize "Hello, this is tebis" and play
                          it via system audio. Validates full pipeline
                          without requiring Telegram round-trip.
tebis tts status          Show TTS subsystem state (backend, cached
                          model version, last synthesis stats).
```

Modeled on the existing `tebis hooks {install,uninstall,status}` shape. Saves users from spelunking to debug audio quality / voice choice.

Implementation: ~100 LoC in a new `src/tts_cli.rs` (mirrors `src/hooks_cli.rs`).

---

## 6. First-run flow

```
T=0.00s  User sets TELEGRAM_TTS=on (or answered "yes" in wizard step 8).
T=0.05s  Config parse: AudioConfig.tts = Some(TtsConfig { backend: Kokoro, voice: "af_bella", ... }).
T=0.12s  AudioSubsystem::new() dispatches to build_kokoro_tts().
T=0.13s  manifest::get() resolves kokoro-v1.0 entry.
         validate_tts_usable() passes (SHAs pinned).
T=0.13s  cache::models_dir()/kokoro/ created (0700).
T=0.14s  Check kokoro/model.onnx — MISSING.
T=0.14s  fetch::download_verified(onnx_url, onnx_sha, tmp, final, progress, cancel)
         Progress every 2 s: "[12/90 MB · 13%]"
T=12.4s  Hash ✓, atomic rename.
T=12.5s  Same for voices.bin — ~26 MB, 3-4 s.
T=16.0s  Both files present.
T=16.0s  any-tts::KokoroBackend::load(model_path, voices_path)
         Candle initializes; Metal backend detected on macOS.
T=16.9s  Model resident (~250 MB RAM on macOS Metal, ~200 MB on Linux CPU).
T=16.9s  Health check: synthesize "test" → expect 16 kHz f32 PCM > 0 samples.
T=17.2s  Backend::Kokoro(tts) stored on AudioSubsystem.
T=17.2s  main loop starts accepting messages.

T=65s    User sends voice note "open the README" (STT path, unchanged).
T=66.5s  Bridge responds. Reply is Response::Text("README opened. Here's ...").
T=66.5s  inbound_was_voice = true → audio.should_tts_reply(true) = true.
T=66.5s  synthesize_and_send_voice:
         - strip_html_for_tts(body)
         - audio.synthesize(text)
           -> KokoroTts.synthesize(text, "af_bella")
             -> spawn_blocking: Candle inference → 24 kHz mono f32
             -> resample 24 kHz → 16 kHz
           -> Synthesis { pcm, duration_ms }
         - codec::encode_pcm_to_opus(pcm) -> OGG/Opus
         - telegram::send_voice(chat_id, opus, Some(duration))
T=67.7s  Voice note lands on phone. Total end-to-end: ~2.7 s.
```

Subsequent boots: ~300 ms (SHA re-verify + Candle context load).

---

## 7. Edge cases and error matrix

| Failure | Behavior |
|---|---|
| Kokoro ONNX download fails (network) | 3 retries → log error → `TtsError::Init` → `AudioSubsystem::new` returns `Err` → main.rs logs warn and continues text-only (same fail-open as current). |
| voices.bin download fails | Same. Partial model is unusable — refuse to init. |
| SHA mismatch on either file | Delete `.tmp`, single retry. If still mismatched, fatal; log expected/got and fall back to text-only. Cached-file-corrupt case handled by invariant 20. |
| any-tts model load fails (OOM, format error) | `TtsError::Init("load: {e}")`. Log, fall back to text-only. |
| SIGTERM during model download | Fetch observes `cancel.cancelled()`, returns `FetchError::Cancelled`. AudioSubsystem::new bubbles; main.rs treats as shutdown. |
| User sets `TELEGRAM_TTS_BACKEND=say` on Linux | Config parse rejects with "backend `say` is macOS-only". |
| User sets `TELEGRAM_TTS_VOICE=mx_alfred` (nonexistent voice) | On first synthesis, any-tts returns error. Convert to `TtsError::Synthesis("unknown voice `mx_alfred` — see `tebis tts voices`")`. Best-effort: fall back to default voice? No — loud error is clearer. |
| Text contains emoji / exotic Unicode | Kokoro phonemizer strips unknown tokens or reads them literally. Document as "may produce odd audio for non-text content." No filter in tebis. |
| Text is empty / whitespace-only after `strip_html_for_tts` | `synthesize_and_send_voice` bails silently — we already checked `plain.trim().is_empty()`. Text reply already sent. |
| Text is > 4000 chars (should never happen — capped in handler path) | any-tts processes in chunks. Worst case 2–4 s synthesis. Acceptable. |
| Telegram `sendVoice` fails (network / 413 / 429) | Already handled — metric recorded, log warn, continue. Text reply is primary. |
| Resample gives 0 samples | Defensive check in `Synthesis` constructor: `if pcm.is_empty() → EmptyOutput`. Error surfaces to bridge, no voice sent. |
| Concurrent synthesis (two replies landing simultaneously) | Handler semaphore (8 permits) bounds parallelism. any-tts's Kokoro backend is thread-safe per its docs. If not: synth blocks behind a Mutex inside `KokoroTts`. Tested under load. |
| Model version upgrade mid-fleet | Manifest bump = new tebis release. Cached file SHAs re-verified per startup (invariant 20); mismatched files renamed `.corrupt-<ts>` and re-fetched. |
| User runs on Windows | Blocked at build time via cfg gates on `any-tts` deps + backend enum. Already not a supported target. |
| Low RAM (< 1 GB total, e.g. Pi 3) | Kokoro q4 needs ~150 MB resident. Whisper small.en adds another ~200 MB. Base tebis ~50 MB. Total ~400 MB — tight on 1 GB but fits on 2 GB. Document minimum system requirements. |
| Synthesis mid-shutdown | `spawn_blocking` closures aren't cancellable. Worst case: ~3 s of continued CPU after SIGTERM before the task exits. Acceptable — bounded. |
| `say` backend selected but not macOS | Compile-time error via cfg gating on the enum variant. Config parse checks platform too. |

---

## 8. Resource profiles per platform

| | macOS M4 Pro | Ubuntu Ryzen 5825U | RPi 5 / 2 GB | Docker minimal |
|---|---|---|---|---|
| Backend | Kokoro q4 + Metal | Kokoro q4 + CPU | Kokoro q4 + CPU | Kokoro q4 + CPU |
| Model load time | ~0.9 s | ~1.2 s | ~3 s | ~1.5 s |
| Synth latency for 3k chars | ~500 ms | ~1.5–2.5 s | ~4–6 s | ~2 s |
| Resident RAM | ~250 MB | ~200 MB | ~150 MB | ~200 MB |
| Transient RAM during synth | +50 MB | +50 MB | +40 MB | +50 MB |
| First-run download | ~130 MB (model+voices) | Same | Same (tight) | Same |
| Full tebis + whisper+kokoro RAM | ~700 MB | ~600 MB | ~500 MB | ~600 MB |

**Minimum supported RAM**: 1.5 GB free at daemon start time. Document in README.

---

## 9. Migration path

Users on the `voice-bridge` branch with TTS enabled:

- Before: `TELEGRAM_TTS=on` → used `say` (macOS only; Linux errored).
- After: `TELEGRAM_TTS=on` → defaults to `kokoro` (both platforms).
- On next `tebis restart`:
  - macOS: downloads Kokoro model/voices (~130 MB, ~12 s), loads. Old `say` path still available via `TELEGRAM_TTS_BACKEND=say`.
  - Linux: downloads Kokoro model/voices, loads. Linux users who had `TELEGRAM_TTS=on` set but got `UnsupportedPlatform` errors now start working.

No env-file migration needed — adding `TELEGRAM_TTS_BACKEND=say` is optional for mac users who prefer `say`.

---

## 10. Testing strategy

Mirrors existing `stt::local` and `tts::say` test patterns.

- `kokoro::tests::resample_preserves_length` — unit test for the 24 kHz → 16 kHz downsampler.
- `kokoro::tests::error_on_unknown_voice` — inject nonexistent voice, expect clean error.
- `kokoro::tests::synthesize_non_empty_output` — synth "test", assert > 100 samples returned. **Gated `#[ignore]`** — needs the real model downloaded.
- `bridge::tests::voice_tts_backend_dispatch` — RecordingTts fake (to add), verify backend enum dispatch.
- Manifest tests: `validate_tts_usable_accepts_pinned_sha` passes once voices.bin SHA is pinned.
- `examples/audio-smoke`: extend to synthesize via Kokoro (both platforms); compare to `say` output on macOS.
- New `examples/tts-bench`: extended from current bench to run both backends side-by-side on macOS so you can A/B quality.

CI:
- `cargo deny check` — verify no `reqwest` in the transitive graph.
- `cargo clippy --all-targets -- -D warnings -W clippy::pedantic -W clippy::nursery`.

---

## 11. Phased rollout

### Phase 4b-1: Model assets pinning (~30 min)
1. Find correct `voices.bin` URL on HF (the current `onnx-community/Kokoro-82M-v1.0-ONNX/resolve/main/voices.bin` returns 404).
2. Re-run `scripts/pin-model-shas.sh --apply` to pin the SHA.
3. Verify with a manual `curl | shasum -a 256`.

### Phase 4b-2: Core backend (~250 LoC)
4. Add `any-tts` to `Cargo.toml` with cfg-gated features.
5. `cargo deny check` — verify no reqwest.
6. `src/audio/tts/kokoro.rs`: `KokoroTts` struct, `load()`, `synthesize()`, resample helper.
7. `src/audio/tts/mod.rs`: `Backend::Kokoro` variant, dispatch.
8. `src/audio/mod.rs::build_tts`: match on backend config.

### Phase 4b-3: Config + wizard (~80 LoC)
9. `src/config.rs`: `TELEGRAM_TTS_BACKEND` parsing. Linux rejects `say`.
10. `src/setup/steps.rs`: step 8 no longer cfg-gated to macOS; add voice picker + backend picker (macOS).
11. `WIZARD_MANAGED_KEYS`: add `TELEGRAM_TTS_BACKEND`.

### Phase 4b-4: CLI (~100 LoC)
12. `src/tts_cli.rs`: `tts voices | test | status` subcommands.
13. `src/main.rs`: dispatch `tebis tts <verb>`.

### Phase 4b-5: Dashboard (~30 LoC)
14. `src/inspect/render.rs`: Voice section shows backend name + voice.

### Phase 4b-6: Tests + smoke (~80 LoC)
15. Unit tests for resample + voice validation.
16. `examples/audio-smoke`: extend to synthesize via Kokoro on both platforms.
17. Integration test (gated) that actually synthesizes.

### Phase 4b-7: README + docs (~20 lines)
18. Update README's TTS bullet: cross-platform, single code path.
19. Document minimum RAM (1.5 GB), first-run download sizes.

**Total**: ~560 LoC + doc. One commit per phase for reviewability.

---

## 12. Accepted risks

1. **Binary grows +25 MB.** Candle ML framework + phonemizer libs. Release binary from 5 MB → 30 MB. Worth it for quality jump + Linux support.
2. **First-run download (~130 MB total).** Acceptable given existing ~180 MB Whisper download.
3. **~1.5 GB minimum RAM** means Pi 3 / very small VPS won't work. Document.
4. **Candle is newer than whisper.cpp.** Less battle-tested. Risk: edge-case bugs. Mitigation: `any-tts` is a focused crate using Candle for one purpose — Kokoro inference — well-exercised by other users of `any-tts`.
5. **Kokoro is English-primary** — for voice synthesis on our default voice list. Non-English text won't sound great unless user picks a matching voice (`jf_alpha` for Japanese, etc.). Document.
6. **`any-tts` is at 0.1.1** — a young crate. Version pin + vendored fallback plan if upstream breaks.

---

## 13. Non-goals

- **Streaming synthesis** (emit audio as we synthesize). Kokoro is fast enough batch (<2 s on M4). Adds complexity to the `sendVoice` upload path (we'd need to delay-start the upload). Deferred.
- **Voice cloning** — Kokoro doesn't support it; would need a different model (F5-TTS, Chatterbox).
- **Emotion / prosody controls** — Kokoro has style mixing (`af_sky+af_nicole.3`) as its only knob. Document as power-user territory; no wizard prompt.
- **Multi-language UI** — we ship English voices by default; users can override via config to any of the 54 voices if they want.
- **GPU (CUDA) acceleration on Linux** — `any-tts` supports `cuda` feature, but most deploy targets won't have CUDA. Leave off by default; users with GPU hosts can opt in via a fork.
- **Windows support** — not a tebis target anyway.
- **Dropping `say` on macOS** — keep as opt-in for users who prefer Siri voices. Minimal maintenance burden.

---

## 14. Open questions / blockers

### BLOCKER: Kokoro voices.bin URL

The URL in the current manifest (`onnx-community/Kokoro-82M-v1.0-ONNX/resolve/main/voices.bin`) returns 404. Possibilities:

- File renamed / relocated in the HF repo since the any-tts docs were written.
- Voices bundle lives at a different path (e.g., `hexgrad/Kokoro-82M/` instead of `onnx-community/`).
- Voices are now shipped as individual `.bin` files per voice (would change the fetch logic).

**Action before Phase 4b-2**: manually browse https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/tree/main and https://huggingface.co/hexgrad/Kokoro-82M/tree/main to identify the correct voices-data path. Update manifest.

Fallback: use any-tts's own download mechanism for the voices if we can't find a stable URL. That reintroduces reqwest. Prefer to avoid.

### OQ1: Default voice

`af_bella` (female) vs `am_adam` (male) as the out-of-box default. Kokoro-82M docs call out `af_bella` as the demo default; `any-tts` may have a different recommendation. **Lean**: `af_bella` for consistency with upstream.

### OQ2: Resample quality

Hand-rolled linear downsample (24 → 16 kHz) is simple but introduces mild aliasing above 8 kHz. For speech (fundamental <~300 Hz, formants up to 4 kHz) this is inaudible. Still: if anyone complains, swap to `rubato` crate (high-quality polyphase resampler). **Lean**: ship linear; file a follow-up only if a user notices.

### OQ3: `tts voices` output format

Plain list vs grouped by language / gender. 54 voices is noisy in a flat list. **Lean**: group by language-then-gender, ANSI colors (matching existing `tebis hooks status` aesthetic).

### OQ4: RAM probe

Should `AudioSubsystem::new` check available RAM before attempting Kokoro load? Rationale: on a 1 GB Pi with other services running, Kokoro OOM-kills the daemon. A 500 MB pre-check warning would be kinder than an OOM panic. **Lean**: skip for v1; add if reports come in.

---

## 15. Success criteria

Before merge to `voice-bridge`:

- [ ] `cargo build --release` succeeds on macOS M4 Pro.
- [ ] `cargo build --release` succeeds on Linux x86_64 (cross-compile or actual).
- [ ] `cargo test --lib` passes (all 227+ tests + new Kokoro tests).
- [ ] `cargo clippy --all-targets -- -D warnings -W clippy::pedantic -W clippy::nursery` clean.
- [ ] `cargo deny check` clean — no `reqwest` in graph.
- [ ] `examples/audio-smoke` produces intelligible speech on macOS (audible test).
- [ ] `tebis tts test` subcommand works and plays correctly.
- [ ] Binary size delta ≤ 30 MB.
- [ ] First-run download ≤ 150 MB total.
- [ ] End-to-end voice-note round-trip ≤ 3 s on M4 Pro for a 1 k char reply.

---

## 16. Timeline

- Phase 4b-1 (voices.bin URL): ~30 min (research + pin)
- Phase 4b-2 (core backend): ~2 hours
- Phase 4b-3 (config + wizard): ~45 min
- Phase 4b-4 (CLI): ~1 hour
- Phase 4b-5 (dashboard): ~15 min
- Phase 4b-6 (tests + smoke): ~45 min
- Phase 4b-7 (docs): ~15 min

Total: ~5–6 hours of focused work. One PR or squash to voice-bridge as single commit.

---

## 17. Code design discipline — decoupling, SOLID, performance

This implementation must land as **clean, decoupled code** that could have any of its layers swapped without touching the others. The current audio subsystem already sketches this shape (trait seams for `Stt`/`Tts`, dedicated `fetch`/`cache`/`codec`/`manifest` modules); Kokoro must reinforce the pattern, not erode it.

### 17.1 Layer decomposition (single-responsibility)

Each layer owns exactly one concern. No layer reaches across boundaries.

```
┌───────────────────────────────────────────────────────────────┐
│  bridge::handle_update                                        │ ← message orchestration only
│       │                                                       │
│       ▼  calls only audio::AudioSubsystem::synthesize(text)   │
│  audio::AudioSubsystem                                        │ ← composition root
│       │                                                       │
│       ▼  dispatches on enum, doesn't know backends exist      │
│  audio::tts::Backend (enum)                                   │ ← backend selection
│       ├── Kokoro(KokoroTts)                                   │
│       └── Say(SayTts)                                         │
│           │                                                   │
│           ▼  implement the Tts trait                          │
│  audio::tts::Tts (trait)                                      │ ← synthesis contract
│           │                                                   │
│           ▼  produces Synthesis { pcm: Vec<f32>, sr, dur }    │
│  audio::codec::encode_pcm_to_opus                             │ ← encoding (backend-agnostic)
│           │                                                   │
│           ▼                                                   │
│  audio::codec::resample_to_16k     (new, pure function)       │ ← DSP (backend-agnostic)
│           │                                                   │
│           ▼                                                   │
│  telegram::send_voice                                         │ ← transport
└───────────────────────────────────────────────────────────────┘
```

**Rule**: no module "up" the stack imports from modules further "down" than its direct neighbor. Backend implementations don't import `telegram`. Codec doesn't import `tts`. Violations = refactor before shipping.

### 17.2 Interface-first: the `Tts` trait is the contract

- `Tts::synthesize(text, voice) -> Synthesis` is the **only** entry any concrete backend exposes to the rest of the codebase.
- All backend-specific knobs (Kokoro voice pack location, `say` voice name) are held **inside** the backend struct, constructed at `::new` / `::load` time from config.
- `Backend::Kokoro` / `Backend::Say` variants do not expose their inner structs publicly. The `pub enum` just dispatches `synthesize`.
- The bridge never sees `KokoroTts`; it sees `&AudioSubsystem`.
- Tests inject `RecordingTts` via the trait without knowing anything about Candle or `say`.

### 17.3 SOLID applied

**S — Single Responsibility**

- `kokoro.rs` does one thing: wrap any-tts into the `Tts` trait. Not downloading, not caching, not resampling (those are separate modules).
- `codec.rs` does encoding/decoding/resampling — no knowledge of TTS backends.
- `fetch.rs` does SHA-verified downloads — no knowledge of Kokoro's voice format.
- `manifest.rs` owns pinned URLs + SHAs — no knowledge of how the bytes get loaded.

**O — Open/Closed**

- Adding a third TTS backend (e.g. future Piper / Orpheus) = new module + new `Backend` enum variant. Zero changes to `AudioSubsystem`, `bridge`, `telegram`, or existing backends.
- Tests: existing tests stay unchanged; new backend brings its own tests.

**L — Liskov**

- All `Tts` implementers must satisfy the same contract: given valid `text` + `voice`, return 16 kHz mono `f32` PCM in `[-1.0, 1.0]`. Resampling lives in `codec::resample_to_16k` so backends can emit whatever rate they natively produce — Kokoro emits 24 kHz, `say` emits 16 kHz, future backends can emit 48 kHz. The trait takes **same-rate PCM** at a specific rate (`Synthesis` carries the `sample_rate`), and `AudioSubsystem::synthesize` resamples post-hoc. Substitutable backends.

**I — Interface Segregation**

- `Tts` exposes only `synthesize`. No loading, no introspection, no voice listing — those are backend-specific concerns separated into backend-specific API (`KokoroTts::load`, `KokoroTts::list_voices`).
- Callers who only want to synthesize don't depend on load/listing APIs.

**D — Dependency Inversion**

- `bridge` depends on `AudioSubsystem` (concrete but acts as a facade) via an `Option<Arc<AudioSubsystem>>` — never on `KokoroTts` or `SayTts`.
- `AudioSubsystem` depends on the `Tts` trait abstraction via its `Backend` enum.
- Concrete backends sit at the bottom of the dependency graph — nothing else imports them.

### 17.4 Clean code rules

- **Functions ≤ 50 lines.** Break out helpers aggressively.
- **No `pub` unless necessary.** Backend struct internals stay `pub(crate)` or private. Cross-crate surface = `Tts` trait + `Backend` enum + `AudioSubsystem::synthesize`.
- **Errors travel as typed enums**, not `String`. Sub-modules use `thiserror`; `bridge` converts to user-facing HTML-escaped strings at the boundary (invariant 4).
- **No panics in library code.** `todo!()`, `unwrap()`, `expect()` banned in non-test paths unless they represent a "known-impossible at build time" invariant (manifest parse failure, hyper `Request::builder` with static args).
- **Naming**: verbs for functions, nouns for types. No abbreviations in public APIs. `synthesize` not `synth`, `pcm_samples` not `pcm`.
- **Comments explain WHY, not WHAT.** The code shows what; comments explain invariants, tradeoffs, and non-obvious choices.
- **No leaky abstractions.** Don't expose Candle types (`candle_core::Tensor`) through `Tts`. Convert at the backend boundary.

### 17.5 Performance discipline

- **`spawn_blocking` for inference.** Candle/any-tts Kokoro inference is CPU/GPU-bound work that holds the runtime thread — must run on the blocking pool. Pattern mirrors `stt::local::LocalStt::infer`.
- **Model loaded once, reused many.** `KokoroTts::load` at subsystem init; inference calls share the loaded context via `Arc<KokoroBackend>` inside `KokoroTts`. **No per-call model load.**
- **Zero allocation in the hot path** where practical. Pre-allocate buffers for resampling (`Vec::with_capacity(expected_len)`), reuse across calls.
- **Resample in-place when possible.** For 24k → 16k it's a 2/3 ratio — straightforward stride loop, no new allocations beyond the output buffer.
- **Pass `&str` / `&[f32]` across boundaries**, not `String` / `Vec<f32>`. Owned values only where ownership transfer is real (e.g. `Synthesis::pcm` is owned because the caller consumes it end-to-end).
- **`Arc` sparingly.** Current code uses `Arc<AudioSubsystem>` (shared across handlers — correct) and `Arc<WhisperContext>` (shared across inference calls — correct). Do the same for Kokoro. Don't `Arc`-wrap things used by a single owner.
- **Cancellation: opt in.** Inference can't be cancelled once started (Candle has no cancel API). Accept the ~3 s worst-case shutdown drain. Document; don't attempt to forcibly abort.

### 17.6 Testability

- Every module has a `#[cfg(test)] mod tests` with unit tests. At least one test per public function.
- Backend fakes: `RecordingTts` (tests-only) implements `Tts` with a recorded-call log + canned output. Bridge dispatch tests use it — no real Kokoro model loaded in CI.
- Pure functions (resample, parse WAV) are trivially testable — prefer them over stateful methods where the domain allows.
- Integration tests (real model loads) are `#[ignore]` with a clear "requires X MB download / Y system" comment; CI doesn't run them by default.

### 17.7 Non-negotiable invariants carried forward from `PLAN-VOICE.md`

- Inv 4 (HTML-escape user-facing text) — reinforced by `strip_html_for_tts` before synthesis.
- Inv 5 (never log transcript text) — TTS input = Claude's output, which the user already has in chat. Still: log at `debug!(bytes = text.len())`, not the text.
- Inv 12 (TaskTracker for background tasks) — any deferred loads / retries go on the shared tracker.
- Inv 20 (re-verify model SHA on startup) — applies to Kokoro onnx + voices.bin.
- Inv 21 (no audio bytes on disk) — TTS pipeline holds `Vec<f32>` → `Bytes` in memory, encodes to OGG, sends over HTTP. No persistent audio artifacts.

### 17.8 Code review checklist (before merging Phase 4b)

- [ ] No public API that takes or returns Candle / `any-tts` types.
- [ ] `KokoroTts` inner struct is `pub(crate)` or narrower.
- [ ] No imports that reach across more than one layer of the diagram in §17.1.
- [ ] All public functions in new modules have doc comments stating WHY they exist.
- [ ] All new error variants have `thiserror::Error` impls.
- [ ] No `String` parameters where `&str` would do.
- [ ] No `Vec<T>` returns where `impl Iterator<Item = T>` is viable.
- [ ] Every backend-specific detail (voice lists, URLs, sample rates) is in `manifest.json` or the backend module's private scope, not sprinkled across bridge / config.
- [ ] `cargo bloat --release -n 20` output reviewed — new Candle deps account for the expected ~20 MB, nothing else surprising.

---

## 18. References

- [any-tts on crates.io](https://crates.io/crates/any-tts) — 0.1.1, MIT OR Apache-2.0, features matrix
- [any-tts on GitHub (flow-like)](https://github.com/TM9657/any-tts) — source
- [hexgrad/Kokoro-82M](https://huggingface.co/hexgrad/Kokoro-82M) — upstream model, Apache-2.0
- [onnx-community/Kokoro-82M-v1.0-ONNX](https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX) — ONNX + voices distribution
- [huggingface/candle](https://github.com/huggingface/candle) — Rust ML framework
- [Kokoros (lucasjinreal)](https://github.com/lucasjinreal/Kokoros) — reference CLI using Kokoro
