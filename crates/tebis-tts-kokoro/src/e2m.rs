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
    // espeak pronounces "kokoro" using dental `r`, not the rhotic `ɹ`
    // Kokoro's training vocab expects. Replace the whole word at raw
    // espeak level — the output side already uses `ɹ` so later `r→ɹ`
    // passes don't disturb it. Tilde marks and ties are already gone
    // by the time we reach this string; we match against the form
    // espeak-ng actually emits for "kokoro".
    ("kəkˈoːroʊ", "kˈoʊkəɹoʊ"),
    // Most common diphthong form from `espeak-ng --ipa=3`.
    ("a\u{0361}ɪ", "I"),
    ("e\u{0361}ɪ", "A"),
    ("o\u{0361}ʊ", "O"),
    ("ɔ\u{0361}ɪ", "Y"),
    ("a\u{0361}ʊ", "W"),
    // Some espeak-ng builds / flag combos emit `a^ɪ` instead of the
    // combining form. Handle both for portability.
    ("a^ɪ", "I"),
    ("e^ɪ", "A"),
    ("o^ʊ", "O"),
    ("ɔ^ɪ", "Y"),
    ("a^ʊ", "W"),
    ("d\u{0361}ʒ", "ʤ"),
    ("t\u{0361}ʃ", "ʧ"),
    ("d^ʒ", "ʤ"),
    ("t^ʃ", "ʧ"),
    // Must match before `r→ɹ` and `ː` drop.
    ("ɜː", "ɜɹ"),
    ("ɚ", "əɹ"),
    ("r", "ɹ"),
    ("ɐ", "ə"),
    // Velar fricatives aren't in Kokoro's English vocab.
    ("x", "k"),
    ("ç", "k"),
    ("ʲ", "j"),
    ("ɬ", "l"),
    // American English flap-T uses capital-T in Kokoro's training vocab.
    ("ɾ", "T"),
    ("ʔ", "t"),
    // Any tie / tilde that survived the above patterns would otherwise
    // leak into the vocab filter (where they'd be dropped — but we
    // prefer to strip cleanly here so debug output is readable).
    ("\u{0361}", ""),
    ("\u{0303}", ""),
    ("^", ""),
];

/// Single-pass: each rule's replacement is visible to every later rule.
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
        assert_eq!(apply_e2m("pɹa\u{0361}ɪs"), "pɹIs");
        assert_eq!(apply_e2m("fe\u{0361}ɪs"), "fAs");
        assert_eq!(apply_e2m("ɡo\u{0361}ʊt"), "ɡOt");
    }

    #[test]
    fn diphthong_ascii_caret_merges() {
        assert_eq!(apply_e2m("pɹa^ɪs"), "pɹIs");
        assert_eq!(apply_e2m("fe^ɪs"), "fAs");
    }

    #[test]
    fn ligatures_merge() {
        assert_eq!(apply_e2m("d\u{0361}ʒʌd\u{0361}ʒ"), "ʤʌʤ");
        assert_eq!(apply_e2m("t\u{0361}ʃɜːt\u{0361}ʃ"), "ʧɜɹʧ");
    }

    #[test]
    fn rhotacization_replaces_before_single_r() {
        assert_eq!(apply_e2m("bəɾɚ"), "bəTəɹ");
        assert_eq!(apply_e2m("fɜː"), "fɜɹ");
    }

    #[test]
    fn single_char_r_to_rhotic() {
        assert_eq!(apply_e2m("rɛd"), "ɹɛd");
    }

    #[test]
    fn velar_fricative_folds_to_k() {
        assert_eq!(apply_e2m("lɒx"), "lɒk");
        assert_eq!(apply_e2m("bɑːç"), "bɑːk");
    }

    #[test]
    fn flap_t_becomes_capital_t() {
        assert_eq!(apply_e2m("wˈɑːɾɚ"), "wˈɑːTəɹ");
    }

    #[test]
    fn glottal_stop_becomes_t() {
        assert_eq!(apply_e2m("bˈʌʔən"), "bˈʌtən");
    }

    #[test]
    fn strip_remaining_combining_marks() {
        assert_eq!(apply_e2m("foo\u{0361}bar"), "foobaɹ");
        assert_eq!(apply_e2m("a\u{0303}"), "a");
    }

    #[test]
    fn kokoro_word_override() {
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
        let raw = "a\u{0361}ɪ e^ɪ d\u{0361}ʒ ɜː ɚ r ɐ x ç ʲ ɬ ɾ ʔ\u{0361}\u{0303}^";
        let out = apply_e2m(raw);
        assert_eq!(out, "I A ʤ ɜɹ əɹ ɹ ə k k j l T t");
    }
}
