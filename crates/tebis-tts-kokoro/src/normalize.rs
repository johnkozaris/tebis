//! Text normalization, runs before espeak-ng.
//!
//! Pass order is load-bearing — each pass consumes text that later
//! passes would mis-handle:
//! `titles → currency → percent → ordinals → years → decimals → cardinals → whitespace`
//!
//! Scope matches the concerns that break assistant-reply audio
//! (numbers, currency, titles). URL / date / phone / unit parsing
//! intentionally out of scope — rare in markdown-stripped replies,
//! adds ~300 LoC for marginal quality. Reference:
//! `Kokoro-FastAPI/api/src/services/text_processing/normalizer.py`.

use std::sync::OnceLock;

use num2words::{Currency, Lang, Num2Words};
use regex::Regex;

/// Apply all normalization passes in order.
#[must_use]
pub fn preprocess(text: &str) -> String {
    let t = titles(text);
    let t = currency(&t);
    let t = percent(&t);
    let t = ordinals(&t);
    let t = years(&t);
    let t = decimals(&t);
    let t = cardinals(&t);
    collapse_whitespace(&t)
}

// ---- Titles ----

/// Word-boundary-anchored title expansion. Trailing `\s` requirement
/// so "Dr.Smith" (no space) is deliberately left alone — tebis text
/// rarely has it, and espeak handles it acceptably.
fn titles(text: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"\b(Dr|Mr|Mrs|Ms|vs|Jr|Sr|St|Prof)\.\s").expect("title regex")
    });
    re.replace_all(text, |caps: &regex::Captures<'_>| match &caps[1] {
        "Dr" => "doctor ".to_string(),
        "Mr" => "mister ".to_string(),
        "Mrs" => "missus ".to_string(),
        "Ms" => "miss ".to_string(),
        "vs" => "versus ".to_string(),
        "Jr" => "junior ".to_string(),
        "Sr" => "senior ".to_string(),
        "St" => "saint ".to_string(),
        "Prof" => "professor ".to_string(),
        other => format!("{other} "),
    })
    .into_owned()
}

// ---- Currency ----

/// `$42`, `$42.50`, `$0.99`. Emits e.g. "forty-two dollars" or
/// "three dollars and fifty cents". Only handles USD; extension to €
/// / £ is straightforward but out of scope for v1.
fn currency(text: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    // Require a non-digit right boundary so `$0.999` doesn't partial-
    // match as "zero dollars and 99 cents" with a stray `9` left over.
    // The cents group, if present, must be exactly two digits followed
    // by a non-digit / end-of-string. Matching `$42abc` (extracts 42)
    // is acceptable — letters aren't valid currency suffixes anyway.
    let re =
        RE.get_or_init(|| Regex::new(r"\$(\d+)(?:\.(\d{2}))?(?:\b|$)").expect("currency regex"));
    re.replace_all(text, |caps: &regex::Captures<'_>| {
        let dollars: i64 = caps[1].parse().unwrap_or(0);
        let cents: Option<i64> = caps.get(2).and_then(|m| m.as_str().parse().ok());
        match cents {
            Some(c) if c > 0 => format!(
                "{} and {}",
                int_to_currency_dollars(dollars),
                int_to_currency_cents(c),
            ),
            _ => int_to_currency_dollars(dollars),
        }
    })
    .into_owned()
}

fn int_to_currency_dollars(n: i64) -> String {
    let words = int_to_words(n);
    let unit = if n == 1 { "dollar" } else { "dollars" };
    format!("{words} {unit}")
}

fn int_to_currency_cents(n: i64) -> String {
    let words = int_to_words(n);
    let unit = if n == 1 { "cent" } else { "cents" };
    format!("{words} {unit}")
}

// ---- Percent ----

fn percent(text: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    // Match an integer or decimal immediately before `%`.
    let re = RE.get_or_init(|| Regex::new(r"(\d+(?:\.\d+)?)%").expect("percent regex"));
    re.replace_all(text, |caps: &regex::Captures<'_>| {
        let raw = &caps[1];
        let words = if raw.contains('.') {
            decimal_to_words(raw)
        } else {
            int_to_words(raw.parse().unwrap_or(0))
        };
        format!("{words} percent")
    })
    .into_owned()
}

// ---- Ordinals ----

/// `1st`, `2nd`, `3rd`, `42nd`, `101st`, etc. → "first", "second",
/// "third", "forty-second", "one hundred first".
fn ordinals(text: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re =
        RE.get_or_init(|| Regex::new(r"\b(\d+)(st|nd|rd|th)\b").expect("ordinal regex"));
    re.replace_all(text, |caps: &regex::Captures<'_>| {
        let n: i64 = caps[1].parse().unwrap_or(0);
        ordinal_to_words(n)
    })
    .into_owned()
}

fn ordinal_to_words(n: i64) -> String {
    Num2Words::new(n)
        .lang(Lang::English)
        .ordinal()
        .to_words()
        .unwrap_or_else(|_| format!("{n}th"))
}

// ---- Years ----

/// Reads 4-digit year-like numbers with year-aware grouping: 1995 →
/// "nineteen ninety-five", 2024 → "twenty twenty-four", 2005 → "two
/// thousand five", 2000 → "two thousand".
///
/// Only fires on unambiguous year-shaped numbers (1500..=2099). Larger
/// or smaller 4-digit-looking numbers (ages, prices already handled
/// upstream) fall through to cardinal reading.
fn years(text: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\b(1[5-9]\d{2}|20\d{2})\b").expect("year regex"));
    re.replace_all(text, |caps: &regex::Captures<'_>| {
        let n: i64 = caps[1].parse().unwrap_or(0);
        year_to_words(n)
    })
    .into_owned()
}

/// Human year-reading logic. num2words 1.x doesn't honor a year hint
/// (it falls through to normal cardinal), so we do it by hand:
///
/// - `19XX` / `20XX` → split into century + two-digit remainder
///   (e.g. `1995` → "nineteen ninety-five", `2024` → "twenty twenty-four")
/// - `N000` → "N thousand" (e.g. `2000` → "two thousand")
/// - `200X` (1..=9) → "two thousand X" — the normal way these years
///   are read in US English
/// - `XX00` (`1900`) → "nineteen hundred"
/// - Out-of-range inputs fall back to cardinal reading.
fn year_to_words(n: i64) -> String {
    if !(1000..=9999).contains(&n) {
        return int_to_words(n);
    }
    let century = n / 100;
    let rest = n % 100;
    if n % 1000 == 0 {
        // 1000, 2000, 3000 — normal cardinal reads fine
        return int_to_words(n);
    }
    if rest == 0 {
        // 1900, 2100, etc. — "nineteen hundred"
        return format!("{} hundred", int_to_words(century));
    }
    if (2000..2010).contains(&n) {
        // 2001-2009 → "two thousand X"
        return format!("two thousand {}", int_to_words(rest));
    }
    // General: century + two-digit chunk, with leading-zero chunks
    // like 05 being read "oh five". num2words reads `5` as "five";
    // we need "oh five" for year context.
    let rest_words = if rest < 10 {
        format!("oh {}", int_to_words(rest))
    } else {
        int_to_words(rest)
    };
    format!("{} {}", int_to_words(century), rest_words)
}

// ---- Decimals ----

/// `3.14` → "three point one four". Each digit after the decimal is
/// read individually, matching how num2words handles this when `prefer`
/// is set to "decimal" — but we do it manually so we control the output
/// exactly.
fn decimals(text: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\b(\d+)\.(\d+)\b").expect("decimal regex"));
    re.replace_all(text, |caps: &regex::Captures<'_>| {
        let whole = &caps[1];
        let frac = &caps[2];
        decimal_parts_to_words(whole, frac)
    })
    .into_owned()
}

fn decimal_to_words(raw: &str) -> String {
    if let Some((w, f)) = raw.split_once('.') {
        decimal_parts_to_words(w, f)
    } else {
        int_to_words(raw.parse().unwrap_or(0))
    }
}

fn decimal_parts_to_words(whole: &str, frac: &str) -> String {
    let whole_num: i64 = whole.parse().unwrap_or(0);
    let whole_words = int_to_words(whole_num);
    let frac_words: Vec<String> = frac
        .chars()
        .filter_map(|c| c.to_digit(10))
        .map(|d| digit_word(d))
        .map(str::to_string)
        .collect();
    if frac_words.is_empty() {
        whole_words
    } else {
        format!("{whole_words} point {}", frac_words.join(" "))
    }
}

const fn digit_word(d: u32) -> &'static str {
    match d {
        0 => "zero",
        1 => "one",
        2 => "two",
        3 => "three",
        4 => "four",
        5 => "five",
        6 => "six",
        7 => "seven",
        8 => "eight",
        _ => "nine",
    }
}

// ---- Cardinals ----

/// Any remaining standalone integer → words. Runs last because the
/// previous passes have already consumed years, ordinals, currency,
/// and decimal numbers — what's left is genuinely just a count.
fn cardinals(text: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"\b(\d+)\b").expect("cardinal regex"));
    re.replace_all(text, |caps: &regex::Captures<'_>| {
        let n: i64 = caps[1].parse().unwrap_or(0);
        int_to_words(n)
    })
    .into_owned()
}

fn int_to_words(n: i64) -> String {
    Num2Words::new(n)
        .lang(Lang::English)
        .to_words()
        .unwrap_or_else(|_| n.to_string())
}

// ---- Whitespace cleanup ----

fn collapse_whitespace(text: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"[ \t\r\n]+").expect("whitespace regex"));
    re.replace_all(text.trim(), " ").into_owned()
}

// ---- Type juggling: num2words's Currency enum unused (we build the
// string ourselves for explicit singular/plural control). Kept as a
// compile-time reference that we're aware of the crate's builder. ----
#[allow(dead_code, reason = "explicit marker that we considered num2words Currency builder")]
const _: Option<Currency> = None;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_expand() {
        assert_eq!(
            titles("Dr. Smith met Mr. Jones and Mrs. Lee."),
            "doctor Smith met mister Jones and missus Lee."
        );
        assert_eq!(titles("vs. the world"), "versus the world");
    }

    #[test]
    fn title_without_space_is_left_alone() {
        // "Dr.Smith" and "U.S.A." shouldn't mangle.
        assert_eq!(titles("Dr.Smith"), "Dr.Smith");
        assert_eq!(titles("U.S.A."), "U.S.A.");
    }

    #[test]
    fn currency_whole_dollars() {
        assert_eq!(currency("$42"), "forty-two dollars");
        assert_eq!(currency("$1"), "one dollar");
        assert_eq!(currency("$0"), "zero dollars");
        assert_eq!(currency("$100"), "one hundred dollars");
    }

    #[test]
    fn currency_with_cents() {
        assert_eq!(
            currency("$3.50"),
            "three dollars and fifty cents"
        );
        assert_eq!(
            currency("$0.99"),
            "zero dollars and ninety-nine cents"
        );
        assert_eq!(
            currency("$1.01"),
            "one dollar and one cent"
        );
    }

    #[test]
    fn currency_zero_cents_omits_and_clause() {
        // $42.00 — the ".00" shouldn't produce "and zero cents".
        assert_eq!(currency("$42.00"), "forty-two dollars");
    }

    #[test]
    fn currency_three_decimal_digits_do_not_partial_match() {
        // $0.999 is malformed currency — the cents group requires
        // exactly 2 digits with a word boundary after. We should
        // NOT greedily extract "$0.99" leaving a stray "9".
        // Either full-match or no-match is acceptable; silent
        // truncation to "zero dollars and ninety-nine cents" with
        // a trailing 9 is not.
        let out = currency("$0.999");
        assert!(
            !out.contains("ninety-nine"),
            "currency should not partial-match $0.999, got: {out}"
        );
    }

    #[test]
    fn percent_integer() {
        assert_eq!(percent("50%"), "fifty percent");
    }

    #[test]
    fn percent_decimal() {
        assert_eq!(
            percent("42.5%"),
            "forty-two point five percent"
        );
    }

    #[test]
    fn ordinals_first_three() {
        assert_eq!(ordinals("1st place"), "first place");
        assert_eq!(ordinals("2nd best"), "second best");
        assert_eq!(ordinals("3rd try"), "third try");
    }

    #[test]
    fn ordinals_larger() {
        assert_eq!(ordinals("42nd"), "forty-second");
        assert_eq!(ordinals("101st"), "one hundred first");
    }

    #[test]
    fn years_modern() {
        assert!(years("2024").contains("twenty"), "got: {}", years("2024"));
    }

    #[test]
    fn years_older() {
        assert!(
            years("1995").contains("nineteen"),
            "got: {}",
            years("1995")
        );
    }

    #[test]
    fn years_only_four_digit_range_matches() {
        // 1499 and 2100 should fall through to cardinal reading, not
        // the year reader. We don't test cardinals here (they'd also
        // produce words), we just check the year regex doesn't fire.
        let re = Regex::new(r"\b(1[5-9]\d{2}|20\d{2})\b").unwrap();
        assert!(!re.is_match("1499"));
        assert!(!re.is_match("2100"));
        assert!(re.is_match("1500"));
        assert!(re.is_match("2099"));
    }

    #[test]
    fn decimals_simple() {
        assert_eq!(
            decimals("3.14"),
            "three point one four"
        );
        assert_eq!(
            decimals("0.5"),
            "zero point five"
        );
    }

    #[test]
    fn cardinals_standalone() {
        assert_eq!(cardinals("42"), "forty-two");
        assert_eq!(cardinals("100"), "one hundred");
    }

    #[test]
    fn whitespace_collapses_and_trims() {
        assert_eq!(
            collapse_whitespace("  hello   world\n\n!  "),
            "hello world !"
        );
    }

    #[test]
    fn full_pipeline_on_assistant_reply() {
        // A realistic assistant reply that exercises most passes.
        let input = "Dr. Smith's 2024 report shows $42.50 and 50% improvement on the 1st test.";
        let out = preprocess(input);
        // Assert key transformations rather than the full string —
        // num2words minor output drift shouldn't flake the test.
        assert!(out.contains("doctor Smith"), "titles: {out}");
        assert!(out.contains("twenty twenty-four"), "year: {out}");
        assert!(
            out.contains("forty-two dollars and fifty cents"),
            "currency: {out}"
        );
        assert!(out.contains("fifty percent"), "percent: {out}");
        assert!(out.contains("first test"), "ordinal: {out}");
        assert!(!out.contains("$"), "currency symbol leaked: {out}");
        assert!(!out.contains("%"), "percent symbol leaked: {out}");
    }

    #[test]
    fn empty_input_returns_empty() {
        assert_eq!(preprocess(""), "");
    }

    #[test]
    fn plain_text_passes_through_unchanged() {
        let input = "hello world";
        let out = preprocess(input);
        assert_eq!(out, "hello world");
    }
}
