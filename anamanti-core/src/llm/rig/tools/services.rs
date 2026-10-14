//! Service tools that answer from (or act on) an external service through an
//! injected provider: `calendar_lookup`, `directions_lookup`, `spotify_control`,
//! `shopping_list_add` and `send_phone_message`. None of them drives the display.
//!
//! A failure past argument parsing is prefixed with the tool's "… failed" label
//! (e.g. "calendar lookup failed: …") before it reaches the model.

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Local;
use rig_core::completion::ToolDefinition;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{definition, parse_args, parse_args_or_default, ToolCtx, VoiceTool};
use crate::appsaid::{OutgoingMessage, PhoneMessenger};
use crate::cadora::{GroceryCommand, GroceryController};
use crate::calendar::CalendarSource;
use crate::directions::{DirectionsProvider, LiveHomeLocation, TravelMode};
use crate::music::{SearchKind, SpotifyCommand, SpotifyController};

/// Tool name: read the user's connected calendars.
pub(super) const CALENDAR_LOOKUP: &str = "calendar_lookup";
/// Tool name: distance / travel time / live traffic between two places.
pub(super) const DIRECTIONS_LOOKUP: &str = "directions_lookup";
/// Tool name: house-wide Spotify playback control.
pub(super) const SPOTIFY_CONTROL: &str = "spotify_control";
/// Tool name: add an item to the shared shopping list.
pub(super) const SHOPPING_LIST_ADD: &str = "shopping_list_add";
/// Tool name: push a message to a family member's phone.
pub(super) const SEND_PHONE_MESSAGE: &str = "send_phone_message";

/// Default number of events folded into a `calendar_lookup` result.
const DEFAULT_CALENDAR_RESULTS: u8 = 10;

// ---------------------------------------------------------------------------
// Calendar (read-only web iCalendar subscriptions)
// ---------------------------------------------------------------------------

/// The `calendar_lookup` tool over `source`.
pub(super) fn calendar_lookup(source: Arc<dyn CalendarSource>) -> Arc<dyn VoiceTool> {
    Arc::new(CalendarLookup { source })
}

/// Typed arguments for `calendar_lookup`.
#[derive(Debug, Default, Deserialize)]
struct CalendarArgs {
    /// Which window to look at: `today`, `tomorrow`, `this_week`, `next_7_days`,
    /// `next_14_days`, `next_30_days`/`next_month`, `this_month`, `on:YYYY-MM-DD`, or
    /// `range:YYYY-MM-DD..YYYY-MM-DD`. Defaults to `next_7_days`.
    #[serde(default)]
    when: Option<String>,
    /// Optional person name to filter by (fuzzy match on attendees/organizer/title).
    #[serde(default)]
    person: Option<String>,
    /// Optional free-text filter over event title/description/location.
    #[serde(default)]
    query: Option<String>,
    /// Maximum events to return (defaults to [`DEFAULT_CALENDAR_RESULTS`], clamped
    /// to 1..=50).
    #[serde(default)]
    max_results: Option<u8>,
}

struct CalendarLookup {
    source: Arc<dyn CalendarSource>,
}

impl CalendarLookup {
    async fn run(&self, args: CalendarArgs) -> Result<String> {
        let (window, label) = crate::calendar::resolve_window(args.when.as_deref(), Local::now())
            .map_err(anyhow::Error::msg)?;
        let mut events = self.source.events(window).await?;
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

#[async_trait]
impl VoiceTool for CalendarLookup {
    fn name(&self) -> &'static str {
        CALENDAR_LOOKUP
    }

    fn definition(&self) -> ToolDefinition {
        let names = self.source.calendar_names().join(", ");
        definition(
            CALENDAR_LOOKUP,
            &format!(
                "Look up events from the user's connected calendars ({names}). Use this for \
                 ANY question about the user's schedule, meetings, appointments, plans, or \
                 availability — what is coming up, what is on a particular day, or when they \
                 are seeing a specific person. Returns a short text list of matching events."
            ),
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
            }),
        )
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(
            "You have a `calendar_lookup` tool that reads the user's real connected calendars. \
             Whenever the user asks about their schedule, meetings, appointments, plans, \
             availability, or when they are seeing a particular person, you MUST call \
             `calendar_lookup` and answer from its results. Never guess at their schedule or claim \
             you cannot access their calendar — use the tool.",
        )
    }

    async fn invoke(&self, args: &Value, _ctx: &ToolCtx<'_>) -> Result<String> {
        let args: CalendarArgs = parse_args_or_default(args, CALENDAR_LOOKUP)?;
        self.run(args).await.context("calendar lookup failed")
    }
}

// ---------------------------------------------------------------------------
// Directions (distance / travel time / live traffic)
// ---------------------------------------------------------------------------

/// The `directions_lookup` tool. The default origin is read from `home_location` at
/// call time, so a household edit takes effect without a rebuild.
pub(super) fn directions_lookup(
    provider: Arc<dyn DirectionsProvider>,
    home_location: LiveHomeLocation,
    imperial: bool,
) -> Arc<dyn VoiceTool> {
    Arc::new(DirectionsLookup {
        provider,
        home_location,
        imperial,
    })
}

/// Typed arguments for `directions_lookup`.
#[derive(Debug, Deserialize)]
struct DirectionsArgs {
    /// Where to start from. Omit to use the device's home location.
    #[serde(default)]
    origin: Option<String>,
    /// Where to go (required).
    destination: String,
    /// How to travel: `driving` (default, live-traffic), `walking`, or `cycling`.
    #[serde(default)]
    mode: Option<String>,
}

struct DirectionsLookup {
    provider: Arc<dyn DirectionsProvider>,
    home_location: LiveHomeLocation,
    imperial: bool,
}

impl DirectionsLookup {
    async fn run(&self, args: DirectionsArgs) -> Result<String> {
        let origin = args
            .origin
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| self.home_location.full())
            .context("no starting point given and no home location is configured")?;
        let destination = args.destination.trim();
        if destination.is_empty() {
            anyhow::bail!("no destination given");
        }
        let mode = TravelMode::from_arg(args.mode.as_deref());
        let directions = self.provider.directions(&origin, destination, mode).await?;
        Ok(crate::directions::render_directions(
            &directions,
            self.imperial,
        ))
    }
}

#[async_trait]
impl VoiceTool for DirectionsLookup {
    fn name(&self) -> &'static str {
        DIRECTIONS_LOOKUP
    }

    fn definition(&self) -> ToolDefinition {
        let home = self
            .home_location
            .full()
            .map(|h| format!(" The origin defaults to the device's home ({h}) when omitted."))
            .unwrap_or_default();
        definition(
            DIRECTIONS_LOOKUP,
            &format!(
                "Get the real driving distance, travel time, and live traffic between two places. \
                 Use this for ANY question about how to get somewhere, how far away it is, how long \
                 it takes to drive/walk/bike there, or current traffic conditions.{home} Returns a \
                 short spoken summary."
            ),
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
            }),
        )
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(
            "You have a `directions_lookup` tool that returns real distance, travel time, and \
             live traffic between two places. Whenever the user asks how to get somewhere, how \
             far away it is, how long it takes to drive/walk/bike there, or about current \
             traffic, you MUST call `directions_lookup` and answer from its result. If the user \
             names only a destination, omit the origin — it defaults to home. Never guess at \
             distances or travel times.",
        )
    }

    async fn invoke(&self, args: &Value, _ctx: &ToolCtx<'_>) -> Result<String> {
        let args: DirectionsArgs = parse_args(args, DIRECTIONS_LOOKUP)?;
        self.run(args).await.context("directions lookup failed")
    }
}

// ---------------------------------------------------------------------------
// Spotify (house-wide music control via the Web API)
// ---------------------------------------------------------------------------

/// The `spotify_control` tool. The controller targets the librespot Connect device;
/// audio never flows through here.
pub(super) fn spotify_control(controller: Arc<dyn SpotifyController>) -> Arc<dyn VoiceTool> {
    Arc::new(SpotifyControl { controller })
}

/// Typed arguments for `spotify_control`.
#[derive(Debug, Deserialize)]
struct SpotifyArgs {
    /// What to do: `play`, `pause`, `resume`, `next`, `previous`, `queue`, or
    /// `volume`.
    action: String,
    /// What to play/queue (song, artist, album, or playlist name). Only used by
    /// `play`/`queue`; omit for `play` to resume the current track.
    #[serde(default)]
    query: Option<String>,
    /// For `play`: what the query names — `track` (a specific song, default),
    /// `artist`, `album`, or `playlist` (a vibe / "some X" plays a whole context).
    #[serde(default)]
    kind: Option<String>,
    /// For `volume`: the target level, 0–100.
    #[serde(default)]
    volume_percent: Option<u8>,
}

impl SpotifyArgs {
    /// Map the typed args to a [`SpotifyCommand`]. Errors on an unknown action or a
    /// missing required field (returned to the model as the tool result).
    fn into_command(self) -> Result<SpotifyCommand> {
        let query = self
            .query
            .map(|q| q.trim().to_string())
            .filter(|q| !q.is_empty());
        match self.action.trim().to_lowercase().as_str() {
            "play" | "start" => Ok(SpotifyCommand::Play {
                query,
                kind: SearchKind::from_arg(self.kind.as_deref()),
            }),
            "queue" | "add" => Ok(SpotifyCommand::Queue {
                query: query.context("queue needs the name of a song to add")?,
            }),
            "pause" | "stop" => Ok(SpotifyCommand::Pause),
            "resume" | "unpause" => Ok(SpotifyCommand::Resume),
            "next" | "skip" | "forward" => Ok(SpotifyCommand::Next),
            "previous" | "prev" | "back" => Ok(SpotifyCommand::Previous),
            "volume" | "set_volume" => Ok(SpotifyCommand::SetVolume {
                percent: self
                    .volume_percent
                    .context("volume needs a level from 0 to 100")?,
            }),
            other => anyhow::bail!("unknown action `{other}`"),
        }
    }
}

struct SpotifyControl {
    controller: Arc<dyn SpotifyController>,
}

#[async_trait]
impl VoiceTool for SpotifyControl {
    fn name(&self) -> &'static str {
        SPOTIFY_CONTROL
    }

    fn definition(&self) -> ToolDefinition {
        definition(
            SPOTIFY_CONTROL,
            "Control house-wide Spotify music playback on the speakers. Use this for ANY request to \
             play, pause, resume, skip, go back, queue a song, or change the music volume. For \
             \"play some Radiohead\" or a genre/mood, use action=play with kind=artist or \
             kind=playlist; for a specific song use kind=track. Returns a short spoken confirmation.",
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
            }),
        )
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(
            "You can control the house music with the `spotify_control` tool. Whenever the user \
             asks to play, pause, resume, skip, go back to, queue, or change the volume of music \
             or a song/artist/album/playlist, you MUST call `spotify_control` — never say you \
             cannot play music. For \"play some <artist>\" or a mood/genre use action=play with \
             kind=artist or kind=playlist; for one named song use kind=track.",
        )
    }

    async fn invoke(&self, args: &Value, _ctx: &ToolCtx<'_>) -> Result<String> {
        let args: SpotifyArgs = parse_args(args, SPOTIFY_CONTROL)?;
        async { self.controller.command(args.into_command()?).await }
            .await
            .context("spotify control failed")
    }
}

// ---------------------------------------------------------------------------
// Shopping list (Cadora shared household via the voice API)
// ---------------------------------------------------------------------------

/// The `shopping_list_add` tool. The list lives on the Cadora server; this only
/// issues the add and relays the server's spoken confirmation.
pub(super) fn shopping_list_add(controller: Arc<dyn GroceryController>) -> Arc<dyn VoiceTool> {
    Arc::new(ShoppingListAdd { controller })
}

/// Typed arguments for `shopping_list_add`.
#[derive(Debug, Deserialize)]
struct GroceryArgs {
    /// The item to add (e.g. "milk", "paper towels"). One item per call.
    item: String,
    /// How many, when the user says a number (1–99). Omit for a plain add.
    #[serde(default)]
    quantity: Option<u32>,
}

struct ShoppingListAdd {
    controller: Arc<dyn GroceryController>,
}

#[async_trait]
impl VoiceTool for ShoppingListAdd {
    fn name(&self) -> &'static str {
        SHOPPING_LIST_ADD
    }

    fn definition(&self) -> ToolDefinition {
        definition(
            SHOPPING_LIST_ADD,
            "Add an item to the household's shared shopping/grocery list. Use this for ANY \
             request to add, put, or write something on the shopping list, grocery list, or \
             store list (\"add milk\", \"put eggs on the list\", \"we need paper towels\"). \
             Call it once per distinct item; include a quantity only when the user says one. \
             Returns a short spoken confirmation to relay.",
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
            }),
        )
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(
            "You can add items to the household's shared shopping list with the \
             `shopping_list_add` tool. Whenever the user asks to add, put, or write \
             something on the shopping/grocery/store list (e.g. \"add milk\", \"put eggs \
             on the shopping list\", \"we need paper towels\"), you MUST call \
             `shopping_list_add`, one call per distinct item, with the item name and a \
             quantity when they say one. Relay the tool's spoken confirmation.",
        )
    }

    async fn invoke(&self, args: &Value, _ctx: &ToolCtx<'_>) -> Result<String> {
        let args: GroceryArgs = parse_args(args, SHOPPING_LIST_ADD)?;
        self.controller
            .command(GroceryCommand::AddItem {
                item: args.item,
                quantity: args.quantity,
            })
            .await
            .context("shopping list add failed")
    }
}

// ---------------------------------------------------------------------------
// Phone message (push a message/map link to a family member via AppSaid)
// ---------------------------------------------------------------------------

/// The `send_phone_message` tool. Delivery + HTML sanitization happen on the AppSaid
/// worker; this only issues the send and relays a spoken confirmation.
pub(super) fn send_phone_message(messenger: Arc<dyn PhoneMessenger>) -> Arc<dyn VoiceTool> {
    Arc::new(SendPhoneMessage { messenger })
}

/// Typed arguments for `send_phone_message`.
#[derive(Debug, Deserialize)]
struct PhoneMessageArgs {
    /// Family-member name to send to (case-insensitive). Omit to use the configured
    /// default recipient.
    #[serde(default)]
    recipient: Option<String>,
    /// The message text to send.
    message: String,
    /// An explicit link to attach as a tappable button (optional).
    #[serde(default)]
    url: Option<String>,
    /// A place/address; when given and `url` is not, a Google Maps link is built and
    /// sent (optional).
    #[serde(default)]
    place: Option<String>,
}

struct SendPhoneMessage {
    messenger: Arc<dyn PhoneMessenger>,
}

#[async_trait]
impl VoiceTool for SendPhoneMessage {
    fn name(&self) -> &'static str {
        SEND_PHONE_MESSAGE
    }

    fn definition(&self) -> ToolDefinition {
        definition(
            SEND_PHONE_MESSAGE,
            "Send a message to a family member's phone (via the AppSaid push app). Use this \
             for ANY request to text, send, or share something — a note, a location, or a \
             map link — to a person or \"to my phone\" (\"text Mom the address\", \"send Dad \
             this restaurant\", \"share the directions with me\"). Put the note in `message`; \
             to attach a link pass an explicit `url`, or a place/address in `place` and a \
             Google Maps link is built for you. Call it once per recipient. Returns a short \
             spoken confirmation to relay.",
            json!({
                "type": "object",
                "properties": {
                    "recipient": {
                        "type": "string",
                        "description": "The family member to send to, e.g. \"Mom\" (case-insensitive). \
                            For \"text me …\" pass the name of the person speaking when you \
                            know who they are; if you don't, ask them instead of guessing. \
                            Omit only when the household has a single recipient."
                    },
                    "message": {
                        "type": "string",
                        "description": "The message text to send."
                    },
                    "url": {
                        "type": "string",
                        "description": "An explicit link to attach as a tappable button (optional)."
                    },
                    "place": {
                        "type": "string",
                        "description": "A place or address to turn into a Google Maps link when no \
                            explicit `url` is given (optional)."
                    }
                },
                "required": ["message"]
            }),
        )
    }

    fn guidance(&self) -> Option<&'static str> {
        Some(
            "You can push a message to a family member's phone with the \
             `send_phone_message` tool. Whenever the user asks to text, send, or share \
             something — a note, a location, or a map link — to a person or \"to my \
             phone\" (e.g. \"text Mom the address\", \"send Dad this restaurant\", \
             \"share the directions with me\"), you MUST call `send_phone_message`, one \
             call per recipient. Put the note in `message`; pass an explicit link in \
             `url`, or a place/address in `place` and it becomes a Google Maps link. \
             Relay the tool's spoken confirmation.",
        )
    }

    async fn invoke(&self, args: &Value, _ctx: &ToolCtx<'_>) -> Result<String> {
        let args: PhoneMessageArgs = parse_args(args, SEND_PHONE_MESSAGE)?;
        self.messenger
            .send(OutgoingMessage {
                recipient: args.recipient,
                message: args.message,
                url: args.url,
                place: args.place,
            })
            .await
            .context("send phone message failed")
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::directions::HomeAddress;

    fn spotify(action: &str, query: Option<&str>, kind: Option<&str>) -> Result<SpotifyCommand> {
        SpotifyArgs {
            action: action.into(),
            query: query.map(Into::into),
            kind: kind.map(Into::into),
            volume_percent: None,
        }
        .into_command()
    }

    #[test]
    fn spotify_args_map_to_commands() {
        assert_eq!(
            spotify("PLAY", Some("  radiohead "), Some("artist")).unwrap(),
            SpotifyCommand::Play {
                query: Some("radiohead".into()),
                kind: SearchKind::Artist
            }
        );
        // Resume: play with no query.
        assert_eq!(
            spotify("play", None, None).unwrap(),
            SpotifyCommand::Play {
                query: None,
                kind: SearchKind::Track
            }
        );
        assert_eq!(spotify("skip", None, None).unwrap(), SpotifyCommand::Next);
        // volume without a level, and unknown action, both error (surfaced to model).
        assert_eq!(
            spotify("volume", None, None).unwrap_err().to_string(),
            "volume needs a level from 0 to 100"
        );
        assert_eq!(
            spotify("teleport", None, None).unwrap_err().to_string(),
            "unknown action `teleport`"
        );
    }

    #[tokio::test]
    async fn spotify_errors_carry_the_tool_prefix() {
        let tool = spotify_control(Arc::new(StaticSpotify::default()));
        let err = tool
            .invoke(&json!({ "action": "teleport" }), &ToolCtx { actions: None })
            .await
            .unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "spotify control failed: unknown action `teleport`"
        );
    }

    /// The directions default origin reads the live household location, so an edit
    /// (config page Household tab → `LiveHomeLocation::set`) changes the origin the
    /// tool uses on the very next call — no backend rebuild.
    #[tokio::test]
    async fn directions_default_origin_tracks_live_home_location() {
        let provider = Arc::new(StaticDirections::default());
        let home = home("Austin, TX");
        let tool = directions_lookup(provider.clone(), home.clone(), true);
        let ctx = ToolCtx { actions: None };

        tool.invoke(&json!({ "destination": "the airport" }), &ctx)
            .await
            .unwrap();
        assert_eq!(
            provider.seen_origin.lock().unwrap().as_deref(),
            Some("Austin, TX")
        );

        // Edit the household location live — the next call uses the new origin.
        home.set(HomeAddress {
            city: Some("Boston, MA".to_string()),
            ..Default::default()
        });
        tool.invoke(&json!({ "destination": "the airport" }), &ctx)
            .await
            .unwrap();
        assert_eq!(
            provider.seen_origin.lock().unwrap().as_deref(),
            Some("Boston, MA")
        );

        // No origin and no home → a prefixed error for the model.
        let tool = directions_lookup(provider, LiveHomeLocation::default(), true);
        let err = tool
            .invoke(&json!({ "destination": "the airport" }), &ctx)
            .await
            .unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "directions lookup failed: no starting point given and no home location is configured"
        );
    }

    #[tokio::test]
    async fn calendar_accepts_null_args() {
        let tool = calendar_lookup(Arc::new(StaticCalendar(Vec::new())));
        let out = tool
            .invoke(&Value::Null, &ToolCtx { actions: None })
            .await
            .unwrap();
        assert!(!out.is_empty());
    }
}
