//! [`LlmBackend`] implemented on top of the **rig-core** agent framework
//! (selected at runtime with `llm.engine="rig"`).
//!
//! This routes the same `respond(turn) -> ReplyStream` seam through rig's unified
//! `CompletionModel` trait instead of the hand-rolled HTTP clients in `ollama.rs`
//! / `anthropic.rs`. One provider is chosen at construction; both funnel into
//! rig's single `StreamingCompletionResponse`.
//!
//! ## Tool calling
//!
//! Unlike the native backends (plain text only), the rig path supports **tool
//! calling**. When tools are configured (currently [`InternetSearch`], enabled via
//! `llm.web_search`), `respond` runs a short negotiation loop: it streams a
//! turn, and if the model asks to call a tool it executes it, appends the result
//! to the conversation, and streams again — up to [`MAX_TOOL_ROUNDS`] rounds —
//! forwarding assistant text deltas as reply tokens throughout. A turn that needs
//! no tool streams straight through in one pass (no regression for the common
//! case).
//!
//! Kept behind a feature flag during the migration so the default build stays lean
//! and the proven native backends remain the fallback until this reaches parity.

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};

use rig_core::client::completion::CompletionClient;
use rig_core::completion::{CompletionModel, ToolDefinition};
use rig_core::message::{AssistantContent, Message, ToolCall};
use rig_core::providers::{anthropic, ollama};
use rig_core::streaming::{StreamedAssistantContent, StreamingCompletionResponse};
use rig_core::tool::PortableTool;

use chrono::Local;

use super::{ActionSink, DeviceAction, LlmBackend, LlmTurn, RecipeNav, ReplyStream};
use crate::cadora::{GroceryCommand, GroceryController};
use crate::calendar::CalendarSource;
use crate::directions::{DirectionsConfig, DirectionsProvider, LiveHomeLocation, TravelMode};
use crate::music::{SearchKind, SpotifyCommand, SpotifyController};
use crate::recipe::{render_confirmation, RecipeProvider};

/// Upper bound on tool-negotiation rounds per turn, so a model that loops on tool
/// calls can never spin forever. Each round is one streamed completion pass.
pub const MAX_TOOL_ROUNDS: usize = 4;

/// Build the system-preamble guidance for exactly the tools present this turn, so
/// the model actually calls them (some models otherwise refuse real-time questions
/// or claim they "can't set timers" instead of using the tool). Naming only the
/// tools that are actually advertised avoids prompting the model to call a tool that
/// isn't there. Returns an empty string when no known tool is present.
fn tool_guidance(tool_defs: &[ToolDefinition]) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let has = |name: &str| tool_defs.iter().any(|d| d.name == name);
    if has(InternetSearch::NAME) {
        parts.push(
            "You have an `internet_search` tool that fetches live information from the web. \
             Whenever the user asks about current events, news, weather, sports, prices, or any \
             real-time or factual detail you are not certain of, you MUST call `internet_search` \
             first and base your answer on its results. Never claim you lack real-time or internet \
             access — use the tool.",
        );
    }
    if has(SET_TIMER) || has(CANCEL_TIMER) {
        parts.push(
            "You can start and cancel countdown timers on the device with the `set_timer` \
             (convert the requested time to whole seconds) and `cancel_timer` tools. When the user \
             asks to set, start, or cancel a timer, call the tool — never say you are unable to set \
             timers. There can be any number of timers running at once.",
        );
    }
    if has(CalendarLookup::NAME) {
        parts.push(
            "You have a `calendar_lookup` tool that reads the user's real connected calendars. \
             Whenever the user asks about their schedule, meetings, appointments, plans, \
             availability, or when they are seeing a particular person, you MUST call \
             `calendar_lookup` and answer from its results. Never guess at their schedule or claim \
             you cannot access their calendar — use the tool.",
        );
    }
    if has(DirectionsLookup::NAME) {
        parts.push(
            "You have a `directions_lookup` tool that returns real distance, travel time, and \
             live traffic between two places. Whenever the user asks how to get somewhere, how \
             far away it is, how long it takes to drive/walk/bike there, or about current \
             traffic, you MUST call `directions_lookup` and answer from its result. If the user \
             names only a destination, omit the origin — it defaults to home. Never guess at \
             distances or travel times.",
        );
    }
    if has(SpotifyControl::NAME) {
        parts.push(
            "You can control the house music with the `spotify_control` tool. Whenever the user \
             asks to play, pause, resume, skip, go back to, queue, or change the volume of music \
             or a song/artist/album/playlist, you MUST call `spotify_control` — never say you \
             cannot play music. For \"play some <artist>\" or a mood/genre use action=play with \
             kind=artist or kind=playlist; for one named song use kind=track.",
        );
    }
    if has(RECIPE_LOOKUP) {
        parts.push(
            "You can pull up cooking recipes on the display with the `recipe_lookup` tool. \
             Whenever the user asks for a recipe, how to make or cook a dish, or to show a \
             recipe for something, you MUST call `recipe_lookup` with the dish name — never \
             recite a full recipe as prose. When a recipe is already on screen (the turn \
             context will tell you, including which tab and scroll position), use \
             `recipe_control` to navigate it — switching to the Overview / Ingredients / \
             Steps tab or scrolling the current tab — whenever the user says things like \
             \"show the ingredients\", \"go to the steps\", \"next\", \"scroll down\", \
             \"scroll back up\", or \"start over\". Use `close_recipe` when they say they're \
             done cooking or ask to close the recipe. Relay each tool's short spoken \
             confirmation.",
        );
    }
    if has(ShoppingListControl::NAME) {
        parts.push(
            "You can add items to the household's shared shopping list with the \
             `shopping_list_add` tool. Whenever the user asks to add, put, or write \
             something on the shopping/grocery/store list (e.g. \"add milk\", \"put eggs \
             on the shopping list\", \"we need paper towels\"), you MUST call \
             `shopping_list_add`, one call per distinct item, with the item name and a \
             quantity when they say one. Relay the tool's spoken confirmation.",
        );
    }
    parts.join(" ")
}

/// The system preamble for a turn: the base prompt, plus per-tool usage guidance
/// when any known tool is advertised.
fn build_preamble(system_prompt: &str, tool_defs: &[ToolDefinition]) -> String {
    let guidance = tool_guidance(tool_defs);
    if guidance.is_empty() {
        system_prompt.to_string()
    } else {
        format!("{system_prompt}\n\n{guidance}")
    }
}

/// Default number of search results to fold into a tool result.
pub const DEFAULT_SEARCH_RESULTS: u8 = 3;

// ===========================================================================
// Web search tool
// ===========================================================================

/// Typed arguments for [`InternetSearch`] (the plan's `SearchArgs`).
#[derive(Debug, Deserialize)]
pub struct SearchArgs {
    /// The search query.
    pub query: String,
    /// Maximum number of results to summarize (defaults to
    /// [`DEFAULT_SEARCH_RESULTS`], clamped to 1..=10).
    #[serde(default)]
    pub max_results: Option<u8>,
}

/// A concrete, `std::error::Error` failure for the search tool (rig requires the
/// tool's error type to implement `std::error::Error`, which `anyhow::Error` does
/// not).
#[derive(Debug)]
pub struct SearchError(pub String);

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "web search failed: {}", self.0)
    }
}

impl std::error::Error for SearchError {}

/// Pluggable web-search implementation, injected so the tool is testable offline.
#[async_trait]
pub trait SearchProvider: Send + Sync {
    /// Run `query` and return a compact, speakable text summary of up to
    /// `max_results` findings.
    async fn search(&self, query: &str, max_results: u8) -> Result<String>;
}

/// Keyless web search via DuckDuckGo's Instant Answer API (`no auth`). Folds the
/// abstract and related-topic snippets into a short summary.
pub struct DuckDuckGoSearch {
    client: reqwest::Client,
    base_url: String,
}

impl Default for DuckDuckGoSearch {
    fn default() -> Self {
        Self::with_base_url("https://api.duckduckgo.com")
    }
}

impl DuckDuckGoSearch {
    /// Point the search at a specific Instant-Answer endpoint (overridable for
    /// tests).
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
        }
    }
}

#[async_trait]
impl SearchProvider for DuckDuckGoSearch {
    async fn search(&self, query: &str, max_results: u8) -> Result<String> {
        let value: Value = self
            .client
            .get(&self.base_url)
            .query(&[
                ("q", query),
                ("format", "json"),
                ("no_html", "1"),
                ("no_redirect", "1"),
                ("t", "anamanti-core"),
            ])
            .send()
            .await
            .context("GET DuckDuckGo Instant Answer")?
            .error_for_status()
            .context("DuckDuckGo returned an error status")?
            .json()
            .await
            .context("parsing DuckDuckGo JSON")?;

        let mut lines: Vec<String> = Vec::new();
        if let Some(abstract_text) = value
            .get("AbstractText")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            lines.push(abstract_text.to_string());
        }
        if let Some(topics) = value.get("RelatedTopics").and_then(Value::as_array) {
            for topic in topics {
                if lines.len() >= max_results as usize {
                    break;
                }
                if let Some(text) = topic
                    .get("Text")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    lines.push(text.to_string());
                }
            }
        }
        if lines.is_empty() {
            lines.push(format!("No web results found for \"{query}\"."));
        }
        Ok(lines.join("\n"))
    }
}

/// Real general web search via the **Tavily** API (`https://api.tavily.com`), an
/// LLM-oriented search service. Needs `TAVILY_API_KEY`. Unlike DuckDuckGo's
/// Instant Answer API, this returns actual web results for news / weather /
/// current-event queries.
pub struct TavilySearch {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl TavilySearch {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url("https://api.tavily.com", api_key)
    }

    pub fn with_base_url(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
        }
    }
}

#[async_trait]
impl SearchProvider for TavilySearch {
    async fn search(&self, query: &str, max_results: u8) -> Result<String> {
        let body = json!({
            "api_key": self.api_key,
            "query": query,
            "max_results": max_results,
            "search_depth": "basic",
            "include_answer": true,
        });
        let value: Value = self
            .client
            .post(format!("{}/search", self.base_url))
            .json(&body)
            .send()
            .await
            .context("POST Tavily /search")?
            .error_for_status()
            .context("Tavily returned an error status")?
            .json()
            .await
            .context("parsing Tavily JSON")?;

        // Prefer Tavily's synthesized answer when present; otherwise fold result
        // snippets into a compact summary.
        if let Some(answer) = value
            .get("answer")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return Ok(answer.to_string());
        }
        let mut lines: Vec<String> = Vec::new();
        if let Some(results) = value.get("results").and_then(Value::as_array) {
            for result in results.iter().take(max_results as usize) {
                let title = result.get("title").and_then(Value::as_str).unwrap_or("");
                let content = result.get("content").and_then(Value::as_str).unwrap_or("");
                if !content.is_empty() {
                    lines.push(if title.is_empty() {
                        content.to_string()
                    } else {
                        format!("{title}: {content}")
                    });
                }
            }
        }
        if lines.is_empty() {
            anyhow::bail!("Tavily returned no results for \"{query}\"");
        }
        Ok(lines.join("\n"))
    }
}

/// The web-search tool the model can call. Genuinely implements rig's
/// [`PortableTool`]; the runtime path uses [`InternetSearch::definition`] to
/// advertise it and [`InternetSearch::invoke`] to execute it from raw JSON args.
pub struct InternetSearch {
    provider: Arc<dyn SearchProvider>,
}

impl InternetSearch {
    pub fn new(provider: Arc<dyn SearchProvider>) -> Self {
        Self { provider }
    }

    /// The rig tool definition to advertise on a completion request.
    pub fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: self.description(),
            parameters: self.parameters(),
        }
    }

    /// Execute the tool from the model's raw JSON arguments (the runtime path),
    /// routed through the rig [`PortableTool::call`] implementation.
    pub async fn invoke(&self, arguments: &Value) -> Result<String> {
        let args: SearchArgs = serde_json::from_value(arguments.clone())
            .context("parsing internet_search arguments")?;
        self.call(args)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))
    }
}

impl PortableTool for InternetSearch {
    const NAME: &'static str = "internet_search";
    type Args = SearchArgs;
    type Output = String;
    type Error = SearchError;

    fn description(&self) -> String {
        "Search the public web for current, real-time, or factual information the \
         assistant does not already know (news, weather, prices, events, look-ups). \
         Returns a short text summary."
            .to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query, phrased as you would type it into a search engine."
                },
                "max_results": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 10,
                    "description": "How many results to summarize (default 3)."
                }
            },
            "required": ["query"]
        })
    }

    async fn call(&self, arguments: Self::Args) -> Result<Self::Output, Self::Error> {
        let max = arguments
            .max_results
            .unwrap_or(DEFAULT_SEARCH_RESULTS)
            .clamp(1, 10);
        self.provider
            .search(&arguments.query, max)
            .await
            .map_err(|e| SearchError(format!("{e:#}")))
    }
}

// ===========================================================================
// Timer tools (device actions)
// ===========================================================================

/// Tool name: start a countdown timer on the device.
pub const SET_TIMER: &str = "set_timer";
/// Tool name: cancel one or all countdown timers on the device.
pub const CANCEL_TIMER: &str = "cancel_timer";

/// Hard cap on a timer duration (24h) so a mis-parsed request can't spin an
/// effectively-infinite timer on the device.
const MAX_TIMER_SECS: u64 = 24 * 60 * 60;

/// Typed args for [`SET_TIMER`].
#[derive(Debug, Deserialize)]
struct SetTimerArgs {
    /// Timer length in whole seconds (the model converts "5 minutes" → 300).
    duration_seconds: u64,
    /// Optional spoken label ("pasta", "laundry").
    #[serde(default)]
    label: Option<String>,
}

/// Typed args for [`CANCEL_TIMER`].
#[derive(Debug, Deserialize, Default)]
struct CancelTimerArgs {
    /// Which timer to cancel by label; absent/empty cancels *all* timers.
    #[serde(default)]
    label: Option<String>,
}

fn set_timer_definition() -> ToolDefinition {
    ToolDefinition {
        name: SET_TIMER.to_string(),
        description: "Start a countdown timer on the device. Use for any \"set a timer for …\" / \
                      \"remind me in …\" request. Convert the requested time to whole seconds."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "duration_seconds": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Timer length in whole seconds (e.g. 5 minutes → 300)."
                },
                "label": {
                    "type": "string",
                    "description": "Optional short name for the timer, e.g. \"pasta\"."
                }
            },
            "required": ["duration_seconds"]
        }),
    }
}

fn cancel_timer_definition() -> ToolDefinition {
    ToolDefinition {
        name: CANCEL_TIMER.to_string(),
        description: "Cancel a running countdown timer. Pass the timer's label to cancel just \
                      that one, or omit it to cancel all timers."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "label": {
                    "type": "string",
                    "description": "Label of the timer to cancel; omit to cancel every timer."
                }
            }
        }),
    }
}

/// Render a whole-second duration as a short, speakable phrase ("5 minutes",
/// "1 hour 30 minutes", "45 seconds").
fn humanize_secs(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let mut parts = Vec::new();
    let unit = |n: u64, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    if h > 0 {
        parts.push(unit(h, "hour"));
    }
    if m > 0 {
        parts.push(unit(m, "minute"));
    }
    if s > 0 || parts.is_empty() {
        parts.push(unit(s, "second"));
    }
    parts.join(" ")
}

/// Execute `set_timer`: emit a [`DeviceAction::StartTimer`] on the per-turn sink and
/// return a speakable confirmation for the model to relay. Errors (no device
/// attached / closed channel) surface as the tool result so the model apologizes.
fn set_timer_invoke(arguments: &Value, actions: Option<&ActionSink>) -> Result<String> {
    let args: SetTimerArgs =
        serde_json::from_value(arguments.clone()).context("parsing set_timer arguments")?;
    let sink = actions.context("no device is connected to run a timer on right now")?;
    let secs = args.duration_seconds.clamp(1, MAX_TIMER_SECS);
    let label = args.label.filter(|s| !s.trim().is_empty());
    sink.send(DeviceAction::StartTimer {
        label: label.clone(),
        duration_secs: secs,
    })
    .map_err(|_| anyhow::anyhow!("the device disconnected before the timer could start"))?;
    Ok(match &label {
        Some(l) => format!("Started a {} timer for {l}.", humanize_secs(secs)),
        None => format!("Started a {} timer.", humanize_secs(secs)),
    })
}

/// Execute `cancel_timer`: emit a [`DeviceAction::CancelTimer`] on the per-turn sink.
fn cancel_timer_invoke(arguments: &Value, actions: Option<&ActionSink>) -> Result<String> {
    let args: CancelTimerArgs = if arguments.is_null() {
        CancelTimerArgs::default()
    } else {
        serde_json::from_value(arguments.clone()).context("parsing cancel_timer arguments")?
    };
    let sink = actions.context("no device is connected to manage timers on right now")?;
    let label = args.label.filter(|s| !s.trim().is_empty());
    sink.send(DeviceAction::CancelTimer {
        label: label.clone(),
    })
    .map_err(|_| anyhow::anyhow!("the device disconnected before the timer could be cancelled"))?;
    Ok(match &label {
        Some(l) => format!("Cancelled the {l} timer."),
        None => "Cancelled all timers.".to_string(),
    })
}

// ===========================================================================
// Calendar tool (read-only web iCalendar subscriptions)
// ===========================================================================

/// Default number of events folded into a `calendar_lookup` result.
pub const DEFAULT_CALENDAR_RESULTS: u8 = 10;

/// Typed arguments for [`CalendarLookup`].
#[derive(Debug, Deserialize)]
pub struct CalendarArgs {
    /// Which window to look at: `today`, `tomorrow`, `this_week`, `next_7_days`,
    /// `next_14_days`, `next_30_days`/`next_month`, `this_month`, `on:YYYY-MM-DD`, or
    /// `range:YYYY-MM-DD..YYYY-MM-DD`. Defaults to `next_7_days`.
    #[serde(default)]
    pub when: Option<String>,
    /// Optional person name to filter by (fuzzy match on attendees/organizer/title).
    #[serde(default)]
    pub person: Option<String>,
    /// Optional free-text filter over event title/description/location.
    #[serde(default)]
    pub query: Option<String>,
    /// Maximum events to return (defaults to [`DEFAULT_CALENDAR_RESULTS`], clamped
    /// to 1..=50).
    #[serde(default)]
    pub max_results: Option<u8>,
}

/// A concrete, `std::error::Error` failure for the calendar tool (rig requires the
/// tool's error type to implement `std::error::Error`, which `anyhow::Error` does
/// not).
#[derive(Debug)]
pub struct CalendarError(pub String);

impl std::fmt::Display for CalendarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "calendar lookup failed: {}", self.0)
    }
}

impl std::error::Error for CalendarError {}

/// Reads the user's connected web calendars. Injected [`CalendarSource`] so the tool
/// is testable offline, mirroring how [`InternetSearch`] holds a `SearchProvider`.
pub struct CalendarLookup {
    source: Arc<dyn CalendarSource>,
}

impl CalendarLookup {
    pub fn new(source: Arc<dyn CalendarSource>) -> Self {
        Self { source }
    }

    /// The rig tool definition to advertise on a completion request.
    pub fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: self.description(),
            parameters: self.parameters(),
        }
    }

    /// Execute the tool from the model's raw JSON arguments (the runtime path).
    pub async fn invoke(&self, arguments: &Value) -> Result<String> {
        let args: CalendarArgs = if arguments.is_null() {
            CalendarArgs {
                when: None,
                person: None,
                query: None,
                max_results: None,
            }
        } else {
            serde_json::from_value(arguments.clone())
                .context("parsing calendar_lookup arguments")?
        };
        self.call(args)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))
    }
}

impl PortableTool for CalendarLookup {
    const NAME: &'static str = "calendar_lookup";
    type Args = CalendarArgs;
    type Output = String;
    type Error = CalendarError;

    fn description(&self) -> String {
        let names = self.source.calendar_names().join(", ");
        format!(
            "Look up events from the user's connected calendars ({names}). Use this for \
             ANY question about the user's schedule, meetings, appointments, plans, or \
             availability — what is coming up, what is on a particular day, or when they \
             are seeing a specific person. Returns a short text list of matching events."
        )
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "when": {
                    "type": "string",
                    "description": "Time window to search. One of: 'today', 'tomorrow', \
                        'this_week', 'next_7_days', 'next_14_days', 'next_30_days' (use \
                        this for 'next month'/'this month'/'the coming weeks'), \
                        'on:YYYY-MM-DD' (a specific day), or \
                        'range:YYYY-MM-DD..YYYY-MM-DD'. Prefer a wider window when the \
                        user is vague. Defaults to 'next_7_days'."
                },
                "person": {
                    "type": "string",
                    "description": "Optional. Only return events involving this person \
                        (matched against attendees, organizer, and the event title)."
                },
                "query": {
                    "type": "string",
                    "description": "Optional free-text filter over the event title, \
                        description, and location."
                },
                "max_results": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 50,
                    "description": "How many events to list (default 10)."
                }
            }
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let (window, label) = crate::calendar::resolve_window(args.when.as_deref(), Local::now())
            .map_err(CalendarError)?;
        let mut events = self
            .source
            .events(window)
            .await
            .map_err(|e| CalendarError(format!("{e:#}")))?;

        if let Some(person) = args.person.as_deref().filter(|s| !s.trim().is_empty()) {
            events.retain(|e| crate::calendar::person_matches(e, person));
        }
        if let Some(query) = args.query.as_deref().filter(|s| !s.trim().is_empty()) {
            events.retain(|e| crate::calendar::query_matches(e, query));
        }

        let max = args
            .max_results
            .unwrap_or(DEFAULT_CALENDAR_RESULTS)
            .clamp(1, 50) as usize;
        Ok(crate::calendar::render_events(
            &events,
            &label,
            max,
            args.person.as_deref(),
            args.query.as_deref(),
        ))
    }
}

// ===========================================================================
// Directions tool (distance / travel time / live traffic)
// ===========================================================================

/// Typed arguments for [`DirectionsLookup`].
#[derive(Debug, Deserialize)]
pub struct DirectionsArgs {
    /// Where to start from. Omit to use the device's home location.
    #[serde(default)]
    pub origin: Option<String>,
    /// Where to go (required).
    pub destination: String,
    /// How to travel: `driving` (default, live-traffic), `walking`, or `cycling`.
    #[serde(default)]
    pub mode: Option<String>,
}

/// A concrete, `std::error::Error` failure for the directions tool (rig requires the
/// tool's error type to implement `std::error::Error`, which `anyhow::Error` does
/// not).
#[derive(Debug)]
pub struct DirectionsError(pub String);

impl std::fmt::Display for DirectionsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "directions lookup failed: {}", self.0)
    }
}

impl std::error::Error for DirectionsError {}

/// Looks up a route via an injected [`DirectionsProvider`] so the tool is testable
/// offline, mirroring [`CalendarLookup`]. Holds the live home location (read as the
/// default origin when the user names only a destination) and the preferred distance
/// units. Because the origin is read from [`LiveHomeLocation`] at call time, editing
/// the household location takes effect without rebuilding the backend.
pub struct DirectionsLookup {
    provider: Arc<dyn DirectionsProvider>,
    home_location: LiveHomeLocation,
    imperial: bool,
}

impl DirectionsLookup {
    pub fn new(
        provider: Arc<dyn DirectionsProvider>,
        home_location: LiveHomeLocation,
        imperial: bool,
    ) -> Self {
        Self {
            provider,
            home_location,
            imperial,
        }
    }

    /// The rig tool definition to advertise on a completion request.
    pub fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: self.description(),
            parameters: self.parameters(),
        }
    }

    /// Execute the tool from the model's raw JSON arguments (the runtime path).
    pub async fn invoke(&self, arguments: &Value) -> Result<String> {
        let args: DirectionsArgs = serde_json::from_value(arguments.clone())
            .context("parsing directions_lookup arguments")?;
        self.call(args)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))
    }
}

impl PortableTool for DirectionsLookup {
    const NAME: &'static str = "directions_lookup";
    type Args = DirectionsArgs;
    type Output = String;
    type Error = DirectionsError;

    fn description(&self) -> String {
        let home = self
            .home_location
            .get()
            .map(|h| format!(" The origin defaults to the device's home ({h}) when omitted."))
            .unwrap_or_default();
        format!(
            "Get the real driving distance, travel time, and live traffic between two places. \
             Use this for ANY question about how to get somewhere, how far away it is, how long \
             it takes to drive/walk/bike there, or current traffic conditions.{home} Returns a \
             short spoken summary."
        )
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "origin": {
                    "type": "string",
                    "description": "Where the trip starts (an address, place, or city). Omit to \
                        start from the device's home location."
                },
                "destination": {
                    "type": "string",
                    "description": "Where the trip ends (an address, place, or city)."
                },
                "mode": {
                    "type": "string",
                    "enum": ["driving", "walking", "cycling"],
                    "description": "Travel mode. Defaults to driving (with live traffic)."
                }
            },
            "required": ["destination"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let origin = args
            .origin
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| self.home_location.get())
            .ok_or_else(|| {
                DirectionsError(
                    "no starting point given and no home location is configured".to_string(),
                )
            })?;
        let destination = args.destination.trim();
        if destination.is_empty() {
            return Err(DirectionsError("no destination given".to_string()));
        }
        let mode = TravelMode::from_arg(args.mode.as_deref());
        let directions = self
            .provider
            .directions(&origin, destination, mode)
            .await
            .map_err(|e| DirectionsError(format!("{e:#}")))?;
        Ok(crate::directions::render_directions(
            &directions,
            self.imperial,
        ))
    }
}

// ===========================================================================
// Spotify tool (house-wide music control via the Web API)
// ===========================================================================

/// Typed arguments for [`SpotifyControl`].
#[derive(Debug, Deserialize)]
pub struct SpotifyArgs {
    /// What to do: `play`, `pause`, `resume`, `next`, `previous`, `queue`, or
    /// `volume`.
    pub action: String,
    /// What to play/queue (song, artist, album, or playlist name). Only used by
    /// `play`/`queue`; omit for `play` to resume the current track.
    #[serde(default)]
    pub query: Option<String>,
    /// For `play`: what the query names — `track` (a specific song, default),
    /// `artist`, `album`, or `playlist` (a vibe / "some X" plays a whole context).
    #[serde(default)]
    pub kind: Option<String>,
    /// For `volume`: the target level, 0–100.
    #[serde(default)]
    pub volume_percent: Option<u8>,
}

/// A concrete, `std::error::Error` failure for the Spotify tool (rig requires the
/// tool's error type to implement `std::error::Error`, which `anyhow::Error` does
/// not).
#[derive(Debug)]
pub struct SpotifyToolError(pub String);

impl std::fmt::Display for SpotifyToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "spotify control failed: {}", self.0)
    }
}

impl std::error::Error for SpotifyToolError {}

/// Controls house-wide Spotify playback through an injected [`SpotifyController`]
/// so the tool is testable offline, mirroring [`InternetSearch`]. The controller
/// targets the librespot Connect device; audio never flows through here.
pub struct SpotifyControl {
    controller: Arc<dyn SpotifyController>,
}

impl SpotifyControl {
    pub fn new(controller: Arc<dyn SpotifyController>) -> Self {
        Self { controller }
    }

    /// The rig tool definition to advertise on a completion request.
    pub fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: self.description(),
            parameters: self.parameters(),
        }
    }

    /// Map the typed args to a [`SpotifyCommand`]. Errors on an unknown action or a
    /// missing required field (returned to the model as the tool result).
    fn to_command(args: SpotifyArgs) -> Result<SpotifyCommand, SpotifyToolError> {
        let query = args
            .query
            .map(|q| q.trim().to_string())
            .filter(|q| !q.is_empty());
        match args.action.trim().to_lowercase().as_str() {
            "play" | "start" => Ok(SpotifyCommand::Play {
                query,
                kind: SearchKind::from_arg(args.kind.as_deref()),
            }),
            "queue" | "add" => {
                let q = query.ok_or_else(|| {
                    SpotifyToolError("queue needs the name of a song to add".to_string())
                })?;
                Ok(SpotifyCommand::Queue { query: q })
            }
            "pause" | "stop" => Ok(SpotifyCommand::Pause),
            "resume" | "unpause" => Ok(SpotifyCommand::Resume),
            "next" | "skip" | "forward" => Ok(SpotifyCommand::Next),
            "previous" | "prev" | "back" => Ok(SpotifyCommand::Previous),
            "volume" | "set_volume" => {
                let percent = args.volume_percent.ok_or_else(|| {
                    SpotifyToolError("volume needs a level from 0 to 100".to_string())
                })?;
                Ok(SpotifyCommand::SetVolume { percent })
            }
            other => Err(SpotifyToolError(format!("unknown action `{other}`"))),
        }
    }

    /// Execute the tool from the model's raw JSON arguments (the runtime path).
    pub async fn invoke(&self, arguments: &Value) -> Result<String> {
        let args: SpotifyArgs = serde_json::from_value(arguments.clone())
            .context("parsing spotify_control arguments")?;
        self.call(args)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))
    }
}

impl PortableTool for SpotifyControl {
    const NAME: &'static str = "spotify_control";
    type Args = SpotifyArgs;
    type Output = String;
    type Error = SpotifyToolError;

    fn description(&self) -> String {
        "Control house-wide Spotify music playback on the speakers. Use this for ANY request to \
         play, pause, resume, skip, go back, queue a song, or change the music volume. For \
         \"play some Radiohead\" or a genre/mood, use action=play with kind=artist or \
         kind=playlist; for a specific song use kind=track. Returns a short spoken confirmation."
            .to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["play", "pause", "resume", "next", "previous", "queue", "volume"],
                    "description": "The playback operation to perform."
                },
                "query": {
                    "type": "string",
                    "description": "What to play or queue (song, artist, album, or playlist). \
                        Omit with action=play to resume the current track."
                },
                "kind": {
                    "type": "string",
                    "enum": ["track", "artist", "album", "playlist"],
                    "description": "For action=play: what 'query' names. Use 'track' for one \
                        specific song (default), or 'artist'/'playlist' to play a whole set."
                },
                "volume_percent": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 100,
                    "description": "For action=volume: the target level, 0–100."
                }
            },
            "required": ["action"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let cmd = Self::to_command(args)?;
        self.controller
            .command(cmd)
            .await
            .map_err(|e| SpotifyToolError(format!("{e:#}")))
    }
}

// ===========================================================================
// Shopping-list tool (Cadora shared household via the voice API)
// ===========================================================================

/// Typed arguments for [`ShoppingListControl`].
#[derive(Debug, Deserialize)]
pub struct GroceryArgs {
    /// The item to add (e.g. "milk", "paper towels"). One item per call.
    pub item: String,
    /// How many, when the user says a number (1–99). Omit for a plain add.
    #[serde(default)]
    pub quantity: Option<u32>,
}

/// A concrete, `std::error::Error` failure for the shopping tool (rig requires the
/// tool's error type to implement `std::error::Error`, which `anyhow::Error` does not).
#[derive(Debug)]
pub struct GroceryToolError(pub String);

impl std::fmt::Display for GroceryToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "shopping list add failed: {}", self.0)
    }
}

impl std::error::Error for GroceryToolError {}

/// Adds items to the household's shared shopping list through an injected
/// [`GroceryController`] (Cadora's voice API), so the tool is testable offline —
/// mirroring [`SpotifyControl`]. The list lives on the Cadora server; this only
/// issues the add and relays the server's spoken confirmation.
pub struct ShoppingListControl {
    controller: Arc<dyn GroceryController>,
}

impl ShoppingListControl {
    pub fn new(controller: Arc<dyn GroceryController>) -> Self {
        Self { controller }
    }

    /// The rig tool definition to advertise on a completion request.
    pub fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: self.description(),
            parameters: self.parameters(),
        }
    }

    /// Execute the tool from the model's raw JSON arguments (the runtime path).
    pub async fn invoke(&self, arguments: &Value) -> Result<String> {
        let args: GroceryArgs = serde_json::from_value(arguments.clone())
            .context("parsing shopping_list_add arguments")?;
        self.call(args)
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))
    }
}

impl PortableTool for ShoppingListControl {
    const NAME: &'static str = "shopping_list_add";
    type Args = GroceryArgs;
    type Output = String;
    type Error = GroceryToolError;

    fn description(&self) -> String {
        "Add an item to the household's shared shopping/grocery list. Use this for ANY \
         request to add, put, or write something on the shopping list, grocery list, or \
         store list (\"add milk\", \"put eggs on the list\", \"we need paper towels\"). \
         Call it once per distinct item; include a quantity only when the user says one. \
         Returns a short spoken confirmation to relay."
            .to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "item": {
                    "type": "string",
                    "description": "The single item to add, e.g. \"milk\" or \"paper towels\"."
                },
                "quantity": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 99,
                    "description": "How many, when the user states a number. Omit otherwise."
                }
            },
            "required": ["item"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        self.controller
            .command(GroceryCommand::AddItem {
                item: args.item,
                quantity: args.quantity,
            })
            .await
            .map_err(|e| GroceryToolError(format!("{e:#}")))
    }
}

// ===========================================================================
// Recipe tool (fetch + parse a recipe, show it on the display)
// ===========================================================================

/// Tool name for showing a recipe on the display.
pub const RECIPE_LOOKUP: &str = "recipe_lookup";
/// Tool name for dismissing the recipe screen.
pub const CLOSE_RECIPE: &str = "close_recipe";
/// Tool name for navigating the already-open recipe screen (switch tab / scroll).
pub const RECIPE_CONTROL: &str = "recipe_control";

/// Typed arguments for [`RecipeLookup`].
#[derive(Debug, Deserialize)]
struct RecipeArgs {
    /// The dish to find a recipe for (e.g. "carbonara"). Required.
    dish: String,
    /// A specific recipe URL to use instead of searching (optional; the model
    /// usually only has a dish name).
    #[serde(default)]
    url: Option<String>,
}

/// The recipe tool: finds a source page for a dish, parses it, pushes the structured
/// recipe to the display (a [`DeviceAction::ShowRecipe`]), and returns a short spoken
/// confirmation. Like the timer tools it emits a device action, so it takes the
/// per-turn [`ActionSink`] directly (rather than implementing rig's `PortableTool`,
/// whose `call` has no action channel). The provider is injected so it's testable
/// offline (see `crate::recipe`).
pub struct RecipeLookup {
    provider: Arc<dyn RecipeProvider>,
}

impl RecipeLookup {
    pub fn new(provider: Arc<dyn RecipeProvider>) -> Self {
        Self { provider }
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: RECIPE_LOOKUP.to_string(),
            description: "Find a cooking recipe for a dish and show it on the display's \
                          recipe screen (Overview / Ingredients / Steps tabs). Use whenever \
                          the user asks for a recipe, how to make/cook a dish, or to pull up \
                          a recipe. Returns a short spoken confirmation to relay."
                .to_string(),
            parameters: json!({
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
        }
    }

    /// Execute the tool: fetch + parse the recipe, push it to the device, and return
    /// a speakable confirmation. Fetch/parse failures and a missing device surface as
    /// the tool result so the model apologizes aloud.
    async fn invoke(&self, arguments: &Value, actions: Option<&ActionSink>) -> Result<String> {
        let args: RecipeArgs =
            serde_json::from_value(arguments.clone()).context("parsing recipe_lookup arguments")?;
        let sink = actions.context("no display is connected to show a recipe on right now")?;
        let recipe = self
            .provider
            .lookup(&args.dish, args.url.as_deref())
            .await
            .map_err(|e| anyhow::anyhow!("{e:#}"))?;
        let confirmation = render_confirmation(&recipe);
        sink.send(DeviceAction::ShowRecipe(recipe)).map_err(|_| {
            anyhow::anyhow!("the display disconnected before the recipe could show")
        })?;
        Ok(confirmation)
    }
}

fn close_recipe_definition() -> ToolDefinition {
    ToolDefinition {
        name: CLOSE_RECIPE.to_string(),
        description: "Close the recipe screen on the display and return to the idle screen. \
                      Use when the user says they're done cooking or asks to close/dismiss the \
                      recipe."
            .to_string(),
        parameters: json!({ "type": "object", "properties": {} }),
    }
}

/// Execute `close_recipe`: emit a [`DeviceAction::DismissRecipe`] on the per-turn sink.
fn close_recipe_invoke(actions: Option<&ActionSink>) -> Result<String> {
    let sink = actions.context("no display is connected right now")?;
    sink.send(DeviceAction::DismissRecipe)
        .map_err(|_| anyhow::anyhow!("the display disconnected before the recipe could close"))?;
    Ok("Okay, closing the recipe.".to_string())
}

/// Typed arguments for [`recipe_control_invoke`].
#[derive(Debug, Deserialize)]
struct RecipeControlArgs {
    /// One of the fixed navigation actions (see [`recipe_control_definition`]).
    action: String,
}

fn recipe_control_definition() -> ToolDefinition {
    ToolDefinition {
        name: RECIPE_CONTROL.to_string(),
        description: "Navigate the recipe screen that is already open on the display: switch \
                      which tab is showing, or scroll the current tab. Use ONLY when a recipe \
                      is currently on screen (the turn context says so). To close it, use \
                      close_recipe instead."
            .to_string(),
        parameters: json!({
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
                        "scroll_bottom"
                    ],
                    "description": "The navigation action: switch to the Overview / Ingredients / \
                                    Steps tab, or scroll the current tab up/down a page or to the \
                                    top/bottom."
                }
            },
            "required": ["action"]
        }),
    }
}

/// Execute `recipe_control`: map the action to a [`DeviceAction::RecipeControl`] and emit
/// it on the per-turn sink.
fn recipe_control_invoke(arguments: &Value, actions: Option<&ActionSink>) -> Result<String> {
    let args: RecipeControlArgs =
        serde_json::from_value(arguments.clone()).context("parsing recipe_control arguments")?;
    let (nav, confirmation) = match args.action.as_str() {
        "show_overview" => (RecipeNav::TabOverview, "Showing the overview."),
        "show_ingredients" => (RecipeNav::TabIngredients, "Showing the ingredients."),
        "show_steps" => (RecipeNav::TabSteps, "Showing the steps."),
        "scroll_up" => (RecipeNav::ScrollUp, "Scrolling up."),
        "scroll_down" => (RecipeNav::ScrollDown, "Scrolling down."),
        "scroll_top" => (RecipeNav::ScrollTop, "Back to the top."),
        "scroll_bottom" => (RecipeNav::ScrollBottom, "Jumping to the bottom."),
        other => anyhow::bail!("unknown recipe_control action `{other}`"),
    };
    let sink = actions.context("no display is connected right now")?;
    sink.send(DeviceAction::RecipeControl(nav))
        .map_err(|_| anyhow::anyhow!("the display disconnected before the recipe could update"))?;
    Ok(confirmation.to_string())
}

// ===========================================================================
// Tool set
// ===========================================================================

/// The set of tools available to a turn: their advertised definitions plus a
/// dispatcher that executes a call by name. Cheaply shared behind an `Arc`.
///
/// The **timer** tools (`set_timer` / `cancel_timer`) are always present — they are
/// stateless device actions needing no config. The **web-search** tool is included
/// only when a provider is configured (`web_search` on); the **calendar** tool only
/// when calendar subscriptions are configured (`calendar.subscriptions`); the
/// **directions** tool only when a routing provider is configured (`MAPBOX_TOKEN`).
pub struct Tools {
    definitions: Vec<ToolDefinition>,
    search: Option<Arc<InternetSearch>>,
    calendar: Option<Arc<CalendarLookup>>,
    directions: Option<Arc<DirectionsLookup>>,
    spotify: Option<Arc<SpotifyControl>>,
    grocery: Option<Arc<ShoppingListControl>>,
    recipe: Option<Arc<RecipeLookup>>,
}

impl Tools {
    /// Build the tool set. Timer tools are always advertised; the web-search tool is
    /// added when `search` is `Some`, the calendar tool when `calendar` is `Some`, the
    /// directions tool when `directions` is `Some`, and the Spotify tool when
    /// `spotify` is `Some`.
    pub fn new(
        search: Option<Arc<dyn SearchProvider>>,
        calendar: Option<Arc<dyn CalendarSource>>,
        directions: Option<DirectionsConfig>,
        spotify: Option<Arc<dyn SpotifyController>>,
        grocery: Option<Arc<dyn GroceryController>>,
        recipe: Option<Arc<dyn RecipeProvider>>,
    ) -> Self {
        let mut definitions = vec![set_timer_definition(), cancel_timer_definition()];
        let search = search.map(|provider| Arc::new(InternetSearch::new(provider)));
        if let Some(s) = &search {
            definitions.push(s.definition());
        }
        let calendar = calendar.map(|source| Arc::new(CalendarLookup::new(source)));
        if let Some(c) = &calendar {
            definitions.push(c.definition());
        }
        let directions = directions.map(|cfg| {
            Arc::new(DirectionsLookup::new(
                cfg.provider,
                cfg.home_location,
                cfg.imperial,
            ))
        });
        if let Some(d) = &directions {
            definitions.push(d.definition());
        }
        let spotify = spotify.map(|c| Arc::new(SpotifyControl::new(c)));
        if let Some(s) = &spotify {
            definitions.push(s.definition());
        }
        let grocery = grocery.map(|c| Arc::new(ShoppingListControl::new(c)));
        if let Some(g) = &grocery {
            definitions.push(g.definition());
        }
        let recipe = recipe.map(|p| Arc::new(RecipeLookup::new(p)));
        if let Some(r) = &recipe {
            definitions.push(r.definition());
            definitions.push(close_recipe_definition());
            definitions.push(recipe_control_definition());
        }
        Self {
            definitions,
            search,
            calendar,
            directions,
            spotify,
            grocery,
            recipe,
        }
    }

    /// Execute a model-requested tool call by name, returning its text result.
    /// `actions` is the per-turn device-action sink (for the timer tools); `None`
    /// makes the timer tools report that no device is available.
    async fn dispatch(
        &self,
        name: &str,
        arguments: &Value,
        actions: Option<&ActionSink>,
    ) -> Result<String> {
        match name {
            SET_TIMER => set_timer_invoke(arguments, actions),
            CANCEL_TIMER => cancel_timer_invoke(arguments, actions),
            InternetSearch::NAME => match &self.search {
                Some(search) => search.invoke(arguments).await,
                None => anyhow::bail!("web search is not enabled"),
            },
            CalendarLookup::NAME => match &self.calendar {
                Some(calendar) => calendar.invoke(arguments).await,
                None => anyhow::bail!("calendar lookup is not enabled"),
            },
            DirectionsLookup::NAME => match &self.directions {
                Some(directions) => directions.invoke(arguments).await,
                None => anyhow::bail!("directions lookup is not enabled"),
            },
            SpotifyControl::NAME => match &self.spotify {
                Some(spotify) => spotify.invoke(arguments).await,
                None => anyhow::bail!("spotify control is not enabled"),
            },
            ShoppingListControl::NAME => match &self.grocery {
                Some(grocery) => grocery.invoke(arguments).await,
                None => anyhow::bail!("the shopping list tool is not enabled"),
            },
            RECIPE_LOOKUP => match &self.recipe {
                Some(recipe) => recipe.invoke(arguments, actions).await,
                None => anyhow::bail!("recipe lookup is not enabled"),
            },
            CLOSE_RECIPE => match &self.recipe {
                Some(_) => close_recipe_invoke(actions),
                None => anyhow::bail!("recipe lookup is not enabled"),
            },
            RECIPE_CONTROL => match &self.recipe {
                Some(_) => recipe_control_invoke(arguments, actions),
                None => anyhow::bail!("recipe lookup is not enabled"),
            },
            other => anyhow::bail!("model called unknown tool `{other}`"),
        }
    }
}

/// Build a search provider from an explicit label + optional API key.
/// `tavily` (with a key) gives real general web results; anything else (or Tavily
/// with no key) falls back to the keyless DuckDuckGo Instant Answer API, which
/// only answers encyclopedic/entity queries (news/weather return nothing).
pub fn build_search_provider(provider: &str, api_key: Option<&str>) -> Arc<dyn SearchProvider> {
    match provider.to_lowercase().as_str() {
        "tavily" => match api_key {
            Some(key) if !key.is_empty() => {
                log::info!("web search provider: Tavily");
                Arc::new(TavilySearch::new(key))
            }
            _ => {
                log::warn!(
                    "search provider `tavily` selected but no API key is set; \
                     falling back to DuckDuckGo Instant Answer (entity queries only)"
                );
                Arc::new(DuckDuckGoSearch::default())
            }
        },
        _ => {
            log::info!(
                "web search provider: DuckDuckGo Instant Answer (keyless; entity queries \
                 only — choose Tavily + a key for general search)"
            );
            Arc::new(DuckDuckGoSearch::default())
        }
    }
}

/// Build the tool set from explicit config. Always returns a tool set (the timer
/// tools are unconditional device actions); the web-search tool is included only
/// when `web_search` is on.
#[allow(clippy::too_many_arguments)]
pub fn tools_from_config(
    web_search: bool,
    provider: &str,
    api_key: Option<&str>,
    home_location: LiveHomeLocation,
    spotify: Option<Arc<dyn SpotifyController>>,
    calendar: Option<Arc<dyn CalendarSource>>,
    directions: Option<(Arc<dyn DirectionsProvider>, bool)>,
    grocery: Option<Arc<dyn GroceryController>>,
) -> Option<Arc<Tools>> {
    let search = web_search.then(|| build_search_provider(provider, api_key));
    // The recipe tool needs real web search to find a source page, so it rides the
    // same Tavily key as the web-search tool (regardless of the web_search toggle).
    // `None` when no Tavily key is configured → recipe_lookup isn't advertised.
    let recipe = crate::recipe::from_search(provider, api_key);
    // Calendar + directions are prebuilt once on the `LlmFactory` from the JSON
    // config (read-only web .ics subscriptions / the Mapbox provider); absent → the
    // corresponding tool simply isn't advertised. The directions default origin reads
    // from the live household location (`home_location`), so a config-page edit takes
    // effect without a restart — inject the shared handle into the config here.
    let directions = directions.map(|(provider, imperial)| DirectionsConfig {
        provider,
        home_location,
        imperial,
    });
    // Spotify control is passed in from the live settings (`SpotifyConfig::controller`),
    // seeded from the config file's `spotify` block at boot and updated by the
    // config-page consent flow; `None` → the spotify_control tool isn't advertised.
    // Grocery (Cadora shopping list) is likewise passed in from the live settings
    // (`CadoraConfig::controller`); `None` → the shopping_list_add tool isn't advertised.
    Some(Arc::new(Tools::new(
        search, calendar, directions, spotify, grocery, recipe,
    )))
}

// ===========================================================================
// Backend
// ===========================================================================

/// The rig completion model for the selected provider, wrapped in `Arc` so the
/// backend is cheaply clonable into the (`'static`) reply stream. `Arc<M>` is
/// itself a `CompletionModel`, so both variants stay usable as models.
#[derive(Clone)]
enum Model {
    Ollama(Arc<ollama::CompletionModel>),
    Anthropic(Arc<anthropic::completion::CompletionModel>),
}

/// A rig-backed LLM backend. Holds a ready provider model, the `max_tokens` cap
/// (`0` = provider default), and an optional tool set.
pub struct RigBackend {
    name: &'static str,
    model: Model,
    max_tokens: u64,
    tools: Option<Arc<Tools>>,
}

impl RigBackend {
    /// Local Ollama via rig. `base_url` is the Ollama root (no API key).
    pub fn ollama(base_url: &str, model: &str, tools: Option<Arc<Tools>>) -> Result<Self> {
        let client = ollama::Client::builder()
            .api_key(ollama::OllamaApiKey::default())
            .base_url(base_url)
            .build()
            .context("building rig Ollama client")?;
        Ok(Self {
            name: "ollama",
            model: Model::Ollama(Arc::new(client.completion_model(model))),
            max_tokens: 0,
            tools,
        })
    }

    /// Cloud Anthropic via rig. Pins the API version rig ships as "latest".
    pub fn anthropic(
        base_url: &str,
        api_key: &str,
        model: &str,
        max_tokens: u32,
        tools: Option<Arc<Tools>>,
    ) -> Result<Self> {
        let client = anthropic::Client::builder()
            .api_key(anthropic::client::AnthropicKey::from(api_key))
            .base_url(base_url)
            .anthropic_version(anthropic::completion::ANTHROPIC_VERSION_LATEST)
            .build()
            .context("building rig Anthropic client")?;
        Ok(Self {
            name: "anthropic",
            model: Model::Anthropic(Arc::new(client.completion_model(model))),
            max_tokens: max_tokens as u64,
            tools,
        })
    }
}

/// Build the initial message list for a turn: replay any prior conversation
/// `history` (`(user, assistant)` pairs, oldest first) followed by this turn's
/// user message. History is non-empty only for a **follow-up** turn (the device
/// auto-opened it after a question reply); ordinary turns start with just the user
/// message. See `plans/Plan.MD` (Follow-up listening).
fn seed_messages(history: Vec<(String, String)>, user_message: String) -> Vec<Message> {
    let mut messages = Vec::with_capacity(history.len() * 2 + 1);
    for (user, assistant) in history {
        messages.push(Message::user(user));
        messages.push(Message::assistant(assistant));
    }
    messages.push(Message::user(user_message));
    messages
}

/// Open a streamed completion for the running conversation. Generic over the
/// provider model so both variants share one path.
async fn build_and_stream<M>(
    model: M,
    system_prompt: &str,
    messages: &[Message],
    tool_defs: &[ToolDefinition],
    max_tokens: u64,
) -> Result<StreamingCompletionResponse>
where
    M: CompletionModel + Clone,
{
    let (prompt, history) = messages
        .split_last()
        .expect("conversation always has at least the user message");
    // When tools are available, tell the model to use them (some models otherwise
    // refuse real-time questions instead of calling the tool).
    let preamble = build_preamble(system_prompt, tool_defs);
    let mut builder = model
        .completion_request(prompt.clone())
        .messages(history.iter().cloned())
        .preamble(preamble);
    if !tool_defs.is_empty() {
        builder = builder.tools(tool_defs.to_vec());
    }
    if max_tokens > 0 {
        builder = builder.max_tokens(max_tokens);
    }
    model
        .stream(builder.build())
        .await
        .context("opening rig completion stream")
}

/// Dispatch a streamed completion on the concrete provider model.
async fn open_stream(
    model: &Model,
    system_prompt: &str,
    messages: &[Message],
    tool_defs: &[ToolDefinition],
    max_tokens: u64,
) -> Result<StreamingCompletionResponse> {
    match model {
        Model::Ollama(m) => {
            build_and_stream(m.clone(), system_prompt, messages, tool_defs, max_tokens).await
        }
        Model::Anthropic(m) => {
            build_and_stream(m.clone(), system_prompt, messages, tool_defs, max_tokens).await
        }
    }
}

/// Non-streaming completion used for tool-negotiation rounds. Returns the assistant
/// text (if any) plus every tool call the model requested. Unlike the streaming
/// path, this reliably surfaces tool calls across providers (see `respond`).
async fn open_completion(
    model: &Model,
    system_prompt: &str,
    messages: &[Message],
    tool_defs: &[ToolDefinition],
    max_tokens: u64,
) -> Result<(String, Vec<ToolCall>)> {
    match model {
        Model::Ollama(m) => {
            build_and_complete(m.clone(), system_prompt, messages, tool_defs, max_tokens).await
        }
        Model::Anthropic(m) => {
            build_and_complete(m.clone(), system_prompt, messages, tool_defs, max_tokens).await
        }
    }
}

async fn build_and_complete<M>(
    model: M,
    system_prompt: &str,
    messages: &[Message],
    tool_defs: &[ToolDefinition],
    max_tokens: u64,
) -> Result<(String, Vec<ToolCall>)>
where
    M: CompletionModel + Clone,
{
    let (prompt, history) = messages
        .split_last()
        .expect("conversation always has at least the user message");
    let preamble = build_preamble(system_prompt, tool_defs);
    let mut builder = model
        .completion_request(prompt.clone())
        .messages(history.iter().cloned())
        .preamble(preamble);
    if !tool_defs.is_empty() {
        builder = builder.tools(tool_defs.to_vec());
    }
    if max_tokens > 0 {
        builder = builder.max_tokens(max_tokens);
    }
    let response = model
        .completion(builder.build())
        .await
        .context("rig completion request")?;

    let mut text = String::new();
    let mut calls: Vec<ToolCall> = Vec::new();
    for content in response.choice.into_iter() {
        match content {
            AssistantContent::Text(t) => text.push_str(&t.text),
            AssistantContent::ToolCall(tc) => calls.push(tc),
            _ => {}
        }
    }
    Ok((text.trim().to_string(), calls))
}

#[async_trait]
impl LlmBackend for RigBackend {
    fn name(&self) -> &str {
        self.name
    }

    async fn respond(&self, turn: LlmTurn) -> Result<ReplyStream> {
        let model = self.model.clone();
        let max_tokens = self.max_tokens;
        let tools = self.tools.clone();
        let system_prompt = turn.system_prompt;
        // The per-turn device-action sink (timers). Cloned into the tool loop so
        // action tools can relay to the device while the reply is generated.
        let actions = turn.actions.clone();
        let tool_defs: Vec<ToolDefinition> = tools
            .as_ref()
            .map(|t| t.definitions.clone())
            .unwrap_or_default();

        // No tools → pure streaming (token-by-token) in a single pass.
        if tool_defs.is_empty() {
            let stream = async_stream::try_stream! {
                let messages = seed_messages(turn.history, turn.user_message);
                let response =
                    open_stream(&model, &system_prompt, &messages, &tool_defs, max_tokens).await?;
                futures_util::pin_mut!(response);
                while let Some(part) = response.next().await {
                    if let StreamedAssistantContent::Text(text) = part? {
                        if !text.text.is_empty() {
                            yield text.text;
                        }
                    }
                }
            };
            return Ok(Box::pin(stream));
        }

        // Tools present → drive the tool-negotiation rounds with a **non-streaming**
        // completion. rig-core 0.42's ollama *streaming* parser drops tool calls that
        // arrive in a non-final (`done:false`) chunk — which is exactly how live
        // ollama emits them — so the tool was never executed and the reply came back
        // empty. Non-streaming `completion()` returns the tool calls reliably. We
        // give up token-by-token streaming for tool turns, which is fine: the reply
        // is spoken via TTS (and rendered) from the full text regardless.
        let stream = async_stream::try_stream! {
            let mut messages: Vec<Message> = seed_messages(turn.history, turn.user_message);

            for _round in 0..MAX_TOOL_ROUNDS {
                let (text, calls) =
                    open_completion(&model, &system_prompt, &messages, &tool_defs, max_tokens)
                        .await?;

                if !text.is_empty() {
                    yield text;
                }

                // No tool calls → the model has produced its answer; stop.
                if calls.is_empty() {
                    break;
                }
                let Some(tools) = tools.as_ref() else {
                    break;
                };

                // Record the assistant's tool-call turn, then each tool's result,
                // so the next pass sees the outcomes and can answer.
                messages.push(Message::Assistant {
                    id: None,
                    content: calls.iter().cloned().map(AssistantContent::ToolCall).collect(),
                });
                for call in &calls {
                    log::info!(
                        "rig tool call: {}({})",
                        call.function.name,
                        call.function.arguments
                    );
                    let result = tools
                        .dispatch(&call.function.name, &call.function.arguments, actions.as_ref())
                        .await
                        .unwrap_or_else(|e| format!("tool error: {e:#}"));
                    log::info!(
                        "rig tool result ({} chars): {}",
                        result.len(),
                        result.chars().take(200).collect::<String>()
                    );
                    messages.push(Message::tool_result(
                        call.id.as_str().to_string(),
                        call.function.name.clone(),
                        result,
                    ));
                }
            }
        };

        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::collect_reply;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A canned search provider so tool tests never touch the network.
    struct StaticSearch(&'static str);

    #[async_trait]
    impl SearchProvider for StaticSearch {
        async fn search(&self, _query: &str, _max: u8) -> Result<String> {
            Ok(self.0.to_string())
        }
    }

    /// A canned calendar source so tool tests never touch the network.
    struct StaticCalendar(Vec<crate::calendar::CalEvent>);

    #[async_trait]
    impl CalendarSource for StaticCalendar {
        async fn events(
            &self,
            _window: crate::calendar::TimeWindow,
        ) -> Result<Vec<crate::calendar::CalEvent>> {
            Ok(self.0.clone())
        }
        fn calendar_names(&self) -> Vec<String> {
            vec!["Work".to_string()]
        }
    }

    /// A canned directions provider so tool tests never touch the network. Records
    /// the origin it was called with so tests can assert the home-location default.
    struct StaticDirections {
        seen_origin: std::sync::Mutex<Option<String>>,
    }

    #[async_trait]
    impl DirectionsProvider for StaticDirections {
        async fn directions(
            &self,
            origin: &str,
            destination: &str,
            mode: TravelMode,
        ) -> Result<crate::directions::Directions> {
            *self.seen_origin.lock().unwrap() = Some(origin.to_string());
            Ok(crate::directions::Directions {
                origin_label: origin.to_string(),
                destination_label: destination.to_string(),
                distance_meters: 20000.0,
                duration_seconds: 1200.0,
                duration_typical_seconds: Some(900.0),
                mode,
            })
        }
    }

    /// A canned Spotify controller so tool tests never touch the network. Records
    /// the last command it was asked to run so tests can assert the mapping.
    struct StaticSpotify {
        last: std::sync::Mutex<Option<SpotifyCommand>>,
    }

    #[async_trait]
    impl SpotifyController for StaticSpotify {
        async fn command(&self, cmd: SpotifyCommand) -> Result<String> {
            let reply = match &cmd {
                SpotifyCommand::Play { query: Some(q), .. } => format!("Playing {q}."),
                SpotifyCommand::Play { query: None, .. } | SpotifyCommand::Resume => {
                    "Resuming playback.".to_string()
                }
                SpotifyCommand::Pause => "Paused.".to_string(),
                SpotifyCommand::Next => "Skipping.".to_string(),
                SpotifyCommand::Previous => "Going back.".to_string(),
                SpotifyCommand::Queue { query } => format!("Queued {query}."),
                SpotifyCommand::SetVolume { percent } => format!("Volume {percent}."),
            };
            *self.last.lock().unwrap() = Some(cmd);
            Ok(reply)
        }
    }

    /// A canned grocery controller so shopping-tool tests never touch the network.
    /// Records the last command so tests can assert the arg mapping.
    struct StaticGrocery {
        last: std::sync::Mutex<Option<GroceryCommand>>,
    }

    #[async_trait]
    impl GroceryController for StaticGrocery {
        async fn command(&self, cmd: GroceryCommand) -> Result<String> {
            let reply = match &cmd {
                GroceryCommand::AddItem {
                    item,
                    quantity: Some(q),
                } => format!("Added {q} {item} to your shopping list."),
                GroceryCommand::AddItem {
                    item,
                    quantity: None,
                } => format!("Added {item} to your shopping list."),
            };
            *self.last.lock().unwrap() = Some(cmd);
            Ok(reply)
        }
    }

    /// Serve a fixed sequence of raw HTTP responses, one per inbound connection.
    fn serve_sequence(bodies: Vec<String>) -> (String, tokio::task::JoinHandle<()>) {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let addr = std_listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let listener = TcpListener::from_std(std_listener).unwrap();
            for body in bodies {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                sock.write_all(resp.as_bytes()).await.unwrap();
                sock.flush().await.unwrap();
            }
        });
        (format!("http://{addr}"), handle)
    }

    #[test]
    fn internet_search_definition_advertises_typed_args() {
        let tool = InternetSearch::new(Arc::new(StaticSearch("x")));
        let def = tool.definition();
        assert_eq!(def.name, "internet_search");
        assert_eq!(def.parameters["required"][0], "query");
        assert_eq!(def.parameters["properties"]["query"]["type"], "string");
    }

    #[tokio::test]
    async fn internet_search_invoke_runs_the_provider() {
        let tool = InternetSearch::new(Arc::new(StaticSearch("Sunny, 21C.")));
        let out = tool
            .invoke(&json!({ "query": "weather in paris" }))
            .await
            .unwrap();
        assert_eq!(out, "Sunny, 21C.");
    }

    #[tokio::test]
    async fn duckduckgo_summarizes_instant_answer_json() {
        let body = r#"{"AbstractText":"Rust is a systems language.","RelatedTopics":[{"Text":"Rust (programming language)"}]}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
        });
        let ddg = DuckDuckGoSearch::with_base_url(format!("http://{addr}"));
        let out = ddg.search("rust lang", 3).await.unwrap();
        assert!(out.contains("systems language"));
        assert!(out.contains("Rust (programming language)"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn tavily_prefers_the_synthesized_answer() {
        let body = r#"{"answer":"It is 21C and sunny in Paris.","results":[{"title":"Weather","content":"Paris weather details"}]}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
        });
        let tavily = TavilySearch::with_base_url(format!("http://{addr}"), "k");
        let out = tavily.search("weather in paris", 3).await.unwrap();
        assert_eq!(out, "It is 21C and sunny in Paris.");
        server.await.unwrap();
    }

    /// End-to-end tool loop: round 0 the fake Ollama asks for `internet_search`;
    /// round 1 (after the tool result is threaded back) it streams the answer.
    #[tokio::test]
    async fn rig_ollama_runs_tool_then_streams_answer() {
        let tool_call = "{\"model\":\"m\",\"created_at\":\"t\",\"message\":{\"role\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"function\":{\"name\":\"internet_search\",\"arguments\":{\"query\":\"weather in paris\"}}}]},\"done\":true,\"done_reason\":\"stop\"}\n";
        // Tool turns use a non-streaming completion (see `respond`), so the follow-up
        // answer is a single JSON object rather than streamed deltas.
        let answer = "{\"model\":\"m\",\"created_at\":\"t\",\"message\":{\"role\":\"assistant\",\"content\":\"It is sunny in Paris.\"},\"done\":true,\"done_reason\":\"stop\"}\n";
        let (url, server) = serve_sequence(vec![tool_call.to_string(), answer.to_string()]);

        let tools = Some(Arc::new(Tools::new(
            Some(Arc::new(StaticSearch("Sunny, 21C."))),
            None,
            None,
            None,
            None,
            None,
        )));
        let backend = RigBackend::ollama(&url, "test-model", tools).unwrap();
        let stream = backend
            .respond(LlmTurn::new("sys", "what is the weather in paris"))
            .await
            .unwrap();
        assert_eq!(
            collect_reply(stream).await.unwrap(),
            "It is sunny in Paris."
        );
        server.await.unwrap();
    }

    /// End-to-end tool loop for the calendar tool: round 0 the fake Ollama asks for
    /// `calendar_lookup`; round 1 (after the rendered events are threaded back) it
    /// streams the answer. Proves the tool is advertised, dispatched, and its result
    /// reaches the model — using a canned source so no network/feed is touched.
    #[tokio::test]
    async fn rig_ollama_runs_calendar_tool_then_streams_answer() {
        use chrono::{Duration, Local};

        let tool_call = "{\"model\":\"m\",\"created_at\":\"t\",\"message\":{\"role\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"function\":{\"name\":\"calendar_lookup\",\"arguments\":{\"when\":\"today\",\"person\":\"Alice\"}}}]},\"done\":true,\"done_reason\":\"stop\"}\n";
        let answer = "{\"model\":\"m\",\"created_at\":\"t\",\"message\":{\"role\":\"assistant\",\"content\":\"You have a 1:1 with Alice at 2 PM.\"},\"done\":true,\"done_reason\":\"stop\"}\n";
        let (url, server) = serve_sequence(vec![tool_call.to_string(), answer.to_string()]);

        let start = Local::now() + Duration::hours(2);
        let event = crate::calendar::CalEvent {
            calendar: "Work".into(),
            summary: "1:1".into(),
            start,
            end: Some(start + Duration::hours(1)),
            all_day: false,
            location: None,
            organizer: None,
            attendees: vec!["Alice".into()],
            description: None,
        };
        let tools = Some(Arc::new(Tools::new(
            None,
            Some(Arc::new(StaticCalendar(vec![event]))),
            None,
            None,
            None,
            None,
        )));
        let backend = RigBackend::ollama(&url, "test-model", tools).unwrap();
        let stream = backend
            .respond(LlmTurn::new("sys", "am I seeing Alice today?"))
            .await
            .unwrap();
        assert_eq!(
            collect_reply(stream).await.unwrap(),
            "You have a 1:1 with Alice at 2 PM."
        );
        server.await.unwrap();
    }

    /// End-to-end tool loop for the directions tool: round 0 the fake Ollama asks for
    /// `directions_lookup` with only a destination; round 1 (after the rendered route
    /// is threaded back) it streams the answer. Proves the tool is advertised,
    /// dispatched with the home-location default filled in for the missing origin, and
    /// that its result reaches the model — using a canned provider (no network).
    #[tokio::test]
    async fn rig_ollama_runs_directions_tool_with_home_default() {
        let tool_call = "{\"model\":\"m\",\"created_at\":\"t\",\"message\":{\"role\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"function\":{\"name\":\"directions_lookup\",\"arguments\":{\"destination\":\"the airport\"}}}]},\"done\":true,\"done_reason\":\"stop\"}\n";
        let answer = "{\"model\":\"m\",\"created_at\":\"t\",\"message\":{\"role\":\"assistant\",\"content\":\"About 20 minutes to the airport — traffic is heavy.\"},\"done\":true,\"done_reason\":\"stop\"}\n";
        let (url, server) = serve_sequence(vec![tool_call.to_string(), answer.to_string()]);

        let provider = Arc::new(StaticDirections {
            seen_origin: std::sync::Mutex::new(None),
        });
        let directions = DirectionsConfig {
            provider: provider.clone(),
            home_location: LiveHomeLocation::new(Some("Home, Austin".to_string())),
            imperial: true,
        };
        let tools = Some(Arc::new(Tools::new(
            None,
            None,
            Some(directions),
            None,
            None,
            None,
        )));
        let backend = RigBackend::ollama(&url, "test-model", tools).unwrap();
        let stream = backend
            .respond(LlmTurn::new("sys", "how long to the airport?"))
            .await
            .unwrap();
        assert_eq!(
            collect_reply(stream).await.unwrap(),
            "About 20 minutes to the airport — traffic is heavy."
        );
        // The missing origin defaulted to the configured home location.
        assert_eq!(
            provider.seen_origin.lock().unwrap().as_deref(),
            Some("Home, Austin")
        );
        server.await.unwrap();
    }

    #[test]
    fn directions_tool_advertised_only_when_configured() {
        let none = Tools::new(None, None, None, None, None, None);
        assert!(!none
            .definitions
            .iter()
            .any(|d| d.name == DirectionsLookup::NAME));

        let with = Tools::new(
            None,
            None,
            Some(DirectionsConfig {
                provider: Arc::new(StaticDirections {
                    seen_origin: std::sync::Mutex::new(None),
                }),
                home_location: LiveHomeLocation::default(),
                imperial: false,
            }),
            None,
            None,
            None,
        );
        assert!(with
            .definitions
            .iter()
            .any(|d| d.name == DirectionsLookup::NAME));
    }

    /// The directions default origin reads the live household location, so an edit
    /// (config page Household tab → `LiveHomeLocation::set`) changes the origin the
    /// tool uses on the very next call — no backend rebuild.
    #[tokio::test]
    async fn directions_default_origin_tracks_live_home_location() {
        let provider = Arc::new(StaticDirections {
            seen_origin: std::sync::Mutex::new(None),
        });
        let home = LiveHomeLocation::new(Some("Austin, TX".to_string()));
        let tool = DirectionsLookup::new(provider.clone(), home.clone(), true);

        tool.invoke(&json!({ "destination": "the airport" }))
            .await
            .unwrap();
        assert_eq!(
            provider.seen_origin.lock().unwrap().as_deref(),
            Some("Austin, TX")
        );

        // Edit the household location live — the next call uses the new origin.
        home.set(Some("Boston, MA".to_string()));
        tool.invoke(&json!({ "destination": "the airport" }))
            .await
            .unwrap();
        assert_eq!(
            provider.seen_origin.lock().unwrap().as_deref(),
            Some("Boston, MA")
        );
    }

    #[test]
    fn spotify_tool_advertised_only_when_configured() {
        let none = Tools::new(None, None, None, None, None, None);
        assert!(!none
            .definitions
            .iter()
            .any(|d| d.name == SpotifyControl::NAME));

        let with = Tools::new(
            None,
            None,
            None,
            Some(Arc::new(StaticSpotify {
                last: std::sync::Mutex::new(None),
            })),
            None,
            None,
        );
        assert!(with
            .definitions
            .iter()
            .any(|d| d.name == SpotifyControl::NAME));
    }

    #[test]
    fn shopping_tool_advertised_only_when_configured() {
        let none = Tools::new(None, None, None, None, None, None);
        assert!(!none
            .definitions
            .iter()
            .any(|d| d.name == ShoppingListControl::NAME));

        let with = Tools::new(
            None,
            None,
            None,
            None,
            Some(Arc::new(StaticGrocery {
                last: std::sync::Mutex::new(None),
            })),
            None,
        );
        assert!(with
            .definitions
            .iter()
            .any(|d| d.name == ShoppingListControl::NAME));
    }

    #[test]
    fn spotify_args_map_to_commands() {
        let play = SpotifyControl::to_command(SpotifyArgs {
            action: "PLAY".into(),
            query: Some("  radiohead ".into()),
            kind: Some("artist".into()),
            volume_percent: None,
        })
        .unwrap();
        assert_eq!(
            play,
            SpotifyCommand::Play {
                query: Some("radiohead".into()),
                kind: SearchKind::Artist
            }
        );

        // Resume: play with no query.
        assert_eq!(
            SpotifyControl::to_command(SpotifyArgs {
                action: "play".into(),
                query: None,
                kind: None,
                volume_percent: None,
            })
            .unwrap(),
            SpotifyCommand::Play {
                query: None,
                kind: SearchKind::Track
            }
        );

        assert_eq!(
            SpotifyControl::to_command(SpotifyArgs {
                action: "skip".into(),
                query: None,
                kind: None,
                volume_percent: None,
            })
            .unwrap(),
            SpotifyCommand::Next
        );

        // volume without a level, and unknown action, both error (surfaced to model).
        assert!(SpotifyControl::to_command(SpotifyArgs {
            action: "volume".into(),
            query: None,
            kind: None,
            volume_percent: None,
        })
        .is_err());
        assert!(SpotifyControl::to_command(SpotifyArgs {
            action: "teleport".into(),
            query: None,
            kind: None,
            volume_percent: None,
        })
        .is_err());
    }

    /// End-to-end tool loop for the Spotify tool: round 0 the fake Ollama asks for
    /// `spotify_control` (play an artist); round 1 (after the confirmation is threaded
    /// back) it streams the reply. Proves the tool is advertised, dispatched with the
    /// parsed command, and its result reaches the model — using a canned controller.
    #[tokio::test]
    async fn rig_ollama_runs_spotify_tool_then_streams_answer() {
        let tool_call = "{\"model\":\"m\",\"created_at\":\"t\",\"message\":{\"role\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"function\":{\"name\":\"spotify_control\",\"arguments\":{\"action\":\"play\",\"query\":\"Radiohead\",\"kind\":\"artist\"}}}]},\"done\":true,\"done_reason\":\"stop\"}\n";
        let answer = "{\"model\":\"m\",\"created_at\":\"t\",\"message\":{\"role\":\"assistant\",\"content\":\"Playing Radiohead now.\"},\"done\":true,\"done_reason\":\"stop\"}\n";
        let (url, server) = serve_sequence(vec![tool_call.to_string(), answer.to_string()]);

        let controller = Arc::new(StaticSpotify {
            last: std::sync::Mutex::new(None),
        });
        let tools = Some(Arc::new(Tools::new(
            None,
            None,
            None,
            Some(controller.clone()),
            None,
            None,
        )));
        let backend = RigBackend::ollama(&url, "test-model", tools).unwrap();
        let stream = backend
            .respond(LlmTurn::new("sys", "play some Radiohead"))
            .await
            .unwrap();
        assert_eq!(
            collect_reply(stream).await.unwrap(),
            "Playing Radiohead now."
        );
        // The controller received the parsed command.
        assert_eq!(
            controller.last.lock().unwrap().clone(),
            Some(SpotifyCommand::Play {
                query: Some("Radiohead".into()),
                kind: SearchKind::Artist
            })
        );
        server.await.unwrap();
    }

    /// End-to-end tool loop for the shopping tool: round 0 the fake Ollama asks for
    /// `shopping_list_add` (2 milk); round 1 (after the confirmation is threaded back)
    /// it streams the reply. Proves the tool is advertised, dispatched with the parsed
    /// args, and its result reaches the model — using a canned controller.
    #[tokio::test]
    async fn rig_ollama_runs_shopping_tool_then_streams_answer() {
        let tool_call = "{\"model\":\"m\",\"created_at\":\"t\",\"message\":{\"role\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"function\":{\"name\":\"shopping_list_add\",\"arguments\":{\"item\":\"milk\",\"quantity\":2}}}]},\"done\":true,\"done_reason\":\"stop\"}\n";
        let answer = "{\"model\":\"m\",\"created_at\":\"t\",\"message\":{\"role\":\"assistant\",\"content\":\"Done — 2 milk are on the list.\"},\"done\":true,\"done_reason\":\"stop\"}\n";
        let (url, server) = serve_sequence(vec![tool_call.to_string(), answer.to_string()]);

        let controller = Arc::new(StaticGrocery {
            last: std::sync::Mutex::new(None),
        });
        let tools = Some(Arc::new(Tools::new(
            None,
            None,
            None,
            None,
            Some(controller.clone()),
            None,
        )));
        let backend = RigBackend::ollama(&url, "test-model", tools).unwrap();
        let stream = backend
            .respond(LlmTurn::new("sys", "add two milk to the shopping list"))
            .await
            .unwrap();
        assert_eq!(
            collect_reply(stream).await.unwrap(),
            "Done — 2 milk are on the list."
        );
        // The controller received the parsed command.
        assert_eq!(
            controller.last.lock().unwrap().clone(),
            Some(GroceryCommand::AddItem {
                item: "milk".into(),
                quantity: Some(2),
            })
        );
        server.await.unwrap();
    }

    #[test]
    fn humanize_secs_reads_naturally() {
        assert_eq!(humanize_secs(45), "45 seconds");
        assert_eq!(humanize_secs(60), "1 minute");
        assert_eq!(humanize_secs(300), "5 minutes");
        assert_eq!(humanize_secs(3661), "1 hour 1 minute 1 second");
        assert_eq!(humanize_secs(0), "0 seconds");
    }

    #[test]
    fn set_timer_emits_start_action_and_confirms() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let out = set_timer_invoke(
            &json!({ "duration_seconds": 300, "label": "pasta" }),
            Some(&tx),
        )
        .unwrap();
        assert_eq!(
            rx.try_recv().unwrap(),
            DeviceAction::StartTimer {
                label: Some("pasta".to_string()),
                duration_secs: 300
            }
        );
        assert!(out.contains("5 minutes"));
        assert!(out.contains("pasta"));
    }

    #[test]
    fn set_timer_clamps_and_without_device_errors() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        set_timer_invoke(&json!({ "duration_seconds": 999999999u64 }), Some(&tx)).unwrap();
        assert_eq!(
            rx.try_recv().unwrap(),
            DeviceAction::StartTimer {
                label: None,
                duration_secs: MAX_TIMER_SECS
            }
        );
        // No device attached → the tool reports an error the model can relay.
        assert!(set_timer_invoke(&json!({ "duration_seconds": 60 }), None).is_err());
    }

    #[test]
    fn cancel_timer_null_args_cancels_all() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        cancel_timer_invoke(&Value::Null, Some(&tx)).unwrap();
        assert_eq!(
            rx.try_recv().unwrap(),
            DeviceAction::CancelTimer { label: None }
        );
        cancel_timer_invoke(&json!({ "label": "pasta" }), Some(&tx)).unwrap();
        assert_eq!(
            rx.try_recv().unwrap(),
            DeviceAction::CancelTimer {
                label: Some("pasta".to_string())
            }
        );
    }

    #[test]
    fn timer_tools_are_always_advertised_even_without_web_search() {
        let tools = Tools::new(None, None, None, None, None, None);
        let names: Vec<&str> = tools.definitions.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&SET_TIMER));
        assert!(names.contains(&CANCEL_TIMER));
        assert!(!names.contains(&InternetSearch::NAME));
    }

    /// End-to-end tool loop: the fake ollama asks for `set_timer`; the pipeline's
    /// action sink receives the `StartTimer`, and the follow-up answer streams back.
    #[tokio::test]
    async fn rig_ollama_runs_set_timer_and_relays_the_action() {
        let tool_call = "{\"model\":\"m\",\"created_at\":\"t\",\"message\":{\"role\":\"assistant\",\"content\":\"\",\"tool_calls\":[{\"function\":{\"name\":\"set_timer\",\"arguments\":{\"duration_seconds\":300,\"label\":\"pasta\"}}}]},\"done\":true,\"done_reason\":\"stop\"}\n";
        let answer = "{\"model\":\"m\",\"created_at\":\"t\",\"message\":{\"role\":\"assistant\",\"content\":\"Your pasta timer is set for five minutes.\"},\"done\":true,\"done_reason\":\"stop\"}\n";
        let (url, server) = serve_sequence(vec![tool_call.to_string(), answer.to_string()]);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // No web search — timers are always available regardless.
        let tools = Some(Arc::new(Tools::new(None, None, None, None, None, None)));
        let backend = RigBackend::ollama(&url, "test-model", tools).unwrap();
        let stream = backend
            .respond(LlmTurn::new("sys", "set a 5 minute pasta timer").with_actions(tx))
            .await
            .unwrap();
        let reply = collect_reply(stream).await.unwrap();
        assert_eq!(reply, "Your pasta timer is set for five minutes.");
        assert_eq!(
            rx.try_recv().unwrap(),
            DeviceAction::StartTimer {
                label: Some("pasta".to_string()),
                duration_secs: 300
            }
        );
        server.await.unwrap();
    }

    /// Without tools, a turn still streams straight through in one pass.
    #[tokio::test]
    async fn rig_ollama_streams_text_deltas() {
        let body = "{\"model\":\"m\",\"created_at\":\"t\",\"message\":{\"role\":\"assistant\",\"content\":\"Hi\"},\"done\":false}\n\
                    {\"model\":\"m\",\"created_at\":\"t\",\"message\":{\"role\":\"assistant\",\"content\":\" there\"},\"done\":true,\"done_reason\":\"stop\"}\n";
        let (url, server) = serve_sequence(vec![body.to_string()]);
        let backend = RigBackend::ollama(&url, "test-model", None).unwrap();
        let stream = backend.respond(LlmTurn::new("sys", "hello")).await.unwrap();
        assert_eq!(collect_reply(stream).await.unwrap(), "Hi there");
        server.await.unwrap();
    }
}
