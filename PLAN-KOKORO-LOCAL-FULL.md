# PLAN-KOKORO-LOCAL-FULL

Branch: `kokoro-local-full` off `voice-bridge`.

Goal: close the quality gap between tebis's local Kokoro backend and the
reference Kokoro-FastAPI deployment, so the self-hosted path produces
intelligible, well-prosodied speech instead of the current robotic
bare-espeak-ng output.

Scope: **text-preprocessing + post-espeak IPA fixups only**. The
inference path (ort session, voice loading, 24→16 kHz resample) was
already wired up in the previous phase and works as intended — this
phase is about feeding Kokoro phonemes that match what it was trained
on, instead of raw espeak output the model never saw.

Sources of truth:
- `kokoro-onnx/src/kokoro_onnx/tokenizer.py` (canonical ONNX reference)
- `Kokoro-FastAPI/api/src/services/text_processing/normalizer.py`
  (what the user's remote deployment runs today)
- `Kokoro-FastAPI/api/src/services/text_processing/phonemizer.py`
  (E2M substitution rules applied post-espeak)
- `misaki/espeak.py::EspeakFallback.E2M` (the original ~40-rule table,
  our cross-reference for completeness)

---

## Already shipped — do NOT re-litigate

These landed earlier in `voice-bridge` and are staying:

- `Backend::Kokoro(Box<KokoroTts>)` variant (`src/audio/tts/mod.rs`)
- `TtsConfig::KokoroLocal { model, voice }` env parsing (`src/config.rs`)
- `build_kokoro_local` helper with SHA-verified model + voice download
  and spawn-blocking ort session load (`src/audio/mod.rs`)
- Wizard Simple/Advanced picker + phonemizer probe (`src/setup/`)
- Dashboard backend-kind + detail display (`src/inspect/`)
- Manifest entry for `kokoro-v1.0` with real pinned SHAs
- `src/audio/tts/kokoro/{mod,tokens,voices,resample}.rs`

Current failing piece: `phonemize.rs` calls `espeak-ng` and returns the
raw IPA straight to the vocab filter. Kokoro was trained on misaki's
transformed IPA, so raw espeak output misses most diphthong markers
and flap-T, which is why the current audio sounds "off."

---

## New files this branch ships

### 1. `src/audio/tts/kokoro/e2m.rs` (~80 LoC)

Static IPA substitution table applied **after** espeak-ng and **before**
the vocab filter. Rules lifted directly from two mutually-consistent
upstreams (misaki + Kokoro-FastAPI's phonemizer):

| Input | Output | Reason |
|---|---|---|
| `a͡ɪ` (or `a^ɪ`) | `I` | long "eye" diphthong → merge marker |
| `e͡ɪ` | `A` | long "a" |
| `o͡ʊ` | `O` | long "o" |
| `ɔ͡ɪ` | `Y` | "oi" |
| `a͡ʊ` | `W` | "ow" |
| `d͡ʒ` | `ʤ` | "j" ligature |
| `t͡ʃ` | `ʧ` | "ch" ligature |
| `ɚ` | `əɹ` | r-colored schwa |
| `ɜː` | `ɜɹ` | nurse vowel |
| `r` | `ɹ` | IPA rhotic |
| `ɐ` | `ə` | schwa substitution |
| `x`, `ç` | `k` | velar fricative folding |
| `ʲ` | `j` | palatalization → y-glide |
| `ɬ` | `l` | lateral fricative fold |
| `ɾ` | `T` | flap-T (Kokoro v1 only) |
| `ʔ` | `t` | glottal stop → t (Kokoro v1 only) |
| `͡` (U+0361) | "" | strip tie-bar (combining) |
| `̃` (U+0303) | "" | strip nasal tilde |
| literal `^` | "" | strip ASCII tie marker (some espeak outputs) |

Plus the Kokoro-specific override (from Kokoro-FastAPI's phonemizer.py):
- `kəkˈoːɹoʊ` → `kˈoʊkəɹoʊ` (the word "kokoro" itself)

Implementation: ordered `[(&str, &str)]` table + `apply_e2m(ipa: &str) -> String`.
Order matters (replace multi-char ligatures before single-char remaps).

### 2. `src/audio/tts/kokoro/normalize.rs` (~200 LoC)

Text preprocessing applied **before** espeak-ng. Focused on the cases
that most degrade assistant-reply audio quality:

- **Cardinals + ordinals + years**: `42` → "forty-two", `2024` → "twenty
  twenty-four", `1st` → "first", `3rd` → "third". Via `num2words`
  crate (MIT, Apache-2.0 dual).
- **Decimals**: `3.14` → "three point one four"
- **Currency**: `$3.50` → "three dollars and fifty cents", `$42` →
  "forty-two dollars"
- **Titles**: `Dr.` → "doctor", `Mr.` → "mister", `Mrs.` → "missus",
  `Ms.` → "miss", `vs.` → "versus"
- **Percent**: `50%` → "fifty percent"
- **Punctuation tidy-up**: collapse runs of whitespace, strip control
  chars (belt-and-suspenders; sanitizer in bridge already does this)

Explicitly **NOT** implemented in this pass (deferred — low value for
assistant replies):
- URL / email parsing (markdown-stripped replies rarely contain them raw)
- Date parsing (tebis messages almost never have ambiguous dates)
- Phone-number grouping
- Unit normalization (kg, lb, °F)

### 3. Update `src/audio/tts/kokoro/phonemize.rs`

Pipeline becomes:

```
text
  → normalize::preprocess(text)          [new]
  → espeak-ng -v en-us -q --ipa=3
  → e2m::apply_e2m(raw_ipa)              [new]
  → tokens::ipa_to_token_ids             [already exists]
```

`phonemize()` now returns an `e2m`-fixed IPA string; the caller in
`kokoro::mod::synthesize_blocking` is unchanged.

---

## Dependencies added

| Crate | Version | License | Justification |
|---|---|---|---|
| `num2words` | `1` | MIT OR Apache-2.0 | Saves ~150 LoC of English cardinal/ordinal/year conversion + handles a pile of edge cases (hundred/thousand boundaries, hyphenation rules for 21–99) |

`regex` is already transitive via `tracing-subscriber` and other deps —
check `cargo tree`, promote to direct dep if needed.

Both feature-gated behind `kokoro` so default builds stay as-is.

---

## Execution order

### Phase 1 — branch setup ✓
- [x] `git checkout -b kokoro-local-full`
- [x] Write this plan file

### Phase 2 — E2M substitution table ✓
- [x] `src/audio/tts/kokoro/e2m.rs` with the full rule table (30 rules)
- [x] 14 unit tests covering every rule category + composite + idempotence
- [x] Module wired into `kokoro/mod.rs`

### Phase 3 — text normalization ✓
- [x] Added `num2words = "1"` optional dep under `kokoro` feature
- [x] Promoted `regex = "1"` from transitive to direct dep
- [x] `src/audio/tts/kokoro/normalize.rs` with 100-ish LoC + 18 tests
- [x] Custom year reader (num2words 1.x doesn't honor `prefer("year")`)
      — handles 1XXX, 2000, 200X, 20XX forms correctly

### Phase 4 — phonemize.rs integration ✓
- [x] `phonemize()` chains `normalize → espeak → e2m`
- [x] Module + crate docs updated to document the pipeline
- [x] Integration test (`#[ignore]` — needs espeak-ng) exercising the
      full normalize + espeak + E2M flow on a year + word input

### Phase 5 — smoke path ✓
- [x] `examples/kokoro-smoke.rs` — probes espeak-ng, loads
      AudioSubsystem with `kokoro-local` backend, synthesizes three
      increasingly-hard inputs, writes `/tmp/tebis-kokoro-smoke-<n>.oga`
- [x] Non-feature stub that exits 2 with install instructions
- [x] Exposed `setup::phonemizer` as `pub` so examples can call it

### Phase 6 — verification gate ✓
- [x] `cargo test` (default) — 252 passed
- [x] `cargo test --features kokoro` — 303 passed (+51 from new modules)
- [x] `cargo clippy --all-targets -- -D warnings` — clean
- [x] `cargo clippy --features kokoro --all-targets -- -D warnings` — clean
- [x] `cargo deny check` with `all-features = true` — advisories / bans /
      licenses / sources all ok
- [x] Release binary `+kokoro`: 5.9 MB (was 5.6 — num2words + regex
      add ~300 KB, well within budget)
- [ ] **Manual listening test** — requires `brew install espeak-ng
      onnxruntime` + running `cargo run --release --features kokoro
      --example kokoro-smoke` and listening to the OGA files. User-side
      validation only.

### Phase 7 — doc + PR prep
- [x] This plan file updated with final status
- [ ] Update `PLAN-TTS-V2.md` status section (Kokoro local now fully
      implemented; Piper is a future-work note)
- [ ] README: replace the "Kokoro local scaffolding" paragraph with the
      install-and-pick-it-in-setup instructions
- [ ] Draft PR description

---

## Out-of-scope for this branch

Kept as future work, not blockers for merging:

- Full currency/date/URL normalization (the Kokoro-FastAPI normalizer
  is ~500 LoC in Python; we port the 150 LoC that matters).
- Gold-dictionary OOV handling (misaki-rs showed this costs 34 MB for
  marginal quality — deferred).
- q4f16 manifest entry (cuts 346→55 MB download; we keep fp32 for now
  because the SHA is already pinned).
- libonnxruntime auto-install in the wizard (parallels the espeak-ng
  install flow we already ship).
- Kokoro chunking for > 510-token text (current MAX_OUTPUT_CHARS of
  4000 keeps real replies under the limit).

---

## Verdict

This is the plan. No more option-comparisons, no more research detours.
Two new files, one dep added, ~280 LoC total. Output quality goes from
"robotic and numerically mangled" to "matches user's remote Kokoro-FastAPI
deployment."
