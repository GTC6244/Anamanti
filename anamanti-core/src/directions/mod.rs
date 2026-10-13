//! Driving directions, distance, travel time, and **live traffic**, exposed to the
//! LLM as the `directions_lookup` tool (wired in `llm/rig.rs`).
//!
//! Enabled when a routing provider is configured (v1: **Mapbox**, `directions.provider
//! = "mapbox"` in the JSON config file) with a Mapbox token. The token is a secret,
//! seeded from `MAPBOX_TOKEN`/`MAPBOX_ACCESS_TOKEN` at boot and runtime-settable from
//! the config page's Tools tab. A lookup geocodes the origin + destination and asks the
//! provider for a route; the origin defaults to the household's full home address so "how
//! long to the airport?" works without naming a starting point. Everything is read-only.
//!
//! The provider is abstracted behind the [`DirectionsProvider`] trait so the tool is
//! testable offline and a second backend (Google, HERE, OSRM…) can be dropped in
//! later behind the same seam — mirroring how [`crate::calendar::CalendarSource`]
//! backs the calendar tool.
//!
//! v1 is **voice-only**: the tool returns a short, speakable summary (distance +
//! traffic-aware ETA + a plain-language traffic note). Rendering a map/route on the
//! display is a deferred follow-up (see `TODO.md §7`).

use std::sync::{Arc, OnceLock};
use std::time::Duration as StdDuration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

// The home address and unit preference live with the household record; re-exported
// here for the directions tool's callers.
pub use crate::household::{units_are_imperial, HomeAddress, LiveHomeLocation};

/// How to travel between the two points. `Driving` uses Mapbox's live-traffic
/// profile; the others are traffic-free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TravelMode {
    Driving,
    Walking,
    Cycling,
}

impl TravelMode {
    /// Parse the model's `mode` argument; unknown/empty defaults to [`Driving`].
    pub fn from_arg(mode: Option<&str>) -> Self {
        match mode.map(str::trim).unwrap_or("").to_lowercase().as_str() {
            "walk" | "walking" | "foot" | "pedestrian" => TravelMode::Walking,
            "bike" | "biking" | "cycle" | "cycling" | "bicycle" => TravelMode::Cycling,
            _ => TravelMode::Driving,
        }
    }

    /// The Mapbox Directions routing profile for this mode. `Driving` uses the
    /// **live-traffic** profile so the ETA reflects current conditions.
    fn mapbox_profile(self) -> &'static str {
        match self {
            TravelMode::Driving => "driving-traffic",
            TravelMode::Walking => "walking",
            TravelMode::Cycling => "cycling",
        }
    }

    /// A speakable word for the mode ("driving", "walking", "cycling").
    fn label(self) -> &'static str {
        match self {
            TravelMode::Driving => "driving",
            TravelMode::Walking => "walking",
            TravelMode::Cycling => "cycling",
        }
    }
}

/// A resolved route: the geocoded endpoint labels plus distance / duration. For a
/// live-traffic driving route, `duration_typical_seconds` is the same trip's usual
/// (no-traffic) time, used to describe how bad traffic is right now.
#[derive(Clone, Debug)]
pub struct Directions {
    pub origin_label: String,
    pub destination_label: String,
    pub distance_meters: f64,
    pub duration_seconds: f64,
    pub duration_typical_seconds: Option<f64>,
    pub mode: TravelMode,
}

/// A source of routes. Abstracted so the tool is testable offline and a second
/// routing backend can be added later behind the same seam.
#[async_trait]
pub trait DirectionsProvider: Send + Sync {
    /// Geocode `origin`/`destination` and return a route between them for `mode`.
    async fn directions(
        &self,
        origin: &str,
        destination: &str,
        mode: TravelMode,
    ) -> Result<Directions>;
}

/// A [`DirectionsProvider`] backed by the Mapbox Geocoding v6 + Directions v5 APIs.
/// Needs a `MAPBOX_TOKEN`. Driving routes use the `driving-traffic` profile, whose
/// `duration` reflects live traffic and which also returns a typical (no-traffic)
/// duration for comparison.
///
/// Mapbox only accepts the token as the `access_token` query parameter (no header
/// form), so every request goes through [`crate::http`]'s redacting helpers: a failed
/// request's error text never carries the URL or the token.
pub struct MapboxDirections {
    token: String,
    base_url: String,
}

/// Per-request budget for each Mapbox call.
const MAPBOX_TIMEOUT: StdDuration = StdDuration::from_secs(10);

/// `User-Agent` component label for Mapbox requests.
const COMPONENT: &str = "directions";

/// How long a geocoded origin stays memoized. The origin is almost always the
/// unchanged household address, so this saves one round trip per directions turn; a
/// day keeps a moved pin from lingering, and an edited address is a new key anyway.
const ORIGIN_GEOCODE_TTL: StdDuration = StdDuration::from_secs(24 * 3600);

/// Process-wide memo of geocoded origins, keyed by (API root, address string). Shared
/// across provider rebuilds (a settings save builds a new [`MapboxDirections`]).
fn origin_geocode_cache() -> &'static crate::cache::ToolCache {
    static CACHE: OnceLock<crate::cache::ToolCache> = OnceLock::new();
    CACHE.get_or_init(crate::cache::ToolCache::new)
}

/// A geocoded point: `(longitude, latitude, label)`.
type GeoPoint = (f64, f64, String);

/// Mapbox Geocoding v6 `forward` response (only the fields we read).
#[derive(Debug, Default, Deserialize)]
struct GeocodeResponse {
    #[serde(default)]
    features: Vec<GeocodeFeature>,
}

#[derive(Debug, Default, Deserialize)]
struct GeocodeFeature {
    #[serde(default)]
    geometry: Option<GeocodeGeometry>,
    #[serde(default)]
    properties: Option<GeocodeProperties>,
}

#[derive(Debug, Default, Deserialize)]
struct GeocodeGeometry {
    #[serde(default)]
    coordinates: Vec<f64>,
}

#[derive(Debug, Default, Deserialize)]
struct GeocodeProperties {
    #[serde(default)]
    full_address: Option<String>,
    #[serde(default)]
    place_formatted: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

impl GeocodeProperties {
    /// The best label: the first present of `full_address` / `place_formatted` /
    /// `name`, or `None` when that one is blank.
    fn label(&self) -> Option<&str> {
        self.full_address
            .as_deref()
            .or(self.place_formatted.as_deref())
            .or(self.name.as_deref())
            .filter(|s| !s.is_empty())
    }
}

/// Mapbox Directions v5 response (only the fields we read).
#[derive(Debug, Default, Deserialize)]
struct DirectionsResponse {
    #[serde(default)]
    routes: Vec<Route>,
}

#[derive(Debug, Default, Deserialize)]
struct Route {
    #[serde(default)]
    distance: Option<f64>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    duration_typical: Option<f64>,
    #[serde(default)]
    legs: Vec<RouteLeg>,
}

#[derive(Debug, Default, Deserialize)]
struct RouteLeg {
    #[serde(default)]
    duration_typical: Option<f64>,
}

impl Route {
    /// The typical (no-traffic) duration: the top-level `duration_typical` when
    /// present, else the sum of the legs' `duration_typical`. `None` when the profile
    /// doesn't provide it (walking/cycling).
    fn typical_duration(&self) -> Option<f64> {
        if let Some(d) = self.duration_typical {
            return Some(d);
        }
        let legs: Vec<f64> = self
            .legs
            .iter()
            .filter_map(|l| l.duration_typical)
            .collect();
        (!legs.is_empty()).then(|| legs.iter().sum())
    }
}

impl MapboxDirections {
    pub fn new(token: impl Into<String>) -> Self {
        Self::with_base_url("https://api.mapbox.com", token)
    }

    /// Point the client at a specific API root (overridable for tests).
    pub fn with_base_url(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    /// Forward-geocode a free-text place into `(longitude, latitude, label)` using
    /// the Mapbox Geocoding v6 API. Returns the best (first) match.
    async fn geocode(&self, query: &str) -> Result<GeoPoint> {
        let value: GeocodeResponse = crate::http::send_json(
            crate::http::get(
                format!("{}/search/geocode/v6/forward", self.base_url),
                COMPONENT,
            )
            .query(&[
                ("q", query),
                ("access_token", self.token.as_str()),
                ("limit", "1"),
            ]),
            MAPBOX_TIMEOUT,
            "Mapbox geocoding",
        )
        .await?;

        let feature = value
            .features
            .into_iter()
            .next()
            .with_context(|| format!("no location found for \"{query}\""))?;
        let coords = feature
            .geometry
            .map(|g| g.coordinates)
            .filter(|c| c.len() >= 2)
            .with_context(|| format!("geocoding result for \"{query}\" had no coordinates"))?;
        let label = feature
            .properties
            .as_ref()
            .and_then(GeocodeProperties::label)
            .unwrap_or(query)
            .to_string();
        Ok((coords[0], coords[1], label))
    }

    /// [`Self::geocode`] memoized per address string (see [`ORIGIN_GEOCODE_TTL`]), for
    /// the origin — normally the unchanged home address — so it isn't re-geocoded on
    /// every call. Failures are never memoized.
    async fn geocode_origin(&self, query: &str) -> Result<GeoPoint> {
        let cache = origin_geocode_cache();
        let key = crate::cache::ToolCache::key("mapbox_origin_geocode", &(&self.base_url, query));
        if let Some(hit) = key.as_deref().and_then(|k| cache.get_as::<GeoPoint>(k)) {
            return Ok(hit);
        }
        let point = self.geocode(query).await?;
        if let Some(key) = key {
            cache.put_as(key, &point, ORIGIN_GEOCODE_TTL);
        }
        Ok(point)
    }
}

#[async_trait]
impl DirectionsProvider for MapboxDirections {
    async fn directions(
        &self,
        origin: &str,
        destination: &str,
        mode: TravelMode,
    ) -> Result<Directions> {
        let (o_lon, o_lat, origin_label) = self.geocode_origin(origin).await?;
        let (d_lon, d_lat, destination_label) = self.geocode(destination).await?;

        let coords = format!("{o_lon:.6},{o_lat:.6};{d_lon:.6},{d_lat:.6}");
        let value: DirectionsResponse = crate::http::send_json(
            crate::http::get(
                format!(
                    "{}/directions/v5/mapbox/{}/{}",
                    self.base_url,
                    mode.mapbox_profile(),
                    coords
                ),
                COMPONENT,
            )
            .query(&[
                ("access_token", self.token.as_str()),
                ("overview", "false"),
                ("alternatives", "false"),
            ]),
            MAPBOX_TIMEOUT,
            "Mapbox directions",
        )
        .await?;

        let route = value
            .routes
            .into_iter()
            .next()
            .context("Mapbox returned no route between those points")?;
        let distance_meters = route.distance.context("route had no distance")?;
        let duration_seconds = route.duration.context("route had no duration")?;
        let duration_typical_seconds = route.typical_duration();

        Ok(Directions {
            origin_label,
            destination_label,
            distance_meters,
            duration_seconds,
            duration_typical_seconds,
            mode,
        })
    }
}

/// The directions backend [`from_token`] would build for this provider label + token,
/// or `None` when the tool would not be advertised (unsupported provider, or no
/// usable token). The single source of truth for "is directions on, and with what".
pub fn effective_backend(provider: &str, token: Option<&str>) -> Option<&'static str> {
    let provider = provider.trim().to_lowercase();
    if !(provider.is_empty() || provider == "mapbox") {
        return None;
    }
    crate::util::non_empty(token).map(|_| "mapbox")
}

/// Build the directions provider from an explicit token. Returns `None` (tool not
/// advertised) unless a supported provider is selected and a usable token is present
/// (see [`effective_backend`]).
///
/// - `provider` selects the backend (from `directions.provider`; default/only:
///   `mapbox`; empty is treated as `mapbox`).
/// - `token` is the Mapbox API token — a **secret**. It is seeded from `MAPBOX_TOKEN`/
///   `MAPBOX_ACCESS_TOKEN` at boot but is runtime-settable from the config page's Tools
///   tab, so it's passed in here rather than read from the environment.
/// - `imperial` (derived from the household `weather_units`) picks imperial vs metric.
///
/// Returns the built provider plus the `imperial` flag as a tuple carried on the
/// [`crate::settings::LlmFactory`]; the default origin ([`LiveHomeLocation`]) is not
/// set here — the caller (`llm::rig::tools_from_config`) injects the live handle so a
/// config-page location edit changes the origin without a restart.
pub fn from_token(
    provider: &str,
    token: Option<&str>,
    imperial: bool,
) -> Option<(Arc<dyn DirectionsProvider>, bool)> {
    if effective_backend(provider, Some("x")).is_none() {
        log::warn!(
            "directions.provider='{}' is not supported (only 'mapbox' in v1); \
             directions_lookup disabled",
            provider.trim().to_lowercase()
        );
        return None;
    }
    let token = crate::util::non_empty(token)?;

    log::info!(
        "directions_lookup enabled (mapbox; {} units; origin from the live household location)",
        if imperial { "imperial" } else { "metric" }
    );
    let provider: Arc<dyn DirectionsProvider> = Arc::new(MapboxDirections::new(token));
    Some((provider, imperial))
}

/// Render a route into a compact, speakable answer for the model to relay.
pub fn render_directions(dir: &Directions, imperial: bool) -> String {
    let distance = format_distance(dir.distance_meters, imperial);
    let duration = format_duration(dir.duration_seconds);
    let mut out = format!(
        "It's about {distance} from {} to {}, roughly {duration} {}.",
        dir.origin_label,
        dir.destination_label,
        dir.mode.label(),
    );
    if let Some(note) = traffic_note(dir) {
        out.push(' ');
        out.push_str(&note);
    }
    out
}

/// A plain-language traffic note for a driving route, comparing the live ETA to the
/// typical time. `None` for non-driving modes or when no typical time is available.
fn traffic_note(dir: &Directions) -> Option<String> {
    if dir.mode != TravelMode::Driving {
        return None;
    }
    let typical = dir.duration_typical_seconds?;
    if typical <= 0.0 {
        return None;
    }
    let delay = dir.duration_seconds - typical;
    // Ignore sub-2-minute wobble, and only call it "heavy"/"light" past 10%.
    let ratio = delay / typical;
    if delay >= 120.0 && ratio >= 0.10 {
        Some(format!(
            "Traffic is heavy right now — about {} slower than usual.",
            format_duration(delay)
        ))
    } else if delay <= -120.0 {
        Some("Traffic is lighter than usual.".to_string())
    } else {
        Some("Traffic is about normal.".to_string())
    }
}

/// Format a distance for speech, rounded to a natural precision.
fn format_distance(meters: f64, imperial: bool) -> String {
    if imperial {
        let miles = meters / 1609.344;
        if miles < 0.1 {
            let feet = (meters / 0.3048).round() as i64;
            format!("{feet} feet")
        } else if miles < 10.0 {
            format!("{miles:.1} miles")
        } else {
            format!("{} miles", miles.round() as i64)
        }
    } else {
        let km = meters / 1000.0;
        if km < 1.0 {
            format!("{} meters", (meters / 10.0).round() as i64 * 10)
        } else if km < 10.0 {
            format!("{km:.1} kilometers")
        } else {
            format!("{} kilometers", km.round() as i64)
        }
    }
}

/// Format a duration (seconds) as a short spoken phrase ("24 minutes",
/// "1 hour 10 minutes", "under a minute"); see [`crate::speech::DurationStyle::Minutes`].
fn format_duration(seconds: f64) -> String {
    crate::speech::duration(seconds, crate::speech::DurationStyle::Minutes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::test_support::{refused_base, serve_sequence, serve_statuses};

    #[test]
    fn travel_mode_parsing() {
        assert_eq!(TravelMode::from_arg(None), TravelMode::Driving);
        assert_eq!(TravelMode::from_arg(Some("drive")), TravelMode::Driving);
        assert_eq!(TravelMode::from_arg(Some("Walking")), TravelMode::Walking);
        assert_eq!(TravelMode::from_arg(Some("bike")), TravelMode::Cycling);
        assert_eq!(TravelMode::from_arg(Some("teleport")), TravelMode::Driving);
    }

    #[test]
    fn duration_formatting() {
        assert_eq!(format_duration(20.0), "under a minute");
        assert_eq!(format_duration(60.0), "1 minute");
        assert_eq!(format_duration(1440.0), "24 minutes");
        assert_eq!(format_duration(3600.0), "1 hour");
        assert_eq!(format_duration(4200.0), "1 hour 10 minutes");
    }

    #[test]
    fn distance_formatting_imperial_and_metric() {
        assert_eq!(format_distance(1609.344, true), "1.0 miles");
        assert_eq!(format_distance(32186.9, true), "20 miles");
        assert_eq!(format_distance(2500.0, false), "2.5 kilometers");
        assert_eq!(format_distance(25000.0, false), "25 kilometers");
    }

    fn driving(typical: Option<f64>, duration: f64) -> Directions {
        Directions {
            origin_label: "home".into(),
            destination_label: "airport".into(),
            distance_meters: 20000.0,
            duration_seconds: duration,
            duration_typical_seconds: typical,
            mode: TravelMode::Driving,
        }
    }

    #[test]
    fn traffic_note_reads_congestion() {
        // 20 min actual vs 15 min typical → heavy.
        let heavy = traffic_note(&driving(Some(900.0), 1200.0)).unwrap();
        assert!(heavy.contains("heavy"), "{heavy}");
        // Roughly equal → normal.
        assert_eq!(
            traffic_note(&driving(Some(900.0), 930.0)).unwrap(),
            "Traffic is about normal."
        );
        // Much faster than usual → light.
        assert!(traffic_note(&driving(Some(900.0), 600.0))
            .unwrap()
            .contains("lighter"));
        // No typical time (walking, or absent field) → no note.
        assert!(traffic_note(&driving(None, 1200.0)).is_none());
    }

    #[test]
    fn render_includes_distance_eta_and_traffic() {
        let out = render_directions(&driving(Some(900.0), 1200.0), true);
        assert!(out.contains("12 miles") || out.contains("miles"));
        assert!(out.contains("20 minutes"));
        assert!(out.contains("driving"));
        assert!(out.contains("heavy"));
    }

    #[tokio::test]
    async fn mapbox_geocodes_both_ends_then_routes() {
        // Two geocode responses (origin, destination) then one directions response.
        let geo_home = r#"{"features":[{"geometry":{"coordinates":[-97.7431,30.2672]},"properties":{"full_address":"Home, Austin, Texas"}}]}"#;
        let geo_airport = r#"{"features":[{"geometry":{"coordinates":[-97.6699,30.1975]},"properties":{"full_address":"Austin Airport"}}]}"#;
        let route =
            r#"{"routes":[{"distance":20000.0,"duration":1200.0,"duration_typical":900.0}]}"#;
        let (url, server) = serve_sequence(vec![
            geo_home.to_string(),
            geo_airport.to_string(),
            route.to_string(),
        ]);

        let mapbox = MapboxDirections::with_base_url(url, "tok");
        let dir = mapbox
            .directions("home", "the airport", TravelMode::Driving)
            .await
            .unwrap();
        assert_eq!(dir.origin_label, "Home, Austin, Texas");
        assert_eq!(dir.destination_label, "Austin Airport");
        assert_eq!(dir.distance_meters, 20000.0);
        assert_eq!(dir.duration_seconds, 1200.0);
        assert_eq!(dir.duration_typical_seconds, Some(900.0));

        let spoken = render_directions(&dir, true);
        assert!(spoken.contains("Austin Airport"));
        assert!(spoken.contains("heavy"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn home_origin_is_geocoded_once_across_calls() {
        let geo_home = r#"{"features":[{"geometry":{"coordinates":[-97.7,30.2]},"properties":{"full_address":"Home, Austin"}}]}"#;
        let geo_dest = r#"{"features":[{"geometry":{"coordinates":[-97.6,30.1]},"properties":{"full_address":"Airport"}}]}"#;
        let route = r#"{"routes":[{"distance":1000.0,"duration":600.0}]}"#;
        // First call: origin + destination geocodes + route. Second call: only the
        // destination geocode + route — a re-geocoded origin would consume the
        // destination body and the route would then fail to geocode.
        let (url, server) = serve_sequence(vec![
            geo_home.to_string(),
            geo_dest.to_string(),
            route.to_string(),
            geo_dest.to_string(),
            route.to_string(),
        ]);
        let mapbox = MapboxDirections::with_base_url(url, "tok");
        let home = "1 Memo Test Rd, Austin";
        let first = mapbox
            .directions(home, "airport", TravelMode::Driving)
            .await
            .unwrap();
        // A rebuilt provider (settings save) still shares the memo.
        let mapbox = MapboxDirections::with_base_url(mapbox.base_url.clone(), "tok");
        let second = mapbox
            .directions(home, "airport", TravelMode::Driving)
            .await
            .unwrap();
        assert_eq!(first.origin_label, "Home, Austin");
        assert_eq!(second.origin_label, "Home, Austin");
        assert_eq!(second.destination_label, "Airport");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn mapbox_401_error_does_not_leak_the_token() {
        let (url, server) = serve_statuses(vec![(
            "401 Unauthorized",
            r#"{"message":"Not Authorized - Invalid Token"}"#.to_string(),
        )]);
        let mapbox = MapboxDirections::with_base_url(url, "pk.SECRET-MAPBOX-TOKEN");
        let err = mapbox
            .directions("Leak Test Origin 401", "b", TravelMode::Driving)
            .await
            .unwrap_err();
        let text = format!("{err} | {err:#} | {err:?}");
        assert!(!text.contains("SECRET-MAPBOX-TOKEN"), "{text}");
        assert!(text.contains("401"), "{text}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn mapbox_connection_error_does_not_leak_the_token() {
        let mapbox = MapboxDirections::with_base_url(refused_base(), "pk.SECRET-MAPBOX-TOKEN");
        let err = mapbox
            .directions("Leak Test Origin refused", "b", TravelMode::Driving)
            .await
            .unwrap_err();
        let text = format!("{err} | {err:#} | {err:?}");
        assert!(!text.contains("SECRET-MAPBOX-TOKEN"), "{text}");
        assert!(!text.contains("access_token"), "{text}");
    }

    #[test]
    fn effective_backend_mirrors_from_token() {
        assert_eq!(effective_backend("mapbox", Some("t")), Some("mapbox"));
        assert_eq!(effective_backend("", Some("t")), Some("mapbox"));
        assert_eq!(effective_backend("mapbox", Some("  ")), None);
        assert_eq!(effective_backend("google", Some("t")), None);
        for (p, t) in [("mapbox", Some("t")), ("", None), ("google", Some("t"))] {
            assert_eq!(
                effective_backend(p, t).is_some(),
                from_token(p, t, false).is_some()
            );
        }
    }

    #[test]
    fn typical_duration_sums_legs_when_top_level_absent() {
        let r: Route =
            serde_json::from_str(r#"{"legs":[{"duration_typical":100},{"duration_typical":50}]}"#)
                .unwrap();
        assert_eq!(r.typical_duration(), Some(150.0));
        let r: Route = serde_json::from_str(r#"{"duration_typical":7,"legs":[]}"#).unwrap();
        assert_eq!(r.typical_duration(), Some(7.0));
        let r: Route = serde_json::from_str(r#"{"legs":[{}]}"#).unwrap();
        assert_eq!(r.typical_duration(), None);
    }
}
