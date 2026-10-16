//! The pure sentence-shape half of the Home Assistant fast path
//! (`plans/HomeAssistantPlan.md` §A2).
//!
//! [`parse`] recognizes an **imperative home command** ("turn off the office lights",
//! "porch lights on", "dim the office lights to 30 percent", "are the deck lights on")
//! and returns the verb plus the still-unresolved target phrase. It does no I/O and
//! knows nothing about the house, so it is cheap enough to run on every turn: only a
//! sentence with a home-command *shape* goes on to touch the device catalog, and the
//! catalog lookup (`super::catalog::resolve`) is what makes a match precise — a
//! shape-matching sentence whose target names no device ("start a timer for ten
//! minutes") simply falls through to System-1 / the LLM.

/// What the user asked the target to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    /// Turn on (lights, switches, media players, …; also activates a named scene).
    On,
    /// Turn off.
    Off,
    /// Run a scene or script ("activate movie time", "run the bedtime script").
    Activate,
    /// Set a light's brightness, in percent (0–100).
    Brightness(u8),
    /// Ask whether the target is on ("is the porch light on?").
    Status,
}

/// A parsed home command: the verb and the target phrase (normalized, unresolved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeCommand {
    pub verb: Verb,
    pub target: String,
}

/// The longest target phrase the fast path will try to resolve. Real device / room
/// names are short; a long phrase is conversation, not a command.
const MAX_TARGET_WORDS: usize = 6;

/// Politeness and filler that may lead a command ("please", "can you", …), longest
/// first so a multi-word filler wins over its first word.
const LEADING_FILLERS: &[&str] = &[
    "go ahead and",
    "can you please",
    "could you please",
    "would you please",
    "can you",
    "could you",
    "would you",
    "will you",
    "please",
    "hey",
    "okay",
    "ok",
    "just",
    "and",
    "also",
];

/// Filler that may trail a command ("… please", "… thanks").
const TRAILING_FILLERS: &[&str] = &[
    "thank you",
    "right now",
    "for me",
    "please",
    "thanks",
    "now",
];

/// Lowercase, turn `%` into the word "percent", drop apostrophes (so "Brendon's" →
/// "brendons"), and replace every other non-alphanumeric character with a space,
/// collapsing runs of whitespace. Shared with the catalog so device names and spoken
/// phrases normalize identically.
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch == '%' {
            out.push_str(" percent ");
        } else if ch == '\'' || ch == '\u{2019}' {
            // drop: "brendon's" → "brendons"
        } else if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
        } else {
            out.push(' ');
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Strip leading and trailing fillers, repeatedly ("okay please turn …").
fn strip_fillers(mut words: Vec<String>) -> Vec<String> {
    loop {
        let before = words.len();
        for filler in LEADING_FILLERS {
            let f: Vec<&str> = filler.split(' ').collect();
            if words.len() > f.len() && words.iter().zip(&f).all(|(w, f)| w == f) {
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
                    .all(|(w, f)| w == f)
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

/// `on` / `off` as a verb.
fn on_off(word: &str) -> Option<Verb> {
    match word {
        "on" => Some(Verb::On),
        "off" => Some(Verb::Off),
        _ => None,
    }
}

/// Spoken-number words STT sometimes emits instead of digits.
fn number_word(word: &str) -> Option<u32> {
    Some(match word {
        "zero" => 0,
        "five" => 5,
        "ten" => 10,
        "fifteen" => 15,
        "twenty" => 20,
        "twentyfive" => 25,
        "thirty" => 30,
        "forty" => 40,
        "fifty" => 50,
        "sixty" => 60,
        "seventy" => 70,
        "seventyfive" => 75,
        "eighty" => 80,
        "ninety" => 90,
        "hundred" | "onehundred" => 100,
        "half" => 50,
        "full" | "max" | "maximum" => 100,
        _ => return None,
    })
}

/// Parse a brightness tail: `N`, `N percent`, `twenty five percent`, `half`, … The
/// whole tail must be consumed, so "set a timer to 5 minutes" is rejected.
fn parse_percent(tail: &[String]) -> Option<u8> {
    let mut words: Vec<&str> = tail.iter().map(String::as_str).collect();
    if words.last() == Some(&"percent") {
        words.pop();
    }
    let value = match words.as_slice() {
        [w] => w.parse::<u32>().ok().or_else(|| number_word(w))?,
        // "twenty five", "seventy five"
        [a, b] => number_word(&format!("{a}{b}"))?,
        _ => return None,
    };
    (value <= 100).then_some(value as u8)
}

/// Clean a target phrase: drop articles/possessives, and bound its length. `None` when
/// nothing is left or it is too long to be a device/room name.
fn target(words: &[String]) -> Option<String> {
    let kept: Vec<&str> = words
        .iter()
        .map(String::as_str)
        .filter(|w| !matches!(*w, "the" | "my" | "our" | "a"))
        .collect();
    if kept.is_empty() || kept.len() > MAX_TARGET_WORDS {
        return None;
    }
    Some(kept.join(" "))
}

/// Recognize a home command's shape. `None` for anything else — the caller then falls
/// through to System-1 / the LLM, so a miss here costs nothing.
pub fn parse(transcript: &str) -> Option<HomeCommand> {
    let norm = normalize(transcript);
    let words = strip_fillers(norm.split(' ').map(str::to_string).collect());
    let n = words.len();
    if n < 2 {
        return None;
    }
    let first = words[0].as_str();
    let last = words[n - 1].as_str();
    let cmd = |verb, t: &[String]| target(t).map(|target| HomeCommand { verb, target });

    match first {
        // turn/switch on|off T   ·   turn/switch T on|off   ·   shut [T] off
        "turn" | "switch" | "shut" => {
            let shut = first == "shut";
            if let Some(verb) = on_off(&words[1]) {
                if n >= 3 && !(shut && verb == Verb::On) {
                    return cmd(verb, &words[2..]);
                }
            }
            if let Some(verb) = on_off(last) {
                if n >= 3 && !(shut && verb == Verb::On) {
                    return cmd(verb, &words[1..n - 1]);
                }
            }
            None
        }
        // activate / run / start / trigger [scene|script] T
        "activate" | "run" | "start" | "trigger" => {
            let mut rest = &words[1..];
            while let Some(w) = rest.first() {
                if matches!(w.as_str(), "the" | "scene" | "script") {
                    rest = &rest[1..];
                } else {
                    break;
                }
            }
            cmd(Verb::Activate, rest)
        }
        // set / dim / brighten [the] T to N [percent]
        "set" | "dim" | "brighten" => {
            let to = words.iter().position(|w| w == "to")?;
            let pct = parse_percent(&words[to + 1..])?;
            cmd(Verb::Brightness(pct), &words[1..to])
        }
        // is / are [the] T on|off  → a status question (answered either way)
        "is" | "are" => {
            on_off(last)?;
            if n < 3 {
                return None;
            }
            cmd(Verb::Status, &words[1..n - 1])
        }
        // T on|off   ("porch lights off", "the office lights on")
        _ => {
            let verb = on_off(last)?;
            cmd(verb, &words[..n - 1])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Option<(Verb, String)> {
        parse(s).map(|c| (c.verb, c.target))
    }

    fn hit(verb: Verb, target: &str) -> Option<(Verb, String)> {
        Some((verb, target.to_string()))
    }

    #[test]
    fn normalize_lowercases_and_strips_punctuation() {
        assert_eq!(
            normalize("Turn OFF the Office-Lights!"),
            "turn off the office lights"
        );
        assert_eq!(normalize("Brendon's light"), "brendons light");
        assert_eq!(normalize("dim to 30%"), "dim to 30 percent");
    }

    #[test]
    fn turn_on_off_both_word_orders() {
        assert_eq!(
            p("Turn off the office lights."),
            hit(Verb::Off, "office lights")
        );
        assert_eq!(p("turn the deck lights on"), hit(Verb::On, "deck lights"));
        assert_eq!(p("switch on the fireplace"), hit(Verb::On, "fireplace"));
        assert_eq!(
            p("shut off the porch lights"),
            hit(Verb::Off, "porch lights")
        );
        assert_eq!(p("shut the TV off"), hit(Verb::Off, "tv"));
        assert_eq!(p("shut on the lights"), None);
    }

    #[test]
    fn bare_target_then_on_off() {
        assert_eq!(p("porch lights off"), hit(Verb::Off, "porch lights"));
        assert_eq!(p("Lights on, please."), hit(Verb::On, "lights"));
    }

    #[test]
    fn fillers_are_stripped() {
        assert_eq!(
            p("Okay, can you please turn on the lights in the office for me"),
            hit(Verb::On, "lights in office")
        );
        assert_eq!(
            p("hey turn off all the lights thanks"),
            hit(Verb::Off, "all lights")
        );
    }

    #[test]
    fn activate_scenes_and_scripts() {
        assert_eq!(
            p("run the media room tv on scene"),
            hit(Verb::Activate, "media room tv on scene")
        );
        assert_eq!(
            p("activate scene movie time"),
            hit(Verb::Activate, "movie time")
        );
        assert_eq!(
            p("start the bedtime script"),
            hit(Verb::Activate, "bedtime script")
        );
    }

    #[test]
    fn brightness_needs_a_whole_percent_tail() {
        assert_eq!(
            p("dim the office lights to 30 percent"),
            hit(Verb::Brightness(30), "office lights")
        );
        assert_eq!(
            p("set the dining lights to 75%"),
            hit(Verb::Brightness(75), "dining lights")
        );
        assert_eq!(
            p("set office lights to twenty five percent"),
            hit(Verb::Brightness(25), "office lights")
        );
        assert_eq!(
            p("set the lights to half"),
            hit(Verb::Brightness(50), "lights")
        );
        // Not brightness: the tail isn't a percentage.
        assert_eq!(p("set a timer to 5 minutes"), None);
        assert_eq!(p("set the lights to 150 percent"), None);
    }

    #[test]
    fn status_questions() {
        assert_eq!(
            p("Is the porch light on?"),
            hit(Verb::Status, "porch light")
        );
        assert_eq!(
            p("are the deck lights off"),
            hit(Verb::Status, "deck lights")
        );
        assert_eq!(p("is it on"), hit(Verb::Status, "it"));
        assert_eq!(p("is the store open"), None);
    }

    #[test]
    fn ordinary_speech_does_not_match() {
        for s in [
            "what's the weather",
            "I left the lights on at work yesterday when I went to the store",
            "set a timer for ten minutes",
            "remember that I like tea",
            "on",
            "",
            "tell me a joke",
        ] {
            assert_eq!(p(s), None, "{s:?} should not parse");
        }
    }
}
