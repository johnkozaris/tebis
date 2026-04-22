# PLAN-TTS-REFACTOR

Branch: `tts-refactor` off `voice-bridge`.

## Goals

1. **Decouple Kokoro local TTS** into its own Cargo crate
   (`crates/tebis-tts-kokoro/`) so dropping it becomes a one-line change
   in tebis's `Cargo.toml` — not a large code diff.
2. **Split god files** (anything >800 LoC). Current offenders:
   - `src/setup/steps.rs` at 902 LoC — wizard is one module per step.
   - `src/inspect/render.rs` at 867 LoC — renderer is one module per
     concern (voice, sessions, metrics, JSON, CSS).
3. **SOLID cleanup** — enforce single-responsibility on the modules
   above; keep the `Tts` trait as the stable boundary.
4. **Apply non-blocking code-review findings** that were deferred when
   we merged to `voice-bridge`:
   - `bytemuck::cast_slice` for voice-file parse (voices.rs)
   - Single-pass E2M replacer (optional, defer if complex)
   - `strip_html_for_tts` roundtrip test
   - Document shutdown latency on the Kokoro session mutex
   - Document `RemoteTts::new` ownership of https-only enforcement

## Crate layout

```
tebis/
├── Cargo.toml                        # workspace root + main [package]
├── src/                              # main tebis binary (trimmed)
│   └── audio/tts/
│       ├── mod.rs                    # Tts trait, Synthesis, Backend enum
│       ├── say.rs                    # macOS `say` — stays here
│       ├── remote.rs                 # HTTP — stays here (uses tebis hyper)
│       └── kokoro.rs                 # NEW: thin adapter over the crate
└── crates/
    └── tebis-tts-kokoro/             # NEW
        ├── Cargo.toml                # deps: ort, ndarray, num2words, regex
        └── src/
            ├── lib.rs                # pub API: KokoroTts, KokoroError
            ├── session.rs            # ort Session lifecycle + dtype detect
            ├── phonemize.rs          # espeak-ng shell-out
            ├── normalize.rs          # text preprocess
            ├── tokens.rs             # sparse IPA vocab
            ├── e2m.rs                # post-espeak fixups
            └── voices.rs             # .bin f32 loader
```

**Crate contract**:
- Input: paths to `model.onnx` + `voices_dir`, then `(text, voice_name)`
- Output: `(Vec<f32> @ 24 kHz, duration_ms)`
- No network, no SHA verification, no tebis coupling. tebis handles
  download + manifest + dispatch.

This keeps `tebis-tts-kokoro` self-contained — somebody could literally
`cargo add tebis-tts-kokoro` in an unrelated project and get working
Kokoro synthesis.

## God-file splits

### `src/setup/steps.rs` (902 → ~120 per file)

```
src/setup/steps/
├── mod.rs          # re-exports of step fns + shared imports
├── bot_token.rs    # step_bot_token
├── user_id.rs      # step_user_id
├── sessions.rs     # step_session_allowlist
├── autostart.rs    # step_autostart
├── hooks.rs        # step_hooks_mode
├── inspect.rs      # step_inspect_port
├── voice.rs        # step_voice (STT)
├── tts.rs          # step_tts + Simple/Advanced sub-flows
└── validators.rs   # bot_token + session_list validators + tests
```

### `src/inspect/render.rs` (867 → ~200 per file)

```
src/inspect/render/
├── mod.rs          # top-level page assembly + exports
├── voice.rs        # STT + TTS rows
├── sessions.rs     # live-session list + kill buttons
├── metrics.rs      # counter table
├── json.rs         # JSON API response
└── css.rs          # inline stylesheet (const string)
```

## SOLID checklist

- [x] Single responsibility: each file above owns one concern
- [x] Open/closed: adding a fourth backend (e.g. Piper) = new variant +
      new module, no edits to existing backend code
- [x] Dependency inversion: tebis depends on the `Tts` trait in
      `src/audio/tts/mod.rs`, not on concrete `KokoroTts` — the adapter
      in `kokoro.rs` bridges to the crate

## Execution order

1. [ ] Convert root to workspace layout (keep `tebis` at root)
2. [ ] Create `crates/tebis-tts-kokoro/` — Cargo.toml + files moved
3. [ ] Thin adapter `src/audio/tts/kokoro.rs`; wire into Backend enum
4. [ ] Split `setup/steps.rs` into `setup/steps/`
5. [ ] Split `inspect/render.rs` into `inspect/render/`
6. [ ] Apply review follow-ups (bytemuck, tests, docs)
7. [ ] `cargo test` / `cargo clippy -D warnings` / `cargo deny check`
      green on both default and `--features kokoro`
8. [ ] Merge `tts-refactor` → `voice-bridge`

## Out of scope

- Splitting `src/audio/mod.rs` (662) — under 800, can defer.
- Splitting `src/main.rs` (694) — under 800, can defer.
- Splitting `src/telegram/mod.rs` (765) — borderline; revisit if it
  grows in this work.
- Decoupling STT (whisper-rs) into its own crate — symmetry would be
  nice but out of scope; not requested.
