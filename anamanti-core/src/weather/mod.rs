//! Structured weather: a current-conditions + 7-day forecast, exposed to the LLM as
//! the `weather_lookup` tool (wired in `llm/rig.rs`) and rendered on the device two
//! ways — a full-screen weather screen (today's conditions with big imagery + a 7-day
//! row) when the user asks, and a small icon + temperature beside the idle clock kept
//! fresh by a periodic background push (see [`WeatherService`]).
//!
//! The data comes from one of two backends behind the [`WeatherProvider`] trait, chosen
//! by `weather.provider` (see [`from_config`]):
//! - **Visual Crossing** ([`VisualCrossingWeather`], the default) — its Timeline API
//!   resolves a place *and* returns current conditions + the 7-day forecast in one keyed
//!   request. Needs the `VISUALCROSSING_API_KEY` secret.
//! - **Open-Meteo** ([`OpenMeteoWeather`]) — a keyless, structured forecast API plus its
//!   free geocoding API to resolve the household `home_location` to coordinates. Also the
//!   automatic fallback when the Visual Crossing key is absent.
//!
//! Either way WMO weather codes ride through to the device, which maps them to bundled
//! icons (Visual Crossing's text icons are translated to WMO codes in [`vc_icon_to_wmo`]).
//!
//! The trait keeps the tool and the periodic push testable offline (fixture JSON, no
//! network), mirroring how [`crate::directions::DirectionsProvider`] backs the directions
//! tool. Temperatures
//! and probabilities are stored as whole integers so [`WeatherReport`] is `Eq` (it
//! rides inside [`crate::llm::DeviceAction`]) and marshals trivially to the device.

pub mod service;

pub use service::WeatherService;

use std::sync::Arc;
use std::time::Duration as StdDuration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

/// Current conditions plus a 7-day daily forecast for one place. All temperatures are
/// whole degrees in `units`; `precip_prob` is a whole percent. Plain integers/strings
/// so the type is `Eq` and JSON-marshals directly to the device.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WeatherReport {
    /// A speakable/displayable place label, e.g. "Austin, Texas".
    pub location_label: String,
    /// `"imperial"` (°F) or `"metric"` (°C).
    pub units: String,
    /// Conditions right now.
    pub current: CurrentConditions,
    /// The daily forecast, today first (7 entries when the API cooperates).
    pub daily: Vec<DailyForecast>,
}

/// Conditions right now, plus today's high/low for the ambient indicator.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentConditions {
    /// Temperature now, whole degrees.
    pub temp: i32,
    /// "Feels like" temperature, whole degrees.
    pub feels_like: i32,
    /// WMO weather code (0 clear … 95 thunderstorm); the device maps it to an icon.
    pub weather_code: i32,
    /// Whether it's daytime (picks a day/night icon variant).
    pub is_day: bool,
    /// Today's high, whole degrees.
    pub high: i32,
    /// Today's low, whole degrees.
    pub low: i32,
    /// A short plain-language description of the current code ("Partly cloudy").
    pub description: String,
}

/// One day of the forecast.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DailyForecast {
    /// ISO date, `YYYY-MM-DD`.
    pub date: String,
    /// Short weekday label for the row, e.g. "Thu".
    pub weekday: String,
    /// WMO weather code for the day.
    pub weather_code: i32,
    /// Daily high, whole degrees.
    pub high: i32,
    /// Daily low, whole degrees.
    pub low: i32,
    /// Max chance of precipitation, whole percent (0 when the API omits it).
    pub precip_prob: i32,
}

/// The wired weather capability for the `weather_lookup` tool: the forecast provider,
/// the live home location used when the user names no place, and whether to report in
/// imperial units. Mirrors [`crate::directions::DirectionsConfig`].
pub struct WeatherConfig {
    pub provider: Arc<dyn WeatherProvider>,
    pub home_location: crate::directions::LiveHomeLocation,
    pub imperial: bool,
}

/// A source of weather. Abstracted so the tool + periodic push are testable offline
/// and an alternate backend can be dropped in behind the same seam later.
#[async_trait]
pub trait WeatherProvider: Send + Sync {
    /// Resolve `location` and return current conditions + a 7-day forecast, with
    /// temperatures in imperial (°F) when `imperial` is true, else metric (°C).
    async fn fetch(&self, location: &str, imperial: bool) -> Result<WeatherReport>;
}

/// A [`WeatherProvider`] backed by the keyless Open-Meteo geocoding + forecast APIs.
pub struct OpenMeteoWeather {
    client: reqwest::Client,
    geo_base: String,
    forecast_base: String,
}

impl OpenMeteoWeather {
    pub fn new() -> Self {
        Self::with_base_urls(
            "https://geocoding-api.open-meteo.com",
            "https://api.open-meteo.com",
        )
    }

    /// Point the geocoding + forecast clients at specific API roots (for tests).
    pub fn with_base_urls(geo_base: impl Into<String>, forecast_base: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(StdDuration::from_secs(10))
            .user_agent("anamanti-core/0.1 (weather)")
            .build()
            .unwrap_or_default();
        Self {
            client,
            geo_base: geo_base.into().trim_end_matches('/').to_string(),
            forecast_base: forecast_base.into().trim_end_matches('/').to_string(),
        }
    }

    /// Forward-geocode a free-text place into `(latitude, longitude, label)` using the
    /// Open-Meteo geocoding API (which matches on a place *name*, not a postal address).
    /// Tries the whole string first, then falls back to its locality-ish comma segments
    /// (see [`geocode_candidates`]) so a full household address like
    /// "183 Pine Valley Drive, Kitchener, Ontario, Canada, N2P2V8" still resolves to
    /// "Kitchener". Returns the first candidate that matches.
    async fn geocode(&self, query: &str) -> Result<(f64, f64, String)> {
        for candidate in geocode_candidates(query) {
            if let Some(hit) = self.geocode_one(&candidate).await? {
                return Ok(hit);
            }
        }
        anyhow::bail!("no location found for \"{query}\"")
    }

    /// One geocoding query. `Ok(None)` when the API returns no results (so the caller
    /// can try the next candidate); `Err` only on a transport/parse failure.
    async fn geocode_one(&self, query: &str) -> Result<Option<(f64, f64, String)>> {
        let value: Value = self
            .client
            .get(format!("{}/v1/search", self.geo_base))
            .query(&[("name", query), ("count", "1"), ("format", "json")])
            .send()
            .await
            .context("GET Open-Meteo geocoding")?
            .error_for_status()
            .context("Open-Meteo geocoding returned an error status")?
            .json()
            .await
            .context("parsing Open-Meteo geocoding JSON")?;

        let Some(first) = value
            .get("results")
            .and_then(Value::as_array)
            .and_then(|r| r.first())
        else {
            return Ok(None);
        };
        let lat = first
            .get("latitude")
            .and_then(Value::as_f64)
            .context("geocoding result had no latitude")?;
        let lon = first
            .get("longitude")
            .and_then(Value::as_f64)
            .context("geocoding result had no longitude")?;
        Ok(Some((lat, lon, geocode_label(first, query))))
    }
}

/// Build the ordered list of geocoding queries to try for a free-text location: the
/// whole string first, then each comma-separated segment that looks like a place name —
/// i.e. contains a letter and **no digits**, so a street number ("183 Pine Valley Drive")
/// or a postal code ("N2P2V8") is skipped while the city / region / country
/// ("Kitchener", "Ontario", "Canada") are tried in order. De-duplicated; the full
/// string is always first so a clean "City, Region" still resolves exactly.
fn geocode_candidates(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    let full = query.trim().to_string();
    if !full.is_empty() {
        out.push(full.clone());
    }
    if full.contains(',') {
        for seg in query.split(',') {
            let seg = seg.trim();
            let usable =
                seg.chars().any(|c| c.is_alphabetic()) && !seg.chars().any(|c| c.is_ascii_digit());
            if usable && !out.iter().any(|q| q == seg) {
                out.push(seg.to_string());
            }
        }
    }
    out
}

impl Default for OpenMeteoWeather {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WeatherProvider for OpenMeteoWeather {
    async fn fetch(&self, location: &str, imperial: bool) -> Result<WeatherReport> {
        let location = location.trim();
        anyhow::ensure!(!location.is_empty(), "no home location is set");
        let (lat, lon, label) = self.geocode(location).await?;
        let temp_unit = if imperial { "fahrenheit" } else { "celsius" };
        let value: Value = self
            .client
            .get(format!("{}/v1/forecast", self.forecast_base))
            .query(&[
                ("latitude", format!("{lat:.4}").as_str()),
                ("longitude", format!("{lon:.4}").as_str()),
                (
                    "current",
                    "temperature_2m,apparent_temperature,weather_code,is_day",
                ),
                (
                    "daily",
                    "weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max",
                ),
                ("temperature_unit", temp_unit),
                ("timezone", "auto"),
                ("forecast_days", "7"),
            ])
            .send()
            .await
            .context("GET Open-Meteo forecast")?
            .error_for_status()
            .context("Open-Meteo forecast returned an error status")?
            .json()
            .await
            .context("parsing Open-Meteo forecast JSON")?;

        Ok(report_from_forecast(&value, &label, imperial))
    }
}

/// A [`WeatherProvider`] backed by the [Visual Crossing Timeline API]. Unlike Open-Meteo
/// this resolves a free-text (or full postal) location *and* returns current conditions +
/// the daily forecast in a single keyed request — no separate geocoding step. Visual
/// Crossing's text `icon`s are mapped back to WMO codes ([`vc_icon_to_wmo`]) so the device
/// renders the same bundled icon set regardless of which provider produced the report.
///
/// The API key is a **secret** (seeded from `VISUALCROSSING_API_KEY`); it is passed in at
/// construction rather than read from the environment here.
///
/// [Visual Crossing Timeline API]: https://www.visualcrossing.com/resources/documentation/weather-api/timeline-weather-api/
pub struct VisualCrossingWeather {
    client: reqwest::Client,
    base: String,
    api_key: String,
}

impl VisualCrossingWeather {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(
            "https://weather.visualcrossing.com/VisualCrossingWebServices/rest/services/timeline",
            api_key,
        )
    }

    /// Point the Timeline client at a specific API root (for tests).
    pub fn with_base_url(base: impl Into<String>, api_key: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(StdDuration::from_secs(10))
            .user_agent("anamanti-core/0.1 (weather)")
            .build()
            .unwrap_or_default();
        Self {
            client,
            base: base.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
        }
    }

    /// Build the Timeline request URL: `{base}/{location}` (the location is a URL-encoded
    /// path segment) with the unit group, key, and the `current,days` include list.
    fn request_url(&self, location: &str, imperial: bool) -> Result<Url> {
        let mut url = Url::parse(&self.base).context("parsing Visual Crossing base URL")?;
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("Visual Crossing base URL cannot be a base"))?
            .push(location);
        url.query_pairs_mut()
            .append_pair("unitGroup", if imperial { "us" } else { "metric" })
            .append_pair("key", &self.api_key)
            .append_pair("include", "current,days")
            .append_pair("iconSet", "icons2")
            .append_pair(
                "elements",
                "datetime,tempmax,tempmin,temp,feelslike,precipprob,icon,conditions",
            );
        Ok(url)
    }
}

#[async_trait]
impl WeatherProvider for VisualCrossingWeather {
    async fn fetch(&self, location: &str, imperial: bool) -> Result<WeatherReport> {
        let location = location.trim();
        anyhow::ensure!(!location.is_empty(), "no home location is set");
        let url = self.request_url(location, imperial)?;
        let value: Value = self
            .client
            .get(url)
            .send()
            .await
            .context("GET Visual Crossing timeline")?
            .error_for_status()
            .context("Visual Crossing returned an error status")?
            .json()
            .await
            .context("parsing Visual Crossing timeline JSON")?;

        Ok(report_from_visualcrossing(&value, location, imperial))
    }
}

/// Map a Visual Crossing timeline JSON body into a [`WeatherReport`]. Missing fields
/// degrade gracefully (zeros/empties, WMO code 0) rather than failing the whole lookup —
/// matching [`report_from_forecast`]'s contract for Open-Meteo.
fn report_from_visualcrossing(value: &Value, query: &str, imperial: bool) -> WeatherReport {
    let label = value
        .get("resolvedAddress")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(query)
        .to_string();

    let daily = vc_daily_from(value.get("days"));
    let (today_high, today_low) = daily.first().map(|d| (d.high, d.low)).unwrap_or((0, 0));

    let cc = value.get("currentConditions");
    let icon = cc
        .and_then(|c| c.get("icon"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let code = vc_icon_to_wmo(icon);
    let current = CurrentConditions {
        temp: round_temp(cc.and_then(|c| c.get("temp"))),
        feels_like: round_temp(cc.and_then(|c| c.get("feelslike"))),
        weather_code: code,
        is_day: vc_icon_is_day(icon),
        high: today_high,
        low: today_low,
        description: weather_description(code).to_string(),
    };

    WeatherReport {
        location_label: label,
        units: if imperial { "imperial" } else { "metric" }.to_string(),
        current,
        daily,
    }
}

/// Build the daily forecast list from Visual Crossing's `days` array (one object per day,
/// unlike Open-Meteo's parallel arrays).
fn vc_daily_from(days: Option<&Value>) -> Vec<DailyForecast> {
    let Some(days) = days.and_then(Value::as_array) else {
        return Vec::new();
    };
    days.iter()
        .map(|day| {
            let date = day
                .get("datetime")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let icon = day.get("icon").and_then(Value::as_str).unwrap_or("");
            DailyForecast {
                weekday: weekday_label(&date),
                date,
                weather_code: vc_icon_to_wmo(icon),
                high: round_temp(day.get("tempmax")),
                low: round_temp(day.get("tempmin")),
                precip_prob: day
                    .get("precipprob")
                    .and_then(Value::as_f64)
                    .map(|p| p.round() as i32)
                    .unwrap_or(0),
            }
        })
        .collect()
}

/// Whether a Visual Crossing icon string denotes daytime. VC encodes day/night in the
/// icon suffix (`clear-day` / `clear-night`); icons with no suffix are treated as day.
fn vc_icon_is_day(icon: &str) -> bool {
    !icon.ends_with("-night")
}

/// Map a Visual Crossing `icon` string to the nearest WMO weather code, so the device's
/// WMO→icon table ([`weather_description`] + the Flutter `weather_icons.dart`) renders VC
/// reports identically to Open-Meteo ones. The full [icons2 set] is covered; anything
/// unrecognized falls back by keyword, then to `0` (clear).
///
/// [icons2 set]: https://www.visualcrossing.com/resources/documentation/weather-api/defining-icons-in-the-weather-api/
fn vc_icon_to_wmo(icon: &str) -> i32 {
    match icon {
        "clear-day" | "clear-night" => 0,
        "partly-cloudy-day" | "partly-cloudy-night" => 2,
        "cloudy" => 3,
        "fog" => 45,
        "wind" => 1,
        "rain" => 63,
        "showers-day" | "showers-night" => 80,
        "thunder-rain" | "thunder-showers-day" | "thunder-showers-night" => 95,
        "snow" => 73,
        "snow-showers-day" | "snow-showers-night" => 85,
        "sleet" => 66,
        "hail" => 96,
        "rain-snow" | "rain-snow-showers-day" | "rain-snow-showers-night" => 66,
        // Unknown icon: best-effort by keyword before giving up on "clear".
        other if other.contains("thunder") => 95,
        other if other.contains("snow") || other.contains("sleet") => 73,
        other if other.contains("rain") || other.contains("shower") => 63,
        other if other.contains("cloud") => 3,
        other if other.contains("fog") => 45,
        _ => 0,
    }
}

/// Build a readable place label from a geocoding result: `name`, plus `admin1`
/// (state/region) when it adds information. Falls back to the query.
fn geocode_label(result: &Value, query: &str) -> String {
    let name = result
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let admin1 = result
        .get("admin1")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    match (name, admin1) {
        (Some(n), Some(a)) if a != n => format!("{n}, {a}"),
        (Some(n), _) => n.to_string(),
        _ => query.to_string(),
    }
}

/// Map an Open-Meteo forecast JSON body into a [`WeatherReport`]. Missing fields
/// degrade gracefully to zeros/empties rather than failing the whole lookup.
fn report_from_forecast(value: &Value, label: &str, imperial: bool) -> WeatherReport {
    let current = value.get("current");
    let daily = value.get("daily");

    let daily_forecast = daily_from(daily);
    let (today_high, today_low) = daily_forecast
        .first()
        .map(|d| (d.high, d.low))
        .unwrap_or((0, 0));
    let code = current
        .and_then(|c| c.get("weather_code"))
        .and_then(Value::as_i64)
        .unwrap_or(0) as i32;

    let current = CurrentConditions {
        temp: round_temp(current.and_then(|c| c.get("temperature_2m"))),
        feels_like: round_temp(current.and_then(|c| c.get("apparent_temperature"))),
        weather_code: code,
        is_day: current
            .and_then(|c| c.get("is_day"))
            .and_then(Value::as_i64)
            .map(|d| d != 0)
            .unwrap_or(true),
        high: today_high,
        low: today_low,
        description: weather_description(code).to_string(),
    };

    WeatherReport {
        location_label: label.to_string(),
        units: if imperial { "imperial" } else { "metric" }.to_string(),
        current,
        daily: daily_forecast,
    }
}

/// Build the daily forecast list from the `daily` object's parallel arrays.
fn daily_from(daily: Option<&Value>) -> Vec<DailyForecast> {
    let Some(daily) = daily else {
        return Vec::new();
    };
    let dates = daily.get("time").and_then(Value::as_array);
    let codes = daily.get("weather_code").and_then(Value::as_array);
    let highs = daily.get("temperature_2m_max").and_then(Value::as_array);
    let lows = daily.get("temperature_2m_min").and_then(Value::as_array);
    let precip = daily
        .get("precipitation_probability_max")
        .and_then(Value::as_array);
    let Some(dates) = dates else {
        return Vec::new();
    };

    fn at_i(arr: Option<&Vec<Value>>, i: usize) -> Option<&Value> {
        arr.and_then(|a| a.get(i))
    }
    dates
        .iter()
        .enumerate()
        .map(|(i, date)| {
            let date = date.as_str().unwrap_or("").to_string();
            DailyForecast {
                weekday: weekday_label(&date),
                date,
                weather_code: at_i(codes, i).and_then(Value::as_i64).unwrap_or(0) as i32,
                high: round_temp(at_i(highs, i)),
                low: round_temp(at_i(lows, i)),
                precip_prob: at_i(precip, i).and_then(Value::as_i64).unwrap_or(0) as i32,
            }
        })
        .collect()
}

/// Round a JSON temperature value to whole degrees; absent/non-numeric → 0.
fn round_temp(v: Option<&Value>) -> i32 {
    v.and_then(Value::as_f64)
        .map(|t| t.round() as i32)
        .unwrap_or(0)
}

/// A short weekday label ("Mon", "Tue", …) for an ISO `YYYY-MM-DD` date, or "" when
/// unparseable.
fn weekday_label(date: &str) -> String {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|d| d.format("%a").to_string())
        .unwrap_or_default()
}

/// Plain-language description for a WMO weather code (for the spoken confirmation and
/// on-screen label). The device selects icons from the code directly.
pub fn weather_description(code: i32) -> &'static str {
    match code {
        0 => "clear",
        1 => "mainly clear",
        2 => "partly cloudy",
        3 => "overcast",
        45 | 48 => "foggy",
        51 | 53 | 55 => "drizzle",
        56 | 57 => "freezing drizzle",
        61 => "light rain",
        63 => "rain",
        65 => "heavy rain",
        66 | 67 => "freezing rain",
        71 => "light snow",
        73 => "snow",
        75 => "heavy snow",
        77 => "snow grains",
        80 | 81 => "rain showers",
        82 => "heavy rain showers",
        85 | 86 => "snow showers",
        95 => "thunderstorms",
        96 | 99 => "thunderstorms with hail",
        _ => "unknown conditions",
    }
}

/// Build the configured weather provider when weather is enabled, or `None` when it is
/// disabled. When enabled the result is always `Some` — the tool/push are still gated on
/// a home location being set (an empty location makes [`WeatherProvider::fetch`] fail
/// gracefully).
///
/// `provider` selects the backend (`weather.provider`):
/// - `visualcrossing` (default; also the empty/unset value) — the [`VisualCrossingWeather`]
///   Timeline API, using `api_key` (seeded from the `VISUALCROSSING_API_KEY` secret). If
///   that key is missing it falls back to keyless Open-Meteo so weather still works.
/// - `openmeteo` — the keyless [`OpenMeteoWeather`] backend.
/// - anything else — a warning, then the Open-Meteo fallback.
pub fn from_config(
    enabled: bool,
    provider: &str,
    api_key: Option<&str>,
) -> Option<Arc<dyn WeatherProvider>> {
    if !enabled {
        return None;
    }
    let key = api_key.map(str::trim).filter(|s| !s.is_empty());
    let provider = match provider.trim().to_lowercase().as_str() {
        "openmeteo" | "open-meteo" | "open_meteo" => {
            log::info!("weather enabled (Open-Meteo; keyless forecast + geocoding)");
            Arc::new(OpenMeteoWeather::new()) as Arc<dyn WeatherProvider>
        }
        "" | "visualcrossing" | "visual-crossing" | "visual_crossing" => match key {
            Some(key) => {
                log::info!("weather enabled (Visual Crossing Timeline API)");
                Arc::new(VisualCrossingWeather::new(key)) as Arc<dyn WeatherProvider>
            }
            None => {
                log::warn!(
                    "weather.provider=visualcrossing but VISUALCROSSING_API_KEY is unset; \
                     falling back to keyless Open-Meteo"
                );
                Arc::new(OpenMeteoWeather::new()) as Arc<dyn WeatherProvider>
            }
        },
        other => {
            log::warn!(
                "weather.provider='{other}' is not supported (use 'visualcrossing' or \
                 'openmeteo'); falling back to keyless Open-Meteo"
            );
            Arc::new(OpenMeteoWeather::new()) as Arc<dyn WeatherProvider>
        }
    };
    Some(provider)
}

/// A short spoken confirmation for the model to relay once the forecast is on screen.
pub fn render_confirmation(report: &WeatherReport) -> String {
    let unit = if report.units == "imperial" { "F" } else { "C" };
    let mut out = format!(
        "It's {} degrees {unit} and {} right now",
        report.current.temp, report.current.description
    );
    if !report.location_label.is_empty() {
        out.push_str(&format!(" in {}", report.location_label));
    }
    out.push_str(&format!(
        ", with a high of {} and a low of {}. The forecast is on the screen.",
        report.current.high, report.current.low
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const GEO: &str = r#"{"results":[{"name":"Austin","latitude":30.2672,"longitude":-97.7431,"admin1":"Texas","country":"United States"}]}"#;
    const FORECAST: &str = r#"{
        "current":{"time":"2026-09-25T12:00","temperature_2m":72.4,"apparent_temperature":70.1,"weather_code":2,"is_day":1},
        "daily":{
            "time":["2026-09-25","2026-09-26","2026-09-27","2026-09-28","2026-09-29","2026-09-30","2026-10-01"],
            "weather_code":[2,3,61,80,0,1,95],
            "temperature_2m_max":[80.2,78.9,75.1,70.0,82.4,83.8,79.0],
            "temperature_2m_min":[60.1,59.4,58.0,55.6,61.2,62.0,60.5],
            "precipitation_probability_max":[10,20,80,60,0,5,90]
        }
    }"#;

    #[test]
    fn geocode_candidates_fall_back_to_locality_segments() {
        // A full household address: the street number and postal code (digit-bearing)
        // are skipped; the city/region/country are tried in order after the whole string.
        assert_eq!(
            geocode_candidates("183 Pine Valley Drive, Kitchener, Ontario, Canada, N2P2V8"),
            vec![
                "183 Pine Valley Drive, Kitchener, Ontario, Canada, N2P2V8".to_string(),
                "Kitchener".to_string(),
                "Ontario".to_string(),
                "Canada".to_string(),
            ]
        );
        // A clean "City, Region" still tries the exact string first.
        assert_eq!(
            geocode_candidates("Kitchener, Ontario"),
            vec![
                "Kitchener, Ontario".to_string(),
                "Kitchener".to_string(),
                "Ontario".to_string()
            ]
        );
        // A bare city has a single candidate.
        assert_eq!(geocode_candidates("Paris"), vec!["Paris".to_string()]);
    }

    #[test]
    fn weekday_labels() {
        assert_eq!(weekday_label("2026-09-25"), "Fri");
        assert_eq!(weekday_label("garbage"), "");
    }

    #[test]
    fn descriptions_cover_common_codes() {
        assert_eq!(weather_description(0), "clear");
        assert_eq!(weather_description(2), "partly cloudy");
        assert_eq!(weather_description(95), "thunderstorms");
        assert_eq!(weather_description(1234), "unknown conditions");
    }

    #[test]
    fn parses_forecast_body() {
        let value: Value = serde_json::from_str(FORECAST).unwrap();
        let report = report_from_forecast(&value, "Austin, Texas", true);
        assert_eq!(report.location_label, "Austin, Texas");
        assert_eq!(report.units, "imperial");
        assert_eq!(report.current.temp, 72);
        assert_eq!(report.current.feels_like, 70);
        assert_eq!(report.current.weather_code, 2);
        assert!(report.current.is_day);
        assert_eq!(report.current.description, "partly cloudy");
        // Current high/low mirror today's daily entry.
        assert_eq!(report.current.high, 80);
        assert_eq!(report.current.low, 60);
        assert_eq!(report.daily.len(), 7);
        assert_eq!(report.daily[0].weekday, "Fri");
        assert_eq!(report.daily[2].weather_code, 61);
        assert_eq!(report.daily[2].precip_prob, 80);
        assert_eq!(report.daily[6].high, 79);
    }

    #[test]
    fn metric_units_label() {
        let value: Value = serde_json::from_str(FORECAST).unwrap();
        let report = report_from_forecast(&value, "x", false);
        assert_eq!(report.units, "metric");
    }

    #[test]
    fn confirmation_mentions_temp_and_location() {
        let value: Value = serde_json::from_str(FORECAST).unwrap();
        let report = report_from_forecast(&value, "Austin, Texas", true);
        let c = render_confirmation(&report);
        assert!(c.contains("72"), "{c}");
        assert!(c.contains("partly cloudy"), "{c}");
        assert!(c.contains("Austin, Texas"), "{c}");
    }

    /// Serve a fixed sequence of raw HTTP JSON responses, one per inbound connection.
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
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                sock.write_all(resp.as_bytes()).await.unwrap();
                sock.flush().await.unwrap();
            }
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn geocodes_then_fetches_forecast() {
        let (url, server) = serve_sequence(vec![GEO.to_string(), FORECAST.to_string()]);
        // Both APIs share the same loopback server here (sequential responses).
        let provider = OpenMeteoWeather::with_base_urls(url.clone(), url);
        let report = provider.fetch("Austin", true).await.unwrap();
        assert_eq!(report.location_label, "Austin, Texas");
        assert_eq!(report.current.temp, 72);
        assert_eq!(report.daily.len(), 7);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn empty_location_is_a_graceful_error() {
        let provider = OpenMeteoWeather::new();
        assert!(provider.fetch("   ", true).await.is_err());
    }

    // ---- Visual Crossing --------------------------------------------------------------

    const VC_TIMELINE: &str = r#"{
        "resolvedAddress":"Austin, TX, United States",
        "currentConditions":{"temp":72.4,"feelslike":70.1,"icon":"partly-cloudy-night","conditions":"Partially cloudy"},
        "days":[
            {"datetime":"2026-09-25","tempmax":80.2,"tempmin":60.1,"precipprob":10,"icon":"partly-cloudy-day","conditions":"Partially cloudy"},
            {"datetime":"2026-09-26","tempmax":78.9,"tempmin":59.4,"precipprob":20,"icon":"cloudy"},
            {"datetime":"2026-09-27","tempmax":75.1,"tempmin":58.0,"precipprob":80,"icon":"rain"},
            {"datetime":"2026-09-28","tempmax":70.0,"tempmin":55.6,"precipprob":60,"icon":"showers-day"},
            {"datetime":"2026-09-29","tempmax":82.4,"tempmin":61.2,"precipprob":0,"icon":"clear-day"},
            {"datetime":"2026-09-30","tempmax":83.8,"tempmin":62.0,"precipprob":5,"icon":"wind"},
            {"datetime":"2026-10-01","tempmax":79.0,"tempmin":60.5,"precipprob":90,"icon":"thunder-showers-day"}
        ]
    }"#;

    #[test]
    fn vc_icon_mapping_covers_the_icon_set() {
        assert_eq!(vc_icon_to_wmo("clear-day"), 0);
        assert_eq!(vc_icon_to_wmo("partly-cloudy-night"), 2);
        assert_eq!(vc_icon_to_wmo("cloudy"), 3);
        assert_eq!(vc_icon_to_wmo("rain"), 63);
        assert_eq!(vc_icon_to_wmo("showers-day"), 80);
        assert_eq!(vc_icon_to_wmo("thunder-showers-night"), 95);
        assert_eq!(vc_icon_to_wmo("snow"), 73);
        assert_eq!(vc_icon_to_wmo("fog"), 45);
        // Unknown icon falls back by keyword, then to clear.
        assert_eq!(vc_icon_to_wmo("heavy-rain-mystery"), 63);
        assert_eq!(vc_icon_to_wmo("mystery"), 0);
    }

    #[test]
    fn vc_icon_day_night() {
        assert!(vc_icon_is_day("partly-cloudy-day"));
        assert!(!vc_icon_is_day("partly-cloudy-night"));
        assert!(vc_icon_is_day("cloudy"));
    }

    #[test]
    fn parses_visualcrossing_body() {
        let value: Value = serde_json::from_str(VC_TIMELINE).unwrap();
        let report = report_from_visualcrossing(&value, "Austin", true);
        assert_eq!(report.location_label, "Austin, TX, United States");
        assert_eq!(report.units, "imperial");
        assert_eq!(report.current.temp, 72);
        assert_eq!(report.current.feels_like, 70);
        assert_eq!(report.current.weather_code, 2); // partly cloudy
        assert!(!report.current.is_day); // "-night" icon
        assert_eq!(report.current.description, "partly cloudy");
        // Current high/low mirror today's daily entry.
        assert_eq!(report.current.high, 80);
        assert_eq!(report.current.low, 60);
        assert_eq!(report.daily.len(), 7);
        assert_eq!(report.daily[0].weekday, "Fri");
        assert_eq!(report.daily[2].weather_code, 63); // rain
        assert_eq!(report.daily[2].precip_prob, 80);
        assert_eq!(report.daily[6].high, 79);
        assert_eq!(report.daily[6].weather_code, 95); // thunder
    }

    #[test]
    fn vc_request_url_encodes_location_and_key() {
        let provider = VisualCrossingWeather::with_base_url("https://example.test/timeline", "K3Y");
        let url = provider.request_url("Austin, TX", true).unwrap();
        // The location is a URL-encoded path segment (the `url` crate leaves commas
        // literal in path segments but escapes the space); key + unit group are query args.
        assert!(url.as_str().contains("/timeline/Austin,%20TX"), "{url}");
        assert!(!url.as_str().contains("/timeline//"), "no double slash: {url}");
        assert!(url.as_str().contains("unitGroup=us"), "{url}");
        assert!(url.as_str().contains("key=K3Y"), "{url}");
        // Metric maps to the "metric" unit group.
        let metric = provider.request_url("Paris", false).unwrap();
        assert!(metric.as_str().contains("unitGroup=metric"), "{metric}");
    }

    #[tokio::test]
    async fn visualcrossing_fetches_in_one_request() {
        let (url, server) = serve_sequence(vec![VC_TIMELINE.to_string()]);
        let provider = VisualCrossingWeather::with_base_url(url, "test-key");
        let report = provider.fetch("Austin", true).await.unwrap();
        assert_eq!(report.location_label, "Austin, TX, United States");
        assert_eq!(report.current.temp, 72);
        assert_eq!(report.daily.len(), 7);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn vc_empty_location_is_a_graceful_error() {
        let provider = VisualCrossingWeather::new("test-key");
        assert!(provider.fetch("   ", true).await.is_err());
    }

    // ---- provider selection -----------------------------------------------------------

    #[test]
    fn from_config_disabled_is_none() {
        assert!(from_config(false, "visualcrossing", Some("k")).is_none());
    }

    #[test]
    fn from_config_selects_visualcrossing_with_a_key() {
        // Present whenever enabled; the concrete backend is an implementation detail, so
        // we assert selection succeeds (no panic / Some) across the key/no-key branches.
        assert!(from_config(true, "visualcrossing", Some("k")).is_some());
        // No key ⇒ falls back to keyless Open-Meteo (still Some).
        assert!(from_config(true, "visualcrossing", None).is_some());
        assert!(from_config(true, "", None).is_some());
    }

    #[test]
    fn from_config_openmeteo_and_unknown_fall_back_to_some() {
        assert!(from_config(true, "openmeteo", None).is_some());
        assert!(from_config(true, "not-a-provider", Some("k")).is_some());
    }
}
