//! The voice-assistant tool set the rig engine offers the model.
//!
//! Every tool implements one crate-local trait, `VoiceTool`: a name, the
//! [`ToolDefinition`] advertised to the model, optional usage guidance for the system
//! preamble, and an async `invoke` that turns the model's JSON arguments into a short
//! speakable result. [`Tools`] is a name-keyed, insertion-ordered registry of them:
//! the advertised definitions, the guidance text and the dispatcher are all derived
//! from that one list, so adding a tool means writing it and registering it in
//! [`Tools::build`] — nothing else.
//!
//! Which tools are present is decided once per backend build from [`ToolDeps`]: the
//! always-on device tools (timers, personality, font), plus each integration whose
//! provider is configured (web search, calendar, directions, Spotify, shopping list,
//! phone, recipe, weather, places).
//!
//! Layout: `search` (web search providers + `internet_search`), `device` (timers,
//! personality, font), `display` (recipe / weather / places screens and their
//! `close_*` tools, and the music player's `music_screen`), `services` (calendar, directions, Spotify, radio, shopping list, phone).

mod device;
mod display;
mod search;
mod services;
#[cfg(test)]
pub(crate) mod test_support;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use rig_core::completion::ToolDefinition;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use crate::appsaid::PhoneMessenger;
use crate::cadora::GroceryController;
use crate::calendar::CalendarSource;
use crate::directions::{DirectionsProvider, LiveHomeLocation};
use crate::llm::{ActionSink, DeviceAction};
use crate::music::SpotifyController;

/// Everything the tool set can be built from. Each integration is advertised only
/// when its provider is present; the device tools (timers, personality, font) are
/// always on. `Default` is the minimal set (device tools only).
#[derive(Clone, Default)]
pub struct ToolDeps {
    /// Advertise `internet_search` (`llm.web_search`).
    pub web_search: bool,
    /// Web-search backend label (`tavily` / `duckduckgo`). A Tavily key also enables
    /// `recipe_lookup` (it finds recipe pages through the same search), independent
    /// of `web_search`.
    pub search_provider: String,
    /// API key for the search backend (Tavily). `None`/blank = unset.
    pub search_api_key: Option<String>,
    /// The live household location: the default origin for directions and the
    /// default place for weather, and the search bias for places. Read at call time,
    /// so a household edit takes effect without a rebuild.
    pub home_location: LiveHomeLocation,
    /// Web-calendar source for `calendar_lookup`.
    pub calendar: Option<Arc<dyn CalendarSource>>,
    /// Routing provider + imperial-units flag for `directions_lookup`.
    pub directions: Option<(Arc<dyn DirectionsProvider>, bool)>,
    /// Spotify controller for `spotify_control`.
    pub spotify: Option<Arc<dyn SpotifyController>>,
    /// Cadora controller for `shopping_list_add`.
    pub grocery: Option<Arc<dyn GroceryController>>,
    /// AppSaid messenger for `send_phone_message`.
    pub appsaid: Option<Arc<dyn PhoneMessenger>>,
    /// Forecast provider for `weather_lookup` / `close_weather`.
    pub weather: Option<Arc<dyn crate::weather::WeatherProvider>>,
    /// Whether weather temperatures are reported in imperial units.
    pub weather_imperial: bool,
    /// Places provider for `places_lookup` / `close_places`.
    pub places: Option<Arc<dyn crate::places::PlacesProvider>>,
    /// Internet-radio player for `radio_control` (present only with ≥1 station).
    pub radio: Option<Arc<crate::music::RadioPlayer>>,
}

/// Per-call context handed to [`VoiceTool::invoke`].
pub(crate) struct ToolCtx<'a> {
    /// The per-turn device-action sink, or `None` when no device is attached.
    pub(crate) actions: Option<&'a ActionSink>,
}

impl<'a> ToolCtx<'a> {
    /// The action sink, or an error carrying `missing` (spoken back by the model) when
    /// no device is attached. Tools that fetch before emitting check this first so
    /// they never fetch for a display that isn't there.
    pub(crate) fn require_sink(&self, missing: &'static str) -> Result<&'a ActionSink> {
        self.actions.context(missing)
    }
}

/// One tool the model can call.
#[async_trait]
pub(crate) trait VoiceTool: Send + Sync {
    /// The tool name the model calls it by.
    fn name(&self) -> &'static str;
    /// The definition advertised to the model (name, description, JSON schema).
    fn definition(&self) -> ToolDefinition;
    /// Usage guidance for the system preamble, if this tool needs any.
    fn guidance(&self) -> Option<&'static str> {
        None
    }
    /// Execute a call from the model's raw JSON arguments, returning the text result
    /// the model reads (usually a short speakable confirmation). Errors are returned
    /// to the model as the tool result so it can apologize aloud.
    async fn invoke(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<String>;
}

/// Build a [`ToolDefinition`].
pub(crate) fn definition(name: &str, description: &str, parameters: Value) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: description.to_string(),
        parameters,
    }
}

/// Parse a tool's JSON arguments into its typed args.
pub(crate) fn parse_args<T: DeserializeOwned>(args: &Value, tool: &str) -> Result<T> {
    serde_json::from_value(args.clone()).with_context(|| format!("parsing {tool} arguments"))
}

/// [`parse_args`], but a `null` argument (some models send it for an all-optional
/// schema) is the default args.
pub(crate) fn parse_args_or_default<T: DeserializeOwned + Default>(
    args: &Value,
    tool: &str,
) -> Result<T> {
    if args.is_null() {
        Ok(T::default())
    } else {
        parse_args(args, tool)
    }
}

/// Send `action` to the device. `None` sink → "no device/display is connected right
/// now"; a closed sink → "the device/display disconnected before `what`". Timer and
/// personality actions target the device; every screen action targets the display.
pub(crate) fn emit(actions: Option<&ActionSink>, action: DeviceAction, what: &str) -> Result<()> {
    let target = match action {
        DeviceAction::StartTimer { .. }
        | DeviceAction::CancelTimer { .. }
        | DeviceAction::SetPersonality { .. } => "device",
        _ => "display",
    };
    let sink = actions.with_context(|| format!("no {target} is connected right now"))?;
    sink.send(action)
        .map_err(|_| anyhow::anyhow!("the {target} disconnected before {what}"))
}

/// A `close_*` tool: no arguments, emits one dismiss action, replies with a fixed line.
struct CloseTool {
    name: &'static str,
    description: &'static str,
    action: DeviceAction,
    /// What closing is, for the "disconnected before …" error (e.g. "the recipe").
    subject: &'static str,
    reply: &'static str,
}

/// A `close_*` tool named `name` that emits `action` and replies `reply`.
pub(crate) fn close_tool(
    name: &'static str,
    description: &'static str,
    action: DeviceAction,
    subject: &'static str,
    reply: &'static str,
) -> Arc<dyn VoiceTool> {
    Arc::new(CloseTool {
        name,
        description,
        action,
        subject,
        reply,
    })
}

#[async_trait]
impl VoiceTool for CloseTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn definition(&self) -> ToolDefinition {
        definition(
            self.name,
            self.description,
            json!({ "type": "object", "properties": {} }),
        )
    }

    async fn invoke(&self, _args: &Value, ctx: &ToolCtx<'_>) -> Result<String> {
        emit(
            ctx.actions,
            self.action.clone(),
            &format!("{} could close", self.subject),
        )?;
        Ok(self.reply.to_string())
    }
}

/// The order tools' guidance paragraphs appear in the preamble. This predates the
/// registry (it is not the advertised order) and is kept so the preamble the model
/// sees is unchanged; a tool not listed here contributes its guidance after these,
/// in registry order, so new tools need no entry.
const GUIDANCE_ORDER: &[&str] = &[
    search::INTERNET_SEARCH,
    device::SET_TIMER,
    device::SET_PERSONALITY,
    device::ADJUST_FONT,
    services::CALENDAR_LOOKUP,
    services::DIRECTIONS_LOOKUP,
    services::SPOTIFY_CONTROL,
    services::RADIO_CONTROL,
    display::MUSIC_SCREEN,
    display::RECIPE_LOOKUP,
    display::WEATHER_LOOKUP,
    display::PLACES_LOOKUP,
    services::SHOPPING_LIST_ADD,
    services::SEND_PHONE_MESSAGE,
];

/// The tools available to a turn: an insertion-ordered, name-keyed registry plus the
/// definitions and preamble guidance derived from it once at build time. Cheaply
/// shared behind an `Arc`.
pub struct Tools {
    tools: Vec<Arc<dyn VoiceTool>>,
    by_name: HashMap<&'static str, usize>,
    definitions: Vec<ToolDefinition>,
    guidance: String,
}

impl Tools {
    /// Build the tool set `deps` describes. The device tools are always present;
    /// each integration is added only when its provider is configured.
    pub fn build(deps: ToolDeps) -> Self {
        let mut tools = device::tools();
        let search_key = crate::util::non_empty(deps.search_api_key.as_deref());
        if deps.web_search {
            tools.push(search::internet_search(search::build_search_provider(
                &deps.search_provider,
                search_key,
            )));
        }
        if let Some(source) = deps.calendar {
            tools.push(services::calendar_lookup(source));
        }
        if let Some((provider, imperial)) = deps.directions {
            tools.push(services::directions_lookup(
                provider,
                deps.home_location.clone(),
                imperial,
            ));
        }
        if let Some(controller) = deps.spotify.clone() {
            tools.push(services::spotify_control(controller));
        }
        if let Some(radio) = deps.radio {
            tools.push(services::radio_control(radio, deps.spotify));
        }
        // The on-screen music player is voice-driven wherever Spotify or radio is.
        if tools.iter().any(|t| {
            matches!(
                t.name(),
                services::SPOTIFY_CONTROL | services::RADIO_CONTROL
            )
        }) {
            tools.push(display::music_screen());
        }
        if let Some(controller) = deps.grocery {
            tools.push(services::shopping_list_add(controller));
        }
        if let Some(messenger) = deps.appsaid {
            tools.push(services::send_phone_message(messenger));
        }
        // The recipe tool finds its source page through Tavily, so it rides the same
        // key as web search (regardless of the web_search toggle); no Tavily key →
        // not advertised.
        if let Some(provider) = crate::recipe::from_search(&deps.search_provider, search_key) {
            tools.extend(display::recipe_tools(provider));
        }
        if let Some(provider) = deps.weather {
            tools.extend(display::weather_tools(
                provider,
                deps.home_location.clone(),
                deps.weather_imperial,
            ));
        }
        if let Some(provider) = deps.places {
            tools.extend(display::places_tools(provider, deps.home_location));
        }
        Self::from_tools(tools)
    }

    /// A registry of exactly `tools`, in order. A later tool with a duplicate name
    /// replaces nothing — the first registration wins (and is the one advertised).
    pub(crate) fn from_tools(tools: Vec<Arc<dyn VoiceTool>>) -> Self {
        let mut by_name = HashMap::new();
        let mut kept: Vec<Arc<dyn VoiceTool>> = Vec::with_capacity(tools.len());
        for tool in tools {
            if by_name.contains_key(tool.name()) {
                log::warn!("duplicate tool `{}` ignored", tool.name());
                continue;
            }
            by_name.insert(tool.name(), kept.len());
            kept.push(tool);
        }
        let definitions = kept.iter().map(|t| t.definition()).collect();
        let guidance = Self::assemble_guidance(&kept);
        Self {
            tools: kept,
            by_name,
            definitions,
            guidance,
        }
    }

    fn assemble_guidance(tools: &[Arc<dyn VoiceTool>]) -> String {
        let rank = |name: &str| {
            GUIDANCE_ORDER
                .iter()
                .position(|n| *n == name)
                .unwrap_or(GUIDANCE_ORDER.len())
        };
        let mut ordered: Vec<(usize, usize, &'static str)> = tools
            .iter()
            .enumerate()
            .filter_map(|(i, t)| t.guidance().map(|g| (rank(t.name()), i, g)))
            .collect();
        ordered.sort_by_key(|(rank, i, _)| (*rank, *i));
        ordered
            .into_iter()
            .map(|(_, _, g)| g)
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The definitions advertised to the model, in registry order.
    pub(crate) fn definitions(&self) -> &[ToolDefinition] {
        &self.definitions
    }

    /// The system preamble for a turn: the base prompt plus the usage guidance for
    /// the advertised tools (so the model actually calls them instead of claiming it
    /// can't set timers or check the weather).
    pub(crate) fn preamble(&self, system_prompt: &str) -> String {
        if self.guidance.is_empty() {
            system_prompt.to_string()
        } else {
            format!("{system_prompt}\n\n{}", self.guidance)
        }
    }

    /// Execute a model-requested tool call by name. `actions` is the per-turn
    /// device-action sink (`None` makes action tools report that no device is
    /// attached).
    pub(crate) async fn dispatch(
        &self,
        name: &str,
        args: &Value,
        actions: Option<&ActionSink>,
    ) -> Result<String> {
        let Some(&i) = self.by_name.get(name) else {
            anyhow::bail!("model called unknown tool `{name}`");
        };
        self.tools[i].invoke(args, &ToolCtx { actions }).await
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    fn names(tools: &Tools) -> Vec<&str> {
        tools
            .definitions()
            .iter()
            .map(|d| d.name.as_str())
            .collect()
    }

    /// Everything the model is told about tools for one turn: each advertised
    /// definition (name, description, JSON schema) in order, plus the full system
    /// preamble (base prompt + per-tool guidance).
    fn advertised_snapshot(tools: &Tools) -> String {
        let definitions: Vec<Value> = tools
            .definitions()
            .iter()
            .map(|d| json!({ "name": d.name, "description": d.description, "parameters": d.parameters }))
            .collect();
        let snapshot = json!({
            "definitions": definitions,
            "preamble": tools.preamble("SYSTEM PROMPT"),
        });
        serde_json::to_string_pretty(&snapshot).unwrap() + "\n"
    }

    /// The model must see exactly the same tool definitions and guidance as before
    /// the tool-registry refactor. The snapshot file is the reference; regenerate it
    /// deliberately with `ANAMANTI_UPDATE_SNAPSHOTS=1` only when a change to what the
    /// model sees is intended.
    #[test]
    fn advertised_tools_and_guidance_match_the_snapshot() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/llm/testdata/advertised_tools.json"
        );
        let actual = advertised_snapshot(&fully_configured_tools());
        if std::env::var_os("ANAMANTI_UPDATE_SNAPSHOTS").is_some() {
            std::fs::create_dir_all(std::path::Path::new(path).parent().unwrap()).unwrap();
            std::fs::write(path, &actual).unwrap();
        }
        let expected = std::fs::read_to_string(path).expect("snapshot file");
        assert_eq!(
            actual, expected,
            "advertised tools/guidance changed; see {path}"
        );
    }

    #[test]
    fn default_deps_advertise_only_the_device_tools() {
        let tools = Tools::build(ToolDeps::default());
        assert_eq!(
            names(&tools),
            vec![
                "set_timer",
                "cancel_timer",
                "set_personality",
                "adjust_font"
            ]
        );
        let guidance = tools.preamble("");
        assert!(guidance.contains("adjust_font"));
        // The font guidance reflects the "only when a surface is on screen" gate.
        assert!(guidance.contains("ONLY when"));
        assert!(!guidance.contains("internet_search"));
    }

    #[test]
    fn each_integration_is_advertised_only_when_configured() {
        let full = names(&fully_configured_tools()).len();
        let base = names(&Tools::build(ToolDeps::default())).len();
        // Every integration adds at least one tool.
        assert!(full > base);
        // web_search off → no internet_search, even with a key (recipe still rides it).
        let mut deps = fully_configured_deps();
        deps.web_search = false;
        let tools = Tools::build(deps);
        assert!(!names(&tools).contains(&"internet_search"));
        assert!(names(&tools).contains(&"recipe_lookup"));
        // DuckDuckGo (no key) → no recipe tool.
        let mut deps = fully_configured_deps();
        deps.search_provider = "duckduckgo".into();
        deps.search_api_key = None;
        let tools = Tools::build(deps);
        assert!(names(&tools).contains(&"internet_search"));
        assert!(!names(&tools).contains(&"recipe_lookup"));
        assert!(!names(&tools).contains(&"close_recipe"));
    }

    #[tokio::test]
    async fn dispatch_routes_by_name_and_rejects_unknown_tools() {
        let tools = fully_configured_tools();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let out = tools
            .dispatch("close_weather", &Value::Null, Some(&tx))
            .await
            .unwrap();
        assert_eq!(out, "Okay, closing the weather.");
        assert_eq!(rx.try_recv().unwrap(), DeviceAction::DismissWeather);
        let err = tools
            .dispatch("teleport", &json!({}), Some(&tx))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "model called unknown tool `teleport`");
    }

    #[tokio::test]
    async fn close_tools_report_a_missing_or_closed_display() {
        let tools = fully_configured_tools();
        let err = tools
            .dispatch("close_recipe", &json!({}), None)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "no display is connected right now");
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        let err = tools
            .dispatch("close_places", &json!({}), Some(&tx))
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "the display disconnected before the place could close"
        );
    }

    #[test]
    fn duplicate_tool_names_keep_the_first_registration() {
        let a = close_tool("close_x", "first", DeviceAction::DismissPlace, "x", "a");
        let b = close_tool("close_x", "second", DeviceAction::DismissPlace, "x", "b");
        let tools = Tools::from_tools(vec![a, b]);
        assert_eq!(tools.definitions().len(), 1);
        assert_eq!(tools.definitions()[0].description, "first");
    }
}
