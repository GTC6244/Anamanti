//! Driving directions, distance, travel time, and **live traffic**, exposed to the
//! LLM as the `directions_lookup` tool (wired in `llm/rig.rs`).
//!
//! Enabled when a routing provider is configured (v1: **Mapbox**, via `MAPBOX_TOKEN`
//! and the default `AMBIENT_DIRECTIONS_PROVIDER=mapbox`). A lookup geocodes the
//! origin + destination and asks the provider for a route; the origin defaults to the
//! device's `AMBIENT_HOME_LOCATION` so "how long to the airport?" works without
//! naming a starting point. Everything is read-only.
//!
//! The provider is abstracted behind the [`DirectionsProvider`] trait so the tool is
//! testable offline and a second backend (Google, HERE, OSRM…) can be dropped in
//! later behind the same seam — mirroring how [`crate::calendar::CalendarSource`]
//! backs the calendar tool.
//!
//! v1 is **voice-only**: the tool returns a short, speakable summary (distance +
//! traffic-aware ETA + a plain-language traffic note). Rendering a map/route on the
//! display is a deferred follow-up (see `TODO.md §7`).

use std::env;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;

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

/// The wired directions capability: the routing provider, the default origin (the
/// device's home location) used when the user names only a destination, and whether
/// to speak distances in imperial units.
pub struct DirectionsConfig {
    pub provider: Arc<dyn DirectionsProvider>,
    pub default_origin: Option<String>,
    pub imperial: bool,
}

/// A [`DirectionsProvider`] backed by the Mapbox Geocoding v6 + Directions v5 APIs.
/// Needs a `MAPBOX_TOKEN`. Driving routes use the `driving-traffic` profile, whose
/// `duration` reflects live traffic and which also returns a typical (no-traffic)
/// duration for comparison.
pub struct MapboxDirections {
    client: reqwest::Client,
    token: String,
    base_url: String,
}

impl MapboxDirections {
    pub fn new(token: impl Into<String>) -> Self {
        Self::with_base_url("https://api.mapbox.com", token)
    }

    /// Point the client at a specific API root (overridable for tests).
    pub fn with_base_url(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(StdDuration::from_secs(10))
            .user_agent("ambient-orchestrator/0.1 (directions)")
            .build()
            .unwrap_or_default();
        Self {
            client,
            token: token.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    /// Forward-geocode a free-text place into `(longitude, latitude, label)` using
    /// the Mapbox Geocoding v6 API. Returns the best (first) match.
    async fn geocode(&self, query: &str) -> Result<(f64, f64, String)> {
        let value: Value = self
            .client
            .get(format!("{}/search/geocode/v6/forward", self.base_url))
            .query(&[
                ("q", query),
                ("access_token", self.token.as_str()),
                ("limit", "1"),
            ])
            .send()
            .await
            .context("GET Mapbox geocoding")?
            .error_for_status()
            .context("Mapbox geocoding returned an error status")?
            .json()
            .await
            .context("parsing Mapbox geocoding JSON")?;

        let feature = value
            .get("features")
            .and_then(Value::as_array)
            .and_then(|f| f.first())
            .with_context(|| format!("no location found for \"{query}\""))?;
        let coords = feature
            .get("geometry")
            .and_then(|g| g.get("coordinates"))
            .and_then(Value::as_array)
            .filter(|c| c.len() >= 2)
            .with_context(|| format!("geocoding result for \"{query}\" had no coordinates"))?;
        let lon = coords[0]
            .as_f64()
            .context("geocoding longitude was not a number")?;
        let lat = coords[1]
            .as_f64()
            .context("geocoding latitude was not a number")?;
        let props = feature.get("properties");
        let label = props
            .and_then(|p| p.get("full_address"))
            .or_else(|| props.and_then(|p| p.get("place_formatted")))
            .or_else(|| props.and_then(|p| p.get("name")))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(query)
            .to_string();
        Ok((lon, lat, label))
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
        let (o_lon, o_lat, origin_label) = self.geocode(origin).await?;
        let (d_lon, d_lat, destination_label) = self.geocode(destination).await?;

        let coords = format!("{o_lon:.6},{o_lat:.6};{d_lon:.6},{d_lat:.6}");
        let value: Value = self
            .client
            .get(format!(
                "{}/directions/v5/mapbox/{}/{}",
                self.base_url,
                mode.mapbox_profile(),
                coords
            ))
            .query(&[
                ("access_token", self.token.as_str()),
                ("overview", "false"),
                ("alternatives", "false"),
            ])
            .send()
            .await
            .context("GET Mapbox directions")?
            .error_for_status()
            .context("Mapbox directions returned an error status")?
            .json()
            .await
            .context("parsing Mapbox directions JSON")?;

        let route = value
            .get("routes")
            .and_then(Value::as_array)
            .and_then(|r| r.first())
            .context("Mapbox returned no route between those points")?;
        let distance_meters = route
            .get("distance")
            .and_then(Value::as_f64)
            .context("route had no distance")?;
        let duration_seconds = route
            .get("duration")
            .and_then(Value::as_f64)
            .context("route had no duration")?;
        let duration_typical_seconds = typical_duration(route);

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

/// Pull the typical (no-traffic) duration from a Mapbox route: the top-level
/// `duration_typical` when present, else the sum of the legs' `duration_typical`.
/// Returns `None` when the profile doesn't provide it (walking/cycling).
fn typical_duration(route: &Value) -> Option<f64> {
    if let Some(d) = route.get("duration_typical").and_then(Value::as_f64) {
        return Some(d);
    }
    let legs = route.get("legs").and_then(Value::as_array)?;
    let mut total = 0.0;
    let mut any = false;
    for leg in legs {
        if let Some(d) = leg.get("duration_typical").and_then(Value::as_f64) {
            total += d;
            any = true;
        }
    }
    any.then_some(total)
}

/// Build the directions capability from the environment. Returns `None` (tool not
/// advertised) unless a supported provider is configured with a usable token.
///
/// - `AMBIENT_DIRECTIONS_PROVIDER` selects the backend (default/only: `mapbox`).
/// - `MAPBOX_TOKEN` (or `MAPBOX_ACCESS_TOKEN`) is the API token.
/// - `AMBIENT_HOME_LOCATION` seeds the default origin.
/// - `AMBIENT_WEATHER_UNITS` picks imperial vs metric distances.
pub fn from_env() -> Option<DirectionsConfig> {
    let provider = env::var("AMBIENT_DIRECTIONS_PROVIDER")
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    if !(provider.is_empty() || provider == "mapbox") {
        log::warn!(
            "AMBIENT_DIRECTIONS_PROVIDER='{provider}' is not supported (only 'mapbox' in v1); \
             directions_lookup disabled"
        );
        return None;
    }
    let token = env::var("MAPBOX_TOKEN")
        .ok()
        .or_else(|| env::var("MAPBOX_ACCESS_TOKEN").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;

    let default_origin = env::var("AMBIENT_HOME_LOCATION")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let imperial = units_are_imperial(env::var("AMBIENT_WEATHER_UNITS").ok().as_deref());

    log::info!(
        "directions_lookup enabled (mapbox; default origin {}, {} units)",
        default_origin.as_deref().unwrap_or("<none>"),
        if imperial { "imperial" } else { "metric" }
    );
    Some(DirectionsConfig {
        provider: Arc::new(MapboxDirections::new(token)),
        default_origin,
        imperial,
    })
}

/// Whether to speak distances in imperial units. Recognizes `imperial` / `us` /
/// `miles` (and the `imp`/`mi` prefixes); everything else — including unset —
/// defaults to metric.
pub fn units_are_imperial(units: Option<&str>) -> bool {
    match units.map(str::trim).map(str::to_lowercase) {
        Some(u) => u.starts_with("imp") || u == "us" || u.starts_with("mile") || u == "mi",
        None => false,
    }
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
/// "1 hour 10 minutes", "under a minute").
fn format_duration(seconds: f64) -> String {
    let total_min = (seconds.abs() / 60.0).round() as i64;
    if total_min < 1 {
        return "under a minute".to_string();
    }
    let (h, m) = (total_min / 60, total_min % 60);
    let unit = |n: i64, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    match (h, m) {
        (0, m) => unit(m, "minute"),
        (h, 0) => unit(h, "hour"),
        (h, m) => format!("{} {}", unit(h, "hour"), unit(m, "minute")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn travel_mode_parsing() {
        assert_eq!(TravelMode::from_arg(None), TravelMode::Driving);
        assert_eq!(TravelMode::from_arg(Some("drive")), TravelMode::Driving);
        assert_eq!(TravelMode::from_arg(Some("Walking")), TravelMode::Walking);
        assert_eq!(TravelMode::from_arg(Some("bike")), TravelMode::Cycling);
        assert_eq!(TravelMode::from_arg(Some("teleport")), TravelMode::Driving);
    }

    #[test]
    fn imperial_detection() {
        assert!(units_are_imperial(Some("imperial")));
        assert!(units_are_imperial(Some("US")));
        assert!(units_are_imperial(Some("miles")));
        assert!(!units_are_imperial(Some("metric")));
        assert!(!units_are_imperial(None));
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
}
