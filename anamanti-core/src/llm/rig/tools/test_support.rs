//! Offline test doubles for every tool provider, plus a fully-configured tool set.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;

use super::search::SearchProvider;
use super::{ToolDeps, Tools};
use crate::appsaid::{OutgoingMessage, PhoneMessenger};
use crate::cadora::{GroceryCommand, GroceryController};
use crate::calendar::CalendarSource;
use crate::directions::{DirectionsProvider, HomeAddress, LiveHomeLocation, TravelMode};
use crate::music::{SpotifyCommand, SpotifyController};

/// A `LiveHomeLocation` seeded from a single City string (full == City here).
pub(crate) fn home(city: &str) -> LiveHomeLocation {
    LiveHomeLocation::new(HomeAddress {
        city: Some(city.to_string()),
        ..Default::default()
    })
}

/// A canned search provider.
pub(crate) struct StaticSearch(pub(crate) &'static str);

#[async_trait]
impl SearchProvider for StaticSearch {
    async fn search(&self, _query: &str, _max: u8) -> Result<String> {
        Ok(self.0.to_string())
    }
}

/// A canned calendar source named "Work".
pub(crate) struct StaticCalendar(pub(crate) Vec<crate::calendar::CalEvent>);

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

/// A canned directions provider that records the origin it was called with.
#[derive(Default)]
pub(crate) struct StaticDirections {
    pub(crate) seen_origin: Mutex<Option<String>>,
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

/// A canned Spotify controller that records the last command.
#[derive(Default)]
pub(crate) struct StaticSpotify {
    pub(crate) last: Mutex<Option<SpotifyCommand>>,
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

/// A canned grocery controller that records the last command.
#[derive(Default)]
pub(crate) struct StaticGrocery {
    pub(crate) last: Mutex<Option<GroceryCommand>>,
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

/// A canned phone messenger that records the last message.
#[derive(Default)]
pub(crate) struct StaticPhone {
    pub(crate) last: Mutex<Option<OutgoingMessage>>,
}

#[async_trait]
impl PhoneMessenger for StaticPhone {
    async fn send(&self, msg: OutgoingMessage) -> Result<String> {
        let who = msg.recipient.clone().unwrap_or_else(|| "you".to_string());
        *self.last.lock().unwrap() = Some(msg);
        Ok(format!("Sent to {who}."))
    }
}

/// A canned weather provider: a fixed report with one daily forecast.
pub(crate) struct StaticWeather;

#[async_trait]
impl crate::weather::WeatherProvider for StaticWeather {
    async fn fetch(
        &self,
        location: &str,
        imperial: bool,
        _when: crate::weather::ForecastWhen,
    ) -> Result<crate::weather::WeatherReport> {
        Ok(crate::weather::WeatherReport {
            location_label: location.to_string(),
            units: if imperial { "imperial" } else { "metric" }.to_string(),
            daily: vec![crate::weather::DailyForecast {
                weekday: "Mon".into(),
                high: 80,
                low: 60,
                ..Default::default()
            }],
            ..Default::default()
        })
    }
}

/// A canned places provider (never matches anything).
pub(crate) struct StaticPlaces;

#[async_trait]
impl crate::places::PlacesProvider for StaticPlaces {
    async fn search(
        &self,
        _query: &str,
        _bias: Option<&str>,
    ) -> Result<Vec<crate::places::PlaceCandidate>> {
        Ok(Vec::new())
    }
    async fn details(&self, _place_id: &str) -> Result<crate::places::PlaceReport> {
        anyhow::bail!("not used")
    }
}

/// Every integration configured with offline doubles (Tavily web search + recipe,
/// calendar, directions, Spotify, radio, shopping list, phone, weather, places, Home
/// Assistant), home in
/// Austin. Building it does no I/O.
pub(crate) fn fully_configured_deps() -> ToolDeps {
    ToolDeps {
        web_search: true,
        search_provider: "tavily".into(),
        search_api_key: Some("test-key".into()),
        home_location: home("Austin, TX"),
        calendar: Some(Arc::new(StaticCalendar(Vec::new()))),
        directions: Some((Arc::new(StaticDirections::default()), true)),
        spotify: Some(Arc::new(StaticSpotify::default())),
        grocery: Some(Arc::new(StaticGrocery::default())),
        appsaid: Some(Arc::new(StaticPhone::default())),
        weather: Some(Arc::new(StaticWeather)),
        weather_imperial: true,
        places: Some(Arc::new(StaticPlaces)),
        radio: Some(static_radio()),
        home_assistant: Some(crate::home_assistant::tests::hub_with(
            Arc::new(crate::home_assistant::tests::FakeApi {
                catalog: crate::home_assistant::catalog::tests::house(),
                ..Default::default()
            }),
            None,
        )),
        camera: Some(crate::camera::tests::hub()),
        music_display: Some(Arc::new(crate::music::MusicDisplayRouter::new())),
    }
}

/// A no-op radio output (mpv + snapserver double): every call succeeds silently.
pub(crate) struct StaticRadioOutput;

#[async_trait::async_trait]
impl crate::music::radio::RadioOutput for StaticRadioOutput {
    async fn load(&self, _url: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn is_idle(&self) -> anyhow::Result<bool> {
        Ok(false)
    }
    async fn icy_title(&self) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    async fn volume(&self) -> anyhow::Result<u8> {
        Ok(100)
    }
    async fn set_volume(&self, _percent: u8) -> anyhow::Result<()> {
        Ok(())
    }
    async fn route(&self, _stream_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

/// A radio player over [`StaticRadioOutput`] with one station (CJDV "107.5 Dave Rocks").
pub(crate) fn static_radio() -> Arc<crate::music::RadioPlayer> {
    let radio = Arc::new(crate::music::RadioPlayer::new(Arc::new(StaticRadioOutput)));
    radio.set_catalog(vec![crate::music::RadioStation {
        id: "CJDV".into(),
        name: "107.5 Dave Rocks".into(),
        url: "https://corus.leanstream.co/CJDVFM-MP3".into(),
        logo_url: String::new(),
        aliases: vec!["Dave".into()],
    }]);
    radio
}

/// [`Tools::build`] over [`fully_configured_deps`].
pub(crate) fn fully_configured_tools() -> Tools {
    Tools::build(fully_configured_deps())
}

/// The device tools plus `internet_search` over a canned [`StaticSearch`] result.
pub(crate) fn tools_with_search(result: &'static str) -> Tools {
    let mut tools = super::device::tools();
    tools.push(super::search::internet_search(Arc::new(StaticSearch(
        result,
    ))));
    Tools::from_tools(tools)
}
