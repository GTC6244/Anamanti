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
    /// "who am I?" / "what's my name?" — answered from the voiceprint resolution, never
    /// from memory text (SpeakerScopedMemoryPlan.md §5).
    WhoAmI,
}

/// Whole-utterance questions asking the assistant to identify the speaker. Matched
/// against the cleaned, lowercased utterance (filler words like "hey" / "jarvis" /
/// "tell me" are stripped first), so only a question that *is* the identity question
/// fires — "what's my name on the booking" does not.
const WHO_AM_I: &[&str] = &[
    "who am i",
    "what's my name",
    "whats my name",
    "what is my name",
    "do you know who i am",
    "do you know my name",
    "do you know who this is",
    "who is this",
    "who's this",
    "who's talking",
    "who is talking",
    "who is speaking",
    "who's speaking",
    "who do you think i am",
    "who do you think this is",
    "can you tell who i am",
    "do you recognize me",
    "do you recognise me",
    "do you recognize my voice",
    "do you recognise my voice",
];

/// Whether `lead_clean` (normalized, punctuation-trimmed) is a who-am-I question.
fn is_who_am_i(lead_clean: &str) -> bool {
    let mut q = lead_clean.replace('\u{2019}', "'").replace(',', "");
    for filler in [
        "hey ", "ok ", "okay ", "so ", "um ", "uh ", "jarvis ", "tell me ", "please ",
    ] {
        while let Some(rest) = q.strip_prefix(filler) {
            q = rest.trim_start().to_string();
        }
    }
    let q = q
        .trim_end_matches(|c: char| !c.is_alphanumeric())
        .trim_end_matches(" jarvis")
        .trim();
    WHO_AM_I.contains(&q)
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

    if is_who_am_i(lead_clean) {
        return Some(MemoryCommand::WhoAmI);
    }

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

// ------------------------------------------------------------------ Portuguese (pt-BR)
//
// `plans/BilingualPlan.md` §2.6: the same commands and inferred patterns in Brazilian
// Portuguese. Matching mirrors the English rules (anchored at the start of the
// utterance, original case preserved in the captured tail). Phrases are listed with
// and without accents because STT output isn't always accented. Content is **stored as
// spoken** (owner decision) — inferred facts use Portuguese templates.

/// Parse an explicit memory command in any of `langs` (tried in order), returning the
/// command and the language whose phrasing matched — which is, by definition, the
/// language the user spoke. `[Lang::En]` is exactly [`parse_command`].
pub fn parse_command_in(
    text: &str,
    langs: &[crate::lang::Lang],
) -> Option<(MemoryCommand, crate::lang::Lang)> {
    use crate::lang::Lang;
    langs.iter().find_map(|&l| {
        match l {
            Lang::En => parse_command(text),
            Lang::PtBr => parse_command_pt(text),
            Lang::FrCa => parse_command_fr(text),
        }
        .map(|c| (c, l))
    })
}

/// Inferred capture in the first of `langs` whose patterns match. `[Lang::En]` is
/// exactly [`infer_memories`].
pub fn infer_memories_in(text: &str, langs: &[crate::lang::Lang]) -> Vec<(MemoryKind, String)> {
    use crate::lang::Lang;
    for &l in langs {
        let found = match l {
            Lang::En => infer_memories(text),
            Lang::PtBr => infer_memories_pt(text),
            Lang::FrCa => infer_memories_fr(text),
        };
        if !found.is_empty() {
            return found;
        }
    }
    Vec::new()
}

/// Portuguese who-am-I questions (whole utterance, fillers stripped).
const WHO_AM_I_PT: &[&str] = &[
    "quem sou eu",
    "quem é que eu sou",
    "quem e que eu sou",
    "qual é o meu nome",
    "qual e o meu nome",
    "qual o meu nome",
    "qual meu nome",
    "qual é meu nome",
    "você sabe quem eu sou",
    "voce sabe quem eu sou",
    "sabe quem eu sou",
    "você sabe o meu nome",
    "voce sabe o meu nome",
    "você sabe meu nome",
    "voce sabe meu nome",
    "quem está falando",
    "quem esta falando",
    "quem tá falando",
    "quem ta falando",
    "você me reconhece",
    "voce me reconhece",
    "você reconhece a minha voz",
    "voce reconhece a minha voz",
    "você reconhece minha voz",
    "voce reconhece minha voz",
    "quem você acha que eu sou",
    "quem voce acha que eu sou",
];

fn is_who_am_i_pt(lead_clean: &str) -> bool {
    let mut q = lead_clean.replace(',', "");
    for filler in [
        "ei ",
        "oi ",
        "ok ",
        "olha ",
        "jarvis ",
        "me diz ",
        "me diga ",
        "por favor ",
    ] {
        while let Some(rest) = q.strip_prefix(filler) {
            q = rest.trim_start().to_string();
        }
    }
    let q = q
        .trim_end_matches(|c: char| !c.is_alphanumeric())
        .trim_end_matches(" jarvis")
        .trim();
    WHO_AM_I_PT.contains(&q)
}

/// Parse an explicit Portuguese memory command ("lembra que …", "esquece isso",
/// "meu nome é …", "quem sou eu?").
pub fn parse_command_pt(text: &str) -> Option<MemoryCommand> {
    let lead = normalize_lead(text);
    let lead_clean = clean(&lead);

    if is_who_am_i_pt(lead_clean) {
        return Some(MemoryCommand::WhoAmI);
    }

    // "esquece isso" (bare) → drop the last entry.
    const FORGET_LAST: &[&str] = &[
        "esquece isso",
        "esqueça isso",
        "esqueca isso",
        "esquece aquilo",
        "esqueça aquilo",
        "esquece o último",
        "esqueça o último",
        "esquece o ultimo",
        "esquece a última coisa",
        "esqueça a última coisa",
        "esquece o que eu disse",
        "esqueça o que eu disse",
    ];
    if FORGET_LAST.contains(&lead_clean) {
        return Some(MemoryCommand::ForgetLast);
    }
    for p in [
        "esquece sobre ",
        "esqueça sobre ",
        "esquece que ",
        "esqueça que ",
        "esquece de ",
        "esqueça de ",
        "esquece ",
        "esqueça ",
        "esqueca ",
    ] {
        if let Some(rest) = lead.strip_prefix(p) {
            let rest = clean(rest);
            if !rest.is_empty() {
                return Some(MemoryCommand::ForgetMatching(rest.to_string()));
            }
        }
    }

    // "meu nome é …" / "me chamo …" / "pode me chamar de …" → name the speaker.
    for p in [
        "meu nome é ",
        "meu nome e ",
        "me chamo ",
        "pode me chamar de ",
        "me chama de ",
        "me chame de ",
    ] {
        if let Some(tail) = tail_after_prefix(text, &lead, p) {
            if let Some(name) = first_name_token(tail) {
                return Some(MemoryCommand::NameSpeaker(name));
            }
        }
    }

    // "lembra que …" → store, as spoken. Longest phrasings first.
    for p in [
        "lembre-se de que ",
        "lembre se de que ",
        "não esqueça que ",
        "nao esqueça que ",
        "não esquece que ",
        "nao esquece que ",
        "lembra de que ",
        "lembre-se que ",
        "lembre se que ",
        "lembre-se de ",
        "lembra que ",
        "lembre que ",
        "anota que ",
        "anote que ",
        "guarda que ",
        "guarde que ",
    ] {
        if let Some(rest) = tail_after_prefix(text, &lead, p) {
            let kind = kind_of_pt(&rest.to_lowercase());
            return Some(MemoryCommand::Remember {
                kind,
                content: rest.to_string(),
            });
        }
    }
    None
}

/// Classify a Portuguese snippet as a preference when it expresses liking/preferring.
fn kind_of_pt(lower: &str) -> MemoryKind {
    const PREF_HINTS: [&str; 10] = [
        "eu gosto",
        "gosto de",
        "eu amo",
        "adoro",
        "prefiro",
        "favorito",
        "favorita",
        "odeio",
        "não gosto",
        "nao gosto",
    ];
    if PREF_HINTS.iter().any(|h| lower.contains(h)) {
        MemoryKind::Preference
    } else {
        MemoryKind::Fact
    }
}

/// Portuguese inferred capture (stored as spoken, in Portuguese templates).
pub fn infer_memories_pt(text: &str) -> Vec<(MemoryKind, String)> {
    let lead = normalize_lead(text);
    let mut out = Vec::new();
    let rules: &[(&str, MemoryKind, &str)] = &[
        ("meu nome é ", MemoryKind::Fact, "O nome do usuário é {}"),
        ("eu moro em ", MemoryKind::Fact, "O usuário mora em {}"),
        ("moro em ", MemoryKind::Fact, "O usuário mora em {}"),
        ("eu sou de ", MemoryKind::Fact, "O usuário é de {}"),
        ("sou de ", MemoryKind::Fact, "O usuário é de {}"),
        (
            "meu trabalho é ",
            MemoryKind::Fact,
            "O trabalho do usuário é {}",
        ),
        (
            "eu trabalho como ",
            MemoryKind::Fact,
            "O usuário trabalha como {}",
        ),
        (
            "trabalho como ",
            MemoryKind::Fact,
            "O usuário trabalha como {}",
        ),
        (
            "eu não gosto de ",
            MemoryKind::Preference,
            "O usuário não gosta de {}",
        ),
        (
            "não gosto de ",
            MemoryKind::Preference,
            "O usuário não gosta de {}",
        ),
        ("eu odeio ", MemoryKind::Preference, "O usuário odeia {}"),
        ("odeio ", MemoryKind::Preference, "O usuário odeia {}"),
        (
            "eu gosto de ",
            MemoryKind::Preference,
            "O usuário gosta de {}",
        ),
        ("gosto de ", MemoryKind::Preference, "O usuário gosta de {}"),
        ("eu amo ", MemoryKind::Preference, "O usuário ama {}"),
        ("eu adoro ", MemoryKind::Preference, "O usuário adora {}"),
        ("adoro ", MemoryKind::Preference, "O usuário adora {}"),
        (
            "eu prefiro ",
            MemoryKind::Preference,
            "O usuário prefere {}",
        ),
        ("prefiro ", MemoryKind::Preference, "O usuário prefere {}"),
    ];
    for (prefix, kind, template) in rules {
        if let Some(tail) = tail_after_prefix(text, &lead, prefix) {
            out.push((*kind, template.replace("{}", tail)));
            return out; // one inference per utterance, like English
        }
    }
    // "minha cor favorita é azul" / "meu time favorito é o Flamengo": the noun comes
    // before "favorito", so keep the whole statement.
    let lc = clean(&lead);
    if (lc.starts_with("meu ") || lc.starts_with("minha "))
        && (lc.contains(" favorito é ") || lc.contains(" favorita é "))
    {
        let original = clean(
            text.trim()
                .trim_start_matches(|c: char| !c.is_alphanumeric()),
        );
        out.push((
            MemoryKind::Preference,
            format!("Preferência do usuário: {original}"),
        ));
    }
    out
}

// -------------------------------------------------------------------- Canadian French
//
// `plans/BilingualPlan.md` §2.6: the same commands and inferred patterns in Canadian
// French, mirroring the English/Portuguese rules. Phrases are listed with hyphens and
// with spaces, and with and without accents, because STT output varies on both. Content
// is **stored as spoken** (owner decision) — inferred facts use French templates.

/// Canadian-French who-am-I questions (whole utterance, fillers stripped).
const WHO_AM_I_FR: &[&str] = &[
    "qui suis-je",
    "qui suis je",
    "qui je suis",
    "c'est quoi mon nom",
    "quel est mon nom",
    "quel est mon prénom",
    "quel est mon prenom",
    "tu sais qui je suis",
    "sais-tu qui je suis",
    "sais tu qui je suis",
    "est-ce que tu sais qui je suis",
    "est ce que tu sais qui je suis",
    "tu me reconnais",
    "est-ce que tu me reconnais",
    "est ce que tu me reconnais",
    "tu reconnais ma voix",
    "est-ce que tu reconnais ma voix",
    "qui parle",
    "qui est-ce qui parle",
    "qui est ce qui parle",
    "tu connais mon nom",
    "tu sais mon nom",
    "est-ce que tu connais mon nom",
];

fn is_who_am_i_fr(lead_clean: &str) -> bool {
    let mut q = lead_clean.replace(',', "");
    for filler in [
        "hé ",
        "he ",
        "salut ",
        "ok ",
        "jarvis ",
        "dis-moi ",
        "dis moi ",
        "s'il te plaît ",
        "s'il te plait ",
    ] {
        while let Some(rest) = q.strip_prefix(filler) {
            q = rest.trim_start().to_string();
        }
    }
    let q = q
        .trim_end_matches(|c: char| !c.is_alphanumeric())
        .trim_end_matches(" jarvis")
        .trim();
    WHO_AM_I_FR.contains(&q)
}

/// Parse an explicit Canadian-French memory command ("rappelle-toi que …", "oublie ça",
/// "je m'appelle …", "qui suis-je?").
pub fn parse_command_fr(text: &str) -> Option<MemoryCommand> {
    // Fold the typographic apostrophe STT emits (U+2019) to ASCII so the apostrophe-heavy
    // French prefixes ("je m'appelle", "n'oublie pas que", "c'est quoi mon nom") match.
    // Done on a working copy so prefix-matching (`lead`) and tail recovery (`text`) stay
    // byte-aligned — the content captured for a memory then uses the ASCII apostrophe too.
    let folded = text.replace('\u{2019}', "'");
    let text = folded.as_str();
    let lead = normalize_lead(text);
    let lead_clean = clean(&lead);

    if is_who_am_i_fr(lead_clean) {
        return Some(MemoryCommand::WhoAmI);
    }

    // "oublie ça" (bare) → drop the last entry.
    const FORGET_LAST: &[&str] = &[
        "oublie ça",
        "oublie ca",
        "oublie cela",
        "oublie le dernier",
        "oublie la dernière",
        "oublie la derniere",
        "oublie la dernière chose",
        "oublie la derniere chose",
        "oublie ce que j'ai dit",
        "oublie ce que je viens de dire",
    ];
    if FORGET_LAST.contains(&lead_clean) {
        return Some(MemoryCommand::ForgetLast);
    }
    // Longest phrasings first so "oublie que" wins over bare "oublie".
    for p in [
        "oublie à propos de ",
        "oublie a propos de ",
        "oublie au sujet de ",
        "oublie le fait que ",
        "oublie que ",
        "oublie ",
    ] {
        if let Some(rest) = lead.strip_prefix(p) {
            let rest = clean(rest);
            if !rest.is_empty() {
                return Some(MemoryCommand::ForgetMatching(rest.to_string()));
            }
        }
    }

    // "je m'appelle …" / "mon nom est …" / "appelle-moi …" → name the speaker. "appelle-moi
    // un taxi" / "appelle-moi quand c'est prêt" are not names, so a first token that is a
    // French article/stopword is rejected (the turn then falls through, not stored).
    const NON_NAMES: &[&str] = &[
        "un", "une", "le", "la", "les", "des", "du", "de", "mon", "ma", "mes", "quand", "si",
        "dès", "des", "plus", "tard", "que", "quoi", "comment", "où", "ou", "moi", "nous",
        "vous", "ça", "ca",
    ];
    for p in [
        "je m'appelle ",
        "mon nom est ",
        "appelle-moi ",
        "appelle moi ",
        "tu peux m'appeler ",
    ] {
        if let Some(tail) = tail_after_prefix(text, &lead, p) {
            if let Some(name) = first_name_token(tail) {
                if !NON_NAMES.contains(&name.to_lowercase().as_str()) {
                    return Some(MemoryCommand::NameSpeaker(name));
                }
            }
        }
    }

    // "rappelle-toi que …" → store, as spoken. Longest phrasings first.
    for p in [
        "n'oublie pas que ",
        "n'oublie pas de ",
        "rappelle-toi que ",
        "rappelle toi que ",
        "rappelle-toi de ",
        "rappelle toi de ",
        "souviens-toi que ",
        "souviens toi que ",
        "souviens-toi de ",
        "souviens toi de ",
        "rappelle-toi ",
        "rappelle toi ",
        "souviens-toi ",
        "souviens toi ",
        "note que ",
        "retiens que ",
    ] {
        if let Some(rest) = tail_after_prefix(text, &lead, p) {
            let kind = kind_of_fr(&rest.to_lowercase());
            return Some(MemoryCommand::Remember {
                kind,
                content: rest.to_string(),
            });
        }
    }
    None
}

/// Classify a Canadian-French snippet as a preference when it expresses liking/preferring.
fn kind_of_fr(lower: &str) -> MemoryKind {
    const PREF_HINTS: [&str; 10] = [
        "j'aime",
        "j'adore",
        "je préfère",
        "je prefere",
        "préféré",
        "prefere",
        "préférée",
        "je déteste",
        "je deteste",
        "je n'aime pas",
    ];
    if PREF_HINTS.iter().any(|h| lower.contains(h)) {
        MemoryKind::Preference
    } else {
        MemoryKind::Fact
    }
}

/// Canadian-French inferred capture (stored as spoken, in French templates).
pub fn infer_memories_fr(text: &str) -> Vec<(MemoryKind, String)> {
    // Fold U+2019 → ASCII apostrophe (see [`parse_command_fr`]) so "j'habite", "j'aime"
    // etc. match whichever apostrophe STT produced; keeps `lead`/`text` byte-aligned.
    let folded = text.replace('\u{2019}', "'");
    let text = folded.as_str();
    let lead = normalize_lead(text);
    let mut out = Vec::new();
    let rules: &[(&str, MemoryKind, &str)] = &[
        ("mon nom est ", MemoryKind::Fact, "Le nom de l'utilisateur est {}"),
        ("je m'appelle ", MemoryKind::Fact, "Le nom de l'utilisateur est {}"),
        ("j'habite à ", MemoryKind::Fact, "L'utilisateur habite à {}"),
        ("j'habite ", MemoryKind::Fact, "L'utilisateur habite à {}"),
        ("je vis à ", MemoryKind::Fact, "L'utilisateur habite à {}"),
        ("je viens de ", MemoryKind::Fact, "L'utilisateur vient de {}"),
        ("je suis de ", MemoryKind::Fact, "L'utilisateur vient de {}"),
        ("mon travail est ", MemoryKind::Fact, "Le travail de l'utilisateur est {}"),
        ("je travaille comme ", MemoryKind::Fact, "L'utilisateur travaille comme {}"),
        ("je travaille à ", MemoryKind::Fact, "L'utilisateur travaille à {}"),
        ("je n'aime pas ", MemoryKind::Preference, "L'utilisateur n'aime pas {}"),
        ("je déteste ", MemoryKind::Preference, "L'utilisateur déteste {}"),
        ("je deteste ", MemoryKind::Preference, "L'utilisateur déteste {}"),
        ("j'adore ", MemoryKind::Preference, "L'utilisateur adore {}"),
        ("j'aime ", MemoryKind::Preference, "L'utilisateur aime {}"),
        ("je préfère ", MemoryKind::Preference, "L'utilisateur préfère {}"),
        ("je prefere ", MemoryKind::Preference, "L'utilisateur préfère {}"),
    ];
    for (prefix, kind, template) in rules {
        if let Some(tail) = tail_after_prefix(text, &lead, prefix) {
            out.push((*kind, template.replace("{}", tail)));
            return out; // one inference per utterance, like English
        }
    }
    // "ma couleur préférée est le bleu": the noun comes before "préféré(e)", so keep the
    // whole statement.
    let lc = clean(&lead);
    if (lc.starts_with("mon ") || lc.starts_with("ma ") || lc.starts_with("mes "))
        && (lc.contains(" préféré est ")
            || lc.contains(" préférée est ")
            || lc.contains(" prefere est ")
            || lc.contains(" préférés est ")
            || lc.contains(" préférées est "))
    {
        let original = clean(
            text.trim()
                .trim_start_matches(|c: char| !c.is_alphanumeric()),
        );
        out.push((
            MemoryKind::Preference,
            format!("Préférence de l'utilisateur : {original}"),
        ));
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
    fn recognizes_who_am_i_questions_only_when_whole() {
        for q in [
            "Who am I?",
            "What's my name?",
            "what is my name",
            "Hey Jarvis, who am I?",
            "Do you know who I am?",
            "Do you recognise my voice?",
            "who\u{2019}s talking",
        ] {
            assert_eq!(parse_command(q), Some(MemoryCommand::WhoAmI), "{q:?}");
        }
        for q in [
            "What's my name on the booking?",
            "who am I meeting tomorrow",
            "my name is Sam",
        ] {
            assert_ne!(parse_command(q), Some(MemoryCommand::WhoAmI), "{q:?}");
        }
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

    #[test]
    fn portuguese_commands_parse_and_report_their_language() {
        use crate::lang::Lang;
        let both = [Lang::En, Lang::PtBr];
        assert_eq!(
            parse_command_in("Lembra que a minha cor favorita é azul.", &both),
            Some((
                MemoryCommand::Remember {
                    kind: MemoryKind::Preference,
                    content: "a minha cor favorita é azul".to_string()
                },
                Lang::PtBr
            ))
        );
        assert_eq!(
            parse_command_pt("Lembre-se de que o dentista é na terça"),
            Some(MemoryCommand::Remember {
                kind: MemoryKind::Fact,
                content: "o dentista é na terça".to_string()
            })
        );
        assert_eq!(
            parse_command_pt("Esquece isso."),
            Some(MemoryCommand::ForgetLast)
        );
        assert_eq!(
            parse_command_pt("esqueça sobre o dentista"),
            Some(MemoryCommand::ForgetMatching("o dentista".to_string()))
        );
        assert_eq!(
            parse_command_pt("Meu nome é Paulo."),
            Some(MemoryCommand::NameSpeaker("Paulo".to_string()))
        );
        assert_eq!(
            parse_command_pt("Quem sou eu?"),
            Some(MemoryCommand::WhoAmI)
        );
        assert_eq!(
            parse_command_pt("Ei, você sabe quem eu sou?"),
            Some(MemoryCommand::WhoAmI)
        );
        // Questions and ordinary speech don't match.
        for s in [
            "Você lembra o que eu disse?",
            "não esquece de comprar leite",
            "que horas são",
            "qual é o nome daquele filme",
        ] {
            assert_eq!(parse_command_pt(s), None, "{s}");
        }
        // English-only (the default) never matches Portuguese, and vice versa.
        assert_eq!(
            parse_command_in("lembra que eu gosto de café", &[Lang::En]),
            None
        );
        assert_eq!(parse_command_pt("remember that I like tea"), None);
        assert_eq!(
            parse_command_in("remember that I like tea", &both).map(|c| c.1),
            Some(Lang::En)
        );
    }

    #[test]
    fn portuguese_inferred_capture_is_stored_as_spoken() {
        assert_eq!(
            infer_memories_pt("Eu moro em Campinas."),
            vec![(MemoryKind::Fact, "O usuário mora em Campinas".to_string())]
        );
        assert_eq!(
            infer_memories_pt("Gosto de pão de queijo"),
            vec![(
                MemoryKind::Preference,
                "O usuário gosta de pão de queijo".to_string()
            )]
        );
        assert_eq!(
            infer_memories_pt("Minha cor favorita é azul."),
            vec![(
                MemoryKind::Preference,
                "Preferência do usuário: Minha cor favorita é azul".to_string()
            )]
        );
        assert!(infer_memories_pt("que horas são").is_empty());
        assert!(infer_memories_pt("I live in Austin").is_empty());
    }

    #[test]
    fn parses_french_commands() {
        use crate::lang::Lang;
        assert_eq!(
            parse_command_fr("Rappelle-toi que le dentiste est mardi"),
            Some(MemoryCommand::Remember {
                kind: MemoryKind::Fact,
                content: "le dentiste est mardi".to_string()
            })
        );
        assert_eq!(
            parse_command_fr("souviens-toi que j'aime le café noir"),
            Some(MemoryCommand::Remember {
                kind: MemoryKind::Preference,
                content: "j'aime le café noir".to_string()
            })
        );
        assert_eq!(
            parse_command_fr("Oublie ça."),
            Some(MemoryCommand::ForgetLast)
        );
        assert_eq!(
            parse_command_fr("oublie que le dentiste est mardi"),
            Some(MemoryCommand::ForgetMatching("le dentiste est mardi".to_string()))
        );
        assert_eq!(
            parse_command_fr("Je m'appelle Paul."),
            Some(MemoryCommand::NameSpeaker("Paul".to_string()))
        );
        // Typographic apostrophe (U+2019), as STT emits, matches the same prefixes.
        assert_eq!(
            parse_command_fr("Je m\u{2019}appelle Paul."),
            Some(MemoryCommand::NameSpeaker("Paul".to_string()))
        );
        assert_eq!(
            parse_command_fr("N\u{2019}oublie pas que le dentiste est mardi"),
            Some(MemoryCommand::Remember {
                kind: MemoryKind::Fact,
                content: "le dentiste est mardi".to_string()
            })
        );
        // "appelle-moi …" + a non-name is not a naming command.
        assert_eq!(parse_command_fr("appelle-moi un taxi"), None);
        assert_eq!(parse_command_fr("appelle-moi quand c'est prêt"), None);
        assert_eq!(parse_command_fr("Qui suis-je?"), Some(MemoryCommand::WhoAmI));
        assert_eq!(
            parse_command_fr("Hé, est-ce que tu sais qui je suis?"),
            Some(MemoryCommand::WhoAmI)
        );
        // Questions and ordinary speech don't match.
        for s in [
            "Est-ce que tu te souviens de ce que j'ai dit?",
            "quelle heure est-il",
            "quel est le nom de ce film",
        ] {
            assert_eq!(parse_command_fr(s), None, "{s}");
        }
        // Cross-language isolation.
        assert_eq!(parse_command_fr("remember that I like tea"), None);
        assert_eq!(
            parse_command_in("rappelle-toi que j'aime le thé", &[Lang::En]),
            None
        );
    }

    #[test]
    fn french_inferred_capture_is_stored_as_spoken() {
        assert_eq!(
            infer_memories_fr("J'habite à Montréal."),
            vec![(MemoryKind::Fact, "L'utilisateur habite à Montréal".to_string())]
        );
        // Typographic apostrophe folds to ASCII before matching.
        assert_eq!(
            infer_memories_fr("J\u{2019}habite à Québec."),
            vec![(MemoryKind::Fact, "L'utilisateur habite à Québec".to_string())]
        );
        assert_eq!(
            infer_memories_fr("J'aime la poutine"),
            vec![(MemoryKind::Preference, "L'utilisateur aime la poutine".to_string())]
        );
        assert_eq!(
            infer_memories_fr("Ma couleur préférée est le bleu."),
            vec![(
                MemoryKind::Preference,
                "Préférence de l'utilisateur : Ma couleur préférée est le bleu".to_string()
            )]
        );
        assert!(infer_memories_fr("quelle heure est-il").is_empty());
        assert!(infer_memories_fr("I live in Austin").is_empty());
    }
}
