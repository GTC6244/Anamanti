//! The **Visual Crossing** Timeline backend (the default `weather.provider`).

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{NaiveDateTime, Timelike};
use serde::Deserialize;
use url::Url;

use super::{COMPONENT, WEATHER_TIMEOUT};
use crate::weather::{
    day_summary, hour_label, lenient, round_temp, take_hours, weather_description, weekday_label,
    window_start, CurrentConditions, DailyForecast, ForecastWhen, HourlyForecast, WeatherProvider,
    WeatherReport,
};

/// A [`WeatherProvider`] backed by the [Visual Crossing Timeline API]. Unlike Open-Meteo
/// this resolves a free-text (or full postal) location *and* returns current conditions +
/// the daily forecast in a single keyed request — no separate geocoding step. Visual
/// Crossing's text `icon`s are mapped back to WMO codes ([`vc_icon_to_wmo`]) so the device
/// renders the same bundled icon set regardless of which provider produced the report.
///
/// The API key is a **secret** (seeded from `VISUALCROSSING_API_KEY`); it is passed in at
/// construction rather than read from the environment here. The Timeline API only
/// accepts it as the `key` query parameter, so requests go through [`crate::http`]'s
/// redacting helpers: a failed request's error text never carries the URL or the key.
///
/// [Visual Crossing Timeline API]: https://www.visualcrossing.com/resources/documentation/weather-api/timeline-weather-api/
pub struct VisualCrossingWeather {
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
        Self {
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
        let value: Timeline = crate::http::send_json(
            crate::http::get(url, COMPONENT),
            WEATHER_TIMEOUT,
            "Visual Crossing timeline",
        )
        .await?;

        Ok(report_from_visualcrossing(&value, location, imperial, when))
    }
}

// ---------------------------------------------------------------------------
// Response shapes (only the fields we read; every field tolerant — see `lenient`).
// ---------------------------------------------------------------------------

/// Timeline API response.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Timeline {
    #[serde(default, deserialize_with = "lenient::string")]
    resolved_address: Option<String>,
    #[serde(default)]
    current_conditions: Option<VcPoint>,
    #[serde(default)]
    days: Option<Vec<VcDay>>,
}

/// One day of the timeline (one object per day, unlike Open-Meteo's parallel arrays).
#[derive(Debug, Default, Deserialize)]
struct VcDay {
    /// `YYYY-MM-DD`.
    #[serde(default, deserialize_with = "lenient::string")]
    datetime: Option<String>,
    #[serde(default, deserialize_with = "lenient::f64")]
    tempmax: Option<f64>,
    #[serde(default, deserialize_with = "lenient::f64")]
    tempmin: Option<f64>,
    #[serde(default, deserialize_with = "lenient::f64")]
    precipprob: Option<f64>,
    #[serde(default, deserialize_with = "lenient::string")]
    icon: Option<String>,
    #[serde(default)]
    hours: Option<Vec<VcPoint>>,
}

/// Current conditions or one hour (same field names).
#[derive(Debug, Default, Deserialize)]
struct VcPoint {
    /// `HH:MM:SS` local clock time.
    #[serde(default, deserialize_with = "lenient::string")]
    datetime: Option<String>,
    #[serde(default, deserialize_with = "lenient::f64")]
    temp: Option<f64>,
    #[serde(default, deserialize_with = "lenient::f64")]
    feelslike: Option<f64>,
    #[serde(default, deserialize_with = "lenient::f64")]
    precipprob: Option<f64>,
    #[serde(default, deserialize_with = "lenient::string")]
    icon: Option<String>,
}

/// Map a Visual Crossing timeline body into a [`WeatherReport`]. Missing fields
/// degrade gracefully (zeros/empties, WMO code 0) rather than failing the whole lookup —
/// matching the Open-Meteo parser's contract.
fn report_from_visualcrossing(
    value: &Timeline,
    query: &str,
    imperial: bool,
    when: ForecastWhen,
) -> WeatherReport {
    let label = value
        .resolved_address
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(query)
        .to_string();

    let days = value.days.as_deref().unwrap_or_default();
    let daily = daily_from(days);
    let all_hours = hours_from(days);

    let cc = value.current_conditions.as_ref();
    // Local "now" = today's date (days[0]) at the current-conditions clock time.
    let now_local = daily
        .first()
        .map(|d| d.date.clone())
        .and_then(|date| {
            let tstr = cc.and_then(|c| c.datetime.as_deref()).unwrap_or("00:00:00");
            NaiveDateTime::parse_from_str(&format!("{date} {tstr}"), "%Y-%m-%d %H:%M:%S").ok()
        })
        .or_else(|| all_hours.first().map(|(t, _)| *t))
        .unwrap_or_default();
    let hourly = take_hours(&all_hours, window_start(when, now_local));

    let (current, when_label) = match when {
        ForecastWhen::Now => {
            let (today_high, today_low) = daily.first().map(|d| (d.high, d.low)).unwrap_or((0, 0));
            let icon = cc.and_then(|c| c.icon.as_deref()).unwrap_or("");
            let code = vc_icon_to_wmo(icon);
            (
                CurrentConditions {
                    temp: round_temp(cc.and_then(|c| c.temp)),
                    feels_like: round_temp(cc.and_then(|c| c.feelslike)),
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
        daily,
        layout: String::new(),
    }
}

/// A whole-percent precipitation probability; absent → 0.
fn precip(p: Option<f64>) -> i32 {
    p.map(|p| p.round() as i32).unwrap_or(0)
}

/// Flatten the per-day `hours` arrays into a single time-sorted list. Each hour's local
/// datetime is its parent day's `datetime` (a date) plus the hour's own `datetime` (a
/// `HH:MM:SS` clock time). Day/night comes from the hour's icon suffix, falling back to
/// a 06:00–20:00 daytime window when the icon is absent.
fn hours_from(days: &[VcDay]) -> Vec<(NaiveDateTime, HourlyForecast)> {
    let mut out = Vec::new();
    for day in days {
        let date = day.datetime.as_deref().unwrap_or("");
        let Some(hours) = &day.hours else {
            continue;
        };
        for h in hours {
            let tstr = h.datetime.as_deref().unwrap_or("");
            let Ok(dt) =
                NaiveDateTime::parse_from_str(&format!("{date} {tstr}"), "%Y-%m-%d %H:%M:%S")
            else {
                continue;
            };
            let icon = h.icon.as_deref().unwrap_or("");
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
                    temp: round_temp(h.temp),
                    precip_prob: precip(h.precipprob),
                    is_day,
                },
            ));
        }
    }
    out.sort_by_key(|(t, _)| *t);
    out
}

/// Build the daily forecast list from the `days` array.
fn daily_from(days: &[VcDay]) -> Vec<DailyForecast> {
    days.iter()
        .map(|day| {
            let date = day.datetime.clone().unwrap_or_default();
            let icon = day.icon.as_deref().unwrap_or("");
            DailyForecast {
                weekday: weekday_label(&date),
                date,
                weather_code: vc_icon_to_wmo(icon),
                high: round_temp(day.tempmax),
                low: round_temp(day.tempmin),
                precip_prob: precip(day.precipprob),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::test_support::{refused_base, serve_sequence, serve_statuses};
    use chrono::NaiveDate;

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

    fn timeline(s: &str) -> Timeline {
        serde_json::from_str(s).unwrap()
    }

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
        let report =
            report_from_visualcrossing(&timeline(VC_TIMELINE), "Austin", true, ForecastWhen::Now);
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
        assert_eq!(report.hourly[4].precip_prob, 20);
        // Daily row: one entry per day, icons mapped.
        assert_eq!(report.daily.len(), 7);
        assert_eq!(report.daily[3].weather_code, 80);
        assert_eq!(report.daily[6].precip_prob, 90);
    }

    #[test]
    fn visualcrossing_future_day_starts_at_8am_with_day_summary() {
        let date = NaiveDate::from_ymd_opt(2026, 9, 26).unwrap();
        let report = report_from_visualcrossing(
            &timeline(VC_TIMELINE),
            "Austin",
            true,
            ForecastWhen::Day(date),
        );
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
    fn sparse_timeline_degrades_to_defaults() {
        let report = report_from_visualcrossing(
            &timeline(r#"{"resolvedAddress":"","days":[{"tempmax":"hot"}]}"#),
            "Austin",
            false,
            ForecastWhen::Now,
        );
        assert_eq!(report.location_label, "Austin");
        assert_eq!(report.daily.len(), 1);
        assert_eq!(report.daily[0].high, 0);
        assert!(report.hourly.is_empty());
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

    #[tokio::test]
    async fn vc_401_error_does_not_leak_the_key() {
        let (url, server) = serve_statuses(vec![(
            "401 Unauthorized",
            r#"{"errorCode":401,"message":"No account found with API key 'VC-SECRET-KEY'"}"#
                .to_string(),
        )]);
        let provider = VisualCrossingWeather::with_base_url(url, "VC-SECRET-KEY");
        let err = provider
            .fetch("Austin", true, ForecastWhen::Now)
            .await
            .unwrap_err();
        let text = format!("{err} | {err:#} | {err:?}");
        assert!(!text.contains("VC-SECRET-KEY"), "{text}");
        assert!(text.contains("401"), "{text}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn vc_connection_error_does_not_leak_the_key() {
        let provider = VisualCrossingWeather::with_base_url(refused_base(), "VC-SECRET-KEY");
        let err = provider
            .fetch("Austin", true, ForecastWhen::Now)
            .await
            .unwrap_err();
        let text = format!("{err} | {err:#} | {err:?}");
        assert!(!text.contains("VC-SECRET-KEY"), "{text}");
        assert!(!text.contains("key="), "{text}");
    }
}
