//! Who a memory is **true for** (SpeakerScopedMemoryPlan.md §2).
//!
//! Three scopes, not two:
//!
//! - [`MemoryScope::Person`] — true for one identified person; recalled only for them.
//! - [`MemoryScope::Household`] — deliberately true for everyone in the home ("the wifi
//!   is FastNet", "we live in Cambridge"); recalled for every speaker, known or not.
//! - [`MemoryScope::Unknown`] — said by someone we could not identify, and about
//!   *themselves*. Kept (for later review/attribution) but **never recalled** for
//!   anyone: "I am Brendon" from an unrecognized voice must not become true for
//!   Kristina.
//!
//! Before this split, "no speaker" (`speaker_id IS NULL`) meant *shared*, so anything an
//! unidentified person said about themselves became a fact about everyone.
//! [`classify_scope`] decides whether a new fact is speaker-relative or household-wide.

/// The storage label of a memory's scope (`memories.scope`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeKind {
    Person,
    Household,
    Unknown,
}

impl ScopeKind {
    /// Stable storage/wire label.
    pub fn as_str(self) -> &'static str {
        match self {
            ScopeKind::Person => "person",
            ScopeKind::Household => "household",
            ScopeKind::Unknown => "unknown",
        }
    }

    /// Parse a stored label; anything unrecognized reads as `household` (the column's
    /// default, and the pre-scope meaning of a speaker-less row).
    pub fn parse(s: &str) -> ScopeKind {
        match s {
            "person" => ScopeKind::Person,
            "unknown" => ScopeKind::Unknown,
            _ => ScopeKind::Household,
        }
    }
}

/// Where a memory is written: who it is true for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryScope {
    /// True for one identified person (`spk-…`).
    Person(String),
    /// True for everyone in the home.
    Household,
    /// Speaker-relative, but the speaker could not be confirmed. `candidate` is the
    /// best guess (a tentative or brand-new voice cluster), kept so an admin can
    /// attribute it with one click.
    Unknown { candidate: Option<String> },
}

impl MemoryScope {
    /// The scope's storage label.
    pub fn kind(&self) -> ScopeKind {
        match self {
            MemoryScope::Person(_) => ScopeKind::Person,
            MemoryScope::Household => ScopeKind::Household,
            MemoryScope::Unknown { .. } => ScopeKind::Unknown,
        }
    }

    /// The owning person's id (`Person` only).
    pub fn speaker_id(&self) -> Option<&str> {
        match self {
            MemoryScope::Person(id) => Some(id),
            _ => None,
        }
    }

    /// The best-guess person (`Unknown` only).
    pub fn candidate(&self) -> Option<&str> {
        match self {
            MemoryScope::Unknown { candidate } => candidate.as_deref(),
            _ => None,
        }
    }
}

/// Who said a memory and how sure the voice match was — recorded alongside the scope
/// so the admin console can show "said by" separately from "true for" (a household
/// fact said by Brendon; an unknown fact whose best guess is Brendon).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Provenance {
    /// The turn's resolved speaker (`spk-…`), `None` when unidentified.
    pub said_by: Option<String>,
    /// `identified | tentative | new | unknown` at write time.
    pub resolution: Option<String>,
    /// Voice-match cosine at write time.
    pub confidence: Option<f32>,
    /// The conversation turn that produced the memory (links to `/conversations`).
    pub turn_id: Option<String>,
}

/// Whether a fact describes the person who said it or the whole home.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactScope {
    /// About the speaker ("I like jazz", "The user's name is Brendon").
    Personal,
    /// About the household ("the wifi password is …", "we live in Cambridge").
    Household,
}

/// Classify a fact as speaker-relative or household-wide (plan §2.3).
///
/// - an explicit "for everyone" / "everybody" marker → household;
/// - any first-person-singular word ("I", "my", "me", …) or the inferred-fact subject
///   "the user" → personal (wins over plural words: "my wife and I live …" is about the
///   speaker);
/// - first-person-plural or impersonal home words ("we", "our", "the house", "the
///   wifi", "the kids", …) → household;
/// - anything else → **personal**. Under-sharing is the safe failure: telling Kristina
///   she is Brendon is worse than not telling Brendon's preference to Kristina.
pub fn classify_scope(content: &str) -> FactScope {
    let lower = content.to_lowercase().replace(['’', '‘'], "'");
    let words: Vec<&str> = lower
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .map(|w| w.trim_matches('\''))
        .filter(|w| !w.is_empty())
        .collect();
    let has = |set: &[&str]| words.iter().any(|w| set.contains(w));

    const EVERYONE: &[&str] = &["everyone", "everybody", "everyone's", "everybody's"];
    if has(EVERYONE) || lower.contains("all of us") {
        return FactScope::Household;
    }
    const PERSONAL: &[&str] = &[
        "i", "i'm", "im", "i've", "i'd", "i'll", "me", "my", "mine", "myself", "user", "user's",
    ];
    if has(PERSONAL) {
        return FactScope::Personal;
    }
    const HOUSEHOLD: &[&str] = &[
        "we",
        "we're",
        "we've",
        "we'd",
        "we'll",
        "our",
        "ours",
        "us",
        "ourselves",
        "house",
        "home",
        "household",
        "family",
        "wifi",
        "wi-fi",
        "kids",
        "children",
        "garage",
        "kitchen",
    ];
    if has(HOUSEHOLD) || lower.contains("wi-fi") {
        return FactScope::Household;
    }
    FactScope::Personal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_speaker_relative_vs_household_facts() {
        use FactScope::*;
        let cases = [
            ("The user's name is Brendan", Personal),
            ("The user likes the words from songs", Personal),
            ("I like my coffee black", Personal),
            ("my anniversary is June 3rd", Personal),
            ("my wife and I live in Cambridge", Personal),
            ("the wifi password is FastNet", Household),
            ("The house wifi is FastNet", Household),
            ("we live in Cambridge", Household),
            ("our dentist is Dr. Smith", Household),
            ("the kids' school is Jacob Hespeler", Household),
            ("for everyone: dinner is at six", Household),
            ("I want everyone to know the alarm code is 1234", Household),
            ("the dentist appointment is Tuesday", Personal), // ambiguous → personal
        ];
        for (text, want) in cases {
            assert_eq!(classify_scope(text), want, "{text:?}");
        }
    }

    #[test]
    fn scope_labels_round_trip() {
        for k in [ScopeKind::Person, ScopeKind::Household, ScopeKind::Unknown] {
            assert_eq!(ScopeKind::parse(k.as_str()), k);
        }
        assert_eq!(ScopeKind::parse(""), ScopeKind::Household);
    }
}
