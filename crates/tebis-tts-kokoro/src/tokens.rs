//! Kokoro v1.0 IPA-to-token-id vocabulary.
//!
//! Source of truth: `config.json` in the `onnx-community/Kokoro-82M-v1.0-ONNX`
//! HuggingFace repo — 114 entries, sparse integer IDs up to 177. kokoro-onnx's
//! `tokenizer.py` (the canonical Python ONNX reference we modeled after)
//! uses exactly this map.
//!
//! IDs are *sparse* (gaps are intentional — the training vocab had more
//! slots than English uses). That rules out a dense `[i64; 256]` array:
//! we need a `HashMap<char, i64>` so unknown chars just miss the lookup.
//!
//! Unknown chars are dropped at tokenize time, mirroring
//! `phonemes = "".join(filter(lambda p: p in self.vocab, phonemes))`
//! from `kokoro_onnx/tokenizer.py:78`. This is how kokoro-onnx tolerates
//! punctuation and whitespace noise from espeak-ng.

use std::collections::HashMap;
use std::sync::OnceLock;

/// Hard cap from the model export — `input_ids` is `(1, <=512)` with two
/// boundary pads, so the useful phoneme budget is 510.
pub const MAX_PHONEMES: usize = 510;

/// (IPA char, token id) pairs. 114 entries, copied verbatim from
/// `onnx-community/Kokoro-82M-v1.0-ONNX/config.json`. Any change here
/// must re-export the model; the table and the weights are coupled.
#[rustfmt::skip]
const VOCAB_ENTRIES: &[(char, i64)] = &[
    // ASCII punctuation + whitespace
    (';', 1), (':', 2), (',', 3), ('.', 4), ('!', 5), ('?', 6),
    // Typographic dash + ellipsis + quotes + brackets + space
    ('\u{2014}', 9),   // em-dash —
    ('\u{2026}', 10),  // ellipsis …
    ('"', 11),
    ('(', 12), (')', 13),
    ('\u{201C}', 14),  // left double-curly-quote "
    ('\u{201D}', 15),  // right double-curly-quote "
    (' ', 16),
    // Combining tilde (nasal marker)
    ('\u{0303}', 17),
    // IPA affricate ligatures
    ('ʣ', 18), ('ʥ', 19), ('ʦ', 20), ('ʨ', 21),
    ('ᵝ', 22), ('ꭧ', 23),
    // Uppercase — Kokoro uses these as diphthong merge markers
    // (see misaki's E2M table: a^ɪ→I, e^ɪ→A, o^ʊ→O, ɔ^ɪ→Y, etc.)
    ('A', 24), ('I', 25), ('O', 31), ('Q', 33), ('S', 35),
    ('T', 36), ('W', 39), ('Y', 41),
    // Superscript schwa
    ('ᵊ', 42),
    // Lowercase ASCII letters (as IPA)
    ('a', 43), ('b', 44), ('c', 45), ('d', 46), ('e', 47),
    ('f', 48), ('h', 50), ('i', 51), ('j', 52), ('k', 53),
    ('l', 54), ('m', 55), ('n', 56), ('o', 57), ('p', 58),
    ('q', 59), ('r', 60), ('s', 61), ('t', 62), ('u', 63),
    ('v', 64), ('w', 65), ('x', 66), ('y', 67), ('z', 68),
    // IPA vowels + consonants
    ('ɑ', 69), ('ɐ', 70), ('ɒ', 71), ('æ', 72),
    ('β', 75), ('ɔ', 76), ('ɕ', 77), ('ç', 78),
    ('ɖ', 80), ('ð', 81), ('ʤ', 82), ('ə', 83),
    ('ɚ', 85), ('ɛ', 86), ('ɜ', 87),
    ('ɟ', 90), ('ɡ', 92),
    ('ɥ', 99), ('ɨ', 101), ('ɪ', 102), ('ʝ', 103),
    ('ɯ', 110), ('ɰ', 111), ('ŋ', 112), ('ɳ', 113),
    ('ɲ', 114), ('ɴ', 115),
    ('ø', 116), ('ɸ', 118), ('θ', 119), ('œ', 120),
    ('ɹ', 123), ('ɾ', 125), ('ɻ', 126),
    ('ʁ', 128), ('ɽ', 129),
    ('ʂ', 130), ('ʃ', 131), ('ʈ', 132), ('ʧ', 133),
    ('ʊ', 135), ('ʋ', 136), ('ʌ', 138),
    ('ɣ', 139), ('ɤ', 140),
    ('χ', 142), ('ʎ', 143),
    ('ʒ', 147), ('ʔ', 148),
    // Stress / length marks
    ('ˈ', 156), ('ˌ', 157), ('ː', 158),
    // Aspiration / palatalization
    ('ʰ', 162), ('ʲ', 164),
    // Tone arrows (used for non-English but present in vocab)
    ('↓', 169), ('→', 171), ('↗', 172), ('↘', 173),
    // Near-close near-front rounded
    ('ᵻ', 177),
];

static VOCAB: OnceLock<HashMap<char, i64>> = OnceLock::new();

fn vocab() -> &'static HashMap<char, i64> {
    VOCAB.get_or_init(|| VOCAB_ENTRIES.iter().copied().collect())
}

/// Map an IPA phoneme string to Kokoro's sparse integer token ids.
/// Characters not in the vocab are silently dropped — this matches
/// kokoro-onnx's `filter(lambda p: p in vocab, phonemes)` behavior
/// and is how espeak-ng output with stray punctuation stays survivable.
///
/// Does NOT prepend/append the boundary pads (`[0, ..., 0]`); that's
/// the caller's job because the model takes `[0] + tokens + [0]` and
/// needs the raw `len(tokens)` for style-row indexing.
#[must_use]
pub fn ipa_to_token_ids(ipa: &str) -> Vec<i64> {
    let v = vocab();
    ipa.chars().filter_map(|c| v.get(&c).copied()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vocab_has_114_entries() {
        // If this ever breaks we either re-exported the model (and must
        // re-pin weights) or accidentally duplicated an entry.
        assert_eq!(VOCAB_ENTRIES.len(), 114);
        assert_eq!(vocab().len(), 114);
    }

    #[test]
    fn vocab_has_no_duplicate_keys() {
        let mut seen = std::collections::HashSet::new();
        for (c, _) in VOCAB_ENTRIES {
            assert!(seen.insert(*c), "duplicate key {c:?}");
        }
    }

    #[test]
    fn vocab_has_no_duplicate_ids() {
        let mut seen = std::collections::HashSet::new();
        for (_, id) in VOCAB_ENTRIES {
            assert!(seen.insert(*id), "duplicate id {id}");
        }
    }

    #[test]
    fn vocab_spot_check_known_entries() {
        // Spot-check a handful against the ONNX config.json we pinned.
        assert_eq!(vocab().get(&' '), Some(&16));
        assert_eq!(vocab().get(&'ˈ'), Some(&156)); // primary stress
        assert_eq!(vocab().get(&'a'), Some(&43));
        assert_eq!(vocab().get(&'ə'), Some(&83));  // schwa
        assert_eq!(vocab().get(&'ː'), Some(&158)); // length
        assert_eq!(vocab().get(&'ɹ'), Some(&123)); // rhotic r
    }

    #[test]
    fn tokenize_drops_unknowns() {
        // "hello" in IPA-ish ("həˈloʊ" approximately). The `ʊ` isn't
        // in the vocab in that combination — Kokoro uses `O` (diphthong
        // merge marker) instead. What isn't in the table gets dropped.
        let tokens = ipa_to_token_ids("həˈloʊ");
        assert!(!tokens.is_empty());
        // None of the tokens can be zero — 0 is the boundary-pad sentinel.
        assert!(tokens.iter().all(|&t| t != 0));
    }

    #[test]
    fn tokenize_empty_input_is_empty() {
        assert!(ipa_to_token_ids("").is_empty());
    }

    #[test]
    fn tokenize_all_unknown_is_empty() {
        // Emoji etc. are dropped silently.
        assert!(ipa_to_token_ids("🎉🎊👋").is_empty());
    }

    #[test]
    fn tokenize_stress_and_length_marks() {
        let tokens = ipa_to_token_ids("ˈaː");
        assert_eq!(tokens, vec![156, 43, 158]);
    }
}
