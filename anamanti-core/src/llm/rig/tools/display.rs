//! Display-screen tools: the recipe screen (`recipe_lookup`, `recipe_control`,
//! `close_recipe`), the weather screen (`weather_lookup`, `close_weather`) and the
//! place card (`places_lookup`, `close_places`), and the music player (`music_screen`,
//! screen-only — playback is `spotify_control`). Each fetches through an injected
//! provider (testable offline), pushes the result to the display as a
//! [`DeviceAction`], and returns a short spoken confirmation.

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Local;
use rig_core::completion::ToolDefinition;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{close_tool, definition, emit, parse_args, ToolCtx, VoiceTool};
use crate::directions::LiveHomeLocation;
use crate::llm::{ActionSink, DeviceAction, MusicNav, RecipeNav};
use crate::music::MusicDisplayStyle;
use crate::recipe::RecipeProvider;

/// Tool name: show a recipe on the display.
pub(super) const RECIPE_LOOKUP: &str = "recipe_lookup";
/// Tool name: dismiss the recipe screen.
const CLOSE_RECIPE: &str = "close_recipe";
/// Tool name: navigate the already-open recipe screen (switch tab / scroll).
const RECIPE_CONTROL: &str = "recipe_control";
/// Tool name: show the weather forecast on the display.
pub(super) const WEATHER_LOOKUP: &str = "weather_lookup";
/// Tool name: dismiss the weather screen.
const CLOSE_WEATHER: &str = "close_weather";
/// Tool name: look up a place and show its card on the display.
pub(super) const PLACES_LOOKUP: &str = "places_lookup";
/// Tool name: dismiss the place card.
const CLOSE_PLACES: &str = "close_places";
/// Tool name: drive the on-screen music player (show now-playing / up-next / close).
pub(super) const MUSIC_SCREEN: &str = "music_screen";

// ---------------------------------------------------------------------------
// Recipe
// ---------------------------------------------------------------------------

/// `recipe_lookup`, `close_recipe` and `recipe_control`, in advertised order.
pub(super) fn recipe_tools(provider: Arc<dyn RecipeProvider>) -> Vec<Arc<dyn VoiceTool>> {
    vec![
        Arc::new(RecipeLookup { provider }),
        close_tool(
            CLOSE_RECIPE,
            "Close the recipe screen on the display and return to the idle screen. \
             Use when the user says they're done cooking or asks to close/dismiss the \
             recipe.",
            DeviceAction::DismissRecipe,
            "the recipe",
            "Okay, closing the recipe.",
        ),
        Arc::new(RecipeControl),
    ]
}

/// Typed arguments for `recipe_lookup`.
#[derive(Debug, Deserialize)]
struct RecipeArgs {
    /// The dish to find a recipe for (e.g. "carbonara"). Required.
    dish: String,
    /// A specific recipe URL to use instead of searching (optional; the model
    /// usually only has a dish name).
    #[serde(default)]
    url: Option<String>,
}

/// Finds a source page for a dish, parses it, and pushes the structured recipe to the
/// display ([`DeviceAction::ShowRecipe`]).
struct RecipeLookup {
    provider: Arc<dyn RecipeProvider>,
}

#[async_trait]
impl VoiceTool for RecipeLookup {
    fn name(&self) -> &'static str {
        RECIPE_LOOKUP
    }

    fn definition(&self) -> ToolDefinition {
        definition(
            RECIPE_LOOKUP,
            "Find a cooking recipe for a dish and show it on the display's \
             recipe screen (Overview / Ingredients / Steps tabs). Use whenever \
             the user asks for a recipe, how to make/cook a dish, or to pull up \
             a recipe. Returns a short spoken confirmation to relay.",
            json!({
                "type": "object",
                "properties": {
                    "dish": {
                        "type": "string",
                        "description": "The dish to find a recipe for, e.g. \"carbonara\" or \
                                        \"chocolate chip cookies\"."
                    },
                    "url": {
                        "type": "string",
                        "description": "A specific recipe page URL to use instead of searching \
                                        (optional)."
                    }
                },
                "required": ["dish"]
            }),
        )
    }

    /// Covers the whole recipe screen (lookup, navigation, close).
    fn guidance(&self) -> Option<&'static str> {
        Some(
            "You can pull up cooking recipes on the display with the `recipe_lookup` tool. \
             Whenever the user asks for a recipe, how to make or cook a dish, or to show a \
             recipe for something, you MUST call `recipe_lookup` with the dish name — never \
             recite a full recipe as prose. When a recipe is already on screen (the turn \
             context will tell you, including which tab, scroll position and highlighted \
             step, plus the full ingredient list and steps), use `recipe_control` to \
             navigate it — switching to the Overview / Ingredients / Steps tab or scrolling \
             the current tab whenever the user says things like \"show the ingredients\", \
             \"go to the steps\", \"scroll down\", \"scroll back up\", or \"start over\". \
             For a specific step (\"go to step 4\", \"next step\", \"previous step\", \
             \"what's step 3\") ALWAYS use `recipe_control` `go_to_step` with the step \
             number — never scroll to find a step. Answer questions about the recipe on \
             screen (\"how many tomatoes do I need?\", \"how long does it bake?\") directly \
             from the ingredients and steps in the turn context — do not look the recipe \
             up again. Use `close_recipe` when they say they're done cooking or ask to \
             close the recipe. Relay each tool's short spoken confirmation.",
        )
    }

    async fn invoke(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String> {
        let args: RecipeArgs = parse_args(args, RECIPE_LOOKUP)?;
        let sink = ctx.require_sink("no display is connected to show a recipe on right now")?;
        let recipe = self
            .provider
            .lookup(&args.dish, args.url.as_deref())
            .await?;
        let confirmation = crate::recipe::render_confirmation(&recipe);
        emit(
            Some(sink),
            DeviceAction::ShowRecipe(recipe),
            "the recipe could show",
        )?;
        Ok(confirmation)
    }
}

/// Typed arguments for `recipe_control`.
#[derive(Debug, Deserialize)]
struct RecipeControlArgs {
    /// One of the fixed navigation actions.
    action: String,
    /// The 1-based step number for `go_to_step`.
    #[serde(default)]
    step: Option<u32>,
}

/// Navigate the open recipe screen ([`DeviceAction::RecipeControl`]).
struct RecipeControl;

#[async_trait]
impl VoiceTool for RecipeControl {
    fn name(&self) -> &'static str {
        RECIPE_CONTROL
    }

    fn definition(&self) -> ToolDefinition {
        definition(
            RECIPE_CONTROL,
            "Navigate the recipe screen that is already open on the display: switch \
             which tab is showing, scroll the current tab, or jump to a specific step \
             (which is scrolled into view and highlighted). Use ONLY when a recipe \
             is currently on screen (the turn context says so). To close it, use \
             close_recipe instead.",
            json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": [
                            "show_overview",
                            "show_ingredients",
                            "show_steps",
                            "scroll_up",
                            "scroll_down",
                            "scroll_top",
                            "scroll_bottom",
                            "go_to_step"
                        ],
                        "description": "The navigation action: switch to the Overview / Ingredients / \
                                        Steps tab, scroll the current tab up/down a page or to the \
                                        top/bottom, or `go_to_step` to show and highlight one step. \
                                        For \"go to step 4\", \"next step\", \"previous step\" or \
                                        \"what's step 3\" ALWAYS use go_to_step (never scroll) — work \
                                        out the number from the highlighted step in the turn context."
                    },
                    "step": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "The 1-based step number. Required for go_to_step; ignored otherwise."
                    }
                },
                "required": ["action"]
            }),
        )
    }

    async fn invoke(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String> {
        let args: RecipeControlArgs = parse_args(args, RECIPE_CONTROL)?;
        let (nav, confirmation) = match args.action.as_str() {
            "show_overview" => (RecipeNav::TabOverview, "Showing the overview.".to_string()),
            "show_ingredients" => (
                RecipeNav::TabIngredients,
                "Showing the ingredients.".to_string(),
            ),
            "show_steps" => (RecipeNav::TabSteps, "Showing the steps.".to_string()),
            "scroll_up" => (RecipeNav::ScrollUp, "Scrolling up.".to_string()),
            "scroll_down" => (RecipeNav::ScrollDown, "Scrolling down.".to_string()),
            "scroll_top" => (RecipeNav::ScrollTop, "Back to the top.".to_string()),
            "scroll_bottom" => (
                RecipeNav::ScrollBottom,
                "Jumping to the bottom.".to_string(),
            ),
            "go_to_step" => match args.step.filter(|&n| n >= 1) {
                Some(n) => (RecipeNav::Step(n), format!("Here's step {n}.")),
                None => anyhow::bail!("recipe_control go_to_step needs a step number of 1 or more"),
            },
            other => anyhow::bail!("unknown recipe_control action `{other}`"),
        };
        emit(
            ctx.actions,
            DeviceAction::RecipeControl(nav),
            "the recipe could update",
        )?;
        Ok(confirmation)
    }
}

// ---------------------------------------------------------------------------
// Music player
// ---------------------------------------------------------------------------

/// The `music_screen` tool. Advertised wherever `spotify_control` is (screen-only;
/// transport stays on `spotify_control`).
pub(super) fn music_screen() -> Arc<dyn VoiceTool> {
    Arc::new(MusicScreen)
}

/// Typed arguments for `music_screen`.
#[derive(Debug, Deserialize)]
struct MusicScreenArgs {
    /// One of the fixed screen actions.
    action: String,
}

/// Show / switch / close the on-screen music player ([`DeviceAction::MusicScreen`]).
/// The device decides whether anything is playing.
struct MusicScreen;

#[async_trait]
impl VoiceTool for MusicScreen {
    fn name(&self) -> &'static str {
        MUSIC_SCREEN
    }

    fn definition(&self) -> ToolDefinition {
        definition(
            MUSIC_SCREEN,
            "Control what the music player shows on the display (Spotify and radio share \
             it): open the full now-playing screen, open the up-next queue, close the \
             player back to the ambient screen, or switch which widget is shown whenever \
             music plays — `fullscreen` (the big full-screen player, remembered) or \
             `compact` (the small bar, remembered). Use this for requests like \"show me \
             what's playing\", \"show the queue / what's up next\", \"hide the music\", \
             \"make the music full screen\" or \"use the small music player\". This only \
             changes the SCREEN — to play/pause/skip/change volume use spotify_control \
             instead.",
            json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["show", "show_queue", "close", "fullscreen", "compact"],
                        "description": "The screen action: `show` opens the now-playing view, \
                                        `show_queue` opens the up-next list, `close` returns to \
                                        the ambient display, `fullscreen` / `compact` switch \
                                        (and remember) the widget shown while music plays."
                    }
                },
                "required": ["action"]
            }),
        )
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(
            "You can also control what the music player SHOWS on the display with the \
             `music_screen` tool (separate from `spotify_control`, which changes playback). \
             Call `music_screen` with action=show when the user says things like \"show me \
             what's playing\" or \"pull up the music\", action=show_queue for \"what's up \
             next\" or \"show the queue\", and action=close for \"hide the music\" or \"close \
             the player\". To change which music widget is shown whenever music plays, \
             call it with action=fullscreen (\"make the music full screen\", \"use the big \
             player\") or action=compact (\"use the small music player\", \"shrink the \
             music\"); that choice is remembered for Spotify and radio. The turn context will \
             tell you when the music screen is already open and what's on it. Relay its short \
             spoken confirmation.",
        )
    }

    async fn invoke(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String> {
        let args: MusicScreenArgs = parse_args(args, MUSIC_SCREEN)?;
        let (action, confirmation) = match args.action.as_str() {
            "show" => (
                DeviceAction::MusicScreen(MusicNav::NowPlaying),
                "Here's what's playing.",
            ),
            "show_queue" => (
                DeviceAction::MusicScreen(MusicNav::Queue),
                "Here's what's up next.",
            ),
            "close" => (
                DeviceAction::MusicScreen(MusicNav::Close),
                "Closing the music player.",
            ),
            "fullscreen" => (
                DeviceAction::MusicDisplayStyle(MusicDisplayStyle::Fullscreen),
                "Okay, music will play full screen.",
            ),
            "compact" => (
                DeviceAction::MusicDisplayStyle(MusicDisplayStyle::Compact),
                "Okay, I'll use the small music player.",
            ),
            other => anyhow::bail!("unknown music_screen action `{other}`"),
        };
        emit(ctx.actions, action, "the music screen could update")?;
        Ok(confirmation.to_string())
    }
}

// ---------------------------------------------------------------------------
// Weather
// ---------------------------------------------------------------------------

/// `weather_lookup` and `close_weather`, in advertised order.
pub(super) fn weather_tools(
    provider: Arc<dyn crate::weather::WeatherProvider>,
    home_location: LiveHomeLocation,
    imperial: bool,
) -> Vec<Arc<dyn VoiceTool>> {
    vec![
        Arc::new(WeatherLookup {
            provider,
            home_location,
            imperial,
        }),
        close_tool(
            CLOSE_WEATHER,
            "Close the weather screen on the display and return to the idle screen. \
             Use when the user asks to close/dismiss the weather.",
            DeviceAction::DismissWeather,
            "the weather",
            "Okay, closing the weather.",
        ),
    ]
}

/// Typed arguments for `weather_lookup`.
#[derive(Debug, Deserialize)]
struct WeatherArgs {
    /// A place to get the weather for (optional; defaults to the household home
    /// location when omitted, so "what's the weather" works with no place named).
    #[serde(default)]
    location: Option<String>,
    /// When to get the forecast for (optional; defaults to right now). Accepts
    /// `today`/`now`, `tomorrow`, a weekday name ("Saturday"), or `YYYY-MM-DD`.
    #[serde(default)]
    when: Option<String>,
    /// Which display layout to show (optional; defaults to the hourly view). `"hourly"`
    /// shows the big-conditions panel + 10-hour row; `"week"` shows the separate 7-day
    /// forecast widget (7 columns of daily highs/lows).
    #[serde(default)]
    layout: Option<String>,
}

/// Fetches current conditions + a 7-day forecast for a place (defaulting to the
/// household home location) and pushes it to the display
/// ([`DeviceAction::ShowWeather`]).
struct WeatherLookup {
    provider: Arc<dyn crate::weather::WeatherProvider>,
    home_location: LiveHomeLocation,
    imperial: bool,
}

#[async_trait]
impl VoiceTool for WeatherLookup {
    fn name(&self) -> &'static str {
        WEATHER_LOOKUP
    }

    fn definition(&self) -> ToolDefinition {
        definition(
            WEATHER_LOOKUP,
            "Get the weather and show it on the display's weather screen. Use \
             whenever the user asks about the weather, temperature, forecast, or \
             how hot/cold/rainy it is. If the user names no place, omit `location` \
             — it defaults to home. For a future day (e.g. \"weather on \
             Saturday\", \"will it rain tomorrow\") pass `when`; omit it for right \
             now. Set `layout` to \"week\" for a multi-day/weekly request (e.g. \
             \"7-day forecast\", \"what's the week look like\", \"forecast for the \
             rest of the week\") to show the 7-day view; otherwise omit it for the \
             default hourly view. Returns a short spoken confirmation to relay.",
            json!({
                "type": "object",
                "properties": {
                    "location": {
                        "type": "string",
                        "description": "The place to get the weather for, e.g. \"Paris\" or \
                                        \"Denver, Colorado\". Omit for the user's home location."
                    },
                    "when": {
                        "type": "string",
                        "description": "The day to forecast: \"today\"/\"now\", \"tomorrow\", a \
                                        weekday name like \"Saturday\", or a date \"YYYY-MM-DD\". \
                                        Omit for right now."
                    },
                    "layout": {
                        "type": "string",
                        "enum": ["hourly", "week"],
                        "description": "The display layout: \"hourly\" (default) for the \
                                        big-conditions panel + 10-hour row, or \"week\" for the \
                                        7-day forecast view. Use \"week\" for weekly/multi-day \
                                        requests; omit otherwise."
                    }
                }
            }),
        )
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(
            "You can pull up the weather on the display with the `weather_lookup` tool. \
             Whenever the user asks about the weather, the temperature, the forecast, or \
             whether it will rain/snow/be hot or cold, you MUST call `weather_lookup` (omit \
             `location` to use home) and relay its short spoken confirmation — never guess at \
             conditions or say you can't check the weather. When they ask about a future day \
             (\"tomorrow\", \"this weekend\", \"Saturday\"), pass `when`. Use `close_weather` \
             when they ask to close/dismiss the weather.",
        )
    }

    async fn invoke(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String> {
        let args: WeatherArgs = parse_args(args, WEATHER_LOOKUP)?;
        let sink = ctx.require_sink("no display is connected to show the weather on right now")?;
        // An omitted/empty `location` means "use home" — the device's own location, which
        // gets the plain confirmation phrasing (no place name, no "on the screen" trailer).
        let explicit = args
            .location
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let is_home = explicit.is_none();
        let location = explicit
            .or_else(|| self.home_location.weather())
            .context("no location was given and no home location is set")?;
        let when = crate::weather::resolve_when(args.when.as_deref(), Local::now())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut report = self.provider.fetch(&location, self.imperial, when).await?;
        // A "week" request switches the display to the 7-day forecast widget; anything
        // else (including an omitted/unknown value) keeps the default hourly view.
        if args
            .layout
            .as_deref()
            .map(str::trim)
            .is_some_and(|l| l.eq_ignore_ascii_case("week"))
        {
            report.layout = "week".to_string();
        }
        let confirmation = crate::weather::render_confirmation(&report, is_home);
        emit(
            Some(sink),
            DeviceAction::ShowWeather(report),
            "the weather could show",
        )?;
        Ok(confirmation)
    }
}

// ---------------------------------------------------------------------------
// Places
// ---------------------------------------------------------------------------

/// `places_lookup` and `close_places`, in advertised order.
pub(super) fn places_tools(
    provider: Arc<dyn crate::places::PlacesProvider>,
    home_location: LiveHomeLocation,
) -> Vec<Arc<dyn VoiceTool>> {
    vec![
        Arc::new(PlacesLookup {
            provider,
            home_location,
        }),
        close_tool(
            CLOSE_PLACES,
            "Close the place card on the display and return to the idle screen. \
             Use when the user asks to close/dismiss the place.",
            DeviceAction::DismissPlace,
            "the place",
            "Okay, closing that.",
        ),
    ]
}

/// Typed arguments for `places_lookup`.
#[derive(Debug, Deserialize)]
struct PlacesArgs {
    /// The place to look up, e.g. "the Louvre" or "coffee shop downtown". Optional only
    /// so the model can re-call with just a `place_id` after disambiguating.
    #[serde(default)]
    query: Option<String>,
    /// A specific candidate id from a prior ambiguous lookup. When set, the tool skips
    /// the search and shows that exact place.
    #[serde(default)]
    place_id: Option<String>,
}

/// Text-searches for a business/point of interest (biased toward home) and — when the
/// match is unambiguous — pushes its card to the display ([`DeviceAction::ShowPlace`]).
/// When several candidates match it returns a spoken list **without** showing
/// anything, so the model asks which one and re-calls with the chosen `place_id`.
struct PlacesLookup {
    provider: Arc<dyn crate::places::PlacesProvider>,
    home_location: LiveHomeLocation,
}

impl PlacesLookup {
    /// Fetch a place's details, push the card, and return the spoken confirmation.
    async fn show(&self, place_id: &str, sink: &ActionSink) -> Result<String> {
        let report = self.provider.details(place_id).await?;
        let confirmation = crate::places::render_confirmation(&report);
        emit(
            Some(sink),
            DeviceAction::ShowPlace(report),
            "the place could show",
        )?;
        Ok(confirmation)
    }
}

#[async_trait]
impl VoiceTool for PlacesLookup {
    fn name(&self) -> &'static str {
        PLACES_LOOKUP
    }

    fn definition(&self) -> ToolDefinition {
        definition(
            PLACES_LOOKUP,
            "Look up a business or point of interest (its address, opening \
             hours, rating, phone number, website) and show a card for it on \
             the display. Use whenever the user asks where a place is, when \
             it's open, its hours/phone/rating, or to tell them about a \
             specific business, restaurant, shop, landmark, or attraction. \
             Pass the user's phrasing as `query`. If the tool returns several \
             options, ask the user which one they mean, then call again with \
             the same `query` plus the chosen `place_id`. Returns a short \
             spoken line to relay.",
            json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The place to look up, e.g. \"the Louvre\" or \
                                        \"coffee shop downtown\". Include any location the \
                                        user mentioned."
                    },
                    "place_id": {
                        "type": "string",
                        "description": "Only when re-calling after the user picked one of \
                                        the options this tool returned: copy that option's \
                                        EXACT place_id from the list (an opaque id like \
                                        \"ChIJ…\" or \"places/…\"). Never invent or guess \
                                        an id from the name/address."
                    }
                },
                "required": ["query"]
            }),
        )
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(
            "You can pull up information about a place on the display with the \
             `places_lookup` tool. Whenever the user asks where a business or landmark is, \
             when it's open, its hours, phone number, rating, or to tell them about a \
             specific restaurant, shop, cafe, or attraction, you MUST call `places_lookup` \
             with their phrasing as `query` — never guess at an address or hours. If it \
             returns several options, ask the user which one they mean and call again with \
             the same `query` plus the chosen `place_id`. Relay the tool's short spoken \
             confirmation. Use `close_places` when they ask to close/dismiss it.",
        )
    }

    async fn invoke(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String> {
        let args: PlacesArgs = parse_args(args, PLACES_LOOKUP)?;
        let sink = ctx.require_sink("no display is connected to show a place on right now")?;

        // Second call: the model pinned a specific candidate → show it directly.
        if let Some(place_id) = args
            .place_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return self.show(place_id, sink).await;
        }

        let query = args
            .query
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .context("no place to look up was given")?;
        let bias = self.home_location.full();
        let candidates = self.provider.search(&query, bias.as_deref()).await?;
        match candidates.as_slice() {
            [] => Ok(format!("I couldn't find anything matching \"{query}\".")),
            [only] => self.show(&only.place_id, sink).await,
            // Ambiguous: ask which one (no card shown yet). The model re-calls with a
            // `place_id` from this list.
            many => Ok(crate::places::render_candidates(many)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{home, StaticWeather};
    use super::*;

    fn weather_tool() -> Arc<dyn VoiceTool> {
        weather_tools(Arc::new(StaticWeather), home("Austin, TX"), true).remove(0)
    }

    #[tokio::test]
    async fn weather_lookup_defaults_to_hourly_layout_and_passes_daily_through() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        weather_tool()
            .invoke(&json!({}), &ToolCtx { actions: Some(&tx) })
            .await
            .unwrap();
        match rx.try_recv().unwrap() {
            DeviceAction::ShowWeather(report) => {
                // No `layout` argument → the default hourly view, at home.
                assert_eq!(report.layout, "");
                assert_eq!(report.location_label, "Austin, TX");
                // The provider's daily forecast rides through for the 7-day widget.
                assert_eq!(report.daily.len(), 1);
                assert_eq!(report.daily[0].high, 80);
            }
            other => panic!("expected ShowWeather, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn weather_lookup_week_layout_selects_the_7_day_widget() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        weather_tool()
            .invoke(
                &json!({ "layout": "Week" }),
                &ToolCtx { actions: Some(&tx) },
            )
            .await
            .unwrap();
        match rx.try_recv().unwrap() {
            // Case-insensitive: "Week" → the 7-day layout.
            DeviceAction::ShowWeather(report) => assert_eq!(report.layout, "week"),
            other => panic!("expected ShowWeather, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn display_tools_check_for_a_display_before_fetching() {
        let err = weather_tool()
            .invoke(&json!({}), &ToolCtx { actions: None })
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "no display is connected to show the weather on right now"
        );
    }

    #[tokio::test]
    async fn recipe_control_maps_actions_and_rejects_unknown_ones() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ToolCtx { actions: Some(&tx) };
        let out = RecipeControl
            .invoke(&json!({ "action": "show_steps" }), &ctx)
            .await
            .unwrap();
        assert_eq!(out, "Showing the steps.");
        assert_eq!(
            rx.try_recv().unwrap(),
            DeviceAction::RecipeControl(RecipeNav::TabSteps)
        );
        let err = RecipeControl
            .invoke(&json!({ "action": "dance" }), &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "unknown recipe_control action `dance`");
    }

    #[tokio::test]
    async fn recipe_control_go_to_step_needs_a_positive_step() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ToolCtx { actions: Some(&tx) };
        let out = RecipeControl
            .invoke(&json!({ "action": "go_to_step", "step": 4 }), &ctx)
            .await
            .unwrap();
        assert_eq!(out, "Here's step 4.");
        assert_eq!(
            rx.try_recv().unwrap(),
            DeviceAction::RecipeControl(RecipeNav::Step(4))
        );
        for bad in [
            json!({ "action": "go_to_step" }),
            json!({ "action": "go_to_step", "step": 0 }),
        ] {
            assert!(RecipeControl.invoke(&bad, &ctx).await.is_err(), "{bad}");
        }
        assert!(rx.try_recv().is_err(), "a rejected step emits nothing");
    }

    #[tokio::test]
    async fn music_screen_maps_actions_and_rejects_unknown_ones() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ToolCtx { actions: Some(&tx) };
        for (action, nav) in [
            ("show", MusicNav::NowPlaying),
            ("show_queue", MusicNav::Queue),
            ("close", MusicNav::Close),
        ] {
            MusicScreen
                .invoke(&json!({ "action": action }), &ctx)
                .await
                .unwrap();
            assert_eq!(rx.try_recv().unwrap(), DeviceAction::MusicScreen(nav));
        }
        for (action, style) in [
            ("fullscreen", MusicDisplayStyle::Fullscreen),
            ("compact", MusicDisplayStyle::Compact),
        ] {
            MusicScreen
                .invoke(&json!({ "action": action }), &ctx)
                .await
                .unwrap();
            assert_eq!(
                rx.try_recv().unwrap(),
                DeviceAction::MusicDisplayStyle(style)
            );
        }
        let err = MusicScreen
            .invoke(&json!({ "action": "teleport" }), &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "unknown music_screen action `teleport`");
        // With no display attached a valid action reports so (and emits nothing).
        assert!(MusicScreen
            .invoke(&json!({ "action": "show" }), &ToolCtx { actions: None })
            .await
            .is_err());
    }
}
