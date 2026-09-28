//! Structured weather: current/summary conditions + a 10-hour hourly forecast, exposed
//! to the LLM as the `weather_lookup` tool (wired in `llm/rig.rs`) and rendered on the
//! device two ways — a full-screen weather screen (a big conditions panel + a 10-hour
//! row) when the user asks, and a small icon + temperature beside the idle clock kept
//! fresh by a periodic background push (see [`WeatherService`]).
//!
//! The hourly row spans 10 hours: for a right-now forecast it starts at the current hour
//! (truncated); for a future day (see [`ForecastWhen`] / [`resolve_when`]) it starts at
//! 08:00 local on that date and the big panel shows that day's summary instead of live
//! current conditions.
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
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveDateTime, Timelike, Weekday};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

/// Current (or a future day's) conditions plus a 10-hour hourly forecast for one place.
/// All temperatures are whole degrees in `units`; `precip_prob` is a whole percent. Plain
/// integers/strings so the type is `Eq` and JSON-marshals directly to the device.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WeatherReport {
    /// A speakable/displayable place label, e.g. "Austin, Texas".
    pub location_label: String,
    /// `"imperial"` (°F) or `"metric"` (°C).
    pub units: String,
    /// A short label for the forecast day when it is **not** the current day (e.g.
    /// "Sat, Oct 3"); empty for a right-now/today forecast. When set, the display shows
    /// it beside the location and treats `current` as that day's summary.
    #[serde(default)]
    pub when_label: String,
    /// Conditions right now, or — when `when_label` is set — the requested day's summary.
    pub current: CurrentConditions,
    /// The hourly forecast: up to 10 entries, starting at the current hour (truncated)
    /// for a right-now forecast, or at 08:00 local on the requested future day.
    #[serde(default)]
    pub hourly: Vec<HourlyForecast>,
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

/// One hour of the forecast (a cell in the display's hourly row).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HourlyForecast {
    /// Short local clock label for the hour, e.g. "3 PM".
    pub time: String,
    /// WMO weather code for the hour.
    pub weather_code: i32,
    /// Temperature, whole degrees.
    pub temp: i32,
    /// Chance of precipitation, whole percent (0 when the API omits it).
    pub precip_prob: i32,
    /// Whether this hour is daytime (picks a day/night icon variant).
    pub is_day: bool,
}

/// Which slice of the forecast the hourly row (and the big panel) should cover.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForecastWhen {
    /// Right now: the hourly row starts at the current hour (truncated) and the big
    /// panel shows live current conditions.
    Now,
    /// A specific future day: the hourly row starts at 08:00 local on that date and the
    /// big panel shows that day's summary.
    Day(NaiveDate),
}

/// Resolve a free-text `when` argument (from the `weather_lookup` tool) into a
/// [`ForecastWhen`], relative to `now`. Accepts (case-insensitively): `today`/`now`/
/// empty (→ [`ForecastWhen::Now`]), `tomorrow`, `day after tomorrow`, a weekday name
/// ("Saturday"/"sat" → the next such day within a week), and an ISO `YYYY-MM-DD`
/// (optionally `on:`-prefixed). A date that resolves to today (or earlier) collapses to
/// `Now`. A malformed `on:` date is an error; any other unrecognized text falls back to
/// `Now` so the tool still shows something.
pub fn resolve_when(when: Option<&str>, now: DateTime<Local>) -> Result<ForecastWhen, String> {
    let today = now.date_naive();
    let day_of = |d: NaiveDate| {
        if d <= today {
            ForecastWhen::Now
        } else {
            ForecastWhen::Day(d)
        }
    };
    let lowered = when.map(|s| s.trim().to_lowercase());
    let spec = lowered.as_deref().filter(|s| !s.is_empty());
    match spec {
        None | Some("now") | Some("today") | Some("current") | Some("tonight") => {
            Ok(ForecastWhen::Now)
        }
        Some("tomorrow") => Ok(day_of(today + Duration::days(1))),
        Some("day after tomorrow") | Some("overmorrow") => Ok(day_of(today + Duration::days(2))),
        Some(s) => {
            let date_part = s.strip_prefix("on:").unwrap_or(s).trim();
            if let Ok(d) = NaiveDate::parse_from_str(date_part, "%Y-%m-%d") {
                return Ok(day_of(d));
            }
            if let Some(wd) = parse_weekday(s) {
                // The next occurrence of that weekday; today's weekday means a week out.
                let mut d = today + Duration::days(1);
                for _ in 0..7 {
                    if d.weekday() == wd {
                        return Ok(ForecastWhen::Day(d));
                    }
                    d += Duration::days(1);
                }
            }
            if s.starts_with("on:") {
                return Err(format!("'{date_part}' is not a valid YYYY-MM-DD date"));
            }
            Ok(ForecastWhen::Now)
        }
    }
}

/// Map an English weekday name (or common short form) to a [`Weekday`].
fn parse_weekday(s: &str) -> Option<Weekday> {
    match s {
        "monday" | "mon" => Some(Weekday::Mon),
        "tuesday" | "tue" | "tues" => Some(Weekday::Tue),
        "wednesday" | "wed" => Some(Weekday::Wed),
        "thursday" | "thu" | "thurs" => Some(Weekday::Thu),
        "friday" | "fri" => Some(Weekday::Fri),
        "saturday" | "sat" => Some(Weekday::Sat),
        "sunday" | "sun" => Some(Weekday::Sun),
        _ => None,
    }
}

/// The instant the hourly window starts, given the request and the forecast's local
/// "now": the current hour truncated for [`ForecastWhen::Now`], else 08:00 on the day.
fn window_start(when: ForecastWhen, now_local: NaiveDateTime) -> NaiveDateTime {
    match when {
        ForecastWhen::Now => now_local
            .date()
            .and_hms_opt(now_local.hour(), 0, 0)
            .unwrap_or(now_local),
        ForecastWhen::Day(d) => d.and_hms_opt(8, 0, 0).unwrap_or(now_local),
    }
}

/// Format a local datetime's hour as a short clock label ("3 PM").
fn hour_label(dt: &NaiveDateTime) -> String {
    dt.format("%-I %p").to_string()
}

/// Pick up to 10 hourly entries at or after `start` from a time-sorted list.
fn take_hours(
    all: &[(NaiveDateTime, HourlyForecast)],
    start: NaiveDateTime,
) -> Vec<HourlyForecast> {
    all.iter()
        .filter(|(t, _)| *t >= start)
        .take(10)
        .map(|(_, h)| h.clone())
        .collect()
}

/// Build a future day's big-panel summary (temperature shown = that day's high) plus a
/// short display label, from the daily forecast. An absent day degrades to defaults.
fn day_summary(daily: &[DailyForecast], date: NaiveDate) -> (CurrentConditions, String) {
    let iso = date.format("%Y-%m-%d").to_string();
    let label = date.format("%a, %b %-d").to_string();
    match daily.iter().find(|d| d.date == iso) {
        Some(d) => (
            CurrentConditions {
                temp: d.high,
                feels_like: d.high,
                weather_code: d.weather_code,
                is_day: true,
                high: d.high,
                low: d.low,
                description: weather_description(d.weather_code).to_string(),
            },
            label,
        ),
        None => (CurrentConditions::default(), label),
    }
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
    /// Resolve `location` and return the conditions + a 10-hour hourly forecast for
    /// `when` (right now, or a future day), with temperatures in imperial (°F) when
    /// `imperial` is true, else metric (°C).
    async fn fetch(
        &self,
        location: &str,
        imperial: bool,
        when: ForecastWhen,
    ) -> Result<WeatherReport>;
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
    async fn fetch(
        &self,
        location: &str,
        imperial: bool,
        when: ForecastWhen,
    ) -> Result<WeatherReport> {
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
                    "hourly",
                    "temperature_2m,weather_code,precipitation_probability,is_day",
                ),
                (
                    "daily",
                    "weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max",
                ),
                ("temperature_unit", temp_unit),
                ("timezone", "auto"),
                // today .. today+7, so a future-day request up to a week out still has
                // both its daily summary and its 08:00→ hourly slice.
                ("forecast_days", "8"),
            ])
            .send()
            .await
            .context("GET Open-Meteo forecast")?
            .error_for_status()
            .context("Open-Meteo forecast returned an error status")?
            .json()
            .await
            .context("parsing Open-Meteo forecast JSON")?;

        Ok(report_from_forecast(&value, &label, imperial, when))
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
            // `hours` adds a per-hour array inside each day (for the hourly row).
            .append_pair("include", "current,days,hours")
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
    async fn fetch(
        &self,
        location: &str,
        imperial: bool,
        when: ForecastWhen,
    ) -> Result<WeatherReport> {
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

        Ok(report_from_visualcrossing(&value, location, imperial, when))
    }
}

/// Map a Visual Crossing timeline JSON body into a [`WeatherReport`]. Missing fields
/// degrade gracefully (zeros/empties, WMO code 0) rather than failing the whole lookup —
/// matching [`report_from_forecast`]'s contract for Open-Meteo.
fn report_from_visualcrossing(
    value: &Value,
    query: &str,
    imperial: bool,
    when: ForecastWhen,
) -> WeatherReport {
    let label = value
        .get("resolvedAddress")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(query)
        .to_string();

    let daily = vc_daily_from(value.get("days"));
    let all_hours = vc_hours_from(value.get("days"));

    let cc = value.get("currentConditions");
    // Local "now" = today's date (days[0]) at the current-conditions clock time.
    let now_local = daily
        .first()
        .map(|d| d.date.clone())
        .and_then(|date| {
            let tstr = cc
                .and_then(|c| c.get("datetime"))
                .and_then(Value::as_str)
                .unwrap_or("00:00:00");
            NaiveDateTime::parse_from_str(&format!("{date} {tstr}"), "%Y-%m-%d %H:%M:%S").ok()
        })
        .or_else(|| all_hours.first().map(|(t, _)| *t))
        .unwrap_or_default();
    let hourly = take_hours(&all_hours, window_start(when, now_local));

    let (current, when_label) = match when {
        ForecastWhen::Now => {
            let (today_high, today_low) = daily.first().map(|d| (d.high, d.low)).unwrap_or((0, 0));
            let icon = cc
                .and_then(|c| c.get("icon"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let code = vc_icon_to_wmo(icon);
            (
                CurrentConditions {
                    temp: round_temp(cc.and_then(|c| c.get("temp"))),
                    feels_like: round_temp(cc.and_then(|c| c.get("feelslike"))),
                    weather_code: code,
                    is_day: vc_icon_is_day(icon),
                    high: today_high,
                    low: today_low,
                    description: weather_description(code).to_string(),
                },
                String::new(),
            )
        }
        ForecastWhen::Day(d) => day_summary(&daily, d),
    };

    WeatherReport {
        location_label: label,
        units: if imperial { "imperial" } else { "metric" }.to_string(),
        when_label,
        current,
        hourly,
    }
}

/// Flatten Visual Crossing's per-day `hours` arrays into a single time-sorted list. Each
/// hour's local datetime is its parent day's `datetime` (a date) plus the hour's own
/// `datetime` (a `HH:MM:SS` clock time). Day/night comes from the hour's icon suffix,
/// falling back to a 06:00–20:00 daytime window when the icon is absent.
fn vc_hours_from(days: Option<&Value>) -> Vec<(NaiveDateTime, HourlyForecast)> {
    let Some(days) = days.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for day in days {
        let date = day.get("datetime").and_then(Value::as_str).unwrap_or("");
        let Some(hours) = day.get("hours").and_then(Value::as_array) else {
            continue;
        };
        for h in hours {
            let tstr = h.get("datetime").and_then(Value::as_str).unwrap_or("");
            let Ok(dt) =
                NaiveDateTime::parse_from_str(&format!("{date} {tstr}"), "%Y-%m-%d %H:%M:%S")
            else {
                continue;
            };
            let icon = h.get("icon").and_then(Value::as_str).unwrap_or("");
            let is_day = if icon.is_empty() {
                (6..20).contains(&dt.hour())
            } else {
                vc_icon_is_day(icon)
            };
            let code = vc_icon_to_wmo(icon);
            out.push((
                dt,
                HourlyForecast {
                    time: hour_label(&dt),
                    weather_code: code,
                    temp: round_temp(h.get("temp")),
                    precip_prob: h
                        .get("precipprob")
                        .and_then(Value::as_f64)
                        .map(|p| p.round() as i32)
                        .unwrap_or(0),
                    is_day,
                },
            ));
        }
    }
    out.sort_by_key(|(t, _)| *t);
    out
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
fn report_from_forecast(
    value: &Value,
    label: &str,
    imperial: bool,
    when: ForecastWhen,
) -> WeatherReport {
    let daily_forecast = daily_from(value.get("daily"));
    let all_hours = om_hours_from(value.get("hourly"));

    // Local "now" from the `current.time` field, falling back to the first hourly slot.
    let now_local = value
        .get("current")
        .and_then(|c| c.get("time"))
        .and_then(Value::as_str)
        .and_then(|s| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M").ok())
        .or_else(|| all_hours.first().map(|(t, _)| *t))
        .unwrap_or_default();
    let hourly = take_hours(&all_hours, window_start(when, now_local));

    let (current, when_label) = match when {
        ForecastWhen::Now => {
            let cur = value.get("current");
            let (today_high, today_low) = daily_forecast
                .first()
                .map(|d| (d.high, d.low))
                .unwrap_or((0, 0));
            let code = cur
                .and_then(|c| c.get("weather_code"))
                .and_then(Value::as_i64)
                .unwrap_or(0) as i32;
            (
                CurrentConditions {
                    temp: round_temp(cur.and_then(|c| c.get("temperature_2m"))),
                    feels_like: round_temp(cur.and_then(|c| c.get("apparent_temperature"))),
                    weather_code: code,
                    is_day: cur
                        .and_then(|c| c.get("is_day"))
                        .and_then(Value::as_i64)
                        .map(|d| d != 0)
                        .unwrap_or(true),
                    high: today_high,
                    low: today_low,
                    description: weather_description(code).to_string(),
                },
                String::new(),
            )
        }
        ForecastWhen::Day(d) => day_summary(&daily_forecast, d),
    };

    WeatherReport {
        location_label: label.to_string(),
        units: if imperial { "imperial" } else { "metric" }.to_string(),
        when_label,
        current,
        hourly,
    }
}

/// Build the hourly list from Open-Meteo's `hourly` object's parallel arrays.
fn om_hours_from(hourly: Option<&Value>) -> Vec<(NaiveDateTime, HourlyForecast)> {
    let Some(hourly) = hourly else {
        return Vec::new();
    };
    let times = hourly.get("time").and_then(Value::as_array);
    let codes = hourly.get("weather_code").and_then(Value::as_array);
    let temps = hourly.get("temperature_2m").and_then(Value::as_array);
    let precip = hourly
        .get("precipitation_probability")
        .and_then(Value::as_array);
    let isday = hourly.get("is_day").and_then(Value::as_array);
    let Some(times) = times else {
        return Vec::new();
    };

    fn at_i(arr: Option<&Vec<Value>>, i: usize) -> Option<&Value> {
        arr.and_then(|a| a.get(i))
    }
    times
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            let dt = NaiveDateTime::parse_from_str(t.as_str()?, "%Y-%m-%dT%H:%M").ok()?;
            Some((
                dt,
                HourlyForecast {
                    time: hour_label(&dt),
                    weather_code: at_i(codes, i).and_then(Value::as_i64).unwrap_or(0) as i32,
                    temp: round_temp(at_i(temps, i)),
                    precip_prob: at_i(precip, i).and_then(Value::as_i64).unwrap_or(0) as i32,
                    is_day: at_i(isday, i)
                        .and_then(Value::as_i64)
                        .map(|d| d != 0)
                        .unwrap_or(true),
                },
            ))
        })
        .collect()
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
    // Wrap every provider in the shared response cache. `from_config` is the single
    // constructor for weather providers (the System-1 fast path, the `weather_lookup`
    // LLM tool, and the ambient push all build through it), and the cache is process-wide,
    // so a repeated forecast for the same place+units inside the TTL is served locally
    // regardless of which caller asks — turning a cold ~1.3 s fetch into a warm lookup.
    // The TTL is per-tool and configurable (`tool_cache.weather_lookup`, default 60 min).
    Some(
        Arc::new(CachingWeatherProvider::new(provider, weather_cache()))
            as Arc<dyn WeatherProvider>,
    )
}

/// The logical tool name weather caches under — its key in the `tool_cache` TTL config
/// and in the shared [`ToolCache`](crate::cache::ToolCache) key space.
pub const WEATHER_TOOL: &str = "weather_lookup";

/// The process-wide weather response cache, shared across every provider built by
/// [`from_config`]. Lazily created on first use.
fn weather_cache() -> Arc<crate::cache::ToolCache> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Arc<crate::cache::ToolCache>> = OnceLock::new();
    CACHE
        .get_or_init(|| Arc::new(crate::cache::ToolCache::new()))
        .clone()
}

/// A [`WeatherProvider`] decorator that serves a repeated `(location, imperial)` fetch
/// from a shared [`ToolCache`](crate::cache::ToolCache) within its configured TTL,
/// delegating to the wrapped provider on a miss and caching the result. The TTL comes
/// from `tool_cache.weather_lookup` (default 60 min); `0` turns caching off. Weather is a
/// pure lookup of its arguments for that window, so this is safe; a miss behaves exactly
/// like the inner provider (including its Open-Meteo fallback and error paths — errors
/// are never cached).
struct CachingWeatherProvider {
    inner: Arc<dyn WeatherProvider>,
    cache: Arc<crate::cache::ToolCache>,
}

impl CachingWeatherProvider {
    fn new(inner: Arc<dyn WeatherProvider>, cache: Arc<crate::cache::ToolCache>) -> Self {
        Self { inner, cache }
    }
}

#[async_trait]
impl WeatherProvider for CachingWeatherProvider {
    async fn fetch(
        &self,
        location: &str,
        imperial: bool,
        when: ForecastWhen,
    ) -> Result<WeatherReport> {
        // Per-tool TTL from the `tool_cache` config (0 ⇒ caching disabled for weather).
        let ttl = crate::cache::config().ttl(WEATHER_TOOL);
        if ttl.is_zero() {
            return self.inner.fetch(location, imperial, when).await;
        }
        // Key on the outgoing call data: the resolved place + units + which window. A
        // right-now request buckets by the current hour so a cached hourly slice never
        // shows a stale start-of-window; a future day buckets by its date.
        let when_key = match when {
            ForecastWhen::Now => format!("now:{}", Local::now().format("%Y-%m-%dT%H")),
            ForecastWhen::Day(d) => format!("day:{}", d.format("%Y-%m-%d")),
        };
        let key =
            crate::cache::ToolCache::key(WEATHER_TOOL, &(location, imperial, when_key.as_str()));
        if let Some(key) = &key {
            if let Some(hit) = self.cache.get_as::<WeatherReport>(key) {
                log::info!("weather cache hit for {location:?} (imperial={imperial}, {when_key})");
                return Ok(hit);
            }
        }
        let report = self.inner.fetch(location, imperial, when).await?;
        if let Some(key) = key {
            self.cache.put_as(key, &report, ttl);
        }
        Ok(report)
    }
}

/// A short spoken confirmation for the model to relay once the forecast is on screen.
pub fn render_confirmation(report: &WeatherReport) -> String {
    let unit = if report.units == "imperial" { "F" } else { "C" };
    let c = &report.current;
    // A future-day forecast: speak the day + its summary rather than "right now".
    if !report.when_label.is_empty() {
        let mut out = format!("{} will be {}", report.when_label, c.description);
        if !report.location_label.is_empty() {
            out.push_str(&format!(" in {}", report.location_label));
        }
        out.push_str(&format!(
            ", with a high of {} degrees {unit} and a low of {}. The forecast is on the screen.",
            c.high, c.low
        ));
        return out;
    }
    let mut out = format!(
        "It's {} degrees {unit} and {} right now",
        c.temp, c.description
    );
    if !report.location_label.is_empty() {
        out.push_str(&format!(" in {}", report.location_label));
    }
    out.push_str(&format!(
        ", with a high of {} and a low of {}. The forecast is on the screen.",
        c.high, c.low
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const GEO: &str = r#"{"results":[{"name":"Austin","latitude":30.2672,"longitude":-97.7431,"admin1":"Texas","country":"United States"}]}"#;
    const FORECAST: &str = r#"{
        "current":{"time":"2026-09-25T12:00","temperature_2m":72.4,"apparent_temperature":70.1,"weather_code":2,"is_day":1},
        "hourly":{
            "time":["2026-09-25T10:00","2026-09-25T11:00","2026-09-25T12:00","2026-09-25T13:00","2026-09-25T14:00","2026-09-25T15:00","2026-09-25T16:00","2026-09-25T17:00","2026-09-25T18:00","2026-09-25T19:00","2026-09-25T20:00","2026-09-25T21:00","2026-09-25T22:00","2026-09-25T23:00"],
            "temperature_2m":[68,70,72,73,74,75,74,73,72,70,69,68,67,66],
            "weather_code":[2,2,2,2,3,3,61,61,2,2,0,0,0,0],
            "precipitation_probability":[0,0,5,10,10,20,20,30,30,40,40,50,50,60],
            "is_day":[1,1,1,1,1,1,1,1,1,1,0,0,0,0]
        },
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
        let report = report_from_forecast(&value, "Austin, Texas", true, ForecastWhen::Now);
        assert_eq!(report.location_label, "Austin, Texas");
        assert_eq!(report.units, "imperial");
        assert!(report.when_label.is_empty()); // a right-now forecast has no day label
        assert_eq!(report.current.temp, 72);
        assert_eq!(report.current.feels_like, 70);
        assert_eq!(report.current.weather_code, 2);
        assert!(report.current.is_day);
        assert_eq!(report.current.description, "partly cloudy");
        // Current high/low mirror today's daily entry.
        assert_eq!(report.current.high, 80);
        assert_eq!(report.current.low, 60);
        // The hourly row is 10 hours starting at the current hour (12:00 → "12 PM").
        assert_eq!(report.hourly.len(), 10);
        assert_eq!(report.hourly[0].time, "12 PM");
        assert_eq!(report.hourly[0].temp, 72);
        assert!(report.hourly[0].is_day);
        assert_eq!(report.hourly[9].time, "9 PM");
        assert!(!report.hourly[9].is_day); // 21:00 is after sunset in the fixture
    }

    #[test]
    fn open_meteo_future_day_starts_at_8am_with_day_summary() {
        // A future day: hourly starts at 08:00 and the panel shows that day's summary.
        let value = serde_json::json!({
            "current": {"time":"2026-09-25T12:00","temperature_2m":72,"weather_code":2,"is_day":1},
            "hourly": {
                "time": (6..=18).map(|h| format!("2026-09-26T{h:02}:00")).collect::<Vec<_>>(),
                "temperature_2m": (6..=18).map(|h| 60 + h).collect::<Vec<_>>(),
                "weather_code": (6..=18).map(|_| 61).collect::<Vec<_>>(),
                "precipitation_probability": (6..=18).map(|_| 40).collect::<Vec<_>>(),
                "is_day": (6..=18).map(|h| i32::from((6..20).contains(&h))).collect::<Vec<_>>(),
            },
            "daily": {
                "time":["2026-09-25","2026-09-26"],
                "weather_code":[2,61],
                "temperature_2m_max":[80,75],
                "temperature_2m_min":[60,58],
                "precipitation_probability_max":[10,80]
            }
        });
        let date = NaiveDate::from_ymd_opt(2026, 9, 26).unwrap();
        let report = report_from_forecast(&value, "Austin", true, ForecastWhen::Day(date));
        assert_eq!(report.when_label, "Sat, Sep 26");
        // Panel = the day's summary (temp shown = the day's high).
        assert_eq!(report.current.temp, 75);
        assert_eq!(report.current.high, 75);
        assert_eq!(report.current.low, 58);
        assert_eq!(report.current.weather_code, 61);
        // Hourly starts at 08:00 → "8 AM", 10 entries.
        assert_eq!(report.hourly.len(), 10);
        assert_eq!(report.hourly[0].time, "8 AM");
        assert_eq!(report.hourly[9].time, "5 PM");
    }

    #[test]
    fn metric_units_label() {
        let value: Value = serde_json::from_str(FORECAST).unwrap();
        let report = report_from_forecast(&value, "x", false, ForecastWhen::Now);
        assert_eq!(report.units, "metric");
    }

    #[test]
    fn confirmation_mentions_temp_and_location() {
        let value: Value = serde_json::from_str(FORECAST).unwrap();
        let report = report_from_forecast(&value, "Austin, Texas", true, ForecastWhen::Now);
        let c = render_confirmation(&report);
        assert!(c.contains("72"), "{c}");
        assert!(c.contains("partly cloudy"), "{c}");
        assert!(c.contains("Austin, Texas"), "{c}");
    }

    #[test]
    fn confirmation_speaks_the_day_for_a_future_forecast() {
        let mut report = WeatherReport {
            location_label: "Austin, Texas".into(),
            units: "imperial".into(),
            when_label: "Sat, Sep 26".into(),
            ..Default::default()
        };
        report.current.description = "rain".into();
        report.current.high = 75;
        report.current.low = 58;
        let c = render_confirmation(&report);
        assert!(c.contains("Sat, Sep 26"), "{c}");
        assert!(c.contains("rain"), "{c}");
        assert!(c.contains("high of 75"), "{c}");
        assert!(!c.contains("right now"), "{c}");
    }

    #[test]
    fn resolve_when_variants() {
        let now = Local.with_ymd_and_hms(2026, 9, 25, 14, 30, 0).unwrap(); // a Friday
        assert_eq!(resolve_when(None, now).unwrap(), ForecastWhen::Now);
        assert_eq!(resolve_when(Some(""), now).unwrap(), ForecastWhen::Now);
        assert_eq!(resolve_when(Some("today"), now).unwrap(), ForecastWhen::Now);
        assert_eq!(
            resolve_when(Some("tomorrow"), now).unwrap(),
            ForecastWhen::Day(NaiveDate::from_ymd_opt(2026, 9, 26).unwrap())
        );
        // A weekday name → the next such day (Saturday is tomorrow here).
        assert_eq!(
            resolve_when(Some("Saturday"), now).unwrap(),
            ForecastWhen::Day(NaiveDate::from_ymd_opt(2026, 9, 26).unwrap())
        );
        // The current weekday resolves a week out, never to "now".
        assert_eq!(
            resolve_when(Some("Friday"), now).unwrap(),
            ForecastWhen::Day(NaiveDate::from_ymd_opt(2026, 10, 2).unwrap())
        );
        // An explicit date; today collapses to Now.
        assert_eq!(
            resolve_when(Some("2026-09-28"), now).unwrap(),
            ForecastWhen::Day(NaiveDate::from_ymd_opt(2026, 9, 28).unwrap())
        );
        assert_eq!(
            resolve_when(Some("2026-09-25"), now).unwrap(),
            ForecastWhen::Now
        );
        // A malformed on: date is an error; unknown free text is a soft fallback to Now.
        assert!(resolve_when(Some("on:not-a-date"), now).is_err());
        assert_eq!(
            resolve_when(Some("banana"), now).unwrap(),
            ForecastWhen::Now
        );
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
        let report = provider
            .fetch("Austin", true, ForecastWhen::Now)
            .await
            .unwrap();
        assert_eq!(report.location_label, "Austin, Texas");
        assert_eq!(report.current.temp, 72);
        assert_eq!(report.hourly.len(), 10);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn empty_location_is_a_graceful_error() {
        let provider = OpenMeteoWeather::new();
        assert!(provider
            .fetch("   ", true, ForecastWhen::Now)
            .await
            .is_err());
    }

    // ---- Visual Crossing --------------------------------------------------------------

    const VC_TIMELINE: &str = r#"{
        "resolvedAddress":"Austin, TX, United States",
        "currentConditions":{"datetime":"12:00:00","temp":72.4,"feelslike":70.1,"icon":"partly-cloudy-night","conditions":"Partially cloudy"},
        "days":[
            {"datetime":"2026-09-25","tempmax":80.2,"tempmin":60.1,"precipprob":10,"icon":"partly-cloudy-day","conditions":"Partially cloudy","hours":[
                {"datetime":"10:00:00","temp":68,"precipprob":0,"icon":"clear-day"},
                {"datetime":"11:00:00","temp":70,"precipprob":0,"icon":"clear-day"},
                {"datetime":"12:00:00","temp":72,"precipprob":5,"icon":"partly-cloudy-day"},
                {"datetime":"13:00:00","temp":73,"precipprob":10,"icon":"partly-cloudy-day"},
                {"datetime":"14:00:00","temp":74,"precipprob":10,"icon":"cloudy"},
                {"datetime":"15:00:00","temp":75,"precipprob":20,"icon":"cloudy"},
                {"datetime":"16:00:00","temp":74,"precipprob":20,"icon":"rain"},
                {"datetime":"17:00:00","temp":73,"precipprob":30,"icon":"rain"},
                {"datetime":"18:00:00","temp":72,"precipprob":30,"icon":"partly-cloudy-day"},
                {"datetime":"19:00:00","temp":70,"precipprob":40,"icon":"partly-cloudy-day"},
                {"datetime":"20:00:00","temp":69,"precipprob":40,"icon":"clear-night"},
                {"datetime":"21:00:00","temp":68,"precipprob":50,"icon":"clear-night"}
            ]},
            {"datetime":"2026-09-26","tempmax":78.9,"tempmin":59.4,"precipprob":20,"icon":"cloudy","hours":[
                {"datetime":"06:00:00","temp":60,"precipprob":10,"icon":"partly-cloudy-day"},
                {"datetime":"07:00:00","temp":61,"precipprob":10,"icon":"partly-cloudy-day"},
                {"datetime":"08:00:00","temp":62,"precipprob":15,"icon":"cloudy"},
                {"datetime":"09:00:00","temp":64,"precipprob":15,"icon":"cloudy"},
                {"datetime":"10:00:00","temp":66,"precipprob":20,"icon":"cloudy"},
                {"datetime":"11:00:00","temp":68,"precipprob":20,"icon":"cloudy"},
                {"datetime":"12:00:00","temp":70,"precipprob":25,"icon":"cloudy"},
                {"datetime":"13:00:00","temp":72,"precipprob":25,"icon":"rain"},
                {"datetime":"14:00:00","temp":73,"precipprob":30,"icon":"rain"},
                {"datetime":"15:00:00","temp":74,"precipprob":30,"icon":"rain"},
                {"datetime":"16:00:00","temp":73,"precipprob":35,"icon":"cloudy"},
                {"datetime":"17:00:00","temp":72,"precipprob":35,"icon":"cloudy"}
            ]},
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
        let report = report_from_visualcrossing(&value, "Austin", true, ForecastWhen::Now);
        assert_eq!(report.location_label, "Austin, TX, United States");
        assert_eq!(report.units, "imperial");
        assert!(report.when_label.is_empty());
        assert_eq!(report.current.temp, 72);
        assert_eq!(report.current.feels_like, 70);
        assert_eq!(report.current.weather_code, 2); // partly cloudy
        assert!(!report.current.is_day); // "-night" icon
        assert_eq!(report.current.description, "partly cloudy");
        // Current high/low mirror today's daily entry.
        assert_eq!(report.current.high, 80);
        assert_eq!(report.current.low, 60);
        // Hourly = 10 hours from the current-conditions clock time (12:00 → "12 PM").
        assert_eq!(report.hourly.len(), 10);
        assert_eq!(report.hourly[0].time, "12 PM");
        assert_eq!(report.hourly[0].temp, 72);
        assert_eq!(report.hourly[4].weather_code, 63); // 16:00 "rain" icon → WMO 63
    }

    #[test]
    fn visualcrossing_future_day_starts_at_8am_with_day_summary() {
        let value: Value = serde_json::from_str(VC_TIMELINE).unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 9, 26).unwrap();
        let report = report_from_visualcrossing(&value, "Austin", true, ForecastWhen::Day(date));
        assert_eq!(report.when_label, "Sat, Sep 26");
        // Panel = the day's summary (temp shown = day high, rounded from tempmax 78.9).
        assert_eq!(report.current.temp, 79);
        assert_eq!(report.current.high, 79);
        assert_eq!(report.current.low, 59);
        // Hourly starts at 08:00 → "8 AM", 10 entries within that day.
        assert_eq!(report.hourly.len(), 10);
        assert_eq!(report.hourly[0].time, "8 AM");
        assert_eq!(report.hourly[9].time, "5 PM");
    }

    #[test]
    fn vc_request_url_encodes_location_and_key() {
        let provider = VisualCrossingWeather::with_base_url("https://example.test/timeline", "K3Y");
        let url = provider.request_url("Austin, TX", true).unwrap();
        // The location is a URL-encoded path segment (the `url` crate leaves commas
        // literal in path segments but escapes the space); key + unit group are query args.
        assert!(url.as_str().contains("/timeline/Austin,%20TX"), "{url}");
        assert!(
            !url.as_str().contains("/timeline//"),
            "no double slash: {url}"
        );
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
        let report = provider
            .fetch("Austin", true, ForecastWhen::Now)
            .await
            .unwrap();
        assert_eq!(report.location_label, "Austin, TX, United States");
        assert_eq!(report.current.temp, 72);
        assert_eq!(report.hourly.len(), 10);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn vc_empty_location_is_a_graceful_error() {
        let provider = VisualCrossingWeather::new("test-key");
        assert!(provider
            .fetch("   ", true, ForecastWhen::Now)
            .await
            .is_err());
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

    /// A provider that counts how many times the upstream `fetch` actually ran, so the
    /// caching decorator can be verified without any network.
    struct CountingProvider {
        calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait]
    impl WeatherProvider for CountingProvider {
        async fn fetch(
            &self,
            location: &str,
            imperial: bool,
            _when: ForecastWhen,
        ) -> Result<WeatherReport> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(WeatherReport {
                location_label: location.to_string(),
                units: if imperial { "imperial" } else { "metric" }.to_string(),
                ..Default::default()
            })
        }
    }

    #[tokio::test]
    async fn caching_provider_serves_repeat_calls_without_refetching() {
        use std::sync::atomic::Ordering;
        let inner = Arc::new(CountingProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        // A dedicated cache (not the process-global one) so the test is isolated. The TTL
        // comes from the process cache config, which defaults `weather_lookup` to 60 min.
        let cache = Arc::new(crate::cache::ToolCache::new());
        let provider = CachingWeatherProvider::new(inner.clone(), cache);

        // First call misses → upstream runs once.
        let a = provider
            .fetch("Austin", true, ForecastWhen::Now)
            .await
            .unwrap();
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
        // Identical call is served from cache → upstream NOT hit again.
        let b = provider
            .fetch("Austin", true, ForecastWhen::Now)
            .await
            .unwrap();
        assert_eq!(
            inner.calls.load(Ordering::SeqCst),
            1,
            "second call must be cached"
        );
        assert_eq!(a, b);
        // Different units is a distinct key → upstream runs again.
        provider
            .fetch("Austin", false, ForecastWhen::Now)
            .await
            .unwrap();
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
        // Different place → another upstream call.
        provider
            .fetch("Dallas", true, ForecastWhen::Now)
            .await
            .unwrap();
        assert_eq!(inner.calls.load(Ordering::SeqCst), 3);
        // A future-day request is a distinct window → another upstream call, then cached.
        let day = ForecastWhen::Day(NaiveDate::from_ymd_opt(2999, 1, 2).unwrap());
        provider.fetch("Austin", true, day).await.unwrap();
        assert_eq!(inner.calls.load(Ordering::SeqCst), 4);
        provider.fetch("Austin", true, day).await.unwrap();
        assert_eq!(
            inner.calls.load(Ordering::SeqCst),
            4,
            "future day is cached too"
        );
    }
}
