//! Post-espeak IPA substitutions ("E2M" table for Kokoro v1).
//!
//! Runs after `espeak-ng --ipa=3` and before the vocab filter. Without
//! these, Kokoro sees `a͡ɪ` where it expects `I` (its diphthong merge
//! marker), dental `r` where it wants rhotic `ɹ`, raw flap-T where
//! it wants `T`, etc. — produces flat, unnatural prosody.
//!
//! Table cross-referenced with `misaki/espeak.py::EspeakFallback.E2M`
//! and `Kokoro-FastAPI/api/src/services/text_processing/phonemizer.py`.

/// E2M substitution rules.
///
/// Order is **load-bearing**: multi-character patterns must come first so
/// they match before their constituent characters get remapped by later
/// single-character rules. Specifically:
///   1. Literal word overrides (e.g. the "kokoro" pronunciation fix)
///   2. Three-char diphthongs (`a + tie + ɪ` etc.) — combining-tie form
///   3. Three-char diphthongs using ASCII caret (some espeak flags)
///   4. Two-char ligatures (`d + tie + ʒ` etc.)
///   5. Two-char rhotacization (`ɜː`, `ɚ`)
///   6. Single-char substitutions (`r→ɹ`, `x→k`, etc.)
///   7. Single-char drops (`ɬ`→`l`, `ɾ`→`T`, `ʔ`→`t`)
///   8. Strip any remaining tie / tilde combining marks
///
/// Changing the order will silently corrupt output — the substitution
/// engine is a string `replace` chain, not a regex alternation, so each
/// pass sees the state after every earlier pass. Add new rules at the
/// correct position, not the end.
const E2M_RULES: &[(&str, &str)] = &[
    // ---- 1. Word-level overrides ----
    // espeak pronounces "kokoro" using dental `r`, not the rhotic `ɹ`
    // Kokoro's training vocab expects. Replace the whole word at raw
    // espeak level — the output side already uses `ɹ` so later `r→ɹ`
    // passes don't disturb it. Tilde marks and ties are already gone
    // by the time we reach this string; we match against the form
    // espeak-ng actually emits for "kokoro".
    ("kəkˈoːroʊ", "kˈoʊkəɹoʊ"),

    // ---- 2. Diphthong merges with combining tie (U+0361) ----
    // These are the most common forms from `espeak-ng --ipa=3`.
    ("a\u{0361}ɪ", "I"),
    ("e\u{0361}ɪ", "A"),
    ("o\u{0361}ʊ", "O"),
    ("ɔ\u{0361}ɪ", "Y"),
    ("a\u{0361}ʊ", "W"),

    // ---- 3. Same diphthongs using ASCII caret ----
    // Some espeak-ng builds / flag combos emit `a^ɪ` instead of the
    // combining form. Handle both for portability.
    ("a^ɪ", "I"),
    ("e^ɪ", "A"),
    ("o^ʊ", "O"),
    ("ɔ^ɪ", "Y"),
    ("a^ʊ", "W"),

    // ---- 4. Ligature merges (both tie forms) ----
    ("d\u{0361}ʒ", "ʤ"),
    ("t\u{0361}ʃ", "ʧ"),
    ("d^ʒ", "ʤ"),
    ("t^ʃ", "ʧ"),

    // ---- 5. Rhotacization (must match before `r→ɹ` and `ː` drop) ----
    ("ɜː", "ɜɹ"),
    ("ɚ", "əɹ"),

    // ---- 6. Single-char substitutions ----
    // `r → ɹ` is the biggest win: espeak emits the dental `r` while
    // Kokoro wants the alveolar approximant `ɹ`.
    ("r", "ɹ"),
    // `ɐ → ə` — near-open central vowel collapses to schwa.
    ("ɐ", "ə"),
    // `x`, `ç` → `k` — velar fricatives aren't in Kokoro's English vocab.
    ("x", "k"),
    ("ç", "k"),
    // `ʲ → j` — palatalization becomes a y-glide.
    ("ʲ", "j"),
    // `ɬ → l` — lateral fricative folds to plain L.
    ("ɬ", "l"),

    // ---- 7. Flap-T and glottal stop (Kokoro v1 vocab specifics) ----
    // `ɾ → T` — American English flap-T (as in "butter") uses the
    //   capital-T merge marker in Kokoro's training vocab.
    ("ɾ", "T"),
    // `ʔ → t` — glottal stop folds back to `t`.
    ("ʔ", "t"),

    // ---- 8. Strip remaining tie / tilde combining marks ----
    // Any tie / tilde that survived the above patterns would otherwise
    // leak into the vocab filter (where they'd be dropped — but we
    // prefer to strip cleanly here so debug output is readable).
    ("\u{0361}", ""),
    ("\u{0303}", ""),
    ("^", ""),
];

/// Apply the full E2M substitution table to an IPA string, in order.
/// Single-pass: each rule's replacement is visible to every later rule.
///
/// Allocates a fresh `String` once per rule that matches. For assistant
/// reply sizes (bounded at 4000 bytes pre-truncation) the allocation
/// overhead is in the single-digit microseconds — negligible next to
/// ~300 ms of ort inference.
#[must_use]
pub fn apply_e2m(ipa: &str) -> String {
    let mut s = ipa.to_string();
    for (pat, rep) in E2M_RULES {
        if s.contains(pat) {
            s = s.replace(pat, rep);
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diphthong_combining_tie_merges() {
        // "price" phoneme "pɹa͡ɪs" → "pɹIs"
        assert_eq!(apply_e2m("pɹa\u{0361}ɪs"), "pɹIs");
        // "face" → "feA s"-ish
        assert_eq!(apply_e2m("fe\u{0361}ɪs"), "fAs");
        // "goat" — note plain `t` stays `t`; `T` is only for flap-T `ɾ`.
        assert_eq!(apply_e2m("ɡo\u{0361}ʊt"), "ɡOt");
    }

    #[test]
    fn diphthong_ascii_caret_merges() {
        assert_eq!(apply_e2m("pɹa^ɪs"), "pɹIs");
        assert_eq!(apply_e2m("fe^ɪs"), "fAs");
    }

    #[test]
    fn ligatures_merge() {
        // "judge" → ʤʌʤ
        assert_eq!(apply_e2m("d\u{0361}ʒʌd\u{0361}ʒ"), "ʤʌʤ");
        // "church" → ʧɜɹʧ  (ɜː→ɜɹ also exercised)
        assert_eq!(apply_e2m("t\u{0361}ʃɜːt\u{0361}ʃ"), "ʧɜɹʧ");
    }

    #[test]
    fn rhotacization_replaces_before_single_r() {
        // ɚ → əɹ — must fire before `r→ɹ`, otherwise the synthesized
        // ɹ would get left unchanged (which is actually fine because
        // we don't remap ɹ further), but the multi-char match has to
        // win vs. the single-char `r` rule.
        // "butter" in US English: the espeak US-flap form `bəɾɚ` → bəTəɹ.
        // Plain `t` (RP form `bətə`) wouldn't become `T`.
        assert_eq!(apply_e2m("bəɾɚ"), "bəTəɹ");
        assert_eq!(apply_e2m("fɜː"), "fɜɹ"); // "fur"
    }

    #[test]
    fn single_char_r_to_rhotic() {
        // Vanilla "red" from espeak.
        assert_eq!(apply_e2m("rɛd"), "ɹɛd");
    }

    #[test]
    fn velar_fricative_folds_to_k() {
        // "loch" / "Bach"-ish — espeak emits x or ç which aren't in vocab.
        assert_eq!(apply_e2m("lɒx"), "lɒk");
        assert_eq!(apply_e2m("bɑːç"), "bɑːk");
    }

    #[test]
    fn flap_t_becomes_capital_t() {
        // "water" → wˈɔːtɚ in RP, but US flap-T rendering is wˈɑːɾɚ.
        assert_eq!(apply_e2m("wˈɑːɾɚ"), "wˈɑːTəɹ");
    }

    #[test]
    fn glottal_stop_becomes_t() {
        // "button" in some US dialects → bˈʌʔən.
        assert_eq!(apply_e2m("bˈʌʔən"), "bˈʌtən");
    }

    #[test]
    fn strip_remaining_combining_marks() {
        // A tie that didn't pair with a known diphthong still gets stripped.
        assert_eq!(apply_e2m("foo\u{0361}bar"), "foobaɹ");
        // A nasal tilde likewise.
        assert_eq!(apply_e2m("a\u{0303}"), "a");
    }

    #[test]
    fn kokoro_word_override() {
        // The project-name override: espeak mis-pronounces "kokoro".
        // Input is the raw-espeak form (dental `r`); override emits
        // the rhotic form (`ɹ`) so later `r→ɹ` rules don't disturb it.
        assert_eq!(apply_e2m("kəkˈoːroʊ"), "kˈoʊkəɹoʊ");
    }

    #[test]
    fn kokoro_override_runs_before_r_substitution() {
        // Raw espeak input — dental `r`. The override runs first and
        // replaces the full word with the rhotic form; then the later
        // `r → ɹ` pass is a no-op on what's already been remapped.
        let input = "sˈɛnd məssˈɐdʒ tɒ kəkˈoːroʊ";
        let out = apply_e2m(input);
        assert!(out.contains("kˈoʊkəɹoʊ"), "got: {out}");
    }

    #[test]
    fn idempotent_on_already_fixed_input() {
        // Applying twice should match applying once — no rule
        // accidentally re-matches its own output.
        let once = apply_e2m("pɹa\u{0361}ɪs wˈɑːɾɚ");
        let twice = apply_e2m(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn empty_input_returns_empty() {
        assert_eq!(apply_e2m(""), "");
    }

    #[test]
    fn all_rules_can_fire_in_one_pass() {
        // Dense input hitting many rules at once — sanity that we don't
        // corrupt on cascading matches.
        let raw =
            "a\u{0361}ɪ e^ɪ d\u{0361}ʒ ɜː ɚ r ɐ x ç ʲ ɬ ɾ ʔ\u{0361}\u{0303}^";
        let out = apply_e2m(raw);
        // After all rules: I A ʤ ɜɹ əɹ ɹ ə k k j l T t  (plus stripped ties/tildes)
        // Whitespace survives all rules (no rule touches space).
        assert_eq!(out, "I A ʤ ɜɹ əɹ ɹ ə k k j l T t");
    }
}
