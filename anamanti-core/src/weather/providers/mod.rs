//! The concrete [`WeatherProvider`](crate::weather::WeatherProvider) backends, one per
//! file, each with `#[derive(Deserialize)]` response structs for its fixed-schema API.
//! Both ride the shared [`crate::http`] client with a per-request timeout.

mod openmeteo;
mod visualcrossing;

pub use openmeteo::OpenMeteoWeather;
pub use visualcrossing::VisualCrossingWeather;

#[cfg(test)]
pub(crate) use openmeteo::tests::fixture_report;

/// Per-request budget for every weather API call.
const WEATHER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// `User-Agent` component label for weather requests.
const COMPONENT: &str = "weather";
