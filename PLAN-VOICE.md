# tebis — Voice Bridge Design (STT + TTS)

> **📜 HISTORICAL** — This document captures the Phase 0–4a design as it was
> when the voice bridge was first planned. Several sections (cloud providers,
> whisper-server sidecar, initial TTS plans) were pivoted during
> implementation. The execution source-of-truth now lives in `PLAN.md`, and
> the latest design rationale lives in `PLAN-KOKORO-TTS.md`. Keep this file
> for historical context; do not use it as the current reference.

**Status:** Design · updated 2026-04-21 after architectural pivots
**Scope:** Phone → voice → tmux pipeline. STT is the primary v1 feature. TTS is Phase 4, flag-gated, off by default.
**Audience:** tebis core; public release targets Mac (M-series) and Linux x86_64 / ARM64.

---

## 0. Executive summary

Adds an `audio/` subsystem to tebis so voice notes from Telegram drive the existing `handler::parse` pipeline as if they had been typed.

**Locked architectural decisions** (resulting from multiple pivots in design review):

1. **Everything runs in-process in the tebis daemon.** No HTTP server, no sidecar child process, no Docker. Single binary, single process — same shape as tebis today, just with a model loaded into memory.
2. **`whisper-rs` v0.16 for STT**, linked as a Cargo dep with platform-appropriate acceleration features. C++ whisper.cpp under the hood via FFI; from the developer-POV it's one `cargo add`. Binary grows ~20 MB.
3. **`any-tts` for TTS** (Phase 4 only), linked in-process. Ships Kokoro-82M via ONNX Runtime with a **pure-Rust phonemizer** (no `espeak-ng` system dep). MIT/Apache-2.0.
4. **`ogg-opus` crate for Telegram audio codec** — handles decode (inbound) and encode (outbound). Replaces the earlier ffmpeg plan. Pulls in `libopus` via `audiopus_sys` (vendored).
5. **Models downloaded on first use**, SHA-256 pinned in an embedded manifest (`src/audio/manifest.json`), re-verified on every startup. Never packed into the binary.
6. **Provider abstraction**: every STT/TTS backend is an impl of a thin trait. Local (in-process) is default; cloud (Groq, OpenAI) and `openai_compat` (user-pointed URL) are alternatives. The public release works out of the box with zero API keys and zero LAN infrastructure.
7. **No `tebis-sidecars` release repo, no whisper-server, no Kokoro-FastAPI assumption.** Power users who have their own Kokoro-FastAPI / Whisper-ASR on a LAN box set `openai_compat` base-URL — that's a per-user config, not a project-level assumption.

Respects every CLAUDE.md invariant — especially 4 (HTML-escape), 5 (never log text), 6 (redact bot-token URLs), 9–11 (cache hardening), and 12 (`TaskTracker` for background tasks).

---

## 1. Library research

### 1.1 STT — `whisper-rs` v0.16 in-process

Upstream [tazz4843/whisper-rs](https://github.com/tazz4843/whisper-rs), latest v0.16.0 (March 2026). MIT OR Apache-2.0. Rust bindings to whisper.cpp via `whisper-rs-sys`.

**Cargo usage per platform:**

```toml
[target.'cfg(target_os = "macos")'.dependencies]
whisper-rs = { version = "0.16", features = ["metal", "coreml"] }

[target.'cfg(target_os = "linux")'.dependencies]
whisper-rs = { version = "0.16", features = ["openblas"] }  # optional CPU accel
```

**Build prerequisites** (once per dev box):
- macOS: Xcode Command Line Tools + `brew install cmake`
- Ubuntu: `apt install build-essential cmake libopenblas-dev`

**API shape.** Takes `Vec<f32>` samples at 16 kHz mono. Does NOT decode audio formats — caller must decode (see §1.7).

**Why this over alternatives:**
- `candle-whisper` (pure Rust, HF framework): viable fallback; less battle-tested for production
- `ct2rs` (CTranslate2 FFI): no Metal on Apple Silicon; loses on M-series
- `whisper-cli` subprocess: 300 ms model-load per call, bad hot-path
- upstream whisper.cpp releases: don't ship darwin-arm64 or linux-x86_64 binaries

**Reference:** Linux builds "just work out of the box" per the upstream README; M-series requires `BUILDING.md` steps (CLT + cmake). Active project, v0.16.0 shipped 2026-03-12.

### 1.2 STT models

Hosted at [ggerganov/whisper.cpp on Hugging Face](https://huggingface.co/ggerganov/whisper.cpp/tree/main).

| Model | Size | Default for |
|---|---|---|
| `ggml-tiny.en.bin` | 77.7 MB | (not shipped — quality too low) |
| `ggml-base.en.bin` | 148 MB | **Default on both platforms** |
| `ggml-small.en.bin` | 488 MB | Opt-in via config for better accuracy |

**SHA caveat**: HF doesn't expose stable SHA-256 HTTP headers (they use git-LFS or Xet-checksum which aren't consumable as Rust-crate-friendly constants). Must `shasum -a 256` a known-good download once per model and pin that hex in the manifest. Placeholders (`TBD-PLACEHOLDER-*`) until done.

Distil-Whisper models are NOT available in the ggml format from this repo — dropped. If users want faster-than-base, `small.en` is the next step.

### 1.3 TTS — `any-tts` in-process (Phase 4)

Crate: [`any-tts` on crates.io](https://crates.io/crates/any-tts). MIT/Apache-2.0. Candle + ORT hybrid, trait-based.

**Why `any-tts` over the alternatives** (all evaluated as of 2026-04):

| Crate | Phonemizer | License | Verdict |
|---|---|---|---|
| **`any-tts`** | **Pure-Rust in-tree** | MIT/Apache-2.0 | ✅ **Chosen** |
| `tts-rs` 2026.2.1 | Kokoro-onnx built-in | Unclear | Needs license audit before adopting |
| `kokoroxide` | espeak-ng system dep | MIT/Apache-2.0 (crate) | Extra install; worse UX |
| `kokorox` | espeak-rs-sys static | **GPL-3.0** | ❌ Blocked by `deny.toml` |
| `Kokoros` | E2E built-in | Unclear | Unclear license |

`any-tts` is the only actively-maintained path with a permissive license AND zero `espeak-ng` system dep — this matters because installing `espeak-ng` is an extra step the public version shouldn't require.

**Cargo usage:**

```toml
any-tts = { version = "0.1", default-features = false, features = ["kokoro", "download", "metal"] }
```

Under the hood `any-tts` uses `ort` (Microsoft ONNX Runtime bindings). Use **`ort` with `load-dynamic`** to avoid a ~100 MB binary — that requires the user to install `onnxruntime` once (`brew install onnxruntime` on Mac, `apt install libonnxruntime-dev` on Ubuntu). Binary growth with `load-dynamic`: ~5 MB.

### 1.4 TTS — Kokoro model + voices

Hosted at [onnx-community/Kokoro-82M-v1.0-ONNX](https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX).

| Asset | Size | Purpose |
|---|---|---|
| `kokoro-v1.0.onnx` | ~330 MB | Main TTS model |
| `voices-v1.0.bin` | ~26 MB | Voice embeddings (multiple voices bundled) |

Same SHA-pin flow as Whisper models.

### 1.5 Cloud STT providers

- **Groq** — `POST https://api.groq.com/openai/v1/audio/transcriptions`. Model `whisper-large-v3-turbo`. $0.04/hr audio. Free tier: 2000 req/day, 7200 audio seconds/hour. 25 MB limit. OpenAI-compatible multipart (`file` field). [Docs](https://console.groq.com/docs/speech-to-text).
- **OpenAI** — `POST /v1/audio/transcriptions`. Model `whisper-1` or `gpt-4o-transcribe`. $0.006/min.
- **`openai_compat`** — arbitrary base URL + API key. Power-user escape hatch. Users who run their own Whisper-ASR / Kokoro-FastAPI on a LAN server point this at them. The tebis project makes no assumption about the existence of such an endpoint.

### 1.6 Cloud TTS providers (Phase 4)

- **OpenAI** — `POST /v1/audio/speech`. `tts-1` at $15/1M chars (~$0.04/reply). `tts-1-hd` at $30/1M chars.
- **`openai_compat`** — same escape hatch. A user's personal setup might be `TELEGRAM_TTS_BASE_URL=http://<LAN_IP>:8880/v1` pointing at their own LAN Kokoro-FastAPI. Not a project default.

### 1.7 Audio codec — `ogg-opus` crate

Crate: [`ogg-opus` on crates.io](https://crates.io/crates/ogg-opus). Pulls in `audiopus_sys` which wraps libopus (C library, vendored by default at build time — no runtime system dep).

**Decode (STT path)**: OGG/Opus bytes → `Vec<i16>` PCM at 16 kHz mono → normalize to `Vec<f32>` in `[-1.0, 1.0]` → hand to `whisper-rs`.

**Encode (TTS path, Phase 4)**: `Vec<f32>` PCM from Kokoro → `Vec<i16>` → OGG/Opus bytes at 48 kHz → `sendVoice`.

Same crate both directions. Replaces the earlier ffmpeg shell-out plan.

**Build prerequisite:**
- Mac: `brew install pkg-config opus` (or let audiopus_sys vendor it)
- Ubuntu: `apt install pkg-config libopus-dev` (or vendored)

**Pure-Rust alternatives evaluated and rejected:**
- `symphonia` — pure Rust, OGG container supported, but Opus codec NOT supported yet. Community SILK PR open, not merged.
- `hasenbanck/opus-native` — "under heavy development, most functionality is not working" per upstream README.
- `lu-zero/opus` — pure-Rust decoder, maturity unclear.

### 1.8 Other crates

| Concern | Choice | Alternative rejected |
|---|---|---|
| SHA-256 streaming | **`ring::digest::SHA256`** (already in tree via rustls) | `sha2` — adds a new dep for no win |
| Multipart body | **Hand-roll** (~60 LoC) | `reqwest`, `mpart-async`, `common-multipart` — violate CLAUDE.md "hand-roll on hyper" |
| HF download progress | `tokio::io::copy` with tee-writer → file + hasher | N/A |
| HTTP client | Reuse tebis's existing `hyper-util::legacy::Client` stack (fresh instance — separate redaction rules) | N/A |

**Net new crates**: `whisper-rs`, `ogg-opus` (Phase 1). `any-tts` (Phase 4, if TTS ships).

---

## 2. Manifest design

### 2.1 Format: JSON, embedded

- JSON (reuses existing `serde_json`; no `toml` crate).
- Embedded via `include_str!("manifest.json")` at compile time.
- Never runtime-fetched — anti-rugpull. Bumping a model = new tebis release.
- Location: `src/audio/manifest.json` + deserializer in `src/audio/manifest.rs`.
- **Models-only.** No sidecar binaries to pin.

### 2.2 Schema

```json
{
  "manifest_version": 1,
  "tebis_version": "0.2.0",
  "updated_at": "2026-04-21",
  "stt_models": {
    "base.en": {
      "url": "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin",
      "sha256": "TBD-PLACEHOLDER-base-en",
      "size_bytes": 147964211,
      "display_name": "Whisper Base (English) — 148 MB",
      "default": true
    },
    "small.en": {
      "url": "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.en.bin",
      "sha256": "TBD-PLACEHOLDER-small-en",
      "size_bytes": 511662473,
      "display_name": "Whisper Small (English) — 488 MB",
      "default": false
    }
  },
  "tts_models": {
    "kokoro-v1.0": {
      "onnx_url": "https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/main/onnx/model.onnx",
      "onnx_sha256": "TBD-PLACEHOLDER-kokoro-onnx",
      "onnx_size_bytes": 346000000,
      "voices_url": "https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/main/voices.bin",
      "voices_sha256": "TBD-PLACEHOLDER-kokoro-voices",
      "voices_size_bytes": 26000000,
      "display_name": "Kokoro-82M v1.0 — 346 MB + 26 MB voices",
      "default": true
    }
  }
}
```

### 2.3 Versioning + tamper-detection

- SHA verified on download AND every startup (Invariant 20, §6).
- Mismatch → rename to `{name}.corrupt-{unix_ts}` (preserve forensics) → re-download.
- Cached models never auto-deleted. User can `rm -rf $XDG_DATA_HOME/tebis/models/`.

---

## 3. Module layout

### 3.1 Dependency diagram

```
                         main.rs
                            │
                            ▼
       ┌────────────────────┴─────────────────────┐
       │                                          │
       ▼                                          ▼
 audio/mod.rs                               bridge/mod.rs
       │                                          │
   ┌───┼────────────────┐                         ▼
   ▼   ▼                ▼               bridge::handle_update
 fetch manifest cache                         │
   │                                          ▼
   ▼                              ┌── Payload::Text ─── (existing path)
 codec (ogg-opus wrap)             │
   │                              └── Payload::Voice ─┐
   ▼                                                  │
 stt/                                                 ▼
   │              ┌─────────────────────── telegram::get_file
   │              │                         telegram::download_file
 tts/ (Phase 4)   │                                   │
                  └───────────────────────────────────┘
                             │
                             ▼
                   codec::decode_opus_to_pcm
                             │
                             ▼
                     stt::transcribe
                             │
                             ▼
                    handler::parse(text)
```

No cycles.

### 3.2 New modules

#### `src/audio/mod.rs`
Top-level subsystem. Owns `AudioSubsystem` holding `Stt`/`Tts` providers + cache handle. Lazy: if both STT and TTS are `off`, never touches the cache dir.

```rust
pub struct AudioSubsystem {
    stt: Option<Box<dyn Stt>>,
    tts: Option<Box<dyn Tts>>,     // Phase 4
}

impl AudioSubsystem {
    pub async fn new(cfg: &AudioConfig, tracker: &TaskTracker, shutdown: CancellationToken)
        -> anyhow::Result<Arc<Self>>;
    pub async fn transcribe(&self, ogg_bytes: Bytes, lang: &str) -> Result<Transcription, AudioError>;
    pub async fn synthesize(&self, text: &str, voice: &str) -> Result<Bytes, AudioError>;
}

pub struct Transcription { pub text: String, pub duration_ms: u32, pub language: String }
```

Mirrors `notify/mod.rs` in entry-point shape. Invariants: 12 (all spawns on shared tracker).

#### `src/audio/fetch.rs`
HTTP GET with streaming SHA-256 + atomic-rename-on-success.

```rust
pub async fn download_verified(
    client: &HyperClient,
    url: &str,
    expected_sha256: &str,
    target_path: &Path,
    progress: impl FnMut(u64, Option<u64>),
    cancel: CancellationToken,
) -> Result<(), FetchError>;
```

Behavior:
1. HEAD for Content-Length (best-effort progress).
2. GET, stream body through a `TeeWriter<File, Sha256Context>`.
3. Verify hash on EOF; mismatch → delete tmp, return `ChecksumMismatch`.
4. `sync_all()`, atomic rename.
5. Cancel-safe: stale `.tmp` reaped on next startup.

Uses `ring::digest::SHA256` (no new deps). Invariants: 6 (redacted errors), 9 (perms enforced post-rename), 10 (read-timeout).

#### `src/audio/manifest.rs`
Parses the `include_str!`ed manifest.json.

```rust
pub fn load() -> anyhow::Result<&'static Manifest>;
impl Manifest {
    pub fn stt_model(&self, name: &str) -> anyhow::Result<&ModelAsset>;
    pub fn tts_model(&self, name: &str) -> anyhow::Result<&VoiceAsset>;
    pub fn default_stt_model(&self) -> &str;
    pub fn default_tts_model(&self) -> &str;
}
```

Mirrors `agent_hooks/manifest.rs` in shape. Validated via `#[test]` at crate build.

#### `src/audio/cache.rs`
Owns `$XDG_DATA_HOME/tebis/` layout. On Mac: `~/Library/Application Support/tebis/` (via `dirs::data_dir()`). Creates dirs 0700, models 0644. `reap_stale_tmps()` at startup.

```
$XDG_DATA_HOME/tebis/
├── models/
│   ├── ggml-base.en.bin        (0644)
│   ├── kokoro-v1.0.onnx        (0644, only if TTS enabled)
│   └── voices-v1.0.bin         (0644, only if TTS enabled)
└── installed.json              (existing — agent_hooks)
```

Key fns:

```rust
pub fn base_dir() -> anyhow::Result<PathBuf>;
pub fn model_path(name: &str) -> PathBuf;
pub fn install_model_atomic(src_tmp: &Path, dst: &Path) -> io::Result<()>;
pub fn reap_stale_tmps(base: &Path) -> io::Result<()>;
```

Invariants: 9 (dual-enforcement of perms via `OpenOptions::mode()` + explicit `set_permissions`). Mirrors `env_file::atomic_write_0600`.

#### `src/audio/codec.rs`
Thin wrapper around `ogg-opus` for decode (STT) and encode (TTS).

```rust
pub fn decode_opus_to_pcm16k(oga_bytes: &[u8]) -> Result<Vec<f32>, CodecError>;  // STT
pub fn encode_pcm_to_opus48k(pcm: &[f32]) -> Result<Bytes, CodecError>;          // TTS (Phase 4)
```

No state; pure functions. `Vec<i16>` → `Vec<f32>` normalization (`x / 32768.0`) done inline.

#### `src/audio/stt/mod.rs` + backends

```rust
#[async_trait]
pub trait Stt: Send + Sync + 'static {
    async fn transcribe(&self, pcm: &[f32], lang: &str) -> Result<Transcription, SttError>;
}

pub struct LocalStt { ctx: WhisperContext, params: FullParams }        // whisper-rs in-process
pub struct OpenAiCompatStt { base_url: String, api_key: Option<SecretString>, client, model_name }
pub struct GroqStt { /* base_url pinned to groq; required api_key */ }
pub struct OpenAiStt { /* base_url pinned to openai */ }
```

Shared multipart body builder for the three remote backends (~60 LoC). `LocalStt` is pure Rust calls into whisper-rs — no multipart, no HTTP.

Error taxonomy:

```rust
#[derive(Debug, thiserror::Error)]
pub enum SttError {
    #[error("transcription failed: network: {0}")]
    Network(String),      // redacted
    #[error("transcription failed: provider returned {0}")]
    Provider(u16),
    #[error("transcription failed: audio too long ({secs}s > {cap}s)")]
    TooLong { secs: u32, cap: u32 },
    #[error("transcription failed: local inference error: {0}")]
    LocalInference(String),
    #[error("transcription failed: decoder: {0}")]
    Decoder(String),
    #[error("transcription failed: rate-limit")]
    RateLimited,
}
```

Invariants: 4 (`escape_html` on any provider text before error reply), 6 (URL redaction), 10 (byte cap).

#### `src/audio/tts/mod.rs` + backends (Phase 4)

Analogous. `Tts::synthesize(text, voice) -> Result<Bytes /* PCM f32 */, TtsError>`. Caller pipes through `codec::encode_pcm_to_opus48k`.

Backends:
- `LocalTts` — `any-tts` in-process with Kokoro-82M.
- `OpenAiTts` — `POST /v1/audio/speech`.
- `OpenAiCompatTts` — escape hatch (user's own LAN Kokoro-FastAPI, etc.).

### 3.3 Modified modules

#### `src/telegram/mod.rs`
Three new methods:

```rust
pub async fn get_file(&self, file_id: &str) -> Result<TelegramFile>;
pub async fn download_file(&self, file_path: &str) -> Result<Bytes>;
pub async fn send_voice(&self, chat_id: i64, voice_ogg: Bytes, duration_sec: Option<u32>)
    -> Result<Message>;
```

- `download_file` URL is `https://api.telegram.org/file/bot<TOKEN>/<path>`. Contains bot token — reuse existing `SecretString` pattern; errors through `redact_network_error`.
- `send_voice` hand-rolls multipart. 32-char hex-random boundary, three fields (`chat_id`, `voice`, optional `duration`).
- `MAX_FILE_BYTES = 20 * 1024 * 1024` — well above any plausible voice note.

#### `src/telegram/types.rs`
Add `Voice`, `Audio`, `TelegramFile` DTOs. `Message` gains `voice: Option<Voice>` and `audio: Option<Audio>`.

#### `src/bridge/mod.rs`
`handle_update` split on a new `Payload`:

```rust
pub enum Payload {
    Text(String),
    Voice { file_id: String, duration: u32 },
}
```

Voice-path pseudocode:

```
1. Rate-limit check (reuse existing).
2. Permit acquire.
3. duration > STT_MAX_DURATION_SEC → reply "too long" (escape_html'd).
4. telegram::get_file(file_id) + download_file(path); enforce MAX_BYTES.
5. codec::decode_opus_to_pcm16k(bytes).
6. stt.transcribe(pcm, lang).
7. log: debug!(duration_ms, audio_bytes, transcript_bytes) — NEVER the transcript.
8. handler::parse(transcript) — reuse existing execution path.
9. Metrics.
```

If TTS enabled (Phase 4), after `Response::Text(body)` or `Response::Sent` produces a text reply, additionally synthesize + `sendVoice`. Both sent — text stays scrollable.

`main.rs` unchanged except for `Payload` construction from `(message.text, message.voice)`.

#### `src/config.rs`
`AudioConfig` co-located in `src/audio/mod.rs` per CLAUDE.md convention. See §4.

#### `src/setup/steps.rs`
New **step 7**: "Voice input". Toggle + provider pick. If `local`, download model during wizard with progress bar.

#### `src/inspect/render.rs`
New `Voice` section: STT status (provider, model, cache size), TTS status (if enabled).

---

## 4. Config schema

All env vars follow the `TELEGRAM_*` convention.

### 4.1 STT

| Env var | Default | Controls |
|---|---|---|
| `TELEGRAM_STT` | `off` | Master flag |
| `TELEGRAM_STT_PROVIDER` | `local` | `local, openai_compat, groq, openai` |
| `TELEGRAM_STT_MODEL` | `base.en` | Key from manifest |
| `TELEGRAM_STT_BASE_URL` | unset | For `openai_compat` |
| `TELEGRAM_STT_API_KEY` | unset | SecretString; required for `groq`/`openai`, optional for `openai_compat` |
| `TELEGRAM_STT_LANGUAGE` | `en` | ISO-639-1; empty = auto |
| `TELEGRAM_STT_MAX_DURATION_SEC` | `120` | 1..=900 |
| `TELEGRAM_STT_MAX_BYTES` | `20971520` (20 MB) | 64 KiB..=50 MiB |
| `TELEGRAM_STT_THREADS` | `auto` (half CPUs, min 2, max 8) | whisper-rs thread count |

### 4.2 TTS (Phase 4)

| Env var | Default | Controls |
|---|---|---|
| `TELEGRAM_TTS` | `off` | Master flag |
| `TELEGRAM_TTS_PROVIDER` | `local` | `local, openai, openai_compat, off` |
| `TELEGRAM_TTS_MODEL` | `kokoro-v1.0` | Key from manifest |
| `TELEGRAM_TTS_VOICE` | `af_bella` (Kokoro) / `alloy` (OpenAI) | Provider-specific |
| `TELEGRAM_TTS_BASE_URL` | unset | For `openai_compat` (e.g. a user's own Kokoro-FastAPI) |
| `TELEGRAM_TTS_API_KEY` | unset | SecretString |

SecretString handling mirrors `TELEGRAM_BOT_TOKEN` — `Debug` prints `<redacted>`; never in URL path or log.

---

## 5. First-run flow

### 5.1 Happy path — default config, STT on

```
T=0.00s  User sets TELEGRAM_STT=on in ~/.config/tebis/env. Restart.
T=0.05s  Config parse. AudioConfig says provider=local, model=base.en.
T=0.12s  audio::AudioSubsystem::new()
T=0.13s  manifest::load() parses embedded JSON.
T=0.13s  cache::base_dir() = "/Users/user/Library/Application Support/tebis"
T=0.13s  cache::reap_stale_tmps() — nothing.
T=0.14s  Check models/ggml-base.en.bin — MISSING.
T=0.14s  log: info!("Downloading ggml-base.en.bin (148 MB)…")
T=0.14s  fetch::download_verified(url, sha256, tmp, progress, cancel)
           Progress every 2s: "[12/148 MB · 8%]"
T=53.1s  Hash ✓. Atomic rename.
T=53.2s  whisper-rs: WhisperContext::new_with_params(&path, ctx_params)
T=53.5s  Model loaded (~300 ms on M4 with Core ML).
T=53.5s  Health: inference on 1 s of silent PCM. Returns empty string. ✓
T=53.6s  Stt wired. Bridge main loop starts.

T=65s    Voice note arrives: "open the README"
T=65.1s  bridge sees Payload::Voice { file_id, duration: 1 }
T=65.1s  Rate-limit ok. Permit acquired. Duration ok.
T=65.12s telegram::get_file() → file_path
T=65.21s telegram::download_file() → 12 KB OGG/Opus
T=65.22s codec::decode_opus_to_pcm16k() → 16000 f32 samples
T=65.23s stt.transcribe(&pcm, "en") — whisper-rs in-process
T=65.48s Result: "open the README"
T=65.48s log: debug!(duration_ms=250, audio_bytes=12384, transcript_bytes=17)
T=65.49s handler::parse("open the README") → Command::PlainText → execute
T=~90s   Claude responds; pane settle + Stop-hook (if installed) forward reply.
```

Total first-run: ~53 s (model download on thin pipe). Subsequent boots: ~300 ms (SHA check, whisper-rs context load, health).

**No sidecar, no port picking, no health-check HTTP round-trip** — just a function call.

### 5.2 Error modes

| Failure | Behavior |
|---|---|
| Model download fails (net) | 3 retries → log error → `AudioSubsystem::new` returns `Err`. Main.rs treats as NON-fatal; logs "STT unavailable; set `TELEGRAM_STT_PROVIDER=groq` or check network." Bridge continues text-only. |
| SHA mismatch on download | Delete `.tmp`, single retry. If still mismatched, fatal for that provider; fall back to text-only with warning. |
| Cached model SHA drifts from manifest at startup | Rename to `{name}.corrupt-<ts>` (forensics) + re-download. Log `warn!` with expected/got. |
| Disk full during download | `FetchError::Io`. Text-only fallback. |
| whisper-rs inference error (OOM, corrupt model) | `SttError::LocalInference`. User reply (escape_html'd): "Voice transcription failed; try again or switch provider." |
| OGG/Opus decode error (malformed payload) | `SttError::Decoder`. Same user reply. |
| `TELEGRAM_STT_BASE_URL` set (user has own LAN server) | Skip ALL model download logic. Stt is wired to remote URL. Single-line log. |
| Voice > `MAX_DURATION_SEC` | Reject pre-download, "Voice too long (Xs > 120s cap)". |
| `file_size > MAX_BYTES` | Reject pre-download, same shape. |
| Empty/whitespace transcript | Reply "Could not transcribe (no speech detected)." |
| Transcript > 4000 chars | Cap via `sanitize::wrap_and_truncate` semantics pre-handler. |
| `libonnxruntime` missing at runtime (TTS only) | Phase 4. TTS fails with "onnxruntime not installed; brew install onnxruntime." STT unaffected. |

---

## 6. Security design

### 6.1 Existing CLAUDE.md invariant compliance

| Invariant | How audio subsystem complies |
|---|---|
| **4 (HTML-escape replies)** | All `SttError` / `TtsError` rendered via `sanitize::escape_html` before `send_message`. Transcripts flow through `handler::parse` — same escape gates as typed commands. |
| **5 (never log `message.text`)** | Extended to transcripts. Log at most `debug!(transcript_bytes, duration_ms)`. Audio bytes never in journal (no Debug-printing on `Bytes`). |
| **6 (redact network errors)** | Telegram download URL contains bot token; reuse `redact_network_error`. HF + provider URLs don't carry secrets in path, but run everything through the same redactor as hygiene. |
| **7 (low-level tracing at warn)** | No change — audio subsystem uses the same `hyper` stack as Telegram; filters already cover it. |
| **9 (permission hardening)** | Cache dir 0700, models 0644. Dual-enforced via `OpenOptions::mode()` + `set_permissions`. |
| **10 (payload cap + read timeout)** | STT upload cap = 20 MB. Download read timeout 30 s. No HTTP to a local sidecar (that path is gone). |
| **11 (framing robust)** | Multipart body writes deterministic CRLF with 32-char hex-random boundary. Unit tests on boundary-collision + empty-part edges. |
| **12 (TaskTracker for background)** | Any `spawn`ed fetch/validation tasks go on the shared tracker. Shutdown cancels them. |
| **17 (UDS three-layer defense)** | N/A — no UDS added. |

### 6.2 New invariants proposed

- **Invariant 18: Transcript text is treated as equivalent to `message.text` for logging.** Extension of 5 to the audio path.
- **Invariant 19: Transcript size cap before `handler::parse` (4000 chars).** Matches `TELEGRAM_MAX_OUTPUT_CHARS`; prevents fatfinger/noisy-audio from pasting 100 KB into tmux.
- **Invariant 20: Cached model SHAs re-verified against embedded manifest on every startup.** Mismatch → `{name}.corrupt-<ts>` + re-download. Cost ~800 ms for base.en; acceptable.
- **Invariant 21: Audio bytes never written to disk unencrypted.** `Bytes` held in memory; no `.wav`/`.ogg` intermediate cache.

### 6.3 Threat model

- **Local attacker with write to cache dir.** Cache is 0700, uid-gated. Invariant 20 catches tampering on next restart. Same-uid attacker = game-over for the bot token anyway. Accepted risk.
- **MITM on model download.** HTTPS + rustls + webpki-roots + SHA pin end-to-end.
- **Malicious voice exploiting whisper.cpp.** whisper.cpp fuzzed; CVEs get patched. Sandboxing out of scope for v1.
- **TTS leaking secrets over cell network.** TTS default off; opt-in.

---

## 7. Testing strategy

Match `src/notify/listener.rs::tests` pattern.

| Module | Tests |
|---|---|
| `fetch.rs` | Mock hyper server; SHA mismatch → `ChecksumMismatch`; partial EOF → `UnexpectedEof`; cancel-token cleanup. |
| `cache.rs` | tempdir + mode assertions (mirror `env_file::atomic_write_0600_creates_with_mode`); `reap_stale_tmps` discrimination. |
| `manifest.rs` | `include_str!` parses; every model has non-empty URL+SHA. |
| `codec.rs` | Round-trip: PCM → encode → decode → PCM matches within tolerance. Golden fixture: decode `contrib/fixtures/silent-1s.oga` returns near-zero samples. |
| `stt/mod.rs` | `RecordingStt` test fake mirroring `Recorder: Forwarder` in notify tests. |
| `stt/local.rs` | Integration test: load `ggml-tiny.en.bin` (78 MB, smallest), transcribe the golden fixture, assert non-empty result. Gated by `#[ignore]` + env var so CI can skip unless set up. |
| `stt/openai_compat.rs` | Mock hyper server for multipart shape assertions. |
| `bridge/mod.rs` | `Payload::Voice` dispatch; HTML escape on errors; `RecordingStt` + fake `TelegramClient` wire-up. |

Manual E2E: `contrib/voice-e2e.sh` — send known voice file, assert pane text. Gated by test bot token.

Golden fixtures: `contrib/fixtures/silent-1s.oga` (few hundred bytes).

---

## 8. Rollout plan

Four phases. Each PR independently mergeable and useful.

### Phase 0 — Plumbing
**Scope:** `src/audio/{manifest,fetch,cache,codec}.rs`. Embedded manifest with placeholder SHAs. No providers yet. Live but inert.
**Acceptance:** Unit tests pass. Manual test: `cargo run --example audio-fetch` downloads a real HF file and verifies SHA.
**Risks:** `audiopus_sys` build friction on Ubuntu (apt libopus-dev may not match vendored version).
**LoC estimate:** 500–700.

### Phase 1 — Local STT
**Scope:** `src/audio/mod.rs`, `src/audio/stt/{mod,local}.rs`. Setup wizard step 7. Dashboard row. `whisper-rs` linked with Metal+CoreML on Mac / openblas on Linux.
**Acceptance:** Fresh install, `TELEGRAM_STT=on`, model downloads + transcription works end-to-end on both Mac and Ubuntu.
**Risks:** whisper-rs Core ML build on Mac may require full Xcode (not just CLT) per upstream BUILDING.md. Doc clearly.
**LoC estimate:** 600–800.

### Phase 2 — Remote STT
**Scope:** `src/audio/stt/{openai_compat,groq,openai}.rs`. Shared multipart body builder extracted. Wizard respects `TELEGRAM_STT_PROVIDER`.
**Acceptance:** Groq API key → voice message arrives → transcription in <1 s. `openai_compat` pointed at arbitrary URL works.
**Risks:** Groq 10-second minimum billing surprise; log billable seconds at debug.
**LoC estimate:** 400–600.

### Phase 3 — Bridge integration
**Scope:** `telegram::{get_file, download_file}` + `Voice`/`Audio` types. `bridge::handle_update` → `Payload` dispatch. End-to-end voice → text → tmux. Metrics.
**Acceptance:** Phone voice note → 5 s later pane has the command typed + Claude responding. `/status` shows voice stats.
**LoC estimate:** 300–500.

### Phase 4 (optional, post-ship gauge) — TTS
**Scope:** `src/audio/tts/{mod,local,openai,openai_compat}.rs`. `any-tts` linked with Kokoro. `telegram::send_voice` with multipart. `codec::encode_pcm_to_opus48k`.
**Acceptance:** `TELEGRAM_TTS=on` + local provider → voice reply plays on Telegram desktop + iOS. OpenAI cloud works identically. User with own LAN Kokoro-FastAPI works via `openai_compat`.
**Risks:** `ort` `load-dynamic` setup documentation; `libonnxruntime` install on Ubuntu can drag in many MB.
**LoC estimate:** 500–700.

**Total LoC across all phases:** 2300–3300.

---

## 9. Open questions

1. **Voice arrives while `TELEGRAM_STT=off`**: silent drop or reply "STT disabled"? (lean: silent drop — "misconfig is your problem")
2. **Music-file `message.audio` (not voice note)**: transcribe same way, or ignore? (lean: transcribe — cheap, same code path)
3. **Default STT model**: base.en everywhere, or small.en on M4? (lean: base.en everywhere for small default-download size; power users switch)
4. **Phase 4 shipping criteria**: what signal tells us to build TTS? (some concrete user feedback? explicit John decision?)
5. **`TELEGRAM_STT_CACHE_DIR` override** — useful, or over-engineered? (lean: include it, zero cost)
6. **Transcript confidence threshold**: show a warning reply when Whisper returns low-confidence garbage? (lean: defer — Phase 1 ships raw)

---

## 10. Risks and non-goals

### 10.1 Accepted risks

- **First-run model download (~148 MB base.en) on slow networks.** Minimum ~1 min; user sees progress bar in wizard.
- **whisper-rs build requires C++ toolchain + cmake.** Standard on every Mac/Linux dev box; documented in README.
- **`libonnxruntime` install for TTS is an extra user step.** Only on Phase 4; default-off flag.
- **Local STT accuracy on noisy inputs.** base.en with no VAD tuning may emit garbage on subway noise. Cloud providers (Groq) are a one-flag fallback.
- **Cache dir not encrypted.** Models are public; permissions 0700 is enough.
- **Voice-in latency for Claude permission prompts.** User records another "yes"; accepted UX cost.

### 10.2 Explicit non-goals

- Real-time streaming transcription (batch after EOM only).
- Speaker diarization (single-user tool).
- Multi-language auto-detect by default (English-first; `TELEGRAM_STT_LANGUAGE=` empty enables auto).
- On-device Apple `SFSpeechRecognizer` (Swift FFI cost too high).
- Voice cloning / custom Kokoro voices.
- VAD clipping (trust Telegram push-to-talk).
- Telegram video-note / circle-video support.
- Telegram Local Bot API server support (different URL shape).
- Windows support.

---

## Appendix A — Canonical URLs

| Asset | URL | SHA placeholder |
|---|---|---|
| `ggml-base.en.bin` | https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin | `TBD-PLACEHOLDER-base-en` |
| `ggml-small.en.bin` | https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.en.bin | `TBD-PLACEHOLDER-small-en` |
| `kokoro-v1.0.onnx` | https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/main/onnx/model.onnx | `TBD-PLACEHOLDER-kokoro-onnx` |
| `voices-v1.0.bin` | https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/main/voices.bin | `TBD-PLACEHOLDER-kokoro-voices` |

Each file must be `shasum -a 256`'d once post-download and the hex pasted into the manifest before Phase 0 ships.

---

## Sources

- [tazz4843/whisper-rs](https://github.com/tazz4843/whisper-rs) — v0.16.0; features metal, coreml, openblas; Linux builds "just work"
- [whisper-rs on docs.rs](https://docs.rs/crate/whisper-rs/latest)
- [ggerganov/whisper.cpp models on HF](https://huggingface.co/ggerganov/whisper.cpp/tree/main)
- [any-tts on crates.io](https://crates.io/crates/any-tts) — pure-Rust phonemizer, Kokoro via ort, MIT/Apache-2.0
- [onnx-community/Kokoro-82M-v1.0-ONNX](https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX)
- [ogg-opus on crates.io](https://crates.io/crates/ogg-opus) — 16 kHz mono PCM input/output
- [pdeljanov/Symphonia](https://github.com/pdeljanov/Symphonia) — pure Rust, Opus NOT supported yet
- [ort load-dynamic docs](https://ort.pyke.io/) — ONNX Runtime linking strategies
- [Groq Speech-to-Text](https://console.groq.com/docs/speech-to-text)
- [OpenAI audio API](https://platform.openai.com/docs/guides/speech-to-text)
- [Telegram Bot API](https://core.telegram.org/bots/api) — sendVoice, getFile, voice/audio types
- [ring::digest](https://docs.rs/ring/latest/ring/digest/) — SHA-256 already in tree via rustls

---

## Critical implementation files

- `<repo>/src/audio/mod.rs` (new) — `AudioSubsystem::new`
- `<repo>/src/audio/stt/local.rs` (new) — whisper-rs in-process wrapper
- `<repo>/src/audio/codec.rs` (new) — ogg-opus decode + encode
- `<repo>/src/audio/fetch.rs` (new) — SHA-verified download
- `<repo>/src/bridge/mod.rs` (modified) — `Payload` dispatch seam
- `<repo>/src/telegram/mod.rs` (modified) — get_file / download_file / send_voice
