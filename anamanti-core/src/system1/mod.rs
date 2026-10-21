//! Pluggable **System-1 fast-decision** stage (plans/system1-fast-decisions.md).
//!
//! A System-1 decision engine runs on the Anamanti Core **before** memory recall and
//! the LLM. Given the turn's transcript (plus light context), it either **Resolves** a
//! common intent — which the orchestrator answers via existing tools + `DeviceAction`s,
//! skipping the blocking GraphRAG embedding round-trip *and* the rig+tools full
//! completion — or **Defers** the turn to today's "System-2" path unchanged.
//!
//! Like the LLM backend, the engine is chosen from config and hidden behind one trait
//! ([`DecisionEngine`]), so a local `laya-serve` sidecar or Jev/OpenRouter (both speak
//! the shared `/v1/systemone` contract) are swappable without touching the pipeline.
//!
//! Implemented: the [`NoDecision`] default (`backend = "none"`, which skips the stage
//! entirely), the `laya-serve` / `jev` HTTP backend ([`http::HttpDecider`]), the test
//! [`mock::MockDecider`], and the closed [`Intent`] set the orchestrator's fast-path
//! handlers serve. `laya-embedded` (in-process) is not implemented yet.

pub mod http;
pub mod mock;

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

/// The closed set of intents System-1 knows how to route. The wire carries them as
/// their [`as_str`](Intent::as_str) labels (the `criteria` keys of the `/v1/systemone`
/// request, and [`Resolution::intent`]); the orchestrator parses a resolution back with
/// [`FromStr`](std::str::FromStr) and dispatches with an exhaustive `match`, so adding a
/// variant forces a decision about its handler. A config-supplied label that is not an
/// `Intent` can still be routed, but it never has a handler and always defers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Intent {
    /// Current conditions or the forecast for home (fast path: the weather widget).
    Weather,
    /// Start a timer for a stated duration.
    Timer,
    /// The current time of day.
    Time,
    /// Today's date / day of the week.
    Date,
    /// Cancel the running timer(s).
    TimerCancel,
    /// How much time is left on the running timer.
    TimerQuery,
    /// Add time to / remove time from the running timer.
    TimerAdjust,
    /// Close the weather screen.
    WeatherDismiss,
    /// Close the recipe screen.
    RecipeDismiss,
    /// Jump to a step of the recipe on screen ("go to step 4", "next step"). Resolves
    /// only with a recipe up and a step number parsed from the transcript (pure string
    /// code against the device-reported highlighted step); otherwise defers.
    RecipeStep,
    /// How much / how many of an ingredient the recipe on screen calls for ("how many
    /// tomatoes do I need?"). A **conditional lookup**: resolves only when the asked-about
    /// ingredient string-matches the device-reported ingredient lines; otherwise defers to
    /// System-2, whose prompt carries the full recipe.
    RecipeIngredient,
    /// Close the place card.
    PlaceDismiss,
    /// The user is done: end the follow-up chain.
    EndSession,
    /// A bare "stop"/"cancel" whose target comes from live device state.
    StopDismiss,
    /// A question about a specific place. **Route-only:** it has a free-form slot (the
    /// place name), so it has no fast-path handler and always defers to the System-2
    /// LLM, which calls `places_lookup` (`plans/system1-fast-decisions.md` §15.2, Tier C).
    Place,
}

impl Intent {
    /// Every intent, in the order the default router offers them.
    pub const ALL: [Intent; 15] = [
        Intent::Weather,
        Intent::Timer,
        Intent::Time,
        Intent::Date,
        Intent::TimerCancel,
        Intent::TimerQuery,
        Intent::TimerAdjust,
        Intent::WeatherDismiss,
        Intent::RecipeDismiss,
        Intent::RecipeStep,
        Intent::RecipeIngredient,
        Intent::PlaceDismiss,
        Intent::EndSession,
        Intent::StopDismiss,
        Intent::Place,
    ];

    /// The wire label (`"weather"`, `"timer_cancel"`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            Intent::Weather => "weather",
            Intent::Timer => "timer",
            Intent::Time => "time",
            Intent::Date => "date",
            Intent::TimerCancel => "timer_cancel",
            Intent::TimerQuery => "timer_query",
            Intent::TimerAdjust => "timer_adjust",
            Intent::WeatherDismiss => "weather_dismiss",
            Intent::RecipeDismiss => "recipe_dismiss",
            Intent::RecipeStep => "recipe_step",
            Intent::RecipeIngredient => "recipe_ingredient",
            Intent::PlaceDismiss => "place_dismiss",
            Intent::EndSession => "end_session",
            Intent::StopDismiss => "stop_dismiss",
            Intent::Place => "place",
        }
    }

    /// The human-readable criterion the classifier sees for this intent.
    pub fn description(self) -> &'static str {
        match self {
            Intent::Weather => "current conditions or the forecast",
            Intent::Timer => "start a timer or alarm for a stated duration",
            Intent::Time => "the current time of day right now",
            Intent::Date => "today's date or the current day of the week",
            Intent::TimerCancel => "cancel or stop a running timer or alarm",
            Intent::TimerQuery => "ask how much time is left on a running timer",
            Intent::TimerAdjust => {
                "add time to or remove time from a running timer (\"add five minutes\", \
                 \"give me two more minutes\", \"take a minute off the timer\")"
            }
            Intent::WeatherDismiss => "close or dismiss the weather screen",
            Intent::RecipeDismiss => "close or dismiss the recipe screen",
            Intent::RecipeStep => {
                "go to, show, or read a particular step of the recipe on screen (\"go to step \
                 four\", \"next step\", \"previous step\", \"what's step three\")"
            }
            Intent::RecipeIngredient => {
                "ask how much or how many of an ingredient the recipe on screen needs (\"how \
                 many tomatoes do I need\", \"how much flour\"); NOT cooking times, \
                 temperatures, or substitutions"
            }
            Intent::PlaceDismiss => "close or dismiss the place card",
            Intent::Place => {
                "ask about a specific business, shop, restaurant, cafe, landmark, or point of \
                 interest — where it is, its address, hours, phone, or rating"
            }
            Intent::EndSession => {
                "the user is finished and wants no more replies — a sign-off or dismissal \
                 like \"that's all\", \"nothing else\", or \"goodbye\", including a bare \
                 standalone thanks used to close the conversation in any language \
                 (\"thank you\", \"thanks\", \"gracias\", \"merci\"); NOT stopping a timer \
                 or music, and NOT a thanks that is part of a continuing request \
                 (e.g. \"thanks, now what's the weather\")"
            }
            Intent::StopDismiss => {
                "a bare \"stop\", \"cancel\", \"never mind\", or \"dismiss\" with no named \
                 target (what it refers to depends on what is currently active)"
            }
        }
    }

    /// Whether the answer changes with a place named in the turn, so a turn that names a
    /// *specific* place must defer to System-2 rather than fast-path against home:
    /// `weather` (the fast handler only knows the home forecast) and `time`/`date` (the
    /// wall clock is local — "what time is it in London" is a different timezone).
    pub fn place_sensitive(self) -> bool {
        matches!(self, Intent::Weather | Intent::Time | Intent::Date)
    }

    /// Whether a confidently **future** turn must defer to System-2 (§16.1): the clock
    /// handlers only know *now*, so "what's the date next Friday" / "what time will it be
    /// in three hours" is a calculation they can't answer. Weather is not here — its
    /// handler selects a forecast day itself (or defers when it can't).
    pub fn defers_on_future(self) -> bool {
        matches!(self, Intent::Time | Intent::Date)
    }
}

impl std::fmt::Display for Intent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A label that is not one of the built-in [`Intent`]s (e.g. a custom config entry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownIntent(pub String);

impl std::fmt::Display for UnknownIntent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown System-1 intent `{}`", self.0)
    }
}

impl std::error::Error for UnknownIntent {}

impl std::str::FromStr for Intent {
    type Err = UnknownIntent;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Intent::ALL
            .into_iter()
            .find(|i| i.as_str() == s)
            .ok_or_else(|| UnknownIntent(s.to_string()))
    }
}

/// The built-in intents the router may resolve when the config leaves `intents` empty:
/// every [`Intent`]. An intent listed here still only resolves if the orchestrator's
/// handler preconditions hold (e.g. timer needs a parseable duration), otherwise the
/// turn defers to System-2; the route-only [`Intent::Place`] always defers.
pub fn default_intents() -> Vec<String> {
    Intent::ALL.iter().map(|i| i.as_str().to_string()).collect()
}

/// Parse a spoken timer/alarm duration into whole seconds, or `None` when the text has
/// no clear duration (e.g. "cancel my timer", "set a timer") — the caller then defers to
/// System-2, whose timer tool handles cancels and free-form phrasing. Deliberately
/// conservative: it only fires on an unambiguous `<number> <unit>` (repeatable, e.g.
/// "1 hour 30 minutes"). Supports digits and common number words; `a`/`an` = 1,
/// `half` = 0.5, `quarter` = 0.25 of the following unit.
pub fn parse_duration_secs(text: &str) -> Option<u64> {
    let normalized = text.to_lowercase().replace('-', " ");
    let mut total = 0.0f64;
    let mut current = 0.0f64;
    let mut have_current = false;
    let mut saw_unit = false;

    for raw in normalized.split_whitespace() {
        let tok = raw.trim_matches(|c: char| !c.is_alphanumeric());
        if tok.is_empty() {
            continue;
        }
        if let Ok(n) = tok.parse::<f64>() {
            current += n;
            have_current = true;
        } else if let Some(v) = duration_word_value(tok) {
            current += v;
            have_current = true;
        } else if let Some(u) = duration_unit_secs(tok) {
            // A unit with no explicit number means one ("a minute", "an hour", "half an
            // hour" → the `half` is the number, `an` is ignored, `hour` defaults nothing).
            let n = if have_current { current } else { 1.0 };
            total += n * u;
            current = 0.0;
            have_current = false;
            saw_unit = true;
        }
        // Any other token — the article "a"/"an", "set", "timer", "for", "and", … — is
        // ignored. (Counting "a" as a number would turn "a timer for 10 minutes" into 11.)
    }

    if !saw_unit {
        return None;
    }
    let secs = total.round();
    if secs < 1.0 {
        None
    } else {
        Some(secs as u64)
    }
}

/// A spoken number word usable in a duration ("five", "twenty", "half"), as a value.
fn duration_word_value(tok: &str) -> Option<f64> {
    Some(match tok {
        "one" => 1.0,
        "two" => 2.0,
        "three" => 3.0,
        "four" => 4.0,
        "five" => 5.0,
        "six" => 6.0,
        "seven" => 7.0,
        "eight" => 8.0,
        "nine" => 9.0,
        "ten" => 10.0,
        "eleven" => 11.0,
        "twelve" => 12.0,
        "thirteen" => 13.0,
        "fourteen" => 14.0,
        "fifteen" => 15.0,
        "twenty" => 20.0,
        "thirty" => 30.0,
        "forty" => 40.0,
        "fifty" => 50.0,
        "sixty" => 60.0,
        "ninety" => 90.0,
        "half" => 0.5,
        "quarter" => 0.25,
        _ => return None,
    })
}

/// A duration unit word ("minutes", "hr"), as its length in seconds.
fn duration_unit_secs(tok: &str) -> Option<f64> {
    Some(match tok {
        "hour" | "hours" | "hr" | "hrs" => 3600.0,
        "minute" | "minutes" | "min" | "mins" => 60.0,
        "second" | "seconds" | "sec" | "secs" => 1.0,
        _ => return None,
    })
}

/// What a "start a timer" transcript says about the timer's name — see
/// [`parse_timer_name`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimerName {
    /// Only filler + duration words: a plain, unnamed timer ("set a timer for 10 minutes").
    Unnamed,
    /// A name in a recognised position ("pasta timer", "timer for the pasta", "timer
    /// called chicken wings"), lowercased as spoken.
    Named(String),
    /// Leftover words that don't fit a recognised naming pattern ("remind me in 10
    /// minutes to check the oven") — defer to System-2, whose `set_timer` tool picks a
    /// label from free-form phrasing.
    Unclear,
}

/// Read the timer's spoken name out of a "start a timer" transcript, so the System-1
/// fast path keeps the name the user gave instead of dropping it. Deliberately
/// conservative, like [`parse_duration_secs`]: every word must be timer filler, part of
/// the duration, or a single contiguous run of "name" words anchored by `timer` right
/// after it ("a pasta timer") or by `for`/`called`/`named`/`labeled` right before it
/// (skipping `the`/`my`/`our`). Anything else is [`TimerName::Unclear`].
pub fn parse_timer_name(text: &str) -> TimerName {
    /// Words that carry no name: request scaffolding, articles, politeness, the word
    /// "timer" itself, and the naming keywords.
    const FILLER: &[&str] = &[
        "set",
        "start",
        "make",
        "create",
        "put",
        "begin",
        "run",
        "give",
        "remind",
        "please",
        "can",
        "could",
        "would",
        "will",
        "you",
        "me",
        "i",
        "a",
        "an",
        "the",
        "my",
        "our",
        "for",
        "in",
        "on",
        "up",
        "to",
        "of",
        "and",
        "new",
        "another",
        "more",
        "also",
        "too",
        "again",
        "timer",
        "alarm",
        "countdown",
        "called",
        "named",
        "labeled",
        "labelled",
        "label",
        "it",
        "just",
        "um",
        "uh",
        "hey",
        "ok",
        "okay",
        "now",
    ];
    /// Words that may sit between an anchor and the name ("for the pasta").
    const DETERMINERS: &[&str] = &["the", "my", "our"];
    /// Words that, right before the name, mark it as the name.
    const LEAD_ANCHORS: &[&str] = &["for", "called", "named", "labeled", "labelled"];

    let normalized = text.to_lowercase().replace('-', " ");
    let toks: Vec<&str> = normalized
        .split_whitespace()
        .map(|raw| raw.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|t| !t.is_empty())
        .collect();
    let is_name_word = |t: &str| {
        !FILLER.contains(&t)
            && t.parse::<f64>().is_err()
            && duration_word_value(t).is_none()
            && duration_unit_secs(t).is_none()
    };

    // The name words must form exactly one contiguous run.
    let mut run: Option<(usize, usize)> = None; // [start, end)
    for (i, t) in toks.iter().enumerate() {
        if !is_name_word(t) {
            continue;
        }
        match run {
            None => run = Some((i, i + 1)),
            Some((s, e)) if e == i => run = Some((s, i + 1)),
            Some(_) => return TimerName::Unclear, // two separate runs → ambiguous
        }
    }
    let Some((start, end)) = run else {
        return TimerName::Unnamed;
    };

    // Anchored by "timer" right after ("a pasta timer", "an egg timer")…
    let trailing = toks.get(end) == Some(&"timer");
    // …or by for/called/named right before, skipping a determiner ("for the pasta").
    let mut before = start;
    while before > 0 && DETERMINERS.contains(&toks[before - 1]) {
        before -= 1;
    }
    let leading = before > 0 && LEAD_ANCHORS.contains(&toks[before - 1]);

    if trailing || leading {
        TimerName::Named(toks[start..end].join(" "))
    } else {
        TimerName::Unclear
    }
}

/// Parse a spoken "add/remove time on the timer" request into a **signed** second delta
/// (positive = add, negative = remove), or `None` when the direction or the amount is
/// unclear — the caller then defers to System-2, whose `adjust_timer` tool handles
/// labeled and free-form phrasing. Deliberately conservative: it resolves only when the
/// transcript has both an unambiguous direction keyword (exactly one of add/remove) and a
/// clear `<number> <unit>` duration ([`parse_duration_secs`]). "Change the timer to ten
/// minutes" (an absolute set, which the device can't do — it only takes a delta) has no
/// direction keyword, so it defers.
pub fn parse_timer_adjust(text: &str) -> Option<i64> {
    let normalized = text.to_lowercase().replace('-', " ");
    let has = |kw: &str| {
        normalized
            .split_whitespace()
            .any(|w| w.trim_matches(|c: char| !c.is_alphanumeric()) == kw)
    };
    // Direction keywords. Require exactly one side to avoid guessing on ambiguous or
    // contradictory phrasing.
    const ADD: [&str; 7] = [
        "add", "more", "another", "extend", "plus", "lengthen", "increase",
    ];
    const REMOVE: [&str; 11] = [
        "remove", "subtract", "less", "shorten", "cut", "reduce", "minus", "off", "take", "knock",
        "shave",
    ];
    let adds = ADD.iter().any(|k| has(k));
    let removes = REMOVE.iter().any(|k| has(k));
    let sign = match (adds, removes) {
        (true, false) => 1i64,
        (false, true) => -1i64,
        _ => return None, // neither or both → ambiguous, defer
    };
    let secs = parse_duration_secs(&normalized)? as i64; // unclear amount → defer
    Some(sign * secs)
}

/// A spoken number word or ordinal (`"four"`, `"fourth"`, `"4th"`, `"4"`) as a value,
/// for step numbers. Tens combine with a following unit in the caller ("twenty one").
fn spoken_number(tok: &str) -> Option<u32> {
    if let Ok(n) = tok.parse::<u32>() {
        return Some(n);
    }
    // "1st" / "2nd" / "3rd" / "4th"
    let digits: String = tok.chars().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() && matches!(&tok[digits.len()..], "st" | "nd" | "rd" | "th") {
        return digits.parse().ok();
    }
    Some(match tok {
        "one" | "first" => 1,
        "two" | "second" => 2,
        "three" | "third" => 3,
        "four" | "fourth" => 4,
        "five" | "fifth" => 5,
        "six" | "sixth" => 6,
        "seven" | "seventh" => 7,
        "eight" | "eighth" => 8,
        "nine" | "ninth" => 9,
        "ten" | "tenth" => 10,
        "eleven" | "eleventh" => 11,
        "twelve" | "twelfth" => 12,
        "thirteen" | "thirteenth" => 13,
        "fourteen" | "fourteenth" => 14,
        "fifteen" | "fifteenth" => 15,
        "sixteen" | "sixteenth" => 16,
        "seventeen" | "seventeenth" => 17,
        "eighteen" | "eighteenth" => 18,
        "nineteen" | "nineteenth" => 19,
        "twenty" | "twentieth" => 20,
        "thirty" | "thirtieth" => 30,
        _ => return None,
    })
}

/// Parse a spoken "go to a recipe step" request into a 1-based step number, given the
/// step currently highlighted on the device (`0` = none) and the recipe's step count
/// (`0` = unknown). `None` (defer to System-2) when the transcript names no step, the
/// reference is ambiguous, or the step is out of range. Deliberately conservative: it
/// requires the word "step", then reads either a number next to it ("step 4", "step
/// number four", "the fourth step", "3rd step"), a relative word ("next" / "previous" /
/// "back"), or an end ("first" / "last" / "final").
pub fn parse_recipe_step(text: &str, highlighted: u32, count: u32) -> Option<u32> {
    let normalized = text.to_lowercase().replace('-', " ");
    let toks: Vec<&str> = normalized
        .split_whitespace()
        .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|t| !t.is_empty())
        .collect();
    let step_at = toks.iter().position(|t| *t == "step" || *t == "steps")?;
    let has = |w: &str| toks.contains(&w);

    // A number right after "step" (optionally "step number …"), tens + unit combined.
    let after = |i: usize| -> Option<u32> {
        let mut i = i;
        if toks.get(i) == Some(&"number") {
            i += 1;
        }
        let first = spoken_number(toks.get(i)?)?;
        match toks.get(i + 1).and_then(|t| spoken_number(t)) {
            Some(unit) if (first == 20 || first == 30) && unit < 10 => Some(first + unit),
            _ => Some(first),
        }
    };
    let n = if let Some(n) = after(step_at + 1) {
        n
    } else if has("next") {
        highlighted + 1
    } else if has("previous") || has("back") || has("prior") {
        if highlighted <= 1 {
            return None; // nothing before step 1 (or no step yet) → let System-2 say so
        }
        highlighted - 1
    } else if has("last") || has("final") {
        if count == 0 {
            return None;
        }
        count
    } else if step_at > 0 {
        // An ordinal before "step": "the fourth step", "the 3rd step".
        let prev = spoken_number(toks[step_at - 1])?;
        // "twenty first step"
        match step_at.checked_sub(2).and_then(|j| spoken_number(toks[j])) {
            Some(tens) if (tens == 20 || tens == 30) && prev < 10 => tens + prev,
            _ => prev,
        }
    } else {
        return None;
    };
    if n == 0 || (count > 0 && n > count) {
        return None; // out of range → defer (System-2 can tell them how many steps there are)
    }
    Some(n)
}

/// Words that never name an ingredient in a quantity question — question scaffolding,
/// pronouns, and units — dropped before matching the ingredient lines.
const INGREDIENT_STOPWORDS: &[&str] = &[
    "how",
    "many",
    "much",
    "what",
    "whats",
    "is",
    "are",
    "the",
    "a",
    "an",
    "of",
    "do",
    "does",
    "did",
    "i",
    "we",
    "you",
    "me",
    "my",
    "it",
    "this",
    "that",
    "these",
    "those",
    "need",
    "needs",
    "needed",
    "use",
    "uses",
    "used",
    "call",
    "calls",
    "called",
    "for",
    "in",
    "on",
    "to",
    "and",
    "or",
    "with",
    "recipe",
    "dish",
    "there",
    "amount",
    "quantity",
    "should",
    "put",
    "add",
    "go",
    "goes",
    "again",
    "please",
    "tell",
    "say",
    "says",
    "get",
    "have",
    "has",
    "will",
    "would",
    "can",
    "could",
    "be",
    "s",
    "cup",
    "cups",
    "teaspoon",
    "teaspoons",
    "tsp",
    "tablespoon",
    "tablespoons",
    "tbsp",
    "gram",
    "grams",
    "g",
    "kg",
    "kilogram",
    "kilograms",
    "ounce",
    "ounces",
    "oz",
    "pound",
    "pounds",
    "lb",
    "lbs",
    "ml",
    "milliliter",
    "milliliters",
    "liter",
    "liters",
    "litre",
    "litres",
    "pinch",
    "pinches",
    "clove",
    "cloves",
    "can",
    "cans",
    "piece",
    "pieces",
    "stick",
    "sticks",
    "bunch",
    "handful",
    "total",
    "all",
    "altogether",
    "exactly",
    "about",
    "roughly",
    "approximately",
    "so",
    "um",
    "uh",
    "okay",
    "ok",
    "hey",
];

/// Crude English singularization for matching "tomatoes" ↔ "tomato", "berries" ↔ "berry".
fn singular(word: &str) -> String {
    if let Some(stem) = word.strip_suffix("ies").filter(|s| s.len() >= 2) {
        format!("{stem}y")
    } else if let Some(stem) = word.strip_suffix("oes") {
        format!("{stem}o")
    } else if let Some(stem) = word
        .strip_suffix('s')
        .filter(|s| !s.ends_with('s') && s.len() >= 2)
    {
        stem.to_string()
    } else {
        word.to_string()
    }
}

/// Lowercased, singularized word tokens of `text`.
fn ingredient_words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .replace('-', " ")
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .map(|t| t.trim_matches('\'').replace('\'', ""))
        .filter(|t| !t.is_empty())
        .map(|t| singular(&t))
        .collect()
}

/// Most ingredient lines a System-1 quantity answer will read out; more matches than
/// this is ambiguous ("how much of everything?") → defer.
const MAX_INGREDIENT_MATCHES: usize = 3;

/// The **conditional System-1 lookup** for "how much / how many <ingredient>?": find the
/// recipe ingredient line(s) the question asks about. Returns the matching lines (in
/// recipe order) or `None` to defer — when it isn't a quantity question ("how many" /
/// "how much" / "amount" / "quantity"), names no ingredient word, matches no line, or
/// matches more than [`MAX_INGREDIENT_MATCHES`] lines. A line matches on the most
/// question words it contains (so "how many tomatoes for the salsa" still finds "4 ripe
/// tomatoes" even though "salsa" isn't in it); a tie returns all tied lines.
pub fn match_recipe_ingredients<'a>(text: &str, ingredients: &'a [String]) -> Option<Vec<&'a str>> {
    let lower = text.to_lowercase();
    let quantity_question = ["how many", "how much", "amount", "quantity"]
        .iter()
        .any(|q| lower.contains(q));
    if !quantity_question || ingredients.is_empty() {
        return None;
    }
    let mut keywords: Vec<String> = ingredient_words(&lower)
        .into_iter()
        .filter(|w| !INGREDIENT_STOPWORDS.contains(&w.as_str()))
        .filter(|w| w.parse::<f64>().is_err())
        .collect();
    keywords.dedup();
    if keywords.is_empty() {
        return None;
    }
    let scored: Vec<(usize, &str)> = ingredients
        .iter()
        .map(|line| {
            let words = ingredient_words(line);
            let score = keywords.iter().filter(|k| words.contains(k)).count();
            (score, line.as_str())
        })
        .collect();
    let best = scored.iter().map(|(n, _)| *n).max().unwrap_or(0);
    if best == 0 {
        return None;
    }
    let hits: Vec<&str> = scored
        .into_iter()
        .filter(|(n, _)| *n == best)
        .map(|(_, l)| l)
        .collect();
    (hits.len() <= MAX_INGREDIENT_MATCHES).then_some(hits)
}

/// The turn state handed to a decision engine — **light context only** (no memory
/// recall; that's exactly the cost System-1 exists to skip).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionRequest {
    /// The final STT transcript for this turn.
    pub transcript: String,
    /// A short label for what the display is currently showing (e.g. `"recipe"`), so
    /// screen-relative commands ("scroll down") can route. `None` on an idle display.
    /// From the turn's `DisplayContext`; sent in the request `state` as `screen`.
    pub screen: Option<String>,
    /// Active-timer state the device reported this turn (running count / soonest
    /// remaining / labels), **orthogonal to `screen`** — a timer can run behind another
    /// widget. Ground truth for the `timer_query` / `timer_cancel` / `stop_dismiss`
    /// decisions (plans/system1-fast-decisions.md §17, §19). Empty when nothing runs.
    pub timers: crate::wyoming::protocol::TimerContext,
    /// Who is asking (SpeakerScopedMemoryPlan.md §6, decision D4): how confidently the
    /// voiceprint resolved and the person's household name. Never the internal
    /// `spk-…` id, the match score or any embedding. A personal fast intent with an
    /// unknown speaker must defer to System-2.
    pub speaker: SpeakerSummary,
}

/// The speaker as System-1 sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeakerSummary {
    /// `identified | tentative | new | unknown`.
    pub resolution: String,
    /// The canonical household name, when the speaker is recognized and named.
    pub name: Option<String>,
}

impl Default for SpeakerSummary {
    fn default() -> Self {
        Self {
            resolution: "unknown".to_string(),
            name: None,
        }
    }
}

impl SpeakerSummary {
    /// Whether the speaker is recognized (identified or probable).
    pub fn known(&self) -> bool {
        matches!(self.resolution.as_str(), "identified" | "tentative")
    }
}

/// The turn's time frame from the `temporal` routing question (plans/system1-fast-
/// decisions.md §16): a **defer gate** (`Past` always defers) and a **data selector**
/// (weather: `Present` = current conditions, `Future` = a forecast).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Temporal {
    /// Already happened / historical.
    Past,
    /// Now, the current state, or not time-bound.
    Present,
    /// Upcoming, a forecast, or something scheduled.
    Future,
}

impl std::str::FromStr for Temporal {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "past" => Ok(Temporal::Past),
            "present" => Ok(Temporal::Present),
            "future" => Ok(Temporal::Future),
            _ => Err(()),
        }
    }
}

/// A resolved fast intent: the routed label plus its calibrated confidence. The
/// orchestrator parses `intent` into an [`Intent`] to pick a handler.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolution {
    /// The routed intent label, e.g. `"weather"`, `"timer"` (see [`Intent::as_str`]).
    pub intent: String,
    /// The engine's calibrated confidence in this resolution (`0.0..=1.0`).
    pub confidence: f64,
    /// The turn's time frame, set only when the engine answered the `temporal` question
    /// **confidently** (at or above its floor); `None` = unknown/unsure. Handlers use it
    /// to pick their data (e.g. weather defers a vague future ask it can't pin to a day).
    pub temporal: Option<Temporal>,
}

/// The outcome of a System-1 evaluation.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Confident, closed intent — answer it on the fast path (skips recall + LLM).
    Resolve(Resolution),
    /// Ambiguous / open-ended / low-confidence / disabled — fall through to System-2.
    Defer,
}

/// A swappable System-1 decision engine, selected from config like the LLM backend.
/// Implementations are `Send + Sync` so one instance is shared across concurrent turns
/// behind an `Arc`.
#[async_trait]
pub trait DecisionEngine: Send + Sync {
    /// A short label for logs/settings (e.g. `"none"`, `"laya-serve"`, `"jev"`).
    /// The pipeline skips the whole stage when this is `"none"`.
    fn name(&self) -> &str;

    /// Evaluate the turn. Errors (or low confidence) should surface as `Err`/`Defer`
    /// rather than blocking the turn — the caller always falls through to System-2.
    async fn decide(&self, req: &DecisionRequest) -> Result<Decision>;
}

/// The default no-op engine: always defers to System-2. Selected by `backend = "none"`
/// (the default), so the whole feature is inert until explicitly enabled.
pub struct NoDecision;

#[async_trait]
impl DecisionEngine for NoDecision {
    fn name(&self) -> &str {
        "none"
    }
    async fn decide(&self, _req: &DecisionRequest) -> Result<Decision> {
        Ok(Decision::Defer)
    }
}

/// The default engine (`NoDecision`) as a shared trait object.
pub fn none() -> Arc<dyn DecisionEngine> {
    Arc::new(NoDecision)
}

/// Build a decision engine from a backend label + settings. The single construction
/// point shared by the boot path ([`crate::config::Config::shared_settings`]) and the
/// runtime config-page swap ([`crate::settings::SharedSettings::apply_system1`]), so both
/// agree on what each backend name means. An empty `intents` list falls back to
/// [`default_intents`].
///
/// - `none`/`off`/`""` → [`NoDecision`] (disabled)
/// - `mock` → a deferring [`mock::MockDecider`]
/// - `laya-serve` → local HTTP sidecar (`base_url`)
/// - `jev` → OpenRouter (`base_url` API root + `model` + `api_key`)
/// - `laya-embedded` → not yet implemented (bails)
pub fn build(
    backend: &str,
    base_url: &str,
    model: &str,
    api_key: Option<String>,
    min_confidence: f64,
    intents: Vec<String>,
) -> Result<Arc<dyn DecisionEngine>> {
    use http::HttpDecider;
    let intents = if intents.is_empty() {
        default_intents()
    } else {
        intents
    };
    match backend.to_lowercase().as_str() {
        "none" | "off" | "" => Ok(none()),
        "mock" => Ok(Arc::new(mock::MockDecider::defer())),
        "laya-serve" | "laya_serve" => Ok(Arc::new(HttpDecider::laya_serve(
            base_url,
            min_confidence,
            intents,
        ))),
        "jev" => Ok(Arc::new(HttpDecider::jev(
            base_url,
            model,
            api_key,
            min_confidence,
            intents,
        ))),
        other @ ("laya-embedded" | "laya_embedded") => anyhow::bail!(
            "system1.backend `{other}` (in-process candle) is not implemented yet — it is the \
             optional escape hatch. Use `laya-serve` (local sidecar) or `jev` for now."
        ),
        other => anyhow::bail!(
            "unknown system1.backend `{other}` (expected none/mock/laya-serve/jev/laya-embedded)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(t: &str) -> DecisionRequest {
        DecisionRequest {
            transcript: t.to_string(),
            screen: None,
            timers: Default::default(),
            speaker: Default::default(),
        }
    }

    #[test]
    fn intent_labels_round_trip_and_unknown_labels_are_rejected() {
        for intent in Intent::ALL {
            assert_eq!(intent.as_str().parse::<Intent>(), Ok(intent));
            assert!(
                !intent.description().is_empty(),
                "{intent} needs a criterion"
            );
        }
        assert_eq!(
            "music".parse::<Intent>(),
            Err(UnknownIntent("music".to_string()))
        );
        assert!(Intent::Weather.place_sensitive());
        assert!(Intent::Time.place_sensitive() && Intent::Date.place_sensitive());
        assert!(!Intent::Timer.place_sensitive());
    }

    #[test]
    fn default_intents_cover_every_intent_including_place_dismiss() {
        let defaults = default_intents();
        assert_eq!(defaults.len(), Intent::ALL.len());
        assert!(defaults.iter().any(|i| i == "place_dismiss"));
    }

    #[tokio::test]
    async fn no_decision_always_defers() {
        let e = NoDecision;
        assert_eq!(e.name(), "none");
        assert_eq!(
            e.decide(&req("show me the weather")).await.unwrap(),
            Decision::Defer
        );
    }

    #[test]
    fn parse_duration_handles_common_phrasings() {
        assert_eq!(parse_duration_secs("set a timer for 10 minutes"), Some(600));
        assert_eq!(parse_duration_secs("10 minute timer"), Some(600));
        assert_eq!(
            parse_duration_secs("set a timer for 1 hour 30 minutes"),
            Some(5400)
        );
        assert_eq!(parse_duration_secs("half an hour"), Some(1800));
        assert_eq!(parse_duration_secs("a minute"), Some(60));
        assert_eq!(parse_duration_secs("90 seconds"), Some(90));
        assert_eq!(parse_duration_secs("twenty five minutes"), Some(1500));
        assert_eq!(parse_duration_secs("2 hrs"), Some(7200));
    }

    #[test]
    fn parse_duration_defers_when_unclear() {
        // No unit → defer (the LLM's timer tool handles these).
        assert_eq!(parse_duration_secs("set a timer"), None);
        assert_eq!(parse_duration_secs("cancel my timer"), None);
        assert_eq!(parse_duration_secs("what's the weather"), None);
        assert_eq!(parse_duration_secs(""), None);
    }

    #[test]
    fn parse_timer_name_reads_the_spoken_name() {
        let named = |s: &str| TimerName::Named(s.to_string());
        assert_eq!(
            parse_timer_name("set a pasta timer for 10 minutes"),
            named("pasta")
        );
        assert_eq!(parse_timer_name("start a 5-minute egg timer"), named("egg"));
        assert_eq!(
            parse_timer_name("set a timer for the pasta for 10 minutes"),
            named("pasta")
        );
        assert_eq!(
            parse_timer_name("set a 10 minute timer for my eggs"),
            named("eggs")
        );
        assert_eq!(
            parse_timer_name("set a timer for 10 minutes called chicken wings"),
            named("chicken wings")
        );
        assert_eq!(
            parse_timer_name("Set a timer named Garlic Bread, 8 minutes."),
            named("garlic bread")
        );
    }

    #[test]
    fn parse_timer_name_unnamed_and_unclear() {
        assert_eq!(
            parse_timer_name("set a timer for 10 minutes"),
            TimerName::Unnamed
        );
        assert_eq!(
            parse_timer_name("10 minute timer please"),
            TimerName::Unnamed
        );
        assert_eq!(
            parse_timer_name("set another timer for 5 minutes"),
            TimerName::Unnamed
        );
        assert_eq!(
            parse_timer_name("set an alarm for half an hour"),
            TimerName::Unnamed
        );
        // Leftover words that aren't clearly a name → defer to the LLM's timer tool.
        assert_eq!(
            parse_timer_name("remind me in 10 minutes to check the oven"),
            TimerName::Unclear
        );
        assert_eq!(
            parse_timer_name("pasta 10 minutes sauce timer"),
            TimerName::Unclear
        );
    }

    #[test]
    fn parse_timer_adjust_signs_the_delta_by_direction() {
        assert_eq!(
            parse_timer_adjust("add five minutes to the timer"),
            Some(300)
        );
        assert_eq!(parse_timer_adjust("give me two more minutes"), Some(120));
        assert_eq!(parse_timer_adjust("add another 30 seconds"), Some(30));
        assert_eq!(parse_timer_adjust("take a minute off the timer"), Some(-60));
        assert_eq!(parse_timer_adjust("knock 30 seconds off"), Some(-30));
        assert_eq!(parse_timer_adjust("subtract 5 minutes"), Some(-300));
    }

    #[test]
    fn parse_recipe_step_reads_numbers_ordinals_and_relative_steps() {
        assert_eq!(parse_recipe_step("go to step 4", 0, 8), Some(4));
        assert_eq!(parse_recipe_step("Go to step four.", 0, 8), Some(4));
        assert_eq!(parse_recipe_step("show me step number six", 2, 8), Some(6));
        assert_eq!(parse_recipe_step("what's the third step", 0, 8), Some(3));
        assert_eq!(parse_recipe_step("read the 2nd step", 0, 8), Some(2));
        assert_eq!(
            parse_recipe_step("jump to step twenty one", 0, 30),
            Some(21)
        );
        assert_eq!(parse_recipe_step("next step", 3, 8), Some(4));
        assert_eq!(parse_recipe_step("next step please", 0, 8), Some(1));
        assert_eq!(parse_recipe_step("go back a step", 3, 8), Some(2));
        assert_eq!(parse_recipe_step("previous step", 5, 8), Some(4));
        assert_eq!(parse_recipe_step("show me the last step", 0, 8), Some(8));
        assert_eq!(parse_recipe_step("first step", 6, 8), Some(1));
    }

    #[test]
    fn parse_recipe_step_defers_when_unclear_or_out_of_range() {
        assert_eq!(parse_recipe_step("scroll down", 0, 8), None);
        assert_eq!(parse_recipe_step("what's next", 2, 8), None);
        assert_eq!(parse_recipe_step("go to step 12", 0, 8), None);
        assert_eq!(parse_recipe_step("next step", 8, 8), None);
        assert_eq!(parse_recipe_step("previous step", 1, 8), None);
        assert_eq!(parse_recipe_step("previous step", 0, 8), None);
        assert_eq!(parse_recipe_step("go to the step", 0, 8), None);
        assert_eq!(parse_recipe_step("go to step zero", 0, 8), None);
        // "steps for …" is not "step 4".
        assert_eq!(
            parse_recipe_step("what are the steps for the sauce", 0, 8),
            None
        );
    }

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn match_recipe_ingredients_finds_the_asked_about_line() {
        let ing = lines(&[
            "4 ripe tomatoes, chopped",
            "1 small red onion",
            "2 cups all-purpose flour",
            "1 tbsp olive oil",
            "Salt and pepper to taste",
        ]);
        assert_eq!(
            match_recipe_ingredients("How many tomatoes do I need?", &ing),
            Some(vec!["4 ripe tomatoes, chopped"])
        );
        assert_eq!(
            match_recipe_ingredients("how many tomatoes for the salsa", &ing),
            Some(vec!["4 ripe tomatoes, chopped"])
        );
        assert_eq!(
            match_recipe_ingredients("how much flour", &ing),
            Some(vec!["2 cups all-purpose flour"])
        );
        assert_eq!(
            match_recipe_ingredients("how many cups of flour does it call for", &ing),
            Some(vec!["2 cups all-purpose flour"])
        );
        assert_eq!(
            match_recipe_ingredients("how much salt", &ing),
            Some(vec!["Salt and pepper to taste"])
        );
        assert_eq!(
            match_recipe_ingredients("what amount of red onion", &ing),
            Some(vec!["1 small red onion"])
        );
    }

    #[test]
    fn match_recipe_ingredients_defers_when_not_a_clear_quantity_lookup() {
        let ing = lines(&["4 ripe tomatoes", "1 onion", "2 cups flour"]);
        // Not a quantity question.
        assert_eq!(match_recipe_ingredients("do I need tomatoes", &ing), None);
        // No ingredient word.
        assert_eq!(match_recipe_ingredients("how much do I need", &ing), None);
        // An ingredient the recipe doesn't have.
        assert_eq!(match_recipe_ingredients("how much butter", &ing), None);
        // An older device that sent no lines.
        assert_eq!(match_recipe_ingredients("how many tomatoes", &[]), None);
        // Too many equally good matches → ambiguous.
        let oils = lines(&[
            "1 tbsp oil",
            "oil for frying",
            "2 tsp oil, divided",
            "oil spray",
        ]);
        assert_eq!(match_recipe_ingredients("how much oil", &oils), None);
    }

    #[test]
    fn parse_timer_adjust_defers_when_direction_or_amount_is_unclear() {
        // No direction keyword (an absolute "set to", which the device can't do) → defer.
        assert_eq!(parse_timer_adjust("change the timer to ten minutes"), None);
        // Direction but no clear amount → defer.
        assert_eq!(parse_timer_adjust("add some time"), None);
        // Contradictory directions → defer rather than guess.
        assert_eq!(parse_timer_adjust("add five minutes, no less"), None);
        assert_eq!(parse_timer_adjust(""), None);
    }
}
