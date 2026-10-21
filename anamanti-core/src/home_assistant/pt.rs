//! The Brazilian-Portuguese half of the Home Assistant fast path
//! (`plans/BilingualPlan.md` §2.6).
//!
//! [`parse`] recognizes a Portuguese home command — "desliga a luz do escritório",
//! "acende as luzes da varanda", "apaga tudo", "diminui a luz da sala para 30 por cento",
//! "a luz da varanda está acesa?" — and, like the English matcher, does no I/O. Because
//! the household's Home Assistant names are usually English, it returns a list of
//! **English-shaped target candidates** for the existing resolver
//! ([`super::catalog::resolve`]): Portuguese room words map to their usual English area
//! names ("escritório" → "office"), device nouns to the resolver's vocabulary ("luz" →
//! "light"), and the raw phrase is always tried too (an HA set up with Portuguese names).
//! The hub acts only if exactly one distinct target resolves.
//!
//! [`say`] speaks the confirmation in Portuguese using the user's own target phrase
//! ("Pronto, apaguei a luz do escritório."), with gender/number agreement taken from the
//! article they used.

use super::catalog::Entity;
use super::matcher::{normalize, Verb};
use super::Done;

/// Spoken when HA can't be reached or rejects a call.
pub const UNREACHABLE: &str = "Desculpe, não consegui falar com o Home Assistant.";

/// The longest target phrase (in words) worth resolving.
const MAX_TARGET_WORDS: usize = 7;

/// Which verb family the user said, so the reply mirrors it ("apaga" → "apaguei").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Said {
    Liga,
    Desliga,
    Acende,
    Apaga,
    Other,
}

/// A parsed Portuguese home command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtCommand {
    pub verb: Verb,
    /// English-shaped target phrases to try, most specific first.
    pub candidates: Vec<String>,
    /// The target as the user said it (with its article), for the reply.
    pub phrase: String,
    /// Grammatical gender of the target (from the article/noun): `true` = feminine.
    pub feminine: bool,
    /// Plural target ("as luzes", "todas as luzes").
    pub plural: bool,
    pub said: Said,
}

/// Fold Portuguese accents (é → e, ç → c, ã → a, …) for keyword matching.
pub fn fold(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'á' | 'à' | 'â' | 'ã' | 'ä' => 'a',
            'é' | 'ê' | 'è' | 'ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' => 'i',
            'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
            'ú' | 'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            other => other,
        })
        .collect()
}

const LEADING_FILLERS: &[&str] = &[
    "voce pode por favor",
    "voce poderia",
    "voce pode",
    "por favor",
    "poderia",
    "pode",
    "consegue",
    "da pra",
    "jarvis",
    "ei",
    "oi",
    "ok",
    "so",
    "e",
    "tambem",
];

const TRAILING_FILLERS: &[&str] = &[
    "por favor",
    "obrigado",
    "obrigada",
    "pra mim",
    "para mim",
    "agora",
    "valeu",
];

/// Strip leading/trailing fillers from parallel (original, folded) word lists.
fn strip_fillers(mut words: Vec<(String, String)>) -> Vec<(String, String)> {
    loop {
        let before = words.len();
        for filler in LEADING_FILLERS {
            let f: Vec<&str> = filler.split(' ').collect();
            if words.len() > f.len() && words.iter().zip(&f).all(|(w, f)| w.1 == *f) {
                words.drain(..f.len());
                break;
            }
        }
        for filler in TRAILING_FILLERS {
            let f: Vec<&str> = filler.split(' ').collect();
            if words.len() > f.len()
                && words[words.len() - f.len()..]
                    .iter()
                    .zip(&f)
                    .all(|(w, f)| w.1 == *f)
            {
                words.truncate(words.len() - f.len());
                break;
            }
        }
        if words.len() == before {
            return words;
        }
    }
}

/// Classify the leading verb.
fn verb_of(w: &str) -> Option<(Verb, Said)> {
    Some(match w {
        "liga" | "ligue" | "ligar" => (Verb::On, Said::Liga),
        "acende" | "acenda" | "acender" => (Verb::On, Said::Acende),
        "desliga" | "desligue" | "desligar" => (Verb::Off, Said::Desliga),
        "apaga" | "apague" | "apagar" => (Verb::Off, Said::Apaga),
        "ativa" | "ative" | "ativar" | "executa" | "execute" | "executar" | "roda" | "rode"
        | "rodar" | "inicia" | "inicie" | "iniciar" | "aciona" | "acione" => {
            (Verb::Activate, Said::Other)
        }
        _ => return None,
    })
}

/// Brightness verbs ("diminui a luz da sala para 30 por cento").
fn is_brightness_verb(w: &str) -> bool {
    matches!(
        w,
        "diminui"
            | "diminua"
            | "abaixa"
            | "abaixe"
            | "aumenta"
            | "aumente"
            | "ajusta"
            | "ajuste"
            | "coloca"
            | "coloque"
            | "poe"
            | "ponha"
            | "deixa"
            | "deixe"
            | "muda"
            | "mude"
            | "regula"
            | "regule"
            | "define"
            | "defina"
    )
}

/// The on/off state adjectives of a status question → `true` = asks "on?".
fn state_word(w: &str) -> Option<bool> {
    match w {
        "ligado" | "ligada" | "ligados" | "ligadas" | "aceso" | "acesa" | "acesos" | "acesas" => {
            Some(true)
        }
        "desligado" | "desligada" | "desligados" | "desligadas" | "apagado" | "apagada"
        | "apagados" | "apagadas" => Some(false),
        _ => None,
    }
}

fn is_be(w: &str) -> bool {
    matches!(w, "esta" | "estao" | "ta" | "tao" | "fica" | "ficou")
}

/// Portuguese number words used for brightness.
fn number_word(w: &str) -> Option<u32> {
    Some(match w {
        "zero" => 0,
        "cinco" => 5,
        "dez" => 10,
        "quinze" => 15,
        "vinte" => 20,
        "vinte e cinco" => 25,
        "trinta" => 30,
        "quarenta" => 40,
        "cinquenta" => 50,
        "sessenta" => 60,
        "setenta" => 70,
        "setenta e cinco" => 75,
        "oitenta" => 80,
        "noventa" => 90,
        "cem" => 100,
        "metade" => 50,
        "maximo" | "total" => 100,
        "minimo" => 1,
        _ => return None,
    })
}

/// Parse a brightness tail (`30`, `30 por cento`, `30 percent` — `%` normalizes to
/// "percent" — `cinquenta por cento`, `metade`). The whole tail must be consumed.
fn parse_percent(tail: &[String]) -> Option<u8> {
    let mut words: Vec<&str> = tail.iter().map(String::as_str).collect();
    if words.ends_with(&["por", "cento"]) {
        words.truncate(words.len() - 2);
    } else if words.last() == Some(&"porcento") || words.last() == Some(&"percent") {
        words.pop();
    }
    if words.first() == Some(&"o") {
        words.remove(0); // "no máximo" → "o máximo" after the preposition
    }
    let joined = words.join(" ");
    let value = joined
        .parse::<u32>()
        .ok()
        .or_else(|| number_word(&joined))?;
    (value <= 100).then_some(value as u8)
}

/// Portuguese room words → their usual English Home Assistant area names (folded keys).
const ROOMS: &[(&str, &[&str])] = &[
    ("sala de estar", &["living room", "lounge"]),
    ("sala de jantar", &["dining room", "dining"]),
    ("sala de tv", &["tv room", "media room", "family room"]),
    (
        "sala de televisao",
        &["tv room", "media room", "family room"],
    ),
    (
        "quarto principal",
        &["master bedroom", "primary bedroom", "main bedroom"],
    ),
    ("quarto de hospedes", &["guest room", "guest bedroom"]),
    ("quarto das criancas", &["kids room", "kids bedroom"]),
    ("area externa", &["outside", "outdoor", "backyard"]),
    ("area de servico", &["laundry room", "utility room"]),
    ("escritorio", &["office", "study"]),
    ("cozinha", &["kitchen"]),
    ("sala", &["living room", "lounge", "family room"]),
    ("quarto", &["bedroom"]),
    ("banheiro", &["bathroom"]),
    ("lavabo", &["powder room", "bathroom"]),
    ("varanda", &["porch", "deck", "balcony"]),
    ("sacada", &["balcony"]),
    ("garagem", &["garage"]),
    ("corredor", &["hallway", "hall"]),
    ("entrada", &["entry", "entryway", "front door", "foyer"]),
    ("porao", &["basement"]),
    ("lavanderia", &["laundry room", "laundry"]),
    ("quintal", &["backyard", "yard"]),
    ("jardim", &["garden"]),
    ("deck", &["deck"]),
    ("escada", &["stairs", "stairway"]),
    ("sotao", &["attic"]),
    ("closet", &["closet"]),
    ("academia", &["gym"]),
];

/// Device nouns → (the resolver's English word, feminine?). Lights map to "light"
/// (the resolver singularizes, so "lights" and "light" are the same key).
fn noun_en(w: &str) -> Option<(&'static str, bool)> {
    Some(match w {
        "luz" | "luzes" | "lampada" | "lampadas" | "iluminacao" => ("light", true),
        "abajur" | "abajures" | "luminaria" | "luminarias" => ("lamp", false),
        "ventilador" | "ventiladores" => ("fan", false),
        "tomada" | "tomadas" => ("plug", true),
        "tv" | "televisao" | "televisor" => ("tv", true),
        "lareira" => ("fireplace", true),
        "interruptor" => ("switch", false),
        "ar" => ("ac", false),
        _ => return None,
    })
}

const PREPOSITIONS: &[&str] = &[
    "de", "do", "da", "dos", "das", "no", "na", "nos", "nas", "em",
];

/// Article → (feminine, plural).
fn article(w: &str) -> Option<(bool, bool)> {
    match w {
        "o" => Some((false, false)),
        "a" => Some((true, false)),
        "os" => Some((false, true)),
        "as" => Some((true, true)),
        _ => None,
    }
}

/// Build the target candidates + spoken phrase + agreement from the target words.
fn target(words: &[(String, String)]) -> Option<(Vec<String>, String, bool, bool)> {
    if words.is_empty() || words.len() > MAX_TARGET_WORDS {
        return None;
    }
    let phrase = words
        .iter()
        .map(|w| w.0.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let folded: Vec<&str> = words.iter().map(|w| w.1.as_str()).collect();

    // "tudo" / "todas as luzes (da casa)" → every light.
    let all = matches!(folded.as_slice(), ["tudo"])
        || matches!(
            folded.as_slice(),
            ["todas", "as", n, ..] | ["todos", "os", n, ..] if noun_en(n).map(|x| x.0) == Some("light")
        );
    if all {
        return Some((vec!["all lights".to_string()], phrase, true, true));
    }

    // Leading article → agreement; the rest is the bare target.
    let (mut feminine, mut plural, rest) = match folded.first().and_then(|w| article(w)) {
        Some((f, p)) => (f, p, &folded[1..]),
        None => (false, false, &folded[..]),
    };
    if rest.is_empty() {
        return None;
    }

    let mut candidates: Vec<String> = Vec::new();
    let mut push = |c: String| {
        let c = c.split_whitespace().collect::<Vec<_>>().join(" ");
        if !c.is_empty() && !candidates.contains(&c) {
            candidates.push(c);
        }
    };

    if let Some((noun, fem)) = noun_en(rest[0]) {
        if folded.first().and_then(|w| article(w)).is_none() {
            feminine = fem;
            plural = rest[0].ends_with('s') && rest[0] != "tv";
        }
        let tail = &rest[1..];
        match tail.split_first() {
            // "luz do escritório" / "ventilador do quarto" / "luz do Brendon"
            Some((prep, owner)) if PREPOSITIONS.contains(prep) && !owner.is_empty() => {
                let owner = owner.join(" ");
                let mut matched_room = false;
                for (pt_room, en_rooms) in ROOMS {
                    if owner == *pt_room {
                        matched_room = true;
                        for en in *en_rooms {
                            push(format!("{en} {noun}"));
                        }
                    }
                }
                // An HA area named in Portuguese, or a person's device ("luz do Brendon"
                // → "brendons light", as the resolver normalizes "Brendon's light").
                push(format!("{owner} {noun}"));
                if !matched_room {
                    push(format!("{owner}s {noun}"));
                }
            }
            // A bare noun: "a luz" → this display's room; "a TV" → a unique TV.
            None => push(noun.to_string()),
            // "luz principal", "tv samsung" … → try the noun-first and noun-last orders.
            Some(_) => {
                let rest_words = tail.join(" ");
                push(format!("{rest_words} {noun}"));
                push(format!("{noun} {rest_words}"));
            }
        }
    }
    // Always try the phrase itself (HA names in Portuguese, or English device names said
    // inside a Portuguese sentence: "liga a Samsung TV").
    push(rest.join(" "));
    let raw_unfolded: Vec<&str> = words
        .iter()
        .skip(if folded.first().and_then(|w| article(w)).is_some() {
            1
        } else {
            0
        })
        .map(|w| w.0.as_str())
        .collect();
    push(raw_unfolded.join(" "));

    Some((candidates, phrase, feminine, plural))
}

/// Recognize a Portuguese home command. `None` for anything else (falls through).
pub fn parse(transcript: &str) -> Option<PtCommand> {
    let norm = normalize(transcript);
    let words: Vec<(String, String)> = norm
        .split(' ')
        .filter(|w| !w.is_empty())
        .map(|w| (w.to_string(), fold(w)))
        .collect();
    let words = strip_fillers(words);
    let n = words.len();
    if n < 2 {
        return None;
    }
    let first = words[0].1.as_str();

    // On / off / activate: VERB [a cena | o script | a rotina] TARGET
    if let Some((verb, said)) = verb_of(first) {
        let mut rest = &words[1..];
        if verb == Verb::Activate {
            while let Some(w) = rest.first() {
                if matches!(
                    w.1.as_str(),
                    "a" | "o" | "cena" | "script" | "rotina" | "automacao" | "modo"
                ) {
                    rest = &rest[1..];
                } else {
                    break;
                }
            }
        }
        let (candidates, phrase, feminine, plural) = target(rest)?;
        return Some(PtCommand {
            verb,
            candidates,
            phrase,
            feminine,
            plural,
            said,
        });
    }

    // Brightness: VERB TARGET (para|pra|em|a|no) N [por cento]
    if is_brightness_verb(first) {
        // The LAST "para/pra/em/até/no" before the number: the target itself may contain
        // prepositions ("a luz da sala"), so scan from the right.
        let to = (1..n)
            .rev()
            .find(|&i| matches!(words[i].1.as_str(), "para" | "pra" | "em" | "ate" | "no"))?;
        let tail: Vec<String> = words[to + 1..].iter().map(|w| w.1.clone()).collect();
        let pct = parse_percent(&tail)?;
        let (candidates, phrase, feminine, plural) = target(&words[1..to])?;
        return Some(PtCommand {
            verb: Verb::Brightness(pct),
            candidates,
            phrase,
            feminine,
            plural,
            said: Said::Other,
        });
    }

    // Status, target first: "(a luz da varanda) (está) (acesa)?"
    let last = words[n - 1].1.as_str();
    if n >= 3 && state_word(last).is_some() && is_be(&words[n - 2].1) {
        let (candidates, phrase, feminine, plural) = target(&words[..n - 2])?;
        return Some(PtCommand {
            verb: Verb::Status,
            candidates,
            phrase,
            feminine,
            plural,
            said: Said::Other,
        });
    }
    // Status, verb first: "(está) (acesa) (a luz da varanda)?"
    if n >= 3 && is_be(first) && state_word(&words[1].1).is_some() {
        let (candidates, phrase, feminine, plural) = target(&words[2..])?;
        return Some(PtCommand {
            verb: Verb::Status,
            candidates,
            phrase,
            feminine,
            plural,
            said: Said::Other,
        });
    }
    None
}

/// Uppercase the first character.
fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// "ligado/ligada/ligados/ligadas" (or "aceso…" / "desligado…" / "apagado…").
fn adjective(stem_on: &str, stem_off: &str, on: bool, feminine: bool, plural: bool) -> String {
    let stem = if on { stem_on } else { stem_off };
    format!(
        "{stem}{}{}",
        if feminine { "a" } else { "o" },
        if plural { "s" } else { "" }
    )
}

/// A few HA state strings in Portuguese (others are spoken as HA reports them).
fn state_pt(state: &str) -> Option<&'static str> {
    Some(match state {
        "playing" => "tocando",
        "paused" => "pausada",
        "idle" => "parada",
        "standby" => "em espera",
        "open" => "aberta",
        "closed" => "fechada",
        _ => return None,
    })
}

/// Speak a performed command in Portuguese.
pub fn say(done: &Done, cmd: &PtCommand) -> String {
    let phrase = &cmd.phrase;
    let (fem, pl) = (cmd.feminine, cmd.plural);
    // Lights read better as "acesa/apagada".
    let light_words = matches!(cmd.said, Said::Acende | Said::Apaga);
    match done {
        Done::Switched { on, .. } => {
            let verb = match (on, cmd.said) {
                (true, Said::Acende) => "acendi",
                (false, Said::Apaga) => "apaguei",
                (true, _) => "liguei",
                (false, _) => "desliguei",
            };
            format!("Pronto, {verb} {phrase}.")
        }
        Done::Activated { entity } => {
            if entity.domain() == "script" {
                format!("Executando {}.", entity.name)
            } else {
                format!("Pronto, ativei {}.", entity.name)
            }
        }
        Done::Brightness { pct, .. } => format!("Pronto, {phrase} em {pct} por cento."),
        Done::Unavailable { .. } => unavailable(phrase, pl),
        Done::Status { target, now } => {
            let reachable: Vec<&Entity> = now.iter().filter(|e| e.available()).collect();
            if reachable.is_empty() {
                return unavailable(phrase, pl);
            }
            let is_on = |e: &Entity| !matches!(e.state.as_str(), "off" | "idle" | "standby");
            let (stem_on, stem_off) =
                if light_words || cmd.candidates.iter().any(|c| c.ends_with("light")) {
                    ("aces", "apagad")
                } else {
                    ("ligad", "desligad")
                };
            let be = if pl { "estão" } else { "está" };
            if !target.group {
                if let [e] = reachable.as_slice() {
                    if !matches!(e.state.as_str(), "on" | "off") {
                        let st = state_pt(&e.state)
                            .map(str::to_string)
                            .unwrap_or_else(|| e.state.replace('_', " "));
                        return format!("{} {be} {st}.", capitalize(phrase));
                    }
                }
                let on = reachable.iter().any(|e| is_on(e));
                return format!(
                    "{} {be} {}.",
                    capitalize(phrase),
                    adjective(stem_on, stem_off, on, fem, pl)
                );
            }
            let on = reachable.iter().filter(|e| is_on(e)).count();
            let total = reachable.len();
            if on == 0 || on == total {
                format!(
                    "{} estão {}.",
                    capitalize(phrase),
                    adjective(stem_on, stem_off, on == total, fem, true)
                )
            } else {
                format!(
                    "{on} de {total} estão {}.",
                    adjective(stem_on, stem_off, true, fem, true)
                )
            }
        }
    }
}

fn unavailable(phrase: &str, plural: bool) -> String {
    if plural {
        format!("{} não estão respondendo agora.", capitalize(phrase))
    } else {
        format!("{} não está respondendo agora.", capitalize(phrase))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Option<(Verb, Vec<String>, String)> {
        parse(s).map(|c| (c.verb, c.candidates, c.phrase))
    }

    #[test]
    fn on_off_with_rooms_maps_to_english_area_candidates() {
        let c = parse("Desliga a luz do escritório.").unwrap();
        assert_eq!(c.verb, Verb::Off);
        assert_eq!(c.said, Said::Desliga);
        assert_eq!(c.phrase, "a luz do escritório");
        assert!(c.feminine && !c.plural);
        assert_eq!(c.candidates[0], "office light");
        assert!(c.candidates.contains(&"study light".to_string()));
        assert!(c.candidates.contains(&"escritorio light".to_string()));

        let c = parse("acende as luzes da varanda por favor").unwrap();
        assert_eq!((c.verb, c.said), (Verb::On, Said::Acende));
        assert!(c.plural && c.feminine);
        assert_eq!(
            &c.candidates[..3],
            ["porch light", "deck light", "balcony light"]
        );

        let c = parse("liga o ventilador do quarto").unwrap();
        assert_eq!(c.candidates[0], "bedroom fan");
        assert!(!c.feminine);
    }

    #[test]
    fn all_bare_and_named_targets() {
        assert_eq!(
            p("apaga tudo").map(|c| c.1),
            Some(vec!["all lights".to_string()])
        );
        assert_eq!(
            p("desliga todas as luzes da casa").map(|c| c.1),
            Some(vec!["all lights".to_string()])
        );
        // Bare "a luz" → the resolver's bare "light" (this display's room).
        assert_eq!(p("acende a luz").unwrap().1[0], "light");
        // A person's device.
        assert!(p("desliga a luz do Brendon")
            .unwrap()
            .1
            .contains(&"brendons light".to_string()));
        // An English device name inside a Portuguese sentence.
        assert!(p("liga a Samsung TV")
            .unwrap()
            .1
            .contains(&"samsung tv".to_string()));
        // Scenes.
        let c = parse("ativa a cena hora do filme").unwrap();
        assert_eq!(c.verb, Verb::Activate);
        assert!(c.candidates.contains(&"hora do filme".to_string()));
    }

    #[test]
    fn brightness_and_status() {
        let c = parse("diminui a luz da sala para 30 por cento").unwrap();
        assert_eq!(c.verb, Verb::Brightness(30));
        assert_eq!(c.candidates[0], "living room light");
        assert_eq!(
            parse("deixa a luz do escritório em 50%").unwrap().verb,
            Verb::Brightness(50)
        );
        assert_eq!(
            parse("coloca a luz da cozinha na metade").map(|c| c.verb),
            None,
            "'na' isn't a brightness preposition here"
        );
        assert_eq!(parse("coloca um timer de dez minutos"), None);
        assert_eq!(parse("aumenta o volume para 200"), None);

        let c = parse("A luz da varanda está acesa?").unwrap();
        assert_eq!(c.verb, Verb::Status);
        assert_eq!(c.phrase, "a luz da varanda");
        let c = parse("está ligada a lareira?").unwrap();
        assert_eq!(c.verb, Verb::Status);
        assert_eq!(c.candidates[0], "fireplace");
    }

    #[test]
    fn ordinary_speech_does_not_match() {
        for s in [
            "que horas são",
            "me conta uma piada",
            "lembra que eu gosto de café",
            "liga",
            "",
            "turn off the office lights",
            "a reunião está marcada",
        ] {
            assert_eq!(parse(s), None, "{s:?}");
        }
    }

    fn ent(id: &str, name: &str, state: &str) -> Entity {
        Entity {
            entity_id: id.into(),
            name: name.into(),
            area: "Office".into(),
            state: state.into(),
        }
    }

    #[test]
    fn replies_agree_with_the_target() {
        let cmd = parse("apaga a luz do escritório").unwrap();
        let target = super::super::Target {
            label: "Office lights".into(),
            entities: vec![ent("light.office", "Office", "on")],
            group: true,
        };
        assert_eq!(
            say(
                &Done::Switched {
                    on: false,
                    target: target.clone()
                },
                &cmd
            ),
            "Pronto, apaguei a luz do escritório."
        );
        let q = parse("as luzes do escritório estão acesas?").unwrap();
        assert_eq!(
            say(
                &Done::Status {
                    target: target.clone(),
                    now: vec![ent("light.office", "Office", "off")]
                },
                &q
            ),
            "As luzes do escritório estão apagadas."
        );
        let q = parse("o ventilador do quarto está ligado?").unwrap();
        let fan = super::super::Target {
            label: "Bedroom fan".into(),
            entities: vec![ent("fan.bedroom", "Fan", "on")],
            group: false,
        };
        assert_eq!(
            say(
                &Done::Status {
                    target: fan.clone(),
                    now: vec![ent("fan.bedroom", "Fan", "on")]
                },
                &q
            ),
            "O ventilador do quarto está ligado."
        );
        assert_eq!(
            say(&Done::Unavailable { target: fan }, &q),
            "O ventilador do quarto não está respondendo agora."
        );
    }
}
