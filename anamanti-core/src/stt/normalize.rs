//! Deterministic STT transcript normalization (Mechanism B of
//! `plans/stt-vocabulary-biasing.md`).
//!
//! Whisper is an end-to-end acoustic→text model with no editable pronunciation
//! lexicon, so it reliably mis-spells domain proper nouns it never learned — the
//! grocery chain **“Zehrs”** (said *zay-rz*) comes back as “Zairs”, “Zehr’s”,
//! “zay rz”, etc. This module applies a small, user-maintained rewrite table to the
//! final transcript, mapping those known acoustic mis-hearings to the canonical
//! spelling. It is engine-agnostic: it runs on the one transcript both the Wyoming
//! and in-process Whisper engines produce.
//!
//! The rules are a live, GUI-editable setting (config page → persisted settings
//! overlay), read from the per-turn settings snapshot — so an edit takes effect on
//! the next turn with no restart. Matching is intentionally boring and deterministic
//! for v1: case-insensitive, whole-word/phrase, no fuzzy or phonetic distance.

use serde::{Deserialize, Serialize};

/// One rewrite rule: each `variant` (matched case-insensitively, as a whole
/// word/phrase) is replaced with `canonical` verbatim. You generate the `variants`
/// list *from* the word's pronunciation — "what spellings would Whisper plausibly
/// emit for this sound?" — which is where the phonetic knowledge is encoded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PronunciationRule {
    /// The corrected spelling to emit.
    pub canonical: String,
    /// The mis-heard forms to rewrite to `canonical`.
    #[serde(default)]
    pub variants: Vec<String>,
}

impl PronunciationRule {
    /// Clean a GUI-submitted rule: trim the canonical and every variant, drop blank
    /// variants, de-duplicate variants (case-insensitive), and drop a variant equal to
    /// the canonical (a no-op rewrite). Returns `None` when the rule is unusable — a
    /// blank canonical or no surviving variants — so the caller can filter it out.
    pub fn sanitized(&self) -> Option<PronunciationRule> {
        let canonical = self.canonical.trim().to_string();
        if canonical.is_empty() {
            return None;
        }
        let mut seen = std::collections::HashSet::new();
        let variants: Vec<String> = self
            .variants
            .iter()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .filter(|v| !v.eq_ignore_ascii_case(&canonical))
            .filter(|v| seen.insert(v.to_lowercase()))
            .collect();
        if variants.is_empty() {
            return None;
        }
        Some(PronunciationRule {
            canonical,
            variants,
        })
    }
}

/// Clean a whole GUI-submitted table: sanitize each rule and drop the unusable ones.
pub fn sanitize_rules(rules: &[PronunciationRule]) -> Vec<PronunciationRule> {
    rules.iter().filter_map(PronunciationRule::sanitized).collect()
}

/// Rewrite `text` by applying every rule, in order. Each rule's variants are tried
/// longest-first so a multi-word phrase wins over a shorter contained word. Matching
/// is case-insensitive and whole-word/phrase (bounded by non-alphanumeric characters
/// or the ends of the string); surrounding punctuation and spacing are preserved.
///
/// Rules are applied left-to-right in a single pass each; they do not deliberately
/// cascade (a canonical inserted by one rule is not re-scanned by that same rule, but
/// a *later* rule could in principle match it — keep tables non-overlapping to avoid
/// surprise). An empty table returns `text` unchanged.
pub fn normalize(text: &str, rules: &[PronunciationRule]) -> String {
    let mut out = text.to_string();
    for rule in rules {
        let Some(rule) = rule.sanitized() else {
            continue;
        };
        let mut variants = rule.variants;
        // Longest first: prefer "zay rz" over a bare "rz" if both are listed.
        variants.sort_by_key(|v| std::cmp::Reverse(v.chars().count()));
        for variant in &variants {
            out = replace_whole_word_ci(&out, variant, &rule.canonical);
        }
    }
    out
}

/// Case-insensitive, whole-word/phrase replacement of `needle` with `replacement` in
/// `haystack`. A match must be bounded on both sides by a non-alphanumeric character
/// or a string edge, so `"zairs"` rewrites `"zairs"` and `"Zairs?"` but never the
/// `"zairs"` inside `"zairsville"`. ASCII case-folding (sufficient for the English
/// store/brand names this targets) keeps char indices aligned.
fn replace_whole_word_ci(haystack: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return haystack.to_string();
    }
    let hay: Vec<char> = haystack.chars().collect();
    let need: Vec<char> = needle.chars().collect();
    let n = need.len();
    if hay.len() < n {
        return haystack.to_string();
    }
    let is_word = |c: char| c.is_alphanumeric();
    let mut out = String::with_capacity(haystack.len());
    let mut i = 0;
    while i < hay.len() {
        let fits = i + n <= hay.len();
        let matches = fits && (0..n).all(|k| hay[i + k].eq_ignore_ascii_case(&need[k]));
        // `i == 0` (no preceding char) and `i + n == len` (no following char) are both
        // word boundaries; `get` returning `None` maps to "not a word char".
        let left_ok = i == 0 || !hay.get(i - 1).copied().is_some_and(is_word);
        let right_ok = !matches || !hay.get(i + n).copied().is_some_and(is_word);
        if matches && left_ok && right_ok {
            out.push_str(replacement);
            i += n;
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(canonical: &str, variants: &[&str]) -> PronunciationRule {
        PronunciationRule {
            canonical: canonical.to_string(),
            variants: variants.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn rewrites_single_word_case_insensitively() {
        let rules = [rule("Zehrs", &["zairs"])];
        assert_eq!(normalize("i went to zairs today", &rules), "i went to Zehrs today");
        assert_eq!(normalize("ZAIRS has a sale", &rules), "Zehrs has a sale");
        assert_eq!(normalize("Zairs", &rules), "Zehrs");
    }

    #[test]
    fn preserves_surrounding_punctuation() {
        let rules = [rule("Zehrs", &["zairs", "zehr's"])];
        assert_eq!(normalize("did you mean zairs?", &rules), "did you mean Zehrs?");
        assert_eq!(normalize("(zehr's)", &rules), "(Zehrs)");
    }

    #[test]
    fn matches_whole_words_only() {
        let rules = [rule("Zehrs", &["zairs"])];
        // "zairs" inside a longer token must not be rewritten.
        assert_eq!(normalize("zairsville is nice", &rules), "zairsville is nice");
        assert_eq!(normalize("gozairs", &rules), "gozairs");
    }

    #[test]
    fn matches_multi_word_phrases() {
        let rules = [rule("Zehrs", &["zay rz"])];
        assert_eq!(normalize("buy it at zay rz please", &rules), "buy it at Zehrs please");
    }

    #[test]
    fn empty_table_is_identity() {
        assert_eq!(normalize("nothing to do here", &[]), "nothing to do here");
    }

    #[test]
    fn no_match_passthrough() {
        let rules = [rule("Zehrs", &["zairs"])];
        assert_eq!(normalize("the quick brown fox", &rules), "the quick brown fox");
    }

    #[test]
    fn applies_multiple_rules_and_occurrences() {
        let rules = [rule("Zehrs", &["zairs"]), rule("Fortinos", &["for tino's"])];
        assert_eq!(
            normalize("zairs or for tino's or zairs", &rules),
            "Zehrs or Fortinos or Zehrs"
        );
    }

    #[test]
    fn longer_variant_wins_over_shorter_contained_one() {
        // Both variants in one rule; the phrase should win where it applies.
        let rules = [rule("Zehrs", &["zay rz", "rz"])];
        assert_eq!(normalize("at zay rz", &rules), "at Zehrs");
    }

    #[test]
    fn sanitize_drops_unusable_rules() {
        let rules = [
            rule("Zehrs", &["  zairs  ", "zairs", "", "Zehrs"]), // dupes/blank/self stripped
            rule("", &["x"]),                                    // blank canonical
            rule("Nope", &["   "]),                              // no usable variants
        ];
        let cleaned = sanitize_rules(&rules);
        assert_eq!(cleaned.len(), 1);
        assert_eq!(cleaned[0].canonical, "Zehrs");
        assert_eq!(cleaned[0].variants, vec!["zairs".to_string()]);
    }

    #[test]
    fn unsanitized_rules_are_skipped_by_normalize() {
        // A rule with a blank canonical must never blank out matched text.
        let rules = [rule("", &["hello"])];
        assert_eq!(normalize("hello world", &rules), "hello world");
    }
}
