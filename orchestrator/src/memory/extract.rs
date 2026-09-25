//! Memory extraction heuristics (Plan.MD §4 "explicit + inferred").
//!
//! - [`parse_command`] recognizes the **explicit** voice verbs ("remember …",
//!   "forget …") and turns them into a [`MemoryCommand`] the orchestrator applies
//!   directly, short-circuiting the LLM with a spoken confirmation.
//! - [`infer_memories`] does **inferred** capture: light, deterministic pattern
//!   matching over an ordinary turn to pull out durable facts/preferences.
//!
//! v1 deliberately uses regex-free string matching (no embedding model, no extra
//! LLM pass) so extraction is fast and unit-testable; a future version may add an
//! LLM-based extractor behind the same interface.

use super::MemoryKind;

/// An explicit memory management command parsed from a transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryCommand {
    /// "remember (that) …" — store `content` as the given kind.
    Remember { kind: MemoryKind, content: String },
    /// "forget …" (with a topic) — delete matching entries.
    ForgetMatching(String),
    /// "forget that" (bare) — delete the most recently stored entry.
    ForgetLast,
    /// "my name is …" / "call me …" — name the current speaker (speaker_id_plan.md
    /// Phase C). Carries the given name.
    NameSpeaker(String),
}

/// Trim surrounding whitespace and trailing sentence punctuation.
fn clean(s: &str) -> &str {
    s.trim().trim_end_matches(['.', '!', '?', ',']).trim()
}

/// Normalize a transcript for prefix matching: lowercased, filler/punctuation
/// stripped from the front.
fn normalize_lead(text: &str) -> String {
    text.trim()
        .trim_start_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase()
}

/// Parse an explicit memory command, if the utterance is one.
pub fn parse_command(text: &str) -> Option<MemoryCommand> {
    let lead = normalize_lead(text);
    let lead_clean = clean(&lead);

    // "forget that" / "forget it" (bare) → drop the last entry.
    if lead_clean == "forget that"
        || lead_clean == "forget it"
        || lead_clean == "forget the last one"
    {
        return Some(MemoryCommand::ForgetLast);
    }
    for p in ["forget about ", "forget that ", "forget "] {
        if let Some(rest) = lead.strip_prefix(p) {
            let rest = clean(rest);
            if !rest.is_empty() {
                return Some(MemoryCommand::ForgetMatching(rest.to_string()));
            }
        }
    }

    // "my name is …" / "call me …" → name the current speaker. Take the first
    // token as the name (conservative: avoids "my name is Sam and I like jazz"
    // capturing a whole clause). Only the unambiguous naming phrases are matched —
    // "i'm …" / "i am …" are intentionally excluded ("I'm hungry" is not a name).
    for p in ["my name is ", "call me "] {
        if let Some(tail) = tail_after_prefix(text, &lead, p) {
            if let Some(name) = first_name_token(tail) {
                return Some(MemoryCommand::NameSpeaker(name));
            }
        }
    }

    // "remember (that) …" → store. Preserve the original case of the content.
    for p in ["remember that ", "remember to ", "remember "] {
        if let Some(rest) = tail_after_prefix(text, &lead, p) {
            let kind = kind_of(&rest.to_lowercase());
            return Some(MemoryCommand::Remember {
                kind,
                content: rest.to_string(),
            });
        }
    }
    None
}

/// Classify a snippet as a preference when it expresses liking/preferring.
fn kind_of(lower: &str) -> MemoryKind {
    const PREF_HINTS: [&str; 6] = [
        "i like", "i love", "i prefer", "i enjoy", "favorite", "i hate",
    ];
    if PREF_HINTS.iter().any(|h| lower.contains(h)) {
        MemoryKind::Preference
    } else {
        MemoryKind::Fact
    }
}

/// Return the original-case tail after `phrase`, when the normalized text starts
/// with it. Anchoring to the start keeps questions ("do I like jazz?") from
/// tripping the "i like" rule.
fn tail_after_prefix<'a>(text: &'a str, lead: &str, phrase: &str) -> Option<&'a str> {
    if !lead.starts_with(phrase) {
        return None;
    }
    let original = text.trim();
    // `lead` was produced by trimming leading non-alphanumerics off a lowercased
    // copy; recover the same offset in the original by trimming identically.
    let orig_lead = original.trim_start_matches(|c: char| !c.is_alphanumeric());
    let tail = clean(&orig_lead[phrase.len()..]);
    if tail.is_empty() {
        None
    } else {
        Some(tail)
    }
}

/// The first whitespace-delimited token of `tail`, stripped of surrounding
/// punctuation — used as a spoken name ("my name is Sam." → "Sam"). `None` when
/// there is no alphanumeric token.
fn first_name_token(tail: &str) -> Option<String> {
    let tok = tail
        .split_whitespace()
        .next()?
        .trim_matches(|c: char| !c.is_alphanumeric());
    if tok.is_empty() {
        None
    } else {
        Some(tok.to_string())
    }
}

/// Infer durable facts/preferences from an ordinary (non-command) turn.
pub fn infer_memories(text: &str) -> Vec<(MemoryKind, String)> {
    let lead = normalize_lead(text);
    let mut out = Vec::new();

    // (prefix, kind, template) — template's `{}` is the captured tail.
    let rules: &[(&str, MemoryKind, &str)] = &[
        ("my name is ", MemoryKind::Fact, "The user's name is {}"),
        ("i live in ", MemoryKind::Fact, "The user lives in {}"),
        ("i'm from ", MemoryKind::Fact, "The user is from {}"),
        ("i am from ", MemoryKind::Fact, "The user is from {}"),
        ("my job is ", MemoryKind::Fact, "The user's job is {}"),
        ("i work as ", MemoryKind::Fact, "The user works as {}"),
        (
            "i don't like ",
            MemoryKind::Preference,
            "The user dislikes {}",
        ),
        (
            "i do not like ",
            MemoryKind::Preference,
            "The user dislikes {}",
        ),
        ("i hate ", MemoryKind::Preference, "The user dislikes {}"),
        ("i like ", MemoryKind::Preference, "The user likes {}"),
        ("i love ", MemoryKind::Preference, "The user likes {}"),
        ("i enjoy ", MemoryKind::Preference, "The user enjoys {}"),
        ("i prefer ", MemoryKind::Preference, "The user prefers {}"),
        (
            "my favorite ",
            MemoryKind::Preference,
            "The user's favorite {}",
        ),
    ];

    for (prefix, kind, template) in rules {
        if let Some(tail) = tail_after_prefix(text, &lead, prefix) {
            out.push((*kind, template.replace("{}", tail)));
            break; // one inference per utterance keeps v1 conservative
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_remember_commands() {
        assert_eq!(
            parse_command("Remember that my anniversary is June 3rd"),
            Some(MemoryCommand::Remember {
                kind: MemoryKind::Fact,
                content: "my anniversary is June 3rd".to_string()
            })
        );
        assert_eq!(
            parse_command("remember I like my coffee black"),
            Some(MemoryCommand::Remember {
                kind: MemoryKind::Preference,
                content: "I like my coffee black".to_string()
            })
        );
    }

    #[test]
    fn parses_forget_commands() {
        assert_eq!(
            parse_command("Forget that."),
            Some(MemoryCommand::ForgetLast)
        );
        assert_eq!(
            parse_command("forget about my dentist appointment"),
            Some(MemoryCommand::ForgetMatching(
                "my dentist appointment".to_string()
            ))
        );
    }

    #[test]
    fn non_commands_return_none() {
        assert_eq!(parse_command("what time is it?"), None);
        assert_eq!(parse_command("remember"), None); // no content
    }

    #[test]
    fn parses_name_speaker_commands() {
        assert_eq!(
            parse_command("My name is Sam"),
            Some(MemoryCommand::NameSpeaker("Sam".to_string()))
        );
        assert_eq!(
            parse_command("call me Dana."),
            Some(MemoryCommand::NameSpeaker("Dana".to_string()))
        );
        // Conservative: only the first token becomes the name.
        assert_eq!(
            parse_command("my name is Sam and I like jazz"),
            Some(MemoryCommand::NameSpeaker("Sam".to_string()))
        );
        // Ambiguous "I'm …" is deliberately NOT treated as a name.
        assert_eq!(parse_command("I'm hungry"), None);
    }

    #[test]
    fn infers_facts_and_preferences() {
        assert_eq!(
            infer_memories("My name is Sam"),
            vec![(MemoryKind::Fact, "The user's name is Sam".to_string())]
        );
        assert_eq!(
            infer_memories("I like jazz music"),
            vec![(
                MemoryKind::Preference,
                "The user likes jazz music".to_string()
            )]
        );
        assert_eq!(
            infer_memories("I don't like cilantro"),
            vec![(
                MemoryKind::Preference,
                "The user dislikes cilantro".to_string()
            )]
        );
    }

    #[test]
    fn questions_do_not_trigger_inference() {
        assert!(infer_memories("do I like jazz?").is_empty());
        assert!(infer_memories("what is the weather today").is_empty());
    }
}
