//! Wyoming Protocol wire format for the Mac orchestrator (Plan.MD §3 "Wire
//! format"; architecture.md §4).
//!
//! The codec itself — event type, framing, frame-type constants, constructors and
//! the bounded reader — lives in the shared `anamanti_protocol` crate, which the
//! device engine uses too, so the two sides cannot drift. It is re-exported here
//! wholesale so `crate::wyoming::protocol::…` paths are unchanged throughout the
//! Core.
//!
//! What remains in this module is Core-only: parsing the device's display / timer /
//! font context out of an `audio-start` data block.

pub use anamanti_protocol::*;

use serde_json::{Map, Value};

/// What the display is currently showing, as the device stamps it in the `screen`
/// block of each `audio-start`. It lets the model know what is on screen so it can
/// **drive it by voice** with the matching tool. This is a **general, extensible**
/// concept: each device screen that wants voice control reports its own context here,
/// discriminated by the `screen.kind` string on the wire. Recipe / weather / place each
/// add their own variant (and a device-side setter + a Core-side prompt line) without
/// touching the transport.
///
/// Most contexts enrich the LLM turn, but a context can also **suppress** it: the
/// [`Diagnostics`](DisplayContext::Diagnostics) screen makes the turn transcription-only
/// (the orchestrator echoes the transcript and stops, never reaching the LLM/TTS).
///
/// See `plans/architecture.md` §4 ("Display context") and `plans/RecipePlan.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayContext {
    /// The guided-recipe screen is up (`kind: "recipe"`).
    Recipe(RecipeScreen),
    /// The weather forecast screen is up (`kind: "weather"`).
    Weather(WeatherScreen),
    /// The place card is up (`kind: "place"`).
    Place(PlaceScreen),
    /// The audio-diagnostics screen is up (`kind: "diagnostics"`). This is a
    /// **transcription-only** context: the turn pipeline echoes the STT transcript to
    /// the device for on-screen display and then stops — no speaker ID, no System-1, no
    /// LLM, no TTS, no memory/chat-log. The screen is a diagnostic surface only, so a
    /// voice tone it hears must never be processed by the assistant. Carries no state.
    Diagnostics,
    /// The guided voice-enrollment screen is up (`kind: "enroll"`;
    /// SpeakerEnrollmentPlan.md Phase 2). Like [`Diagnostics`](Self::Diagnostics) this
    /// **suppresses** the normal turn: the orchestrator routes the captured voiced PCM
    /// to the enrollment accumulator (or the read-only verify check) instead of the
    /// LLM, and never recalls/writes memory or speaks a generated reply.
    Enroll(EnrollScreen),
}

/// The enrollment screen's per-capture state, carried in a [`DisplayContext::Enroll`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EnrollScreen {
    /// Device-chosen id grouping this enrollment's captures (one per "Add person" flow).
    pub session: String,
    /// The name the person is being enrolled under (for the spoken/shown confirmation).
    pub name: String,
    /// 0-based index of the phrase being captured (progress display only).
    pub sample_index: u32,
    /// `"sample"` (accumulate a training sample) or `"verify"` (read-only recognition
    /// check after the profile is committed). Unknown values are treated as `"sample"`.
    pub phase: String,
}

/// The recipe screen's state, as carried in a [`DisplayContext::Recipe`]. Tells the
/// model which tab is showing and whether the pane is scrolled to the top/bottom, so it
/// can drive the screen with `recipe_control` (switch tab / scroll) or `close_recipe`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecipeScreen {
    pub title: String,
    /// Active tab: `"overview"` / `"ingredients"` / `"steps"`.
    pub tab: String,
    pub at_top: bool,
    pub at_bottom: bool,
    pub ingredient_count: u32,
    pub step_count: u32,
}

/// The weather screen's state, as carried in a [`DisplayContext::Weather`]. Tells the
/// model the forecast is on screen (and what it currently shows), so it can answer
/// follow-ups ("what about tomorrow" → `weather_lookup`) or `close_weather` on "close it".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WeatherScreen {
    /// The place the forecast is for (e.g. "Kitchener, Ontario").
    pub location: String,
    /// `"imperial"` (°F) or `"metric"` (°C).
    pub units: String,
    /// The current temperature shown, whole degrees.
    pub temp: i32,
    /// A short plain-language description ("partly cloudy").
    pub description: String,
}

/// The place card's state, as carried in a [`DisplayContext::Place`]. Tells the model a
/// place card is on screen (and for which place), so it can answer follow-ups ("is it
/// open on Sunday?") or `close_places` on "close it".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlaceScreen {
    /// The place shown (e.g. "Blue Bottle Coffee").
    pub name: String,
    /// Its one-line address.
    pub address: String,
}

/// Pull the display context out of an `audio-start` data block's `screen` object, or
/// `None` when the display reports nothing (an idle screen, or a device that doesn't
/// stamp context). Dispatches on `screen.kind`; an unknown kind (e.g. a newer device
/// reporting a screen this Core doesn't understand yet) yields `None`.
pub fn display_context(data: &Value) -> Option<DisplayContext> {
    let screen = data.as_object()?.get("screen")?.as_object()?;
    match screen.get("kind").and_then(Value::as_str)? {
        "recipe" => {
            parse_recipe_screen(screen.get("recipe")?.as_object()?).map(DisplayContext::Recipe)
        }
        "weather" => {
            parse_weather_screen(screen.get("weather")?.as_object()?).map(DisplayContext::Weather)
        }
        "place" => parse_place_screen(screen.get("place")?.as_object()?).map(DisplayContext::Place),
        // The audio-diagnostics screen carries no state — its presence alone switches the
        // turn to transcription-only (the orchestrator short-circuits before any LLM work).
        "diagnostics" => Some(DisplayContext::Diagnostics),
        "enroll" => {
            parse_enroll_screen(screen.get("enroll")?.as_object()?).map(DisplayContext::Enroll)
        }
        _ => None,
    }
}

/// Parse the `screen.enroll` object into an [`EnrollScreen`]. A missing `session`
/// is the one hard requirement (it keys the accumulator); the rest default.
fn parse_enroll_screen(obj: &serde_json::Map<String, Value>) -> Option<EnrollScreen> {
    let session = obj.get("session").and_then(Value::as_str)?.to_string();
    if session.is_empty() {
        return None;
    }
    Some(EnrollScreen {
        session,
        name: obj
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        sample_index: obj.get("sample_index").and_then(Value::as_u64).unwrap_or(0) as u32,
        phase: match obj.get("phase").and_then(Value::as_str) {
            Some("verify") => "verify".to_string(),
            _ => "sample".to_string(),
        },
    })
}

/// Active-timer state the device reports on **every** turn, **orthogonal to the
/// foreground widget** (`DisplayContext`): a timer can be counting down while a recipe,
/// the weather, or the idle slideshow is front-and-center, so this rides as a `timers`
/// sibling of `kind` inside the `screen` block rather than as a `kind` variant. The
/// device owns timers (countdown + alarm), so this is the Core's only view of them — it
/// unblocks the System-1 `timer_query` / `timer_cancel` / `stop_dismiss` decisions
/// (plans/system1-fast-decisions.md §17, §19). `ringing` is intentionally absent in this
/// phase (today's alarm is a finite bell that self-removes — see §19.7).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TimerContext {
    /// How many timers are currently running (0 = none).
    pub running: u32,
    /// Whole seconds until the *soonest-to-fire* timer, so "how much time is left?" can
    /// be answered without any Core-side timer state. `None` when nothing is running.
    pub next_remaining_secs: Option<u64>,
    /// Labels of the running timers (only the non-empty ones), for future labeled
    /// query/cancel. May be empty even when `running > 0` (unlabeled timers).
    pub labels: Vec<String>,
}

impl TimerContext {
    /// Whether any timer is currently running.
    pub fn any_running(&self) -> bool {
        self.running > 0
    }
}

/// Font-scaling state the device reports on **every** turn, **orthogonal to the foreground
/// widget** (`DisplayContext`): it describes whether a resizable text surface is on screen
/// and the current global font scale, so it rides as a `font` sibling of `kind` inside the
/// `screen` block (like `timers`). Lets the Core resolve "increase font" / "decrease font"
/// only when there is something to resize (`scalable`), and report when the text is already
/// at the maximum/minimum. `scale` is stored as **permille** (e.g. 1100 = 1.1×) so this
/// derives `Eq` alongside [`DeviceContext`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FontContext {
    /// Whether a resizable text surface is currently on screen (the resolve gate).
    pub scalable: bool,
    /// The current global font scale, in permille (1000 = 1.0×). 0 when never reported.
    pub scale_permille: u16,
    /// Whether the font is already at the minimum scale (a "decrease" would be a no-op).
    pub at_min: bool,
    /// Whether the font is already at the maximum scale (an "increase" would be a no-op).
    pub at_max: bool,
}

impl FontContext {
    /// The current global font scale as a multiplier (1.0 = unscaled); 1.0 when unset.
    pub fn scale(&self) -> f32 {
        if self.scale_permille == 0 {
            1.0
        } else {
            self.scale_permille as f32 / 1000.0
        }
    }
}

/// The full device context for a turn: the foreground widget (if any) **plus** the
/// orthogonal background state (active timers now; media later). Parsed once from the
/// `audio-start` data via [`device_context`]. Keeping [`display_context`] separate lets
/// the System-2 prompt line stay widget-only while System-1 sees the superset.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceContext {
    /// The foreground widget, discriminated by `screen.kind`. `None` on an idle screen.
    pub widget: Option<DisplayContext>,
    /// Background timer state, reported regardless of `widget`.
    pub timers: TimerContext,
    /// Font-scaling state, reported regardless of `widget`.
    pub font: FontContext,
}

impl DeviceContext {
    /// A short label for the foreground widget (`"recipe"` / `"weather"`), or `None` on
    /// an idle screen — the compact form System-1 routes on.
    pub fn widget_label(&self) -> Option<&'static str> {
        match self.widget {
            Some(DisplayContext::Recipe(_)) => Some("recipe"),
            Some(DisplayContext::Weather(_)) => Some("weather"),
            Some(DisplayContext::Place(_)) => Some("place"),
            // Diagnostics never reaches System-1/System-2 (the turn short-circuits before
            // them), so it reports no widget label — nothing can leak into a decision or
            // prompt even if that ordering changes later. Enrollment is the same: the turn
            // is consumed by the voiceprint accumulator, never the LLM.
            Some(DisplayContext::Diagnostics) | Some(DisplayContext::Enroll(_)) => None,
            None => None,
        }
    }
}

/// Parse the active-timer state from an `audio-start` data block's `screen.timers`
/// object. Missing/absent → an empty [`TimerContext`] (older device, or no timers),
/// **independently of `screen.kind`** — so a timer running behind another widget (or an
/// idle screen with no `kind`) is still seen, which [`display_context`] alone would drop.
pub fn timer_context(data: &Value) -> TimerContext {
    let Some(timers) = data
        .as_object()
        .and_then(|o| o.get("screen"))
        .and_then(Value::as_object)
        .and_then(|s| s.get("timers"))
        .and_then(Value::as_object)
    else {
        return TimerContext::default();
    };
    let running = timers.get("running").and_then(Value::as_u64).unwrap_or(0) as u32;
    let next_remaining_secs = timers.get("next_remaining_secs").and_then(Value::as_u64);
    let labels = timers
        .get("labels")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    TimerContext {
        running,
        next_remaining_secs,
        labels,
    }
}

/// Parse the font-scaling state from an `audio-start` data block's `screen.font` object.
/// Missing/absent → a default [`FontContext`] (`scalable == false`), **independently of
/// `screen.kind`** — the font state is reported even on an idle screen. `scale` arrives as
/// a float multiplier on the wire and is stored as permille.
pub fn font_context(data: &Value) -> FontContext {
    let Some(font) = data
        .as_object()
        .and_then(|o| o.get("screen"))
        .and_then(Value::as_object)
        .and_then(|s| s.get("font"))
        .and_then(Value::as_object)
    else {
        return FontContext::default();
    };
    let scalable = font
        .get("scalable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let scale_permille = font
        .get("scale")
        .and_then(Value::as_f64)
        .map(|s| (s * 1000.0).round().clamp(0.0, u16::MAX as f64) as u16)
        .unwrap_or(0);
    let at_min = font.get("at_min").and_then(Value::as_bool).unwrap_or(false);
    let at_max = font.get("at_max").and_then(Value::as_bool).unwrap_or(false);
    FontContext {
        scalable,
        scale_permille,
        at_min,
        at_max,
    }
}

/// Parse the full [`DeviceContext`] (foreground widget + background timers + font state)
/// from an `audio-start` data block. The turn pipeline uses this so System-1 sees the
/// background state; [`display_context`] remains for the System-2 prompt line.
pub fn device_context(data: &Value) -> DeviceContext {
    DeviceContext {
        widget: display_context(data),
        timers: timer_context(data),
        font: font_context(data),
    }
}

/// Parse the `weather` sub-object of a `screen` block into a [`WeatherScreen`].
fn parse_weather_screen(weather: &Map<String, Value>) -> Option<WeatherScreen> {
    Some(WeatherScreen {
        location: weather
            .get("location")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        units: weather
            .get("units")
            .and_then(Value::as_str)
            .unwrap_or("metric")
            .to_string(),
        temp: weather.get("temp").and_then(Value::as_i64).unwrap_or(0) as i32,
        description: weather
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// Parse the `place` sub-object of a `screen` block into a [`PlaceScreen`].
fn parse_place_screen(place: &Map<String, Value>) -> Option<PlaceScreen> {
    Some(PlaceScreen {
        name: place
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        address: place
            .get("address")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// Parse the `recipe` sub-object of a `screen` block into a [`RecipeScreen`].
fn parse_recipe_screen(recipe: &Map<String, Value>) -> Option<RecipeScreen> {
    let s = |k: &str| {
        recipe
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let b = |k: &str, dflt: bool| recipe.get(k).and_then(Value::as_bool).unwrap_or(dflt);
    let n = |k: &str| recipe.get(k).and_then(Value::as_u64).unwrap_or(0) as u32;
    let tab = recipe
        .get("tab")
        .and_then(Value::as_str)
        .unwrap_or("overview")
        .to_string();
    Some(RecipeScreen {
        title: s("title"),
        tab,
        at_top: b("at_top", true),
        at_bottom: b("at_bottom", false),
        ingredient_count: n("ingredient_count"),
        step_count: n("step_count"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn font_context_parses_orthogonally_to_widget_kind() {
        // No screen block → default (not scalable), independent of any widget.
        let plain = WyomingEvent::audio_start(AudioFormat::PCM_16K_MONO, 0);
        assert_eq!(font_context(&plain.data), FontContext::default());

        // Font reported WITHOUT any `kind` (idle screen) is still seen.
        let idle_with_font = json!({
            "screen": { "font": { "scalable": true, "scale": 1.1, "at_min": false, "at_max": false } }
        });
        let font = font_context(&idle_with_font);
        assert_eq!(
            font,
            FontContext {
                scalable: true,
                scale_permille: 1100,
                at_min: false,
                at_max: false,
            }
        );
        assert!((font.scale() - 1.1).abs() < 1e-6);
        // display_context sees nothing here (no `kind`), proving orthogonality.
        assert_eq!(display_context(&idle_with_font), None);

        // Font reported alongside a widget + timers: device_context carries all three.
        let combined = json!({
            "screen": {
                "kind": "recipe",
                "recipe": { "title": "Carbonara", "tab": "steps", "at_top": false, "at_bottom": true, "ingredient_count": 5, "step_count": 7 },
                "timers": { "running": 1, "next_remaining_secs": 30 },
                "font": { "scalable": true, "scale": 1.6, "at_min": false, "at_max": true }
            }
        });
        let dev = device_context(&combined);
        assert_eq!(dev.widget_label(), Some("recipe"));
        assert_eq!(dev.timers.running, 1);
        assert!(dev.font.scalable);
        assert!(dev.font.at_max);
        assert_eq!(dev.font.scale_permille, 1600);
    }

    #[test]
    fn enroll_screen_parses_and_suppresses_the_widget_label() {
        let data = json!({
            "rate": 16_000, "width": 2, "channels": 1, "timestamp": 0,
            "screen": {
                "kind": "enroll",
                "enroll": { "session": "sess-1", "name": "Dana", "sample_index": 2, "phase": "verify" },
            },
        });
        assert_eq!(
            display_context(&data),
            Some(DisplayContext::Enroll(EnrollScreen {
                session: "sess-1".to_string(),
                name: "Dana".to_string(),
                sample_index: 2,
                phase: "verify".to_string(),
            }))
        );
        // Like diagnostics, enrollment never surfaces a widget label (can't leak to the LLM).
        assert_eq!(device_context(&data).widget_label(), None);

        // An unknown phase defaults to "sample"; a missing session → no context.
        let defaulted =
            json!({ "screen": { "kind": "enroll", "enroll": { "session": "s", "name": "" } } });
        assert_eq!(
            display_context(&defaulted),
            Some(DisplayContext::Enroll(EnrollScreen {
                session: "s".to_string(),
                name: String::new(),
                sample_index: 0,
                phase: "sample".to_string(),
            }))
        );
        let no_session = json!({ "screen": { "kind": "enroll", "enroll": { "name": "x" } } });
        assert_eq!(display_context(&no_session), None);
    }

    #[test]
    fn display_context_reads_audio_start_screen_block_only() {
        // A plain audio-start (no `screen` block) → no display context.
        let plain = WyomingEvent::audio_start(AudioFormat::PCM_16K_MONO, 0);
        assert_eq!(display_context(&plain.data), None);

        // A device with the recipe screen up stamps the `screen` block.
        let marked = json!({
            "rate": 16_000, "width": 2, "channels": 1, "timestamp": 0,
            "screen": {
                "kind": "recipe",
                "recipe": {
                    "title": "Carbonara",
                    "tab": "steps",
                    "at_top": false,
                    "at_bottom": true,
                    "ingredient_count": 6,
                    "step_count": 4,
                },
            },
        });
        assert_eq!(
            display_context(&marked),
            Some(DisplayContext::Recipe(RecipeScreen {
                title: "Carbonara".to_string(),
                tab: "steps".to_string(),
                at_top: false,
                at_bottom: true,
                ingredient_count: 6,
                step_count: 4,
            }))
        );

        // A device with the weather screen up stamps a `weather` block.
        let weather = json!({
            "screen": {
                "kind": "weather",
                "weather": {
                    "location": "Kitchener, Ontario",
                    "units": "metric",
                    "temp": 18,
                    "description": "partly cloudy",
                },
            },
        });
        assert_eq!(
            display_context(&weather),
            Some(DisplayContext::Weather(WeatherScreen {
                location: "Kitchener, Ontario".to_string(),
                units: "metric".to_string(),
                temp: 18,
                description: "partly cloudy".to_string(),
            }))
        );

        // The audio-diagnostics screen carries no state — its bare `kind` is enough, and
        // `widget_label()` reports nothing (it must never route a decision or prompt).
        let diag = json!({ "screen": { "kind": "diagnostics" } });
        assert_eq!(display_context(&diag), Some(DisplayContext::Diagnostics));
        assert_eq!(device_context(&diag).widget_label(), None);

        // An unknown screen kind (a screen this Core doesn't understand yet) is ignored.
        let other = json!({ "screen": { "kind": "music" } });
        assert_eq!(display_context(&other), None);
    }

    #[test]
    fn timer_context_parses_orthogonally_to_widget_kind() {
        // No screen block at all → empty timer context.
        let plain = WyomingEvent::audio_start(AudioFormat::PCM_16K_MONO, 0);
        assert_eq!(timer_context(&plain.data), TimerContext::default());
        assert!(!timer_context(&plain.data).any_running());

        // A screen block with a widget but no timers → still empty.
        let widget_only = json!({ "screen": { "kind": "recipe" } });
        assert_eq!(timer_context(&widget_only), TimerContext::default());

        // Timers reported WITHOUT any `kind` (idle screen, timer in background) must
        // still be seen — this is exactly what display_context() alone would drop.
        let idle_with_timer = json!({
            "screen": { "timers": { "running": 2, "next_remaining_secs": 125, "labels": ["pasta", ""] } }
        });
        assert_eq!(
            timer_context(&idle_with_timer),
            TimerContext {
                running: 2,
                next_remaining_secs: Some(125),
                labels: vec!["pasta".to_string()], // the empty label is filtered out
            }
        );
        // display_context sees nothing here (no `kind`), proving orthogonality.
        assert_eq!(display_context(&idle_with_timer), None);

        // Timers reported ALONGSIDE a foreground widget: device_context carries both.
        let recipe_plus_timer = json!({
            "screen": {
                "kind": "weather",
                "weather": { "location": "Austin", "units": "imperial", "temp": 90, "description": "sunny" },
                "timers": { "running": 1, "next_remaining_secs": 30 }
            }
        });
        let dev = device_context(&recipe_plus_timer);
        assert_eq!(dev.widget_label(), Some("weather"));
        assert_eq!(dev.timers.running, 1);
        assert_eq!(dev.timers.next_remaining_secs, Some(30));
        assert!(dev.timers.labels.is_empty());
    }

    #[test]
    fn device_context_defaults_when_empty() {
        let plain = WyomingEvent::audio_start(AudioFormat::PCM_16K_MONO, 0);
        let dev = device_context(&plain.data);
        assert_eq!(dev, DeviceContext::default());
        assert_eq!(dev.widget_label(), None);
        assert!(!dev.timers.any_running());
    }
}
