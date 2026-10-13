//! Provider selection: which weather backend `weather.enabled` / `weather.provider` /
//! the Visual Crossing key actually produce. [`effective_backend`] is the single
//! decision; [`from_config`] builds from it and the admin pages display it, so the two
//! can't drift.

use std::sync::Arc;

use super::cache::{weather_cache, CachingWeatherProvider};
use super::{OpenMeteoWeather, VisualCrossingWeather, WeatherProvider};

/// The weather backend a configuration resolves to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WeatherBackend {
    /// Weather is turned off.
    Disabled,
    /// Keyless Open-Meteo — selected, or the fallback.
    OpenMeteo,
    /// Visual Crossing — selected (or the default) with a key present.
    VisualCrossing,
}

impl WeatherBackend {
    /// The stable label the admin pages use: `disabled` / `openmeteo` / `visualcrossing`.
    pub fn as_str(self) -> &'static str {
        match self {
            WeatherBackend::Disabled => "disabled",
            WeatherBackend::OpenMeteo => "openmeteo",
            WeatherBackend::VisualCrossing => "visualcrossing",
        }
    }
}

/// How a `weather.provider` label parses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProviderChoice {
    OpenMeteo,
    /// Visual Crossing — also the empty/unset default.
    VisualCrossing,
    Unknown,
}

fn parse_provider(provider: &str) -> ProviderChoice {
    match provider.trim().to_lowercase().as_str() {
        "openmeteo" | "open-meteo" | "open_meteo" => ProviderChoice::OpenMeteo,
        "" | "visualcrossing" | "visual-crossing" | "visual_crossing" => {
            ProviderChoice::VisualCrossing
        }
        _ => ProviderChoice::Unknown,
    }
}

/// Whether `provider` selects Visual Crossing (including the empty/unset default), so a
/// caller can explain an Open-Meteo result as "no key" rather than "chosen".
pub fn selects_visualcrossing(provider: &str) -> bool {
    parse_provider(provider) == ProviderChoice::VisualCrossing
}

/// The backend [`from_config`] builds for these inputs — mirror it exactly when showing
/// "what weather is really using":
/// - disabled ⇒ [`WeatherBackend::Disabled`];
/// - `openmeteo` ⇒ Open-Meteo;
/// - `visualcrossing` / empty ⇒ Visual Crossing when `key_set`, else the keyless
///   Open-Meteo fallback;
/// - any other label ⇒ the Open-Meteo fallback, **even with a key**.
pub fn effective_backend(enabled: bool, provider: &str, key_set: bool) -> WeatherBackend {
    if !enabled {
        return WeatherBackend::Disabled;
    }
    match parse_provider(provider) {
        ProviderChoice::VisualCrossing if key_set => WeatherBackend::VisualCrossing,
        ProviderChoice::VisualCrossing | ProviderChoice::OpenMeteo | ProviderChoice::Unknown => {
            WeatherBackend::OpenMeteo
        }
    }
}

/// Build the configured weather provider when weather is enabled, or `None` when it is
/// disabled. When enabled the result is always `Some` — the tool/push are still gated on
/// a home location being set (an empty location makes [`WeatherProvider::fetch`] fail
/// gracefully).
///
/// `provider` selects the backend (`weather.provider`; see [`effective_backend`]):
/// - `visualcrossing` (default; also the empty/unset value) — the [`VisualCrossingWeather`]
///   Timeline API, using `api_key` (seeded from the `VISUALCROSSING_API_KEY` secret). If
///   that key is missing it falls back to keyless Open-Meteo so weather still works.
/// - `openmeteo` — the keyless [`OpenMeteoWeather`] backend.
/// - anything else — a warning, then the Open-Meteo fallback.
///
/// Cheap to call repeatedly (the ambient push rebuilds every tick): providers hold no
/// client of their own, they share [`crate::http::shared_client`].
pub fn from_config(
    enabled: bool,
    provider: &str,
    api_key: Option<&str>,
) -> Option<Arc<dyn WeatherProvider>> {
    let key = crate::util::non_empty(api_key);
    let backend = effective_backend(enabled, provider, key.is_some());
    let (provider, label): (Arc<dyn WeatherProvider>, &'static str) = match (backend, key) {
        (WeatherBackend::Disabled, _) => return None,
        (WeatherBackend::VisualCrossing, Some(key)) => {
            log::info!("weather enabled (Visual Crossing Timeline API)");
            (Arc::new(VisualCrossingWeather::new(key)), "visualcrossing")
        }
        _ => {
            match parse_provider(provider) {
                ProviderChoice::OpenMeteo => {
                    log::info!("weather enabled (Open-Meteo; keyless forecast + geocoding)")
                }
                ProviderChoice::VisualCrossing => log::warn!(
                    "weather.provider=visualcrossing but VISUALCROSSING_API_KEY is unset; \
                     falling back to keyless Open-Meteo"
                ),
                ProviderChoice::Unknown => log::warn!(
                    "weather.provider='{}' is not supported (use 'visualcrossing' or \
                     'openmeteo'); falling back to keyless Open-Meteo",
                    provider.trim().to_lowercase()
                ),
            }
            (Arc::new(OpenMeteoWeather::new()), "openmeteo")
        }
    };
    // Wrap every provider in the shared response cache. `from_config` is the single
    // constructor for weather providers (the System-1 fast path, the `weather_lookup`
    // LLM tool, and the ambient push all build through it), and the cache is process-wide,
    // so a repeated forecast for the same place+units inside the TTL is served locally
    // regardless of which caller asks — turning a cold ~1.3 s fetch into a warm lookup.
    // The TTL is per-tool and configurable (`tool_cache.weather_lookup`, default 60 min).
    Some(Arc::new(CachingWeatherProvider::new(
        provider,
        weather_cache(),
        label,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn effective_backend_covers_every_branch() {
        use WeatherBackend::*;
        assert_eq!(effective_backend(false, "visualcrossing", true), Disabled);
        assert_eq!(
            effective_backend(true, "visualcrossing", true),
            VisualCrossing
        );
        assert_eq!(
            effective_backend(true, "Visual-Crossing", true),
            VisualCrossing
        );
        assert_eq!(effective_backend(true, "", true), VisualCrossing);
        assert_eq!(effective_backend(true, "visualcrossing", false), OpenMeteo);
        assert_eq!(effective_backend(true, "open_meteo", true), OpenMeteo);
        assert_eq!(effective_backend(true, "openmeteo", false), OpenMeteo);
    }

    #[test]
    fn unknown_provider_with_a_key_is_open_meteo_not_visual_crossing() {
        // The admin page used to report Visual Crossing here, while from_config built
        // Open-Meteo (D12).
        assert_eq!(
            effective_backend(true, "not-a-provider", true),
            WeatherBackend::OpenMeteo
        );
        assert_eq!(
            effective_backend(true, "not-a-provider", true).as_str(),
            "openmeteo"
        );
        assert!(!selects_visualcrossing("not-a-provider"));
        assert!(selects_visualcrossing(""));
    }
}
