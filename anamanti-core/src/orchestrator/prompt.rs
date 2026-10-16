//! System-2 prompt assembly: the grounding lines (clock, location, household, who is
//! speaking, what the display shows, font state), the output-personality block, and
//! the deterministic personality-switch voice intent.

use crate::settings::{Household, HouseholdMember, Personality, PersonalityDef, RuntimeSettings};
use crate::speaker::{SpeakerContext, SpeakerResolution};
use crate::wyoming::protocol::{
    DeviceContext, DisplayContext, FontContext, MusicScreen, PlaceScreen, RecipeScreen,
    WeatherScreen,
};

/// The recalled memory for one turn, split by who it is true for
/// (SpeakerScopedMemoryPlan.md §5): the speaker's own memories and household facts.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RecalledMemory {
    /// About the person speaking (only ever non-empty for a recognized speaker).
    pub personal: Vec<String>,
    /// True for everyone in the home.
    pub household: Vec<String>,
}

impl RecalledMemory {
    /// Split recall `hits` into personal and household using the `household` subset,
    /// dropping identity assertions ("The user's name is …") from the personal block:
    /// the voiceprint registry is the only source of who is speaking.
    pub fn split(hits: &[String], household: &[String]) -> Self {
        let mut out = Self::default();
        for h in hits {
            if household.contains(h) {
                out.household.push(h.clone());
            } else if !asserts_identity(h) {
                out.personal.push(h.clone());
            }
        }
        out
    }

    /// Total characters of recalled context (for the timing log).
    pub fn chars(&self) -> usize {
        self.personal
            .iter()
            .chain(&self.household)
            .map(String::len)
            .sum()
    }
}

/// Whether a recalled snippet asserts who the speaker *is* ("The user's name is
/// Brendan", "My name is Brendan"). Such text must never decide identity — a mis-scoped
/// or stale one is exactly how one person got told they were someone else.
fn asserts_identity(snippet: &str) -> bool {
    let l = snippet.to_lowercase();
    [
        "the user's name is",
        "the user’s name is",
        "my name is",
        "call me ",
    ]
    .iter()
    .any(|p| l.contains(p))
}

/// Assemble the full System-2 system prompt for one turn: the configured `base` prompt,
/// the wall clock, who is speaking (from the voiceprint, by resolution), where "here"
/// is, who lives here, what is known about this person and the household, what the
/// display shows, the font gate, and the personality block. `has_history` adds the note
/// that earlier (follow-up) messages are labelled by speaker. Every setting is read from
/// the turn's `runtime` snapshot, so config-page edits apply from the next turn.
pub(crate) fn build_system_prompt(
    base: &str,
    speaker: &SpeakerContext,
    runtime: &RuntimeSettings,
    device_ctx: &DeviceContext,
    memory: &RecalledMemory,
    has_history: bool,
) -> String {
    let household = &runtime.household;
    // Ground the model in the real wall-clock (it has no clock) and tell it who it is
    // speaking with, so time/date questions are answered from fact and it can address
    // the person by name / apply the right person's memory.
    let mut prompt = format!(
        "{base}\n\n{}\n\n# Who is speaking\n{}",
        current_datetime_line(),
        speaker_identity_line(speaker, household)
    );
    if let Some(line) = location_line(
        household.address.weather_location().as_deref(),
        household.weather_units.as_deref(),
    ) {
        prompt.push_str("\n\n");
        prompt.push_str(&line);
    }
    if let Some(line) = household_line(&household.members) {
        prompt.push_str("\n\n");
        prompt.push_str(&line);
    }
    // Personal memory only for a recognized speaker (recall already enforces this; the
    // guard keeps the prompt honest if a caller passes more).
    if speaker.reader().is_some() && !memory.personal.is_empty() {
        prompt.push_str("\n\n# About this person\n");
        prompt.push_str(&bullets(&memory.personal));
    }
    if !memory.household.is_empty() {
        prompt.push_str("\n\n# Household facts (true for everyone here)\n");
        prompt.push_str(&bullets(&memory.household));
    }
    if has_history {
        prompt.push_str(
            "\n\nEarlier messages in this conversation are labelled with who said them: \
             [you] is the person speaking now; other names are other people in the home. \
             Only [you] messages describe the current speaker — never apply another \
             person's \"I am…\" or \"I like…\" to them.",
        );
    }
    // What the display is currently showing, so the model can drive that screen by
    // voice with the matching tool. Absent on an idle display.
    if let Some(screen) = device_ctx.widget.as_ref() {
        prompt.push_str("\n\n");
        prompt.push_str(&display_context_line(screen));
    }
    // Whether the on-screen text can be resized right now (the gate for the
    // `adjust_font` tool). Added only when a resizable surface is present, so the model
    // declines a bare "increase font" with nothing on screen.
    if device_ctx.font.scalable {
        prompt.push_str("\n\n");
        prompt.push_str(&font_context_line(&device_ctx.font));
    }
    // Output personality (pure output tuning — never memory, recall or tool use).
    prompt.push_str(&personality_prompt(&runtime.personality));
    prompt
}

fn bullets(items: &[String]) -> String {
    items
        .iter()
        .map(|c| format!("- {c}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The speaker's display name: the household roster's canonical spelling when the
/// voiceprint name matches a member, else the voiceprint name; `None` while anonymous.
fn canonical_name<'a>(
    speaker: &'a SpeakerContext,
    household: &'a Household,
) -> Option<(String, Option<&'a HouseholdMember>)> {
    let name = speaker
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())?;
    match household.member_matching_spoken(name) {
        Some(m) => Some((m.name.trim().to_string(), Some(m))),
        None => Some((name.to_string(), None)),
    }
}

/// The system-prompt line telling the model who it is speaking with, by how confidently
/// the voiceprint resolved (SpeakerScopedMemoryPlan.md §5). The name always comes from
/// the voiceprint registry (reconciled with the household roster, relationship
/// included) — never from remembered text.
pub(crate) fn speaker_identity_line(speaker: &SpeakerContext, household: &Household) -> String {
    const DONT_GUESS: &str = "Do not guess who they are from the household list or anything \
         else. If they ask who they are or what their name is, say you don't recognise their \
         voice yet and suggest Settings → People → Add a voice on the display. Do not use any \
         one person's personal details or preferences.";
    match (speaker.resolution, canonical_name(speaker, household)) {
        (SpeakerResolution::Identified, Some((name, member))) => {
            let rel = member
                .and_then(|m| m.relationship.as_deref())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let who = match (member, rel) {
                (Some(_), Some(rel)) => {
                    format!("You are speaking with {name} ({rel}), who lives here.")
                }
                (Some(_), None) => format!("You are speaking with {name}, who lives here."),
                (None, _) => format!("You are speaking with {name}."),
            };
            format!(
                "{who} When they say \"me\" or \"my phone\", as in \"text me\", that means {name}."
            )
        }
        (SpeakerResolution::Tentative, Some((name, _))) => format!(
            "You are probably speaking with {name}, but the voice match is not certain. Don't \
             state their identity as fact; if they ask who they are, say you think it's {name} \
             but aren't sure. If they ask to send something to \"me\", confirm it's for {name} \
             first."
        ),
        (SpeakerResolution::Identified | SpeakerResolution::Tentative, None) => {
            "You are speaking with a household member whose voice you recognise but whose name \
             you don't know yet. Don't guess their name; if they tell you, greet them by it."
                .to_string()
        }
        (SpeakerResolution::New | SpeakerResolution::Unknown, _) => {
            format!(
                "You do not know who is speaking: their voice was not recognised. {DONT_GUESS} If \
                 they ask to send something to \"me\", ask who they are first."
            )
        }
    }
}

/// A one-line statement of the current local date + time + timezone, injected into
/// the LLM system prompt each turn so the model answers time/date questions from
/// fact instead of hallucinating (it has no clock of its own). Example:
/// `Current date and time: Monday, 16 September 2026, 6:36 PM EDT (UTC-04:00).`
///
/// Phrased *non-leadingly*: the model must only use it when the user actually asks
/// about the time/date, and must not volunteer it otherwise. Without that guard the
/// prominently-stated clock became the most salient fact in context, so on a vague or
/// mis-transcribed request the model would default to reciting the time.
pub(crate) fn current_datetime_line() -> String {
    let now = chrono::Local::now();
    format!(
        "For reference, the current date and time is {} ({}). Use this only to answer \
         questions that are explicitly about the time, date, or day of week; do not \
         mention the time or date otherwise, and never bring it up on your own.",
        now.format("%A, %-d %B %Y, %-I:%M %p %Z"),
        now.format("UTC%:z"),
    )
}

/// A system-prompt line grounding the assistant in the device's physical location
/// (and preferred units), so location-relative questions — weather, sunset, nearby
/// places — resolve an unqualified "here" to the configured home rather than the
/// model guessing. Returns `None` when no home location is configured, so the prompt
/// omits the line entirely. Phrased non-leadingly (like the clock line) so it only
/// matters when the user actually asks something location-relative.
pub(crate) fn location_line(location: Option<&str>, units: Option<&str>) -> Option<String> {
    let location = location.map(str::trim).filter(|s| !s.is_empty())?;
    let mut line = format!(
        "This device is located in {location}. When the user asks about local conditions \
         (weather, sunset, nearby places) without naming a place, assume {location}. Do \
         not mention the location otherwise."
    );
    if let Some(units) = units.map(str::trim).filter(|s| !s.is_empty()) {
        line.push_str(&format!(
            " Prefer {units} units for temperatures and measurements."
        ));
    }
    Some(line)
}

/// A system-prompt block naming the people who live in the home (the canonical
/// household directory), so the model can address them correctly and has their
/// contact details on hand when a request needs one. Phrased non-leadingly (like the
/// clock/location lines): it must only be used when the user actually asks something
/// that needs it, never volunteered. Returns `None` when the roster is empty so the
/// prompt omits the block entirely.
pub(crate) fn household_line(members: &[HouseholdMember]) -> Option<String> {
    let named: Vec<&HouseholdMember> = members
        .iter()
        .filter(|m| !m.name.trim().is_empty())
        .collect();
    if named.is_empty() {
        return None;
    }
    let mut block = String::from(
        "The people who live in this home (use only when a request needs to know who \
         is here or how to reach them; do not recite this otherwise; never use this list \
         to guess who is speaking):",
    );
    for m in named {
        let mut detail = Vec::new();
        if let Some(rel) = m
            .relationship
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            detail.push(rel.to_string());
        }
        if !m.emails.is_empty() {
            detail.push(format!("email {}", m.emails.join(", ")));
        }
        if !m.phones.is_empty() {
            detail.push(format!("phone {}", m.phones.join(", ")));
        }
        if detail.is_empty() {
            block.push_str(&format!("\n- {}", m.name.trim()));
        } else {
            block.push_str(&format!("\n- {} ({})", m.name.trim(), detail.join("; ")));
        }
    }
    Some(block)
}

/// A prompt line describing what the display is currently showing (its "display
/// context"), so the model can drive that screen by voice. Dispatches per screen kind;
/// add an arm here (plus a device-side setter and a `DisplayContext` variant) when a new
/// screen — music, weather, photos — wants voice control.
pub(crate) fn display_context_line(ctx: &DisplayContext) -> String {
    match ctx {
        DisplayContext::Recipe(screen) => recipe_screen_line(screen),
        DisplayContext::Weather(screen) => weather_screen_line(screen),
        DisplayContext::Place(screen) => place_screen_line(screen),
        DisplayContext::Music(screen) => music_screen_line(screen),
        // Diagnostics and enrollment are suppressing contexts: the turn never reaches the
        // LLM, so there is no prompt line to contribute. (These arms are unreachable in
        // practice — the orchestrator short-circuits such turns before building the prompt.)
        DisplayContext::Diagnostics | DisplayContext::Enroll(_) => String::new(),
    }
}

/// A prompt line, added only when a resizable text surface is on screen, telling the model
/// it may resize the display text with `adjust_font` and whether the scale is already at the
/// max/min — so it can say so instead of a no-op call. Mirrors `display_context_line` as the
/// per-turn gate for the font tool.
pub(crate) fn font_context_line(font: &FontContext) -> String {
    let pct = (font.scale() * 100.0).round() as i32;
    let bound = if font.at_max {
        " The text is already at the maximum size, so decline a request to increase it."
    } else if font.at_min {
        " The text is already at the minimum size, so decline a request to decrease it."
    } else {
        ""
    };
    format!(
        "The on-screen text is resizable (currently {pct}% of normal). Use the `adjust_font` \
         tool to make it bigger or smaller when the user asks.{bound}"
    )
}

/// The prompt line for the music player: names the track on screen and which music view is
/// open, so the model can drive it with `music_screen` (show now-playing / up-next / close)
/// and answer "what's this song?". Transport stays on `spotify_control`.
fn music_screen_line(screen: &MusicScreen) -> String {
    let title = screen.title.trim();
    let artist = screen.artist.trim();
    let track = match (title.is_empty(), artist.is_empty()) {
        (true, _) => "a track".to_string(),
        (false, true) => format!("\"{title}\""),
        (false, false) => format!("\"{title}\" by {artist}"),
    };
    let state = if screen.playing { "playing" } else { "paused" };
    let view = match screen.screen.as_str() {
        "now_playing" => "the now-playing screen is open",
        "up_next" => "the up-next queue is open",
        _ => "only the compact control bar is showing",
    };
    format!(
        "The music player is on the display ({view}), {state} {track}. Use the \
         `music_screen` tool to show the now-playing view, show the up-next queue, close \
         it, or switch the remembered widget (fullscreen / compact), and `spotify_control` to play/pause/skip or change the volume."
    )
}

/// The prompt line for the place card: names the place on screen so the model can answer
/// follow-ups in context ("is it open on Sunday?" → `places_lookup` for the same place)
/// or `close_places` when the user says to close it.
fn place_screen_line(screen: &PlaceScreen) -> String {
    let name = screen.name.trim();
    let which = if name.is_empty() {
        "a place".to_string()
    } else {
        name.to_string()
    };
    format!(
        "The place card is currently open on the display, showing {which}. Use the \
         `places_lookup` tool to look up another place and `close_places` to close it."
    )
}

/// The prompt line for the weather screen: names what the forecast currently shows, so
/// the model can answer follow-ups in context ("what about tomorrow" → `weather_lookup`
/// for the same place) or `close_weather` when the user says to close it.
fn weather_screen_line(screen: &WeatherScreen) -> String {
    let unit = if screen.units == "imperial" { "F" } else { "C" };
    let place = screen.location.trim();
    let at = if place.is_empty() {
        String::new()
    } else {
        format!(" for {place}")
    };
    let desc = screen.description.trim();
    let conditions = if desc.is_empty() {
        String::new()
    } else {
        format!(", currently {desc} at {}°{unit}", screen.temp)
    };
    format!(
        "The weather screen is currently open on the display, showing the forecast{at}{conditions}. \
         Use the `weather_lookup` tool to update it (e.g. another day or place) and \
         `close_weather` to close it."
    )
}

/// The prompt line for the recipe screen: names the active tab and whether the pane is
/// scrolled to the top/bottom, so the model can decide whether a `recipe_control`
/// (switch tab / scroll) or `close_recipe` call is useful.
fn recipe_screen_line(screen: &RecipeScreen) -> String {
    let tab = match screen.tab.as_str() {
        "ingredients" => "Ingredients",
        "steps" => "Steps",
        _ => "Overview",
    };
    let title = screen.title.trim();
    let dish = if title.is_empty() {
        "a recipe".to_string()
    } else {
        format!("the recipe for \"{title}\"")
    };
    let position = if screen.at_top && screen.at_bottom {
        " The whole tab fits on screen."
    } else if screen.at_top {
        " It is scrolled to the top."
    } else if screen.at_bottom {
        " It is scrolled to the bottom."
    } else {
        " It is scrolled partway."
    };
    format!(
        "The recipe screen is currently open on the display, showing {dish} \
         ({ing} ingredients, {steps} steps) on the {tab} tab.{position} Use the \
         `recipe_control` tool to switch tabs or scroll it, or `close_recipe` to close it.",
        ing = screen.ingredient_count,
        steps = screen.step_count,
    )
}

/// Recognize a "switch personality" voice command and return the name to apply — a
/// catalog key, or `"normal"` to turn personality off — or `None` when the utterance
/// isn't a personality switch.
///
/// Deterministic and conservative (mirroring the persona fast path): it fires only when
/// the utterance contains a switch *trigger* phrase — so ordinary speech like "that's not
/// normal" never matches — *and* either names a personality in the live catalog or asks
/// to go back to normal. A "normal / off" intent wins over a catalog-name match, so "turn
/// off the gangster voice" disables rather than re-enabling it. Catalog names are matched
/// case-insensitively, longest first, so a more specific name wins over a shorter one it
/// contains. Handled before the LLM; the returned name is fed to `set_active_personality`,
/// which resolves + persists it (so it can never name a personality that doesn't exist).
pub fn detect_personality_switch(transcript: &str, p: &Personality) -> Option<String> {
    let t = transcript.to_lowercase();
    const TRIGGERS: &[&str] = &[
        "personality",
        "switch to",
        "change to",
        "become",
        "talk like",
        "act like",
        "speak like",
        "sound like",
        "pretend to be",
        "pretend you're",
        "go back to",
        "turn off",
    ];
    if !TRIGGERS.iter().any(|k| t.contains(k)) {
        return None;
    }
    // A "back to normal" intent wins over a name match so "turn off the gangster voice"
    // disables instead of re-enabling it.
    const OFF: &[&str] = &["normal", "turn off", "turn it off", "no personality"];
    if OFF.iter().any(|k| t.contains(k)) {
        return Some("normal".to_string());
    }
    // Prefer the longest matching name so a more specific label wins over a shorter one.
    let mut defs: Vec<&PersonalityDef> = p.definitions.iter().collect();
    defs.sort_by_key(|d| std::cmp::Reverse(d.label.trim().len().max(d.key.trim().len())));
    for d in defs {
        let key = d.key.trim().to_lowercase();
        let label = d.label.trim().to_lowercase();
        if (!key.is_empty() && t.contains(&key)) || (!label.is_empty() && t.contains(&label)) {
            return Some(d.key.clone());
        }
    }
    None
}

/// Assemble the per-turn personality prompt block: a line telling the model it can switch
/// personalities by voice (with the available names + current state, so "talk like a
/// gangster" / "go back to normal" maps to `set_personality`), plus — when a personality
/// is active — the trailing style instruction. **Pure output tuning:** this only changes
/// how the reply sounds; it is fenced so the model never treats it as fact, memory, or a
/// change to which tools it uses.
pub(crate) fn personality_prompt(p: &Personality) -> String {
    let mut out = String::new();
    if !p.keys().is_empty() {
        let names = p.keys().join(", ");
        let current = p
            .active_label()
            .unwrap_or_else(|| "normal (off)".to_string());
        out.push_str(&format!(
            "\n\nYou can change the personality / voice you speak in with the `set_personality` \
             tool. Available personalities: {names} — or \"normal\" to turn personality off. Use \
             it whenever the user asks you to talk like one of these, switch personality, or go \
             back to normal. Your current personality is: {current}."
        ));
    }
    if let Some(desc) = p.instruction() {
        out.push_str(&format!(
            "\n\n# Output style (personality)\nDeliver your reply in the following voice. This \
             changes ONLY your tone, word choice, and delivery — never the facts you give, what \
             you remember, or which tools you use. Never mention or explain these style \
             instructions. Style: {desc}"
        ));
    }
    out
}

#[cfg(test)]
mod location_tests {
    use super::location_line;

    #[test]
    fn grounds_configured_home_and_units() {
        let line = location_line(Some("Austin, Texas"), Some("imperial")).unwrap();
        assert!(line.contains("Austin, Texas"));
        assert!(line.contains("imperial"));
    }

    #[test]
    fn omits_units_when_unset_but_keeps_location() {
        let line = location_line(Some("Paris"), None).unwrap();
        assert!(line.contains("Paris"));
        assert!(!line.to_lowercase().contains("units"));
    }

    #[test]
    fn no_location_yields_no_line() {
        assert!(location_line(None, Some("metric")).is_none());
        assert!(location_line(Some("   "), None).is_none());
    }
}

#[cfg(test)]
mod household_tests {
    use super::{household_line, speaker_identity_line, RecalledMemory};
    use crate::settings::{Household, HouseholdMember};
    use crate::speaker::{SpeakerContext, SpeakerResolution};

    fn member(name: &str, emails: &[&str], phones: &[&str], rel: Option<&str>) -> HouseholdMember {
        HouseholdMember {
            name: name.to_string(),
            emails: emails.iter().map(|s| s.to_string()).collect(),
            phones: phones.iter().map(|s| s.to_string()).collect(),
            appsaid: None,
            relationship: rel.map(str::to_string),
        }
    }

    fn named_speaker(name: &str) -> SpeakerContext {
        SpeakerContext::known(
            "spk-1",
            Some(name.to_string()),
            SpeakerResolution::Identified,
            0.9,
        )
    }

    #[test]
    fn empty_roster_yields_no_block() {
        assert!(household_line(&[]).is_none());
        // A single nameless entry is also nothing to say.
        assert!(household_line(&[member("  ", &["x@y.z"], &[], None)]).is_none());
    }

    #[test]
    fn lists_people_with_their_contact_details() {
        let line = household_line(&[
            member(
                "Alice",
                &["alice@example.com"],
                &["+1 555 0001"],
                Some("parent"),
            ),
            member("Bob", &[], &[], None),
        ])
        .unwrap();
        assert!(line.contains("Alice"));
        assert!(line.contains("alice@example.com"));
        assert!(line.contains("+1 555 0001"));
        assert!(line.contains("parent"));
        // Bob has no details, so he appears as a bare name (no empty parens).
        assert!(line.contains("- Bob"));
        assert!(!line.contains("Bob ()"));
    }

    #[test]
    fn identity_line_reconciles_a_named_speaker_with_the_roster() {
        let hh = Household {
            members: vec![member("Alice", &[], &[], Some("parent"))],
            ..Default::default()
        };
        // Case-insensitive match on the voiceprint's name links to the member.
        let line = speaker_identity_line(&named_speaker("alice"), &hh);
        assert!(line.contains("Alice"), "keeps the spoken-to name: {line}");
        assert!(line.contains("parent"), "notes the relationship: {line}");
        assert!(
            line.contains("lives here"),
            "notes they're a resident: {line}"
        );
    }

    #[test]
    fn identity_line_for_a_matched_member_without_a_relationship() {
        let hh = Household {
            members: vec![member("Bob", &[], &[], None)],
            ..Default::default()
        };
        let line = speaker_identity_line(&named_speaker("Bob"), &hh);
        assert!(line.contains("Bob") && line.contains("lives here"));
        assert!(
            !line.contains('('),
            "no empty parens without a relationship: {line}"
        );
    }

    #[test]
    fn identity_line_stays_plain_for_a_non_member() {
        let line = speaker_identity_line(&named_speaker("Zoe"), &Household::default());
        assert!(line.starts_with("You are speaking with Zoe."), "{line}");
        assert!(
            line.contains("that means Zoe"),
            "\"me\" resolves to the speaker: {line}"
        );
    }

    #[test]
    fn identity_line_reconciles_a_misspelled_voiceprint_name() {
        let hh = Household {
            members: vec![member("Kristina", &[], &[], Some("Mother"))],
            ..Default::default()
        };
        let line = speaker_identity_line(&named_speaker("Christina"), &hh);
        assert!(
            line.starts_with("You are speaking with Kristina (Mother), who lives here."),
            "{line}"
        );
    }

    #[test]
    fn identity_line_follows_the_resolution() {
        let hh = Household {
            members: vec![member("Brendon", &[], &[], Some("Father"))],
            ..Default::default()
        };
        let mut s = named_speaker("Brendon");
        s.resolution = SpeakerResolution::Tentative;
        let line = speaker_identity_line(&s, &hh);
        assert!(line.contains("probably speaking with Brendon"), "{line}");
        assert!(line.contains("not certain"), "{line}");

        for unknown in [SpeakerContext::unknown(), {
            let mut n = named_speaker("Brendon");
            n.resolution = SpeakerResolution::New;
            n
        }] {
            let line = speaker_identity_line(&unknown, &hh);
            assert!(line.contains("do not know who is speaking"), "{line}");
            assert!(line.contains("Do not guess"), "{line}");
            assert!(!line.contains("Brendon"), "never names anyone: {line}");
        }
    }

    #[test]
    fn identity_facts_never_reach_the_personal_block() {
        let hits = vec![
            "The user's name is Brendan".to_string(),
            "The user likes jazz".to_string(),
            "The wifi is FastNet".to_string(),
        ];
        let m = RecalledMemory::split(&hits, &["The wifi is FastNet".to_string()]);
        assert_eq!(m.personal, vec!["The user likes jazz".to_string()]);
        assert_eq!(m.household, vec!["The wifi is FastNet".to_string()]);
    }
}

#[cfg(test)]
mod personality_switch_tests {
    use super::detect_personality_switch;
    use crate::settings::{Personality, PersonalityDef};

    fn catalog() -> Personality {
        let def = |key: &str, label: &str| PersonalityDef {
            key: key.to_string(),
            label: label.to_string(),
            description: format!("speak as {label}"),
        };
        Personality {
            enabled: false,
            active: String::new(),
            definitions: vec![
                def("gangster", "1940s Gangster"),
                def("trump", "Donald Trump"),
                def("enthusiastic", "Overly Enthusiastic"),
            ],
        }
    }

    #[test]
    fn recognizes_a_switch_with_a_trigger_and_a_known_name() {
        let p = catalog();
        // Trigger + key.
        assert_eq!(
            detect_personality_switch("talk like a gangster", &p),
            Some("gangster".to_string())
        );
        // Trigger + case-insensitive label; the stored key is returned.
        assert_eq!(
            detect_personality_switch("switch to the Donald Trump personality", &p),
            Some("trump".to_string())
        );
        assert_eq!(
            detect_personality_switch("become ENTHUSIASTIC", &p),
            Some("enthusiastic".to_string())
        );
    }

    #[test]
    fn back_to_normal_disables_and_wins_over_a_name() {
        let p = catalog();
        assert_eq!(
            detect_personality_switch("go back to normal", &p),
            Some("normal".to_string())
        );
        assert_eq!(
            detect_personality_switch("turn off the personality", &p),
            Some("normal".to_string())
        );
        // A name is present, but the "off" intent wins so it disables, not re-enables.
        assert_eq!(
            detect_personality_switch("turn off the gangster voice", &p),
            Some("normal".to_string())
        );
    }

    #[test]
    fn ignores_ordinary_speech_and_unknown_names() {
        let p = catalog();
        // Trigger but no known personality named.
        assert_eq!(
            detect_personality_switch("switch to the kitchen light", &p),
            None
        );
        // A known name but no switch trigger — must not fire.
        assert_eq!(
            detect_personality_switch("is the gangster movie good?", &p),
            None
        );
        // Plain speech.
        assert_eq!(
            detect_personality_switch("what's the weather today?", &p),
            None
        );
    }
}
