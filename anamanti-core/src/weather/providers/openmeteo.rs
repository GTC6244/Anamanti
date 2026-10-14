//! The keyless **Open-Meteo** backend: its geocoding API resolves a place name to
//! coordinates, then its forecast API returns current conditions + hourly + daily
//! parallel arrays. Also the automatic fallback when no Visual Crossing key is set.

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::NaiveDateTime;
use serde::Deserialize;

use super::{COMPONENT, WEATHER_TIMEOUT};
use crate::weather::{
    day_summary, hour_label, lenient, round_temp, take_hours, weather_description, weekday_label,
    window_start, CurrentConditions, DailyForecast, ForecastWhen, HourlyForecast, WeatherProvider,
    WeatherReport,
};

/// A [`WeatherProvider`] backed by the keyless Open-Meteo geocoding + forecast APIs.
pub struct OpenMeteoWeather {
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
        Self {
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
        let value: GeoResponse = crate::http::send_json(
            crate::http::get(format!("{}/v1/search", self.geo_base), COMPONENT).query(&[
                ("name", query),
                ("count", "1"),
                ("format", "json"),
            ]),
            WEATHER_TIMEOUT,
            "Open-Meteo geocoding",
        )
        .await?;

        let Some(first) = value.results.and_then(|r| r.into_iter().next()) else {
            return Ok(None);
        };
        let lat = first.latitude.context("geocoding result had no latitude")?;
        let lon = first
            .longitude
            .context("geocoding result had no longitude")?;
        Ok(Some((lat, lon, first.label(query))))
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
        let value: Forecast = crate::http::send_json(
            crate::http::get(format!("{}/v1/forecast", self.forecast_base), COMPONENT).query(&[
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
            ]),
            WEATHER_TIMEOUT,
            "Open-Meteo forecast",
        )
        .await?;

        Ok(report_from_forecast(&value, &label, imperial, when))
    }
}

// ---------------------------------------------------------------------------
// Response shapes (only the fields we read; every field tolerant — see `lenient`).
// ---------------------------------------------------------------------------

/// Geocoding `/v1/search` response.
#[derive(Debug, Default, Deserialize)]
struct GeoResponse {
    #[serde(default)]
    results: Option<Vec<GeoResult>>,
}

#[derive(Debug, Default, Deserialize)]
struct GeoResult {
    #[serde(default, deserialize_with = "lenient::f64")]
    latitude: Option<f64>,
    #[serde(default, deserialize_with = "lenient::f64")]
    longitude: Option<f64>,
    #[serde(default, deserialize_with = "lenient::string")]
    name: Option<String>,
    /// State / region.
    #[serde(default, deserialize_with = "lenient::string")]
    admin1: Option<String>,
}

impl GeoResult {
    /// A readable place label: `name`, plus `admin1` (state/region) when it adds
    /// information. Falls back to the query.
    fn label(&self, query: &str) -> String {
        let name = self.name.as_deref().filter(|s| !s.is_empty());
        let admin1 = self.admin1.as_deref().filter(|s| !s.is_empty());
        match (name, admin1) {
            (Some(n), Some(a)) if a != n => format!("{n}, {a}"),
            (Some(n), _) => n.to_string(),
            _ => query.to_string(),
        }
    }
}

/// Forecast `/v1/forecast` response.
#[derive(Debug, Default, Deserialize)]
struct Forecast {
    #[serde(default)]
    current: Option<Current>,
    #[serde(default)]
    hourly: Option<Hourly>,
    #[serde(default)]
    daily: Option<Daily>,
}

#[derive(Debug, Default, Deserialize)]
struct Current {
    #[serde(default, deserialize_with = "lenient::string")]
    time: Option<String>,
    #[serde(default, deserialize_with = "lenient::f64")]
    temperature_2m: Option<f64>,
    #[serde(default, deserialize_with = "lenient::f64")]
    apparent_temperature: Option<f64>,
    #[serde(default, deserialize_with = "lenient::i64")]
    weather_code: Option<i64>,
    #[serde(default, deserialize_with = "lenient::i64")]
    is_day: Option<i64>,
}

/// Parallel per-hour arrays.
#[derive(Debug, Default, Deserialize)]
struct Hourly {
    #[serde(default, deserialize_with = "lenient::vec_string")]
    time: Option<Vec<Option<String>>>,
    #[serde(default, deserialize_with = "lenient::vec_i64")]
    weather_code: Option<Vec<Option<i64>>>,
    #[serde(default, deserialize_with = "lenient::vec_f64")]
    temperature_2m: Option<Vec<Option<f64>>>,
    #[serde(default, deserialize_with = "lenient::vec_i64")]
    precipitation_probability: Option<Vec<Option<i64>>>,
    #[serde(default, deserialize_with = "lenient::vec_i64")]
    is_day: Option<Vec<Option<i64>>>,
}

/// Parallel per-day arrays.
#[derive(Debug, Default, Deserialize)]
struct Daily {
    #[serde(default, deserialize_with = "lenient::vec_string")]
    time: Option<Vec<Option<String>>>,
    #[serde(default, deserialize_with = "lenient::vec_i64")]
    weather_code: Option<Vec<Option<i64>>>,
    #[serde(default, deserialize_with = "lenient::vec_f64")]
    temperature_2m_max: Option<Vec<Option<f64>>>,
    #[serde(default, deserialize_with = "lenient::vec_f64")]
    temperature_2m_min: Option<Vec<Option<f64>>>,
    #[serde(default, deserialize_with = "lenient::vec_i64")]
    precipitation_probability_max: Option<Vec<Option<i64>>>,
}

/// Element `i` of an optional parallel array, `None` when the array or slot is absent.
fn at<T: Copy>(arr: &Option<Vec<Option<T>>>, i: usize) -> Option<T> {
    arr.as_ref().and_then(|a| a.get(i).copied().flatten())
}

/// Map an Open-Meteo forecast body into a [`WeatherReport`]. Missing fields
/// degrade gracefully to zeros/empties rather than failing the whole lookup.
fn report_from_forecast(
    value: &Forecast,
    label: &str,
    imperial: bool,
    when: ForecastWhen,
) -> WeatherReport {
    let daily_forecast = daily_from(value.daily.as_ref());
    let all_hours = hours_from(value.hourly.as_ref());

    // Local "now" from the `current.time` field, falling back to the first hourly slot.
    let now_local = value
        .current
        .as_ref()
        .and_then(|c| c.time.as_deref())
        .and_then(|s| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M").ok())
        .or_else(|| all_hours.first().map(|(t, _)| *t))
        .unwrap_or_default();
    let hourly = take_hours(&all_hours, window_start(when, now_local));

    let (current, when_label) = match when {
        ForecastWhen::Now => {
            let cur = value.current.as_ref();
            let (today_high, today_low) = daily_forecast
                .first()
                .map(|d| (d.high, d.low))
                .unwrap_or((0, 0));
            let code = cur.and_then(|c| c.weather_code).unwrap_or(0) as i32;
            (
                CurrentConditions {
                    temp: round_temp(cur.and_then(|c| c.temperature_2m)),
                    feels_like: round_temp(cur.and_then(|c| c.apparent_temperature)),
                    weather_code: code,
                    is_day: cur.and_then(|c| c.is_day).map(|d| d != 0).unwrap_or(true),
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
        daily: daily_forecast,
        layout: String::new(),
    }
}

/// Build the hourly list from the `hourly` object's parallel arrays.
fn hours_from(hourly: Option<&Hourly>) -> Vec<(NaiveDateTime, HourlyForecast)> {
    let Some(hourly) = hourly else {
        return Vec::new();
    };
    let Some(times) = &hourly.time else {
        return Vec::new();
    };
    times
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            let dt = NaiveDateTime::parse_from_str(t.as_deref()?, "%Y-%m-%dT%H:%M").ok()?;
            Some((
                dt,
                HourlyForecast {
                    time: hour_label(&dt),
                    weather_code: at(&hourly.weather_code, i).unwrap_or(0) as i32,
                    temp: round_temp(at(&hourly.temperature_2m, i)),
                    precip_prob: at(&hourly.precipitation_probability, i).unwrap_or(0) as i32,
                    is_day: at(&hourly.is_day, i).map(|d| d != 0).unwrap_or(true),
                },
            ))
        })
        .collect()
}

/// Build the daily forecast list from the `daily` object's parallel arrays.
fn daily_from(daily: Option<&Daily>) -> Vec<DailyForecast> {
    let Some(daily) = daily else {
        return Vec::new();
    };
    let Some(dates) = &daily.time else {
        return Vec::new();
    };
    dates
        .iter()
        .enumerate()
        .map(|(i, date)| {
            let date = date.clone().unwrap_or_default();
            DailyForecast {
                weekday: weekday_label(&date),
                date,
                weather_code: at(&daily.weather_code, i).unwrap_or(0) as i32,
                high: round_temp(at(&daily.temperature_2m_max, i)),
                low: round_temp(at(&daily.temperature_2m_min, i)),
                precip_prob: at(&daily.precipitation_probability_max, i).unwrap_or(0) as i32,
            }
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::http::test_support::serve_sequence;
    use chrono::NaiveDate;

    const GEO: &str = r#"{"results":[{"name":"Austin","latitude":30.2672,"longitude":-97.7431,"admin1":"Texas","country":"United States"}]}"#;
    pub(crate) const FORECAST: &str = r#"{
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

    fn forecast(s: &str) -> Forecast {
        serde_json::from_str(s).unwrap()
    }

    /// The fixture report used by the render tests.
    pub(crate) fn fixture_report(label: &str, imperial: bool) -> WeatherReport {
        report_from_forecast(&forecast(FORECAST), label, imperial, ForecastWhen::Now)
    }

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
    fn parses_forecast_body() {
        let report = report_from_forecast(
            &forecast(FORECAST),
            "Austin, Texas",
            true,
            ForecastWhen::Now,
        );
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
                                           // The 7-day row parses every day.
        assert_eq!(report.daily.len(), 7);
        assert_eq!(report.daily[6].weather_code, 95);
        assert_eq!(report.daily[6].precip_prob, 90);
        assert_eq!(report.daily[0].weekday, "Fri");
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
        let report = report_from_forecast(
            &serde_json::from_value(value).unwrap(),
            "Austin",
            true,
            ForecastWhen::Day(date),
        );
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
        let report = report_from_forecast(&forecast(FORECAST), "x", false, ForecastWhen::Now);
        assert_eq!(report.units, "metric");
    }

    #[test]
    fn sparse_forecast_degrades_to_defaults() {
        // Missing sections and odd field types don't fail the parse.
        let report = report_from_forecast(
            &forecast(
                r#"{"current":{"temperature_2m":"warm","is_day":0},"hourly":{"time":["bad"]}}"#,
            ),
            "x",
            true,
            ForecastWhen::Now,
        );
        assert_eq!(report.current.temp, 0);
        assert!(!report.current.is_day);
        assert!(report.hourly.is_empty());
        assert!(report.daily.is_empty());
    }

    #[test]
    fn geocode_label_adds_a_distinct_region() {
        let g = |n: Option<&str>, a: Option<&str>| GeoResult {
            name: n.map(str::to_string),
            admin1: a.map(str::to_string),
            ..Default::default()
        };
        assert_eq!(g(Some("Austin"), Some("Texas")).label("q"), "Austin, Texas");
        assert_eq!(g(Some("Texas"), Some("Texas")).label("q"), "Texas");
        assert_eq!(g(None, Some("Texas")).label("q"), "q");
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
}
