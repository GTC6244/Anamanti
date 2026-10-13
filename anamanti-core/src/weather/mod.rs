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
//! by `weather.provider` (see [`from_config`] / [`effective_backend`]):
//! - **Visual Crossing** ([`VisualCrossingWeather`], the default) — its Timeline API
//!   resolves a place *and* returns current conditions + the 7-day forecast in one keyed
//!   request. Needs the `VISUALCROSSING_API_KEY` secret.
//! - **Open-Meteo** ([`OpenMeteoWeather`]) — a keyless, structured forecast API plus its
//!   free geocoding API to resolve the household `home_location` to coordinates. Also the
//!   automatic fallback when the Visual Crossing key is absent.
//!
//! Either way WMO weather codes ride through to the device, which maps them to bundled
//! icons (Visual Crossing's text icons are translated to WMO codes in the provider).
//!
//! Layout: `providers/` (one file per backend, each with typed response structs),
//! `cache` (the [`ToolCache`](crate::cache::ToolCache) decorator every built provider is
//! wrapped in), `factory` ([`from_config`] / [`effective_backend`]), `render` (the
//! spoken confirmation) and `service` (the ambient push). This file holds the shared
//! report types and the forecast-window helpers both providers use.
//!
//! The trait keeps the tool and the periodic push testable offline (fixture JSON, no
//! network), mirroring how [`crate::directions::DirectionsProvider`] backs the directions
//! tool. Temperatures
//! and probabilities are stored as whole integers so [`WeatherReport`] is `Eq` (it
//! rides inside [`crate::llm::DeviceAction`]) and marshals trivially to the device.

mod cache;
mod factory;
pub mod providers;
mod render;
pub mod service;

pub use cache::WEATHER_TOOL;
pub use factory::{effective_backend, from_config, selects_visualcrossing, WeatherBackend};
pub use providers::{OpenMeteoWeather, VisualCrossingWeather};
pub use render::render_confirmation;
pub use service::WeatherService;

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveDateTime, Timelike, Weekday};
use serde::{Deserialize, Serialize};

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
    /// The daily forecast: up to 7 days starting today, used by the separate 7-day
    /// forecast widget (see `layout`). Always populated regardless of `layout` so the
    /// device can switch views without a refetch; empty only if the provider omits it.
    #[serde(default)]
    pub daily: Vec<DailyForecast>,
    /// Presentation hint for the display's full-screen weather screen. Empty or
    /// `"hourly"` (the default) renders the hourly view (big panel + 10-hour row);
    /// `"week"` renders the separate 7-day forecast widget (7 vertical columns of
    /// highs/lows). Set from the `weather_lookup` tool's `layout` argument; the ambient
    /// push always leaves it empty (hourly).
    #[serde(default)]
    pub layout: String,
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
pub(crate) fn window_start(when: ForecastWhen, now_local: NaiveDateTime) -> NaiveDateTime {
    match when {
        ForecastWhen::Now => now_local
            .date()
            .and_hms_opt(now_local.hour(), 0, 0)
            .unwrap_or(now_local),
        ForecastWhen::Day(d) => d.and_hms_opt(8, 0, 0).unwrap_or(now_local),
    }
}

/// Format a local datetime's hour as a short clock label ("3 PM").
pub(crate) fn hour_label(dt: &NaiveDateTime) -> String {
    dt.format("%-I %p").to_string()
}

/// Pick up to 10 hourly entries at or after `start` from a time-sorted list.
pub(crate) fn take_hours(
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
pub(crate) fn day_summary(daily: &[DailyForecast], date: NaiveDate) -> (CurrentConditions, String) {
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

/// Round a temperature to whole degrees; absent/non-numeric → 0.
pub(crate) fn round_temp(v: Option<f64>) -> i32 {
    v.map(|t| t.round() as i32).unwrap_or(0)
}

/// A short weekday label ("Mon", "Tue", …) for an ISO `YYYY-MM-DD` date, or "" when
/// unparseable.
pub(crate) fn weekday_label(date: &str) -> String {
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

/// Tolerant field deserializers for the fixed-schema provider responses. Each reads the
/// field as any JSON and keeps it only when it has the expected type (exactly the old
/// `Value::as_*` semantics), so one odd field degrades to "absent" instead of failing
/// the whole forecast. Use with `#[serde(default, deserialize_with = "…")]`.
pub(crate) mod lenient {
    use serde::{Deserialize, Deserializer};
    use serde_json::Value;

    fn any<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
        Option::<Value>::deserialize(d)
    }

    /// A number (`as_f64`).
    pub fn f64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
        Ok(any(d)?.as_ref().and_then(Value::as_f64))
    }

    /// An integer (`as_i64`; a float is treated as absent).
    pub fn i64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
        Ok(any(d)?.as_ref().and_then(Value::as_i64))
    }

    /// A string.
    pub fn string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
        Ok(any(d)?.and_then(|v| v.as_str().map(str::to_string)))
    }

    /// An array of numbers, each element kept positionally (`None` when not a number).
    pub fn vec_f64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<Option<f64>>>, D::Error> {
        Ok(array(any(d)?, |v| v.as_f64()))
    }

    /// An array of integers, each element kept positionally.
    pub fn vec_i64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<Option<i64>>>, D::Error> {
        Ok(array(any(d)?, |v| v.as_i64()))
    }

    /// An array of strings, each element kept positionally.
    pub fn vec_string<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Vec<Option<String>>>, D::Error> {
        Ok(array(any(d)?, |v| v.as_str().map(str::to_string)))
    }

    fn array<T>(v: Option<Value>, f: impl Fn(&Value) -> Option<T>) -> Option<Vec<Option<T>>> {
        match v? {
            Value::Array(items) => Some(items.iter().map(f).collect()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

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

    #[test]
    fn lenient_fields_degrade_instead_of_failing() {
        #[derive(serde::Deserialize)]
        struct T {
            #[serde(default, deserialize_with = "lenient::i64")]
            a: Option<i64>,
            #[serde(default, deserialize_with = "lenient::vec_f64")]
            b: Option<Vec<Option<f64>>>,
            #[serde(default, deserialize_with = "lenient::string")]
            c: Option<String>,
        }
        let t: T = serde_json::from_str(r#"{"a":2.5,"b":[1,"x",3.5],"c":7}"#).unwrap();
        assert_eq!(t.a, None);
        assert_eq!(t.b, Some(vec![Some(1.0), None, Some(3.5)]));
        assert_eq!(t.c, None);
        let t: T = serde_json::from_str("{}").unwrap();
        assert!(t.a.is_none() && t.b.is_none() && t.c.is_none());
    }
}
