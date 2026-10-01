//! Structured place information: name, address, opening hours, rating, phone, etc.,
//! exposed to the LLM as the `places_lookup` tool (wired in `llm/rig.rs`) and rendered
//! on the device as a full-screen place card (the `PlaceView`) when the user asks about
//! a business or point of interest.
//!
//! The data comes from the **Google Places API (New)** behind the [`PlacesProvider`]
//! trait (see [`from_key`]): a **Text Search** resolves the free-text query to one or
//! more candidates, and **Place Details** fetches the full record for the chosen place.
//! A place photo is resolved to a **keyless** `photoUri` via the photo-media endpoint's
//! `skipHttpRedirect=true` mode, so the device fetches the image directly without ever
//! seeing the API key (the APK stays credential-free, like the recipe hero image).
//!
//! The trait keeps the tool testable offline (fixture JSON, no network), mirroring how
//! [`crate::weather::WeatherProvider`] backs the weather tool and
//! [`crate::directions::DirectionsProvider`] backs directions. Unlike weather there is no
//! keyless fallback (Google Places always needs a key), so [`from_key`] returns `None`
//! — the tool is simply not advertised — when the key is absent, exactly like
//! [`crate::directions::from_token`]. Numbers are stored as whole integers / strings so
//! [`PlaceReport`] is `Eq` (it rides inside [`crate::llm::DeviceAction`]) and JSON-marshals
//! directly to the device.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Everything the device needs to render a place card. Plain strings/integers/booleans
/// (empty string / `0` / `None` when a field is absent) so the type is `Eq` and
/// JSON-marshals directly to the device over the `anamanti-place` frame.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaceReport {
    /// The place's display name, e.g. "Blue Bottle Coffee".
    pub name: String,
    /// A one-line formatted address.
    pub address: String,
    /// A short human category, e.g. "Coffee shop" (empty when absent).
    pub category: String,
    /// Whether the place is open right now: `Some(true)`/`Some(false)`, or `None` when
    /// the API doesn't report it.
    pub open_now: Option<bool>,
    /// Human-readable weekly opening hours, one line per weekday (empty when absent).
    pub hours: Vec<String>,
    /// Average rating as text, e.g. "4.5" (empty when unrated).
    pub rating: String,
    /// Number of user ratings (0 when absent).
    pub rating_count: i32,
    /// Human price level, e.g. "Moderate" (empty when absent).
    pub price_level: String,
    /// National phone number, formatted (empty when absent).
    pub phone: String,
    /// Website URL (empty when absent).
    pub website: String,
    /// A Google Maps URL for the place (empty when absent).
    pub maps_uri: String,
    /// A **keyless** photo URL the device can fetch directly (empty when no photo).
    pub photo_uri: String,
}

/// One candidate from a Text Search, used to disambiguate before showing details.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaceCandidate {
    /// The Places resource id (`places/…` suffix) to fetch details for.
    pub place_id: String,
    /// The candidate's display name.
    pub name: String,
    /// A one-line formatted address to tell candidates apart.
    pub address: String,
}

/// A source of place information. Abstracted so the tool is testable offline (fixture
/// JSON, no network) and an alternate backend could be dropped in behind the same seam.
#[async_trait]
pub trait PlacesProvider: Send + Sync {
    /// Text-search `query`, optionally biased toward `bias` (the household home
    /// location), returning best-first candidates.
    async fn search(&self, query: &str, bias: Option<&str>) -> Result<Vec<PlaceCandidate>>;

    /// Fetch the full [`PlaceReport`] for a Places resource id from a prior [`search`].
    ///
    /// [`search`]: PlacesProvider::search
    async fn details(&self, place_id: &str) -> Result<PlaceReport>;
}

/// The wired place capability for the `places_lookup` tool: the provider plus the live
/// home location used to bias searches. Mirrors [`crate::weather::WeatherConfig`].
pub struct PlacesConfig {
    pub provider: Arc<dyn PlacesProvider>,
    pub home_location: crate::directions::LiveHomeLocation,
}

/// A [`PlacesProvider`] backed by the Google Places API (New). The API key is a
/// **secret** (seeded from `GOOGLE_PLACES_API_KEY`); it is passed in at construction and
/// sent as the `X-Goog-Api-Key` header, never as a query parameter, so it never lands in
/// a logged URL.
pub struct GooglePlaces {
    client: reqwest::Client,
    base: String,
    api_key: String,
}

impl GooglePlaces {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url("https://places.googleapis.com", api_key)
    }

    /// Point the client at a specific API root (for tests).
    pub fn with_base_url(base: impl Into<String>, api_key: impl Into<String>) -> Self {
        let client = crate::http::tuned_builder()
            .timeout(StdDuration::from_secs(10))
            .user_agent("anamanti-core/0.1 (places)")
            .build()
            .unwrap_or_default();
        Self {
            client,
            base: base.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
        }
    }

    /// Resolve a photo resource (`places/…/photos/…`) to a keyless `photoUri` via the
    /// photo-media endpoint with `skipHttpRedirect=true`. Best-effort: any failure
    /// returns an empty string so a missing/failed photo never fails the whole lookup.
    async fn resolve_photo(&self, photo_name: &str) -> String {
        let url = format!("{}/v1/{photo_name}/media", self.base);
        let resp = self
            .client
            .get(url)
            .header("X-Goog-Api-Key", &self.api_key)
            .query(&[("maxWidthPx", "800"), ("skipHttpRedirect", "true")])
            .send()
            .await;
        let Ok(resp) = resp.and_then(reqwest::Response::error_for_status) else {
            return String::new();
        };
        let Ok(value) = resp.json::<Value>().await else {
            return String::new();
        };
        value
            .get("photoUri")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }
}

#[async_trait]
impl PlacesProvider for GooglePlaces {
    async fn search(&self, query: &str, bias: Option<&str>) -> Result<Vec<PlaceCandidate>> {
        let query = query.trim();
        anyhow::ensure!(!query.is_empty(), "no place query given");
        let text_query = compose_query(query, bias);
        let value: Value = self
            .client
            .post(format!("{}/v1/places:searchText", self.base))
            .header("X-Goog-Api-Key", &self.api_key)
            .header(
                "X-Goog-FieldMask",
                "places.id,places.displayName,places.formattedAddress,places.types",
            )
            .json(&json!({ "textQuery": text_query, "maxResultCount": 5 }))
            .send()
            .await
            .context("POST Google Places text search")?
            .error_for_status()
            .context("Google Places text search returned an error status")?
            .json()
            .await
            .context("parsing Google Places text search JSON")?;
        Ok(candidates_from_search(&value))
    }

    async fn details(&self, place_id: &str) -> Result<PlaceReport> {
        let place_id = place_id.trim();
        anyhow::ensure!(!place_id.is_empty(), "no place id given");
        // The resource path is `places/<id>`; accept either form from the caller.
        let path = if place_id.starts_with("places/") {
            place_id.to_string()
        } else {
            format!("places/{place_id}")
        };
        let value: Value = self
            .client
            .get(format!("{}/v1/{path}", self.base))
            .header("X-Goog-Api-Key", &self.api_key)
            .header(
                "X-Goog-FieldMask",
                "id,displayName,formattedAddress,regularOpeningHours,currentOpeningHours,\
                 rating,userRatingCount,nationalPhoneNumber,websiteUri,googleMapsUri,\
                 priceLevel,primaryTypeDisplayName,photos",
            )
            .send()
            .await
            .context("GET Google Places details")?
            .error_for_status()
            .context("Google Places details returned an error status")?
            .json()
            .await
            .context("parsing Google Places details JSON")?;

        let mut report = report_from_details(&value);
        // Resolve the first photo to a keyless URL (a second, best-effort request).
        if let Some(photo_name) = first_photo_name(&value) {
            report.photo_uri = self.resolve_photo(&photo_name).await;
        }
        Ok(report)
    }
}

/// Build the Text Search `textQuery`: fold in the home location as a "near …" hint when
/// a `bias` is set and the query doesn't already carry its own location context (a comma,
/// or the words "near"/"in "/"around"). Keeps "coffee shop" → "coffee shop near Austin,
/// TX" while leaving "the Louvre, Paris" untouched.
fn compose_query(query: &str, bias: Option<&str>) -> String {
    let query = query.trim();
    let Some(bias) = bias.map(str::trim).filter(|s| !s.is_empty()) else {
        return query.to_string();
    };
    let lower = query.to_lowercase();
    let has_location = query.contains(',')
        || lower.contains("near")
        || lower.contains(" in ")
        || lower.contains("around");
    if has_location {
        query.to_string()
    } else {
        format!("{query} near {bias}")
    }
}

/// Map a Text Search response body into best-first candidates, keeping only real
/// **establishments** (see [`is_establishment`]). Google Text Search will happily match a
/// query against a bare street address — e.g. "F-45 River Road" resolves to a
/// `street_address` point named "45 River Rd E f" with no hours/phone/rating — which makes
/// a useless place card. Dropping non-establishment results turns those into a clean
/// "couldn't find it" instead.
fn candidates_from_search(value: &Value) -> Vec<PlaceCandidate> {
    let Some(places) = value.get("places").and_then(Value::as_array) else {
        return Vec::new();
    };
    places
        .iter()
        .filter(|p| is_establishment(p))
        .filter_map(|p| {
            let place_id = p.get("id").and_then(Value::as_str)?.to_string();
            Some(PlaceCandidate {
                place_id,
                name: display_name(p),
                address: p
                    .get("formattedAddress")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect()
}

/// Whether a Text Search result is a real business / point of interest rather than a bare
/// address or geocode. Google tags establishments with `establishment` (and usually
/// `point_of_interest`) in `types`; an address point carries only `street_address` /
/// `subpremise` / `premise` / `route` / `geocode`. Results with no `types` at all are kept
/// (be lenient — the field may be absent), but an explicit address-only result is dropped.
fn is_establishment(place: &Value) -> bool {
    match place.get("types").and_then(Value::as_array) {
        None => true, // no type info → don't over-filter
        Some(types) => {
            let has = |t: &str| types.iter().any(|v| v.as_str() == Some(t));
            has("establishment") || has("point_of_interest")
        }
    }
}

/// Map a Place Details response body into a [`PlaceReport`]. Missing fields degrade to
/// empties/`None` rather than failing the whole lookup. Note: `photo_uri` is filled in
/// separately by a follow-up photo-media request (see [`GooglePlaces::details`]).
fn report_from_details(value: &Value) -> PlaceReport {
    // Prefer the current opening hours' open-now flag; the human weekday lines come from
    // the regular hours (a stable weekly schedule reads better on the card).
    let open_now = value
        .get("currentOpeningHours")
        .or_else(|| value.get("regularOpeningHours"))
        .and_then(|h| h.get("openNow"))
        .and_then(Value::as_bool);
    let hours = value
        .get("regularOpeningHours")
        .or_else(|| value.get("currentOpeningHours"))
        .and_then(|h| h.get("weekdayDescriptions"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    PlaceReport {
        name: display_name(value),
        address: str_field(value, "formattedAddress"),
        category: value
            .get("primaryTypeDisplayName")
            .and_then(|t| t.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        open_now,
        hours,
        rating: value
            .get("rating")
            .and_then(Value::as_f64)
            .map(|r| format!("{r:.1}"))
            .unwrap_or_default(),
        rating_count: value
            .get("userRatingCount")
            .and_then(Value::as_i64)
            .unwrap_or(0) as i32,
        price_level: price_level_label(value.get("priceLevel").and_then(Value::as_str)),
        phone: str_field(value, "nationalPhoneNumber"),
        website: str_field(value, "websiteUri"),
        maps_uri: str_field(value, "googleMapsUri"),
        photo_uri: String::new(),
    }
}

/// The `photos[0].name` resource id, if any (used to resolve a keyless photo URL).
fn first_photo_name(value: &Value) -> Option<String> {
    value
        .get("photos")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Read a place's `displayName.text`, falling back to a bare `displayName` string.
fn display_name(value: &Value) -> String {
    value
        .get("displayName")
        .and_then(|d| d.get("text").or(Some(d)))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Read a top-level string field, empty when absent.
fn str_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Map the Places `priceLevel` enum to a short human label.
fn price_level_label(level: Option<&str>) -> String {
    match level {
        Some("PRICE_LEVEL_FREE") => "Free",
        Some("PRICE_LEVEL_INEXPENSIVE") => "Inexpensive",
        Some("PRICE_LEVEL_MODERATE") => "Moderate",
        Some("PRICE_LEVEL_EXPENSIVE") => "Expensive",
        Some("PRICE_LEVEL_VERY_EXPENSIVE") => "Very expensive",
        _ => "",
    }
    .to_string()
}

/// Build the configured places provider from an explicit key. Returns `None` — the tool
/// is simply not advertised — unless a supported provider is selected and a usable key is
/// present (Google Places has no keyless fallback, mirroring
/// [`crate::directions::from_token`]).
///
/// - `provider` selects the backend (`places.provider`; default/only: `google`; empty is
///   treated as `google`).
/// - `key` is the Google Places API key — a **secret**, seeded from `GOOGLE_PLACES_API_KEY`
///   but runtime-settable from the config page's Tools tab, so it's passed in here.
pub fn from_key(provider: &str, key: Option<&str>) -> Option<Arc<dyn PlacesProvider>> {
    let provider = provider.trim().to_lowercase();
    if !(provider.is_empty() || provider == "google") {
        log::warn!(
            "places.provider='{provider}' is not supported (only 'google' in v1); \
             places_lookup disabled"
        );
        return None;
    }
    let key = key.map(str::trim).filter(|s| !s.is_empty())?;
    log::info!("places_lookup enabled (Google Places API New)");
    Some(Arc::new(GooglePlaces::new(key)) as Arc<dyn PlacesProvider>)
}

/// A short spoken confirmation for the model to relay once the place card is on screen.
pub fn render_confirmation(report: &PlaceReport) -> String {
    let mut out = format!("Here's {}", report.name);
    match report.open_now {
        Some(true) => out.push_str(" — it's open now"),
        Some(false) => out.push_str(" — it's closed right now"),
        None => {}
    }
    out.push_str(". The details are on the screen.");
    out
}

/// The tool result for an ambiguous query: a numbered candidate list that **exposes each
/// candidate's opaque `place_id`** so the model can re-call `places_lookup` with the exact
/// id the user picks. This is critical — without the real id the model invents one (e.g.
/// "F-45 New Dundee Road") and the Details call fails with HTTP 400. The leading
/// instruction tells the model to ask the user and NOT read the ids aloud, so the spoken
/// reply stays natural.
pub fn render_candidates(candidates: &[PlaceCandidate]) -> String {
    let mut out = String::from(
        "Several places match. Ask the user which one they mean, then call places_lookup \
         again with the SAME query plus that option's place_id (never read the id aloud):",
    );
    for (i, c) in candidates.iter().take(5).enumerate() {
        let where_ = if c.address.is_empty() {
            String::new()
        } else {
            format!(" — {}", first_address_segment(&c.address))
        };
        out.push_str(&format!(
            "\n{}. {}{} [place_id: {}]",
            i + 1,
            c.name,
            where_,
            c.place_id
        ));
    }
    out
}

/// The first comma-separated segment of an address (usually the street line), for a
/// compact spoken candidate list.
fn first_address_segment(address: &str) -> &str {
    address.split(',').next().unwrap_or(address).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH: &str = r#"{
        "places": [
            {"id":"ChIJA","displayName":{"text":"Blue Bottle Coffee"},"formattedAddress":"1 Main St, Austin, TX"},
            {"id":"ChIJB","displayName":{"text":"Blue Bottle Coffee"},"formattedAddress":"99 Oak Ave, Austin, TX"}
        ]
    }"#;

    const DETAILS: &str = r#"{
        "id":"ChIJA",
        "displayName":{"text":"Blue Bottle Coffee","languageCode":"en"},
        "formattedAddress":"1 Main St, Austin, TX 78701, USA",
        "regularOpeningHours":{
            "openNow":true,
            "weekdayDescriptions":[
                "Monday: 7:00 AM – 6:00 PM",
                "Tuesday: 7:00 AM – 6:00 PM"
            ]
        },
        "rating":4.6,
        "userRatingCount":1234,
        "nationalPhoneNumber":"(512) 555-0100",
        "websiteUri":"https://bluebottlecoffee.com/",
        "googleMapsUri":"https://maps.google.com/?cid=1",
        "priceLevel":"PRICE_LEVEL_MODERATE",
        "primaryTypeDisplayName":{"text":"Coffee shop","languageCode":"en"},
        "photos":[{"name":"places/ChIJA/photos/PHOTO1","widthPx":4000,"heightPx":3000}]
    }"#;

    #[test]
    fn compose_query_folds_in_the_bias_only_when_needed() {
        assert_eq!(
            compose_query("coffee shop", Some("Austin, TX")),
            "coffee shop near Austin, TX"
        );
        // Query already carries its own location context → left untouched.
        assert_eq!(
            compose_query("the Louvre, Paris", Some("Austin, TX")),
            "the Louvre, Paris"
        );
        assert_eq!(
            compose_query("diner near me", Some("Austin, TX")),
            "diner near me"
        );
        // No bias → the query verbatim.
        assert_eq!(compose_query("coffee shop", None), "coffee shop");
        assert_eq!(compose_query("coffee shop", Some("  ")), "coffee shop");
    }

    #[test]
    fn parses_search_candidates() {
        let value: Value = serde_json::from_str(SEARCH).unwrap();
        let out = candidates_from_search(&value);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].place_id, "ChIJA");
        assert_eq!(out[0].name, "Blue Bottle Coffee");
        assert_eq!(out[0].address, "1 Main St, Austin, TX");
        assert_eq!(out[1].place_id, "ChIJB");
    }

    #[test]
    fn parses_details_body() {
        let value: Value = serde_json::from_str(DETAILS).unwrap();
        let report = report_from_details(&value);
        assert_eq!(report.name, "Blue Bottle Coffee");
        assert_eq!(report.address, "1 Main St, Austin, TX 78701, USA");
        assert_eq!(report.category, "Coffee shop");
        assert_eq!(report.open_now, Some(true));
        assert_eq!(report.hours.len(), 2);
        assert_eq!(report.hours[0], "Monday: 7:00 AM – 6:00 PM");
        assert_eq!(report.rating, "4.6");
        assert_eq!(report.rating_count, 1234);
        assert_eq!(report.price_level, "Moderate");
        assert_eq!(report.phone, "(512) 555-0100");
        assert_eq!(report.website, "https://bluebottlecoffee.com/");
        assert_eq!(report.maps_uri, "https://maps.google.com/?cid=1");
        // The photo URL is resolved by a separate request; the parse leaves it empty.
        assert_eq!(report.photo_uri, "");
        assert_eq!(
            first_photo_name(&value).as_deref(),
            Some("places/ChIJA/photos/PHOTO1")
        );
    }

    #[test]
    fn details_missing_fields_degrade_gracefully() {
        let value: Value = serde_json::from_str(r#"{"displayName":{"text":"X"}}"#).unwrap();
        let report = report_from_details(&value);
        assert_eq!(report.name, "X");
        assert_eq!(report.address, "");
        assert_eq!(report.open_now, None);
        assert!(report.hours.is_empty());
        assert_eq!(report.rating, "");
        assert_eq!(report.rating_count, 0);
        assert_eq!(report.price_level, "");
        assert_eq!(first_photo_name(&value), None);
    }

    #[test]
    fn from_key_requires_a_key_and_supported_provider() {
        assert!(from_key("google", Some("k")).is_some());
        assert!(from_key("", Some("k")).is_some());
        // No key ⇒ tool not advertised.
        assert!(from_key("google", None).is_none());
        assert!(from_key("google", Some("  ")).is_none());
        // Unknown provider ⇒ disabled.
        assert!(from_key("yelp", Some("k")).is_none());
    }

    #[test]
    fn confirmation_mentions_name_and_open_state() {
        let report = PlaceReport {
            name: "Blue Bottle Coffee".to_string(),
            open_now: Some(true),
            ..Default::default()
        };
        let c = render_confirmation(&report);
        assert!(c.contains("Blue Bottle Coffee"), "{c}");
        assert!(c.contains("open now"), "{c}");
    }

    #[test]
    fn candidates_render_exposes_place_ids_for_the_model() {
        let value: Value = serde_json::from_str(SEARCH).unwrap();
        let out = candidates_from_search(&value);
        let line = render_candidates(&out);
        assert!(line.contains("Blue Bottle Coffee"), "{line}");
        assert!(line.contains("1 Main St"), "{line}");
        // The opaque ids MUST be present so the model re-calls with a real id (not a
        // hallucinated one) — the fix for the HTTP 400 disambiguation bug.
        assert!(line.contains("[place_id: ChIJA]"), "{line}");
        assert!(line.contains("[place_id: ChIJB]"), "{line}");
    }

    #[test]
    fn search_drops_bare_address_results() {
        // A query that Google can only match as a street address (e.g. "F-45 River Road"
        // → "45 River Rd E f", types [subpremise, street_address]) must be dropped, so it
        // never becomes a hours-less place card. A real establishment is kept.
        let body = json!({
            "places": [
                {"id":"addr","displayName":{"text":"45 River Rd E f"},
                 "formattedAddress":"45 River Rd E f, Kitchener","types":["subpremise","street_address"]},
                {"id":"gym","displayName":{"text":"F45 Training Sportsworld KW"},
                 "formattedAddress":"1601 River Rd E, Kitchener",
                 "types":["gym","point_of_interest","establishment"]}
            ]
        });
        let out = candidates_from_search(&body);
        assert_eq!(out.len(), 1, "the address point must be filtered out");
        assert_eq!(out[0].place_id, "gym");
    }

    #[tokio::test]
    async fn search_then_details_over_a_loopback_server() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        // Serve the search body, then the details body, then a photo-media body.
        let bodies = vec![
            SEARCH.to_string(),
            DETAILS.to_string(),
            r#"{"name":"x","photoUri":"https://lh3.googleusercontent.com/keyless"}"#.to_string(),
        ];
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let addr = std_listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
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

        let provider = GooglePlaces::with_base_url(format!("http://{addr}"), "test-key");
        let candidates = provider
            .search("coffee shop", Some("Austin"))
            .await
            .unwrap();
        assert_eq!(candidates.len(), 2);
        let report = provider.details(&candidates[0].place_id).await.unwrap();
        assert_eq!(report.name, "Blue Bottle Coffee");
        assert_eq!(
            report.photo_uri,
            "https://lh3.googleusercontent.com/keyless"
        );
        server.await.unwrap();
    }
}
