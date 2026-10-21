//! Text language identification over the enabled languages (`whatlang`, pure Rust).
//!
//! Only consulted when the STT engine reports no language (Wyoming servers that don't
//! send one), and for the first-reply-sentence voice check. Short text is unreliable
//! for any text-based detector, so anything under [`MIN_WORDS`] words is "unknown"
//! rather than a guess.

use super::Lang;

/// Fewer words than this and text ID declines (one- and two-word utterances like
/// "stop" / "para" are exactly where trigram detectors flip).
pub const MIN_WORDS: usize = 3;

/// The minimum `whatlang` confidence (0–1: how far the winner leads the runner-up)
/// needed to switch **away from the household default**. Detection is asymmetric on
/// purpose: `whatlang` picks the right language on short commands but reports a low
/// lead for many plain English sentences ("set a timer for ten minutes" ≈ 0.07), while
/// Portuguese text scores ≈ 1.0. So the default language is accepted on any lead, and a
/// non-default one needs a clear lead — a weak call never moves an English turn to
/// Portuguese (the "English must not regress" gate).
pub const MIN_CONFIDENCE: f64 = 0.5;

fn to_whatlang(l: Lang) -> whatlang::Lang {
    match l {
        Lang::En => whatlang::Lang::Eng,
        Lang::PtBr => whatlang::Lang::Por,
    }
}

fn from_whatlang(l: whatlang::Lang) -> Option<Lang> {
    match l {
        whatlang::Lang::Eng => Some(Lang::En),
        whatlang::Lang::Por => Some(Lang::PtBr),
        _ => None,
    }
}

/// Detect which of `allowed` languages `text` is in, with the detector's confidence.
/// `None` when fewer than two languages are allowed, the text is shorter than
/// [`MIN_WORDS`] words, or a non-`default` winner leads by less than
/// [`MIN_CONFIDENCE`].
pub fn detect_text(text: &str, allowed: &[Lang], default: Lang) -> Option<(Lang, f64)> {
    if allowed.len() < 2 {
        return None;
    }
    let words = text
        .split_whitespace()
        .filter(|w| w.chars().any(char::is_alphabetic))
        .count();
    if words < MIN_WORDS {
        return None;
    }
    let detector =
        whatlang::Detector::with_allowlist(allowed.iter().copied().map(to_whatlang).collect());
    let info = detector.detect(text)?;
    let lang = from_whatlang(info.lang())?;
    if lang != default && info.confidence() < MIN_CONFIDENCE {
        return None;
    }
    Some((lang, info.confidence()))
}

/// High-frequency **function words** of each language — what a transcript is actually
/// made of, independent of names and borrowed words. Words that exist in both languages
/// are excluded ("a", "me", "do", "as", "no", "um", "os", "com", "se"…), so a hit is
/// evidence for exactly one language.
const FUNCTION_WORDS: &[(Lang, &[&str])] = &[
    (
        Lang::En,
        &[
            "the", "is", "are", "was", "were", "what", "what's", "whats", "how", "does", "you",
            "your", "i", "i'm", "my", "in", "on", "of", "to", "and", "for", "with", "it", "it's",
            "this", "that", "that's", "can", "about", "please", "there", "when", "where", "who",
            "why", "will", "would", "could", "should", "from", "at", "an", "be", "have", "has",
            "we", "our", "they", "their", "tell", "say", "much", "many", "any", "some", "if", "or",
            "not", "don't", "did", "today", "tomorrow", "set", "turn", "off",
        ],
    ),
    (
        Lang::PtBr,
        &[
            "o", "de", "da", "dos", "das", "que", "é", "em", "na", "nos", "nas", "uma", "para",
            "pra", "não", "nao", "por", "eu", "você", "voce", "como", "meu", "minha", "qual",
            "quem", "onde", "quando", "porque", "isso", "esse", "essa", "está", "esta", "tá",
            "são", "ao", "à", "mais", "muito", "também", "tambem", "foi", "ser", "tem", "hoje",
            "amanhã", "amanha", "agora", "quanto", "quantos", "sobre", "seu", "sua", "ela", "ele",
            "nós", "gente",
        ],
    ),
];

/// A decisive verdict from [`FUNCTION_WORDS`]: the language whose function words make up
/// the transcript, when the count is lopsided — the winner has ≥ 2 and the other none,
/// or ≥ 3 and three times the other's. `None` when it's close or there are too few.
///
/// This is the cross-check on an STT engine's language label: measured live, ElevenLabs
/// labelled the English question "How do you say good morning in Portuguese?" as
/// Portuguese; its words (how/you/in) settle it as English.
pub fn function_word_language(text: &str, allowed: &[Lang]) -> Option<Lang> {
    if allowed.len() < 2 {
        return None;
    }
    let lower = text.to_lowercase().replace('\u{2019}', "'");
    let words: Vec<&str> = lower
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .map(|w| w.trim_matches('\''))
        .filter(|w| !w.is_empty())
        .collect();
    let mut counts: Vec<(Lang, usize)> = FUNCTION_WORDS
        .iter()
        .filter(|(l, _)| allowed.contains(l))
        .map(|(l, set)| (*l, words.iter().filter(|w| set.contains(w)).count()))
        .collect();
    counts.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    let (winner, top) = *counts.first()?;
    let runner = counts.get(1).map_or(0, |c| c.1);
    let decisive = (top >= 2 && runner == 0) || (top >= 3 && top >= 3 * runner);
    decisive.then_some(winner)
}

/// Whole-utterance short phrases that are unmistakably one language. A one- or
/// two-word command carries too little audio for an STT engine to tell the language
/// reliably (whisper.cpp labels a lone "para" — *stop* — as English), so an exact match
/// here decides the turn's language ahead of the engine's label. The lists are
/// **disjoint**: no entry is a word of the other language (British "oi" is left out),
/// so an English utterance can never be moved to Portuguese by this table.
const SHORT_PHRASES: &[(Lang, &[&str])] = &[
    (
        Lang::PtBr,
        &[
            "para",
            "pare",
            "parar",
            "chega",
            "cancela",
            "cancelar",
            "obrigado",
            "obrigada",
            "muito obrigado",
            "muito obrigada",
            "valeu",
            "tchau",
            "até logo",
            "ate logo",
            "sim",
            "não",
            "nao",
            "olá",
            "ola",
            "bom dia",
            "boa tarde",
            "boa noite",
            "de nada",
            "por favor",
            "deixa pra lá",
            "deixa pra la",
            "deixa",
            "esquece",
            "mais nada",
            "só isso",
            "so isso",
            "é só isso",
            "e so isso",
            "tudo bem",
            "beleza",
            "pode parar",
            "próxima",
            "proxima",
            "próximo",
            "proximo",
            "continua",
            "continuar",
            "pausa",
            "desliga",
            "liga",
            "silêncio",
            "silencio",
            "que horas são",
            "que horas sao",
        ],
    ),
    (
        Lang::En,
        &[
            "stop",
            "cancel",
            "thanks",
            "thank you",
            "thanks a lot",
            "goodbye",
            "bye",
            "yes",
            "nope",
            "hello",
            "never mind",
            "nevermind",
            "that's all",
            "thats all",
            "nothing else",
            "pause",
            "resume",
            "next",
            "good morning",
            "good night",
            "what time is it",
        ],
    ),
];

/// An exact whole-utterance match in [`SHORT_PHRASES`] for an enabled language
/// (case-, punctuation- and surrounding-filler-insensitive).
pub fn short_phrase(text: &str, allowed: &[Lang]) -> Option<Lang> {
    if allowed.len() < 2 {
        return None;
    }
    let norm: String = text
        .to_lowercase()
        .replace('\u{2019}', "'")
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '\'' || c == ' ' {
                c
            } else {
                ' '
            }
        })
        .collect();
    let norm = norm.split_whitespace().collect::<Vec<_>>().join(" ");
    let norm = norm
        .trim_end_matches(" jarvis")
        .trim_start_matches("jarvis ");
    SHORT_PHRASES
        .iter()
        .find(|(l, phrases)| allowed.contains(l) && phrases.contains(&norm))
        .map(|(l, _)| *l)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOTH: &[Lang] = &[Lang::En, Lang::PtBr];

    #[test]
    fn detects_full_sentences_in_either_language() {
        for s in [
            "tell me a short joke about cats",
            "what is the weather like tomorrow",
            "remember that my favorite color is blue",
            "Sure! Here's one: why did the cat sit on the computer?",
            "set a timer for ten minutes",
            "Hey Jarvis what is a samba",
        ] {
            assert_eq!(
                detect_text(s, BOTH, Lang::En).map(|d| d.0),
                Some(Lang::En),
                "{s}"
            );
        }
        for s in [
            "me conta uma piada curta sobre gatos",
            "qual é a previsão do tempo para amanhã",
            "lembra que a minha cor favorita é azul",
            "Claro! Aqui vai uma: por que o gato sentou no computador?",
            "o que é o Super Bowl",
        ] {
            assert_eq!(
                detect_text(s, BOTH, Lang::En).map(|d| d.0),
                Some(Lang::PtBr),
                "{s}"
            );
        }
    }

    #[test]
    fn function_words_are_disjoint_and_decide_lopsided_transcripts() {
        let (en, pt) = (FUNCTION_WORDS[0].1, FUNCTION_WORDS[1].1);
        for w in pt {
            assert!(!en.contains(w), "{w} is in both lists");
        }
        let fw = |s| function_word_language(s, BOTH);
        // The live miss: English audio the engine labelled Portuguese.
        assert_eq!(
            fw("How do you say \"good morning\" in Portuguese?"),
            Some(Lang::En)
        );
        assert_eq!(fw("what is the capital of australia"), Some(Lang::En));
        assert_eq!(fw("Como se diz bom dia em inglês?"), Some(Lang::PtBr));
        assert_eq!(fw("o que é o Super Bowl"), Some(Lang::PtBr));
        assert_eq!(
            fw("Responde em inglês, qual é a capital do Brasil?"),
            Some(Lang::PtBr)
        );
        // Names and borrowed words alone are not decisive.
        assert_eq!(fw("play Garota de Ipanema by Tom Jobim"), None);
        assert_eq!(fw("Samsung TV"), None);
        assert_eq!(
            function_word_language("what is the capital", &[Lang::En]),
            None
        );
    }

    #[test]
    fn short_phrases_are_disjoint_and_exact() {
        let (pt, en) = (SHORT_PHRASES[0].1, SHORT_PHRASES[1].1);
        for p in pt {
            assert!(!en.contains(p), "{p} is in both lists");
        }
        assert_eq!(short_phrase("Para.", BOTH), Some(Lang::PtBr));
        assert_eq!(short_phrase("Obrigado!", BOTH), Some(Lang::PtBr));
        assert_eq!(short_phrase("deixa pra lá", BOTH), Some(Lang::PtBr));
        assert_eq!(short_phrase("Stop.", BOTH), Some(Lang::En));
        assert_eq!(short_phrase("Thank you, Jarvis", BOTH), Some(Lang::En));
        // Only whole utterances; and never in single-language mode.
        assert_eq!(short_phrase("para o timer", BOTH), None);
        assert_eq!(short_phrase("stop the music", BOTH), None);
        assert_eq!(short_phrase("para", &[Lang::En]), None);
    }

    #[test]
    fn declines_short_text_and_single_language() {
        assert_eq!(detect_text("stop", BOTH, Lang::En), None);
        assert_eq!(detect_text("que horas", BOTH, Lang::En), None);
        assert_eq!(
            detect_text("tell me a joke please", &[Lang::En], Lang::En),
            None
        );
    }
}
