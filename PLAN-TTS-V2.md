# PLAN-TTS-V2 — Cross-platform TTS (Remote + Local Kokoro)

Supersedes the TTS section of `PLAN-VOICE.md` and the Kokoro pieces of
`PLAN-KOKORO-TTS.md`. Current STT plan is unchanged.

**Status**: shipped. Phase 4b-v2 landed all three backends; the
follow-up `kokoro-local-full` branch (see `PLAN-KOKORO-LOCAL-FULL.md`)
brought the Kokoro local pipeline to quality parity with the reference
`Kokoro-FastAPI` deployment by adding text normalization (numbers,
currency, titles, years) and the E2M IPA substitution table (diphthong
merging, flap-T, rhotacization, tie-mark stripping). Release binary
with `--features kokoro` ships at ~5.9 MB.

Future work: Piper shell-out backend via `uv tool install piper-tts`
(a faster, lower-quality alternative) remains unbuilt; revisit if users
complain about Kokoro latency on low-end hardware.

## Why this design

The previous Phase 4b design tried to wrap third-party Rust Kokoro crates
(kokoroxide / kokoro-tiny / any-tts / kitten_tts_rs). All four failed for
different reasons — yanked deps, build bugs, banned transitive crates
(aws-lc-rs), and non-feature-gated server dependencies. Vendoring a
partial fork was considered and rejected: we'd own the drift without any
of the upsides.

The revised approach:

1. **Remote HTTP backend**. User's existing Kokoro-FastAPI (or any
   OpenAI-compatible TTS server) is reached via `POST /v1/audio/speech`.
   Zero new deps — uses the existing hyper/rustls stack. Ships in the
   default binary.
2. **Local Kokoro via direct ort**. Write our own ~500 LoC integration
   against `ort` + `ndarray`, with espeak-ng as phonemizer (shell-out,
   not linked — avoids LGPL transit). Feature-gated so the binary size
   penalty (~20 MB of ONNX Runtime static libs) is opt-in.
3. **macOS `say` stays as the zero-config default**. It's already
   working; lighter than Kokoro for users who don't care about quality.

## Platform matrix

Preference order (highest to lowest, for Simple mode defaulting):

| # | Backend | macOS | Linux | Quality | Install | Latency |
|---|---|---|---|---|---|---|
| 1 | `say` (macOS) | ✅ Simple default | — | ★★ | built-in | ~200 ms |
| 2 | `kokoro` (local) | ✅ Simple default if no `say` | ✅ Simple default | ★★★★ | espeak-ng + ~55 MB model | ~300-500 ms |
| 3 | `remote` (HTTP) | ✅ Advanced only | ✅ Advanced only | ★★★★★ | just a URL | network-dep |
| 4 | `none` | ✅ | ✅ | — | — | — |

**Simple mode never picks `remote`** — remote requires user-supplied
credentials and a URL, so Simple defaults to the backend that "just
works" for the platform: `say` on macOS (zero install), Kokoro local on
Linux (install espeak-ng + download model on first run).

## Setup wizard — Simple vs Advanced

The TTS step asks **Simple** or **Advanced** up front:

```
Voice replies

How should tebis reply with voice?
  1. Simple   — use the best-fit option for your platform        [default]
  2. Advanced — pick backend, voice, remote URL, etc.
  3. Skip     — text-only replies

Choice:
```

### Simple path (decides for the user)

Priority: `say` on macOS → Kokoro local everywhere else → skip.

- **macOS**: `say` backend with voice `Samantha`. Zero install, lightest
  option, native — no reason to ship a 55 MB ONNX model when the
  built-in works.
- **Linux**: Kokoro local with voice `af_bella`. Check for `espeak-ng`:
  - Present → done.
  - Absent → offer to install:
    - Detect package manager (apt / dnf / pacman / zypper / apk).
    - Always show the exact command before running it.
    - Ask: "Install espeak-ng via `sudo apt install -y espeak-ng`? [Y/n]"
    - On consent → run live with streamed output.
    - On decline or install failure → skip TTS (text-only). Offer to
      re-run setup later after manual install.

### Advanced path (full picker)

Backend picker order (Kokoro local is the primary recommendation on both
platforms — it's the best quality/privacy combo that still works offline):

```
  1. Kokoro (local)    — neural, offline, needs espeak-ng + ~55 MB model  [recommended]
  2. Kokoro (remote)   — point at your deployed Kokoro-FastAPI
  3. Say (macOS only)  — built-in, no install, lower quality            [macOS only]
  4. None              — text-only replies
```

- **Kokoro local**: probe espeak-ng, offer install if needed, voice
  picker from the manifest (defaults: af_bella, am_adam, bf_emma,
  bm_george).
- **Kokoro remote**: URL → API key (optional) → voice → **test
  connection** (synthesize "tebis setup test" against the URL, expect
  200 + audio bytes). Retry / change URL / skip on failure.
- **Say** (macOS only, hidden on Linux): voice picker from `say -v '?'`.

### Re-run behavior

Wizard re-run preserves the currently-selected backend's config by
default. Changing backends resets to that backend's defaults.

## Config surface

Env keys added / reshaped:

```
# Backend selector. Values: none, say, kokoro, remote. Default: none.
TELEGRAM_TTS_BACKEND=remote

# Voice name. Backend-specific interpretation:
#   say:     `say -v` voice (e.g. "Samantha")
#   kokoro:  manifest voice key (e.g. "af_bella")
#   remote:  voice param sent to the HTTP endpoint
TELEGRAM_TTS_VOICE=af_bella

# Whether to voice-reply to every message (true) or only to voice msgs (false).
TELEGRAM_TTS_RESPOND_TO_ALL=off

# Remote backend only:
TELEGRAM_TTS_REMOTE_URL=https://kokoro.example.com
TELEGRAM_TTS_REMOTE_API_KEY=...              # optional Bearer token
TELEGRAM_TTS_REMOTE_MODEL=kokoro             # optional, default "kokoro"
TELEGRAM_TTS_REMOTE_TIMEOUT_SEC=10           # optional, default 10
TELEGRAM_TTS_REMOTE_ALLOW_HTTP=false         # reject http:// by default

# Backwards-compat: TELEGRAM_TTS=on with no BACKEND is interpreted as
# the backend that was default before this change (say on macOS, none
# on Linux) to avoid breaking existing deployments.
```

## Runtime lifecycle

**Startup** (`AudioSubsystem::new`):
- `backend=none` → no TTS init.
- `backend=say` on macOS → probe `say -?`, fail-open on error.
- `backend=say` on Linux → config validation error at load time (bail).
- `backend=kokoro` → probe espeak-ng + load ONNX model + load voice.
  Any failure → fail-open (warn + continue text-only). Do not download
  the ONNX model at startup; lazy-load on first synth (Kokoro model is
  ~55 MB, startup shouldn't block on it unless the user already has it
  cached from a prior run).
- `backend=remote` → validate URL is parseable + https (or
  `ALLOW_HTTP=true` for opt-in LAN). **Do not** do a liveness check at
  startup; first synth will surface any problem. Rationale: the remote
  might be reachable on the user's VPN but not at boot.

**Per-synthesis**:
- `backend=say` → existing path, unchanged.
- `backend=kokoro` → phonemize (shell out to espeak-ng, ~10 ms) →
  tokenize (IPA → int array) → ort session run (~200-400 ms on M4) →
  PCM → codec::encode_pcm_to_opus.
- `backend=remote` → POST JSON → stream OGG/Opus response bytes. **No
  re-encode** — Kokoro-FastAPI returns audio/ogg already, which is
  exactly what Telegram wants for sendVoice.

**Failure handling**:
- Every backend failure logs a warn with a redacted error, bumps the
  `tts_failures` metric, and sends the reply as text-only. Voice failure
  never blocks text reply — preserves the existing invariant ("voice
  failure ≠ bridge failure").

**Dep loss mid-run** (e.g. user `apt remove espeak-ng` while bridge is
up): The kokoro backend caches its probe result at startup but re-runs
espeak-ng per synthesis. A failing shell-out surfaces as a synthesis
error; same fail-open path applies. No need for active health checks.

## Phonemizer install UX

### Detection order

```
macOS:  brew → fink → macports  (brew takes priority)
Linux:  apt-get → dnf → pacman → zypper → apk → emerge
         (order = distro popularity; first hit wins)
other:  print manual install instructions
```

### Auto-install rules

| Platform | Command | Sudo? | Run from wizard? |
|---|---|---|---|
| macOS brew | `brew install espeak-ng` | no | ✅ stream output, **always confirm** |
| Linux apt | `sudo apt install -y espeak-ng` | yes | ✅ confirm → run through sudo |
| Linux dnf | `sudo dnf install -y espeak-ng` | yes | ✅ confirm → run through sudo |
| Linux pacman | `sudo pacman -S --noconfirm espeak-ng` | yes | ✅ confirm → run through sudo |
| Linux zypper | `sudo zypper install -y espeak-ng` | yes | ✅ confirm → run through sudo |
| Linux apk | `sudo apk add espeak-ng` | yes | ✅ confirm → run through sudo |
| other | (print URL, user installs manually) | — | — |

**Always confirm before running any install.** User sees the exact
command the wizard will execute and says yes — no silent installs, even
for brew. Being a good citizen on someone else's machine means asking.

### Re-probe after install

After the package manager exits 0, we re-probe `espeak-ng --version` on
PATH. Mismatch (exit 0 but binary missing) → warn + skip TTS. This
catches distro oddities (e.g. binary named `espeak-ng-1` on some
older Debians).

## Security notes

- **Redact bot token + API key in all logs.** Reuse `redact_network_error`
  from `src/telegram/mod.rs` for the remote backend. Never log bearer
  tokens anywhere — not in `Debug`, not in error chains.
- **Enforce https by default.** Reject `http://` URLs at config load
  unless `TELEGRAM_TTS_REMOTE_ALLOW_HTTP=true`. Documented escape
  hatch for LAN / self-hosted kokoro-fastapi deployments.
- **Byte cap on remote response.** 10 MB max. Reject larger responses
  with a clear error. Prevents a misbehaving remote from OOMing tebis.
- **Timeout bound.** 10 s default, configurable via
  `TELEGRAM_TTS_REMOTE_TIMEOUT_SEC`. One retry on 5xx / connect error.
- **Hash voices.npz + kokoro.onnx** in the manifest (same SHA-256
  pinning we use for whisper models). Models downloaded from HF are
  CDN-served — pinning defends against a CDN MITM or a repo takeover.

## Code layout

```
src/audio/tts/
├── mod.rs              Backend::{Say, Kokoro, Remote, None}, dispatch
├── say.rs              unchanged
├── remote.rs           NEW — OpenAI-compatible HTTP client
└── kokoro/             NEW — feature-gated `kokoro`
    ├── mod.rs          KokoroTts (loads model, owns voices)
    ├── phonemize.rs    espeak-ng shell-out + IPA parse
    ├── tokens.rs       IPA → Kokoro token ID (static table)
    └── voices.rs       .bin voice embedding loader (styled vectors)

src/setup/
├── steps.rs            step_tts reworked (Simple/Advanced)
└── phonemizer.rs       NEW — probe + pkg manager + auto-install
```

## Feature flags

```toml
[features]
default = []                  # all baseline TTS (say, remote) always built
kokoro = ["dep:ort", "dep:ndarray"]   # opt-in local Kokoro

[dependencies]
ort = {version = "2", optional = true, default-features = false, features = ["download-binaries"]}
ndarray = {version = "0.16", optional = true}
```

Binary size impact:
- Without `kokoro`: ~4.3 MB (same as today + remote.rs ~3 KB).
- With `kokoro`: ~25 MB (ort static libs dominate).

Default stays feature-less to preserve the small-binary story for
remote-only and say-only users. Release process publishes two binaries:
`tebis` (default) and `tebis-kokoro` (with feature).

## Manifest additions

New entries under `tts_models`:

```json
{
  "tts_models": {
    "kokoro-v1.0-q4f16": {
      "display_name": "Kokoro v1.0 (q4f16, ~55 MB)",
      "default": true,
      "model": {
        "url": "https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/main/onnx/model_q4f16.onnx",
        "sha256": "<pin>",
        "size_bytes": 57000000
      },
      "voices": {
        "af_bella": {
          "url": "https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/main/voices/af_bella.bin",
          "sha256": "<pin>",
          "size_bytes": 525000
        },
        "am_adam":   { ... },
        "bf_emma":   { ... },
        "bm_george": { ... }
      }
    }
  }
}
```

## Dashboard updates

- **TTS backend** row shows one of:
  - `Say (macOS, voice=Samantha)`
  - `Kokoro local (q4f16, voice=af_bella, espeak-ng 1.51.1)`
  - `Remote (host=kokoro.[redacted], voice=af_bella, auth=set|unset)`
  - `None`
- **TTS latency** row: per-backend breakdown
  - `Kokoro: phonemize 12ms / synth 340ms (avg last 10)`
  - `Remote: 480ms (avg last 10)`
- **TTS failures** row: counter + last error string (redacted)

## Metrics

New atomic counters on `Metrics`:

```rust
pub last_phonemize_ms: AtomicU32,
pub last_synth_ms: AtomicU32,
pub last_remote_request_ms: AtomicU32,
pub tts_backend_errors: AtomicU64,
```

Existing `tts_success` / `tts_failures` / `last_stt_duration_ms` stay.

## Execution order

1. **PLAN-TTS-V2.md** ← this file (done when you're reading it).
2. **TtsConfig enum** in config.rs — sets the shape for everything else.
3. **Remote backend** — smallest, uses existing deps, no platform branches.
4. **Phonemizer probe + installer** — standalone, testable.
5. **Kokoro local scaffolding** — the big one, feature-gated.
6. **Wizard rework** — depends on all of the above.
7. **Manifest + SHA pinning** — last, needs real model URLs + downloads.
8. **Dashboard + metrics** — cosmetic, independent.
9. **Tests** — unit tests alongside each piece; integration tests at end.
10. **Final gate** — cargo build/test/clippy/deny all green.

## Open trade-offs we're accepting

- **ort is big** (20+ MB of static libs). Accepted by making `kokoro`
  feature opt-in; users who only want remote or say get a small binary.
- **espeak-ng shell-out is slow-ish** (~10 ms overhead per call).
  Acceptable against 300 ms total synth. If it ever becomes the
  bottleneck, we can switch to the `espeak-ng-sys` dynamic-link path
  at the cost of LGPL complexity.
- **Model download on first synth (lazy)**, not at startup. Trade-off:
  first voice reply is slow (2-5 s on fresh install), but startup stays
  fast. Users see the download happen via a typing indicator.
- **Remote doesn't do liveness check at startup.** Startup stays fast
  and VPN-independent, at the cost of not noticing config errors until
  the first voice synth. Acceptable.

## Non-goals

- Windows support. Not today. Add if someone asks.
- Testing against OpenAI's hosted TTS. The `remote` backend is
  OpenAI-compatible incidentally — if someone points it at OpenAI it'll
  work, but we don't test against `api.openai.com` (paid + they
  throttle). Tests target Kokoro-FastAPI shape only.
- Cloud TTS providers built into our code (OpenAI, Groq, ElevenLabs).
  No hardcoded cloud integrations; `remote` points at whatever URL the
  user configures.
- Streaming TTS (token-by-token audio). Kokoro-FastAPI supports SSE
  but Telegram's sendVoice is one-shot, so streaming buys us nothing.
- Cross-language voices. English-only for v1. Kokoro supports more
  but our phoneme tables only cover en-us. Add later if demand exists.
