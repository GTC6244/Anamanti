//! [`LlmBackend`] implemented on top of the **rig-core** agent framework
//! (selected at runtime with `AMBIENT_LLM_ENGINE=rig`).
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
//! `AMBIENT_WEB_SEARCH`), `respond` runs a short negotiation loop: it streams a
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

use super::{ActionSink, DeviceAction, LlmBackend, LlmTurn, ReplyStream};
use crate::calendar::CalendarSource;
use crate::directions::{DirectionsConfig, DirectionsProvider, TravelMode};

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
                ("t", "ambient-orchestrator"),
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
/// offline, mirroring [`CalendarLookup`]. Holds the default origin (the device's
/// home location) and the preferred distance units.
pub struct DirectionsLookup {
    provider: Arc<dyn DirectionsProvider>,
    default_origin: Option<String>,
    imperial: bool,
}

impl DirectionsLookup {
    pub fn new(
        provider: Arc<dyn DirectionsProvider>,
        default_origin: Option<String>,
        imperial: bool,
    ) -> Self {
        Self {
            provider,
            default_origin,
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
            .default_origin
            .as_deref()
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
            .or_else(|| self.default_origin.clone())
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
// Tool set
// ===========================================================================

/// The set of tools available to a turn: their advertised definitions plus a
/// dispatcher that executes a call by name. Cheaply shared behind an `Arc`.
///
/// The **timer** tools (`set_timer` / `cancel_timer`) are always present — they are
/// stateless device actions needing no config. The **web-search** tool is included
/// only when a provider is configured (`web_search` on); the **calendar** tool only
/// when calendar subscriptions are configured (`AMBIENT_CALENDARS`); the
/// **directions** tool only when a routing provider is configured (`MAPBOX_TOKEN`).
pub struct Tools {
    definitions: Vec<ToolDefinition>,
    search: Option<Arc<InternetSearch>>,
    calendar: Option<Arc<CalendarLookup>>,
    directions: Option<Arc<DirectionsLookup>>,
}

impl Tools {
    /// Build the tool set. Timer tools are always advertised; the web-search tool is
    /// added when `search` is `Some`, the calendar tool when `calendar` is `Some`, and
    /// the directions tool when `directions` is `Some`.
    pub fn new(
        search: Option<Arc<dyn SearchProvider>>,
        calendar: Option<Arc<dyn CalendarSource>>,
        directions: Option<DirectionsConfig>,
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
                cfg.default_origin,
                cfg.imperial,
            ))
        });
        if let Some(d) = &directions {
            definitions.push(d.definition());
        }
        Self {
            definitions,
            search,
            calendar,
            directions,
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
pub fn tools_from_config(
    web_search: bool,
    provider: &str,
    api_key: Option<&str>,
) -> Option<Arc<Tools>> {
    let search = web_search.then(|| build_search_provider(provider, api_key));
    // Calendar subscriptions are read from `AMBIENT_CALENDARS` (read-only web .ics);
    // absent → the calendar tool simply isn't advertised.
    let calendar = crate::calendar::from_env();
    // Directions come from a routing provider (`MAPBOX_TOKEN`); absent → the
    // directions tool simply isn't advertised.
    let directions = crate::directions::from_env();
    Some(Arc::new(Tools::new(search, calendar, directions)))
}

/// Build the tool set from the environment (used by the example / env-driven
/// default): `AMBIENT_SEARCH_PROVIDER` + `TAVILY_API_KEY`.
pub fn tools_from_flag(web_search: bool) -> Option<Arc<Tools>> {
    let provider = std::env::var("AMBIENT_SEARCH_PROVIDER").unwrap_or_default();
    let key = std::env::var("TAVILY_API_KEY").ok();
    tools_from_config(web_search, &provider, key.as_deref())
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
                let messages = vec![Message::user(turn.user_message)];
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
            let mut messages: Vec<Message> = vec![Message::user(turn.user_message)];

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
            default_origin: Some("Home, Austin".to_string()),
            imperial: true,
        };
        let tools = Some(Arc::new(Tools::new(None, None, Some(directions))));
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
        let none = Tools::new(None, None, None);
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
                default_origin: None,
                imperial: false,
            }),
        );
        assert!(with
            .definitions
            .iter()
            .any(|d| d.name == DirectionsLookup::NAME));
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
        let tools = Tools::new(None, None, None);
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
        let tools = Some(Arc::new(Tools::new(None, None, None)));
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
