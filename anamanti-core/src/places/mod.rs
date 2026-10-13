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

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

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

/// A [`PlacesProvider`] backed by the Google Places API (New). The API key is a
/// **secret** (seeded from `GOOGLE_PLACES_API_KEY`); it is passed in at construction and
/// sent as the `X-Goog-Api-Key` header, never as a query parameter, so it never lands in
/// a logged URL.
pub struct GooglePlaces {
    base: String,
    api_key: String,
}

/// Per-request budget for each Google Places call.
const PLACES_TIMEOUT: StdDuration = StdDuration::from_secs(10);

/// `User-Agent` component label for Places requests.
const COMPONENT: &str = "places";

impl GooglePlaces {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url("https://places.googleapis.com", api_key)
    }

    /// Point the client at a specific API root (for tests).
    pub fn with_base_url(base: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
        }
    }

    /// Resolve a photo resource (`places/…/photos/…`) to a keyless `photoUri` via the
    /// photo-media endpoint with `skipHttpRedirect=true`. Best-effort: any failure
    /// returns an empty string so a missing/failed photo never fails the whole lookup.
    async fn resolve_photo(&self, photo_name: &str) -> String {
        let req = crate::http::get(format!("{}/v1/{photo_name}/media", self.base), COMPONENT)
            .header("X-Goog-Api-Key", &self.api_key)
            .query(&[("maxWidthPx", "800"), ("skipHttpRedirect", "true")]);
        crate::http::send_json::<PhotoMedia>(req, PLACES_TIMEOUT, "Google Places photo")
            .await
            .ok()
            .and_then(|m| m.photo_uri)
            .unwrap_or_default()
    }
}

#[async_trait]
impl PlacesProvider for GooglePlaces {
    async fn search(&self, query: &str, bias: Option<&str>) -> Result<Vec<PlaceCandidate>> {
        let query = query.trim();
        anyhow::ensure!(!query.is_empty(), "no place query given");
        let text_query = compose_query(query, bias);
        let req = crate::http::post(format!("{}/v1/places:searchText", self.base), COMPONENT)
            .header("X-Goog-Api-Key", &self.api_key)
            .header(
                "X-Goog-FieldMask",
                "places.id,places.displayName,places.formattedAddress,places.types",
            )
            .json(&json!({ "textQuery": text_query, "maxResultCount": 5 }));
        let value: SearchResponse =
            crate::http::send_json(req, PLACES_TIMEOUT, "Google Places text search").await?;
        Ok(candidates_from_search(value))
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
        let req = crate::http::get(format!("{}/v1/{path}", self.base), COMPONENT)
            .header("X-Goog-Api-Key", &self.api_key)
            .header(
                "X-Goog-FieldMask",
                "id,displayName,formattedAddress,regularOpeningHours,currentOpeningHours,\
                 rating,userRatingCount,nationalPhoneNumber,websiteUri,googleMapsUri,\
                 priceLevel,primaryTypeDisplayName,photos",
            );
        let value: PlaceDetails =
            crate::http::send_json(req, PLACES_TIMEOUT, "Google Places details").await?;

        let mut report = report_from_details(&value);
        // Resolve the first photo to a keyless URL (a second, best-effort request).
        if let Some(photo_name) = first_photo_name(&value) {
            report.photo_uri = self.resolve_photo(&photo_name).await;
        }
        Ok(report)
    }
}

// ---------------------------------------------------------------------------
// Google Places (New) response shapes — only the fields we read. Every field is
// optional (`#[serde(default)]`) so a sparse record degrades to empties instead of
// failing the whole lookup.
// ---------------------------------------------------------------------------

/// A localized text (`{ "text": …, "languageCode": … }`), tolerating a bare string or
/// any other shape (read as empty).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LocalizedText {
    Plain(String),
    Localized {
        #[serde(default)]
        text: Option<String>,
    },
    Other(serde::de::IgnoredAny),
}

impl LocalizedText {
    fn text(&self) -> &str {
        match self {
            LocalizedText::Plain(s) => s,
            LocalizedText::Localized { text } => text.as_deref().unwrap_or(""),
            LocalizedText::Other(_) => "",
        }
    }
}

/// Read an optional localized text, empty when absent.
fn text_of(t: &Option<LocalizedText>) -> String {
    t.as_ref()
        .map(LocalizedText::text)
        .unwrap_or("")
        .to_string()
}

/// `places:searchText` response.
#[derive(Debug, Default, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    places: Vec<SearchPlace>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchPlace {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    display_name: Option<LocalizedText>,
    #[serde(default)]
    formatted_address: Option<String>,
    #[serde(default)]
    types: Option<Vec<String>>,
}

/// Place Details response.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaceDetails {
    #[serde(default)]
    display_name: Option<LocalizedText>,
    #[serde(default)]
    formatted_address: Option<String>,
    #[serde(default)]
    regular_opening_hours: Option<OpeningHours>,
    #[serde(default)]
    current_opening_hours: Option<OpeningHours>,
    #[serde(default)]
    rating: Option<f64>,
    #[serde(default)]
    user_rating_count: Option<i64>,
    #[serde(default)]
    national_phone_number: Option<String>,
    #[serde(default)]
    website_uri: Option<String>,
    #[serde(default)]
    google_maps_uri: Option<String>,
    #[serde(default)]
    price_level: Option<String>,
    #[serde(default)]
    primary_type_display_name: Option<LocalizedText>,
    #[serde(default)]
    photos: Vec<Photo>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpeningHours {
    #[serde(default)]
    open_now: Option<bool>,
    #[serde(default)]
    weekday_descriptions: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
struct Photo {
    #[serde(default)]
    name: Option<String>,
}

/// Photo-media response (`skipHttpRedirect=true`).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PhotoMedia {
    #[serde(default)]
    photo_uri: Option<String>,
}

/// Build the Text Search `textQuery`: fold in the home location as a "near …" hint when
/// a `bias` is set and the query doesn't already carry its own location context (a comma,
/// or the words "near"/"in "/"around"). Keeps "coffee shop" → "coffee shop near Austin,
/// TX" while leaving "the Louvre, Paris" untouched.
fn compose_query(query: &str, bias: Option<&str>) -> String {
    let query = query.trim();
    let Some(bias) = crate::util::non_empty(bias) else {
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
fn candidates_from_search(value: SearchResponse) -> Vec<PlaceCandidate> {
    value
        .places
        .into_iter()
        .filter(is_establishment)
        .filter_map(|p| {
            Some(PlaceCandidate {
                name: text_of(&p.display_name),
                address: p.formatted_address.unwrap_or_default(),
                place_id: p.id?,
            })
        })
        .collect()
}

/// Whether a Text Search result is a real business / point of interest rather than a bare
/// address or geocode. Google tags establishments with `establishment` (and usually
/// `point_of_interest`) in `types`; an address point carries only `street_address` /
/// `subpremise` / `premise` / `route` / `geocode`. Results with no `types` at all are kept
/// (be lenient — the field may be absent), but an explicit address-only result is dropped.
fn is_establishment(place: &SearchPlace) -> bool {
    match &place.types {
        None => true, // no type info → don't over-filter
        Some(types) => types
            .iter()
            .any(|t| t == "establishment" || t == "point_of_interest"),
    }
}

/// Map a Place Details response body into a [`PlaceReport`]. Missing fields degrade to
/// empties/`None` rather than failing the whole lookup. Note: `photo_uri` is filled in
/// separately by a follow-up photo-media request (see [`GooglePlaces::details`]).
fn report_from_details(value: &PlaceDetails) -> PlaceReport {
    // Prefer the current opening hours' open-now flag; the human weekday lines come from
    // the regular hours (a stable weekly schedule reads better on the card).
    let open_now = value
        .current_opening_hours
        .as_ref()
        .or(value.regular_opening_hours.as_ref())
        .and_then(|h| h.open_now);
    let hours = value
        .regular_opening_hours
        .as_ref()
        .or(value.current_opening_hours.as_ref())
        .and_then(|h| h.weekday_descriptions.clone())
        .unwrap_or_default();

    PlaceReport {
        name: text_of(&value.display_name),
        address: value.formatted_address.clone().unwrap_or_default(),
        category: text_of(&value.primary_type_display_name),
        open_now,
        hours,
        rating: value.rating.map(|r| format!("{r:.1}")).unwrap_or_default(),
        rating_count: value.user_rating_count.unwrap_or(0) as i32,
        price_level: price_level_label(value.price_level.as_deref()),
        phone: value.national_phone_number.clone().unwrap_or_default(),
        website: value.website_uri.clone().unwrap_or_default(),
        maps_uri: value.google_maps_uri.clone().unwrap_or_default(),
        photo_uri: String::new(),
    }
}

/// The `photos[0].name` resource id, if any (used to resolve a keyless photo URL).
fn first_photo_name(value: &PlaceDetails) -> Option<String> {
    value.photos.first().and_then(|p| p.name.clone())
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

/// The places backend [`from_key`] would build for this provider label + key, or `None`
/// when the tool would not be advertised (unsupported provider, or no usable key).
pub fn effective_backend(provider: &str, key: Option<&str>) -> Option<&'static str> {
    let provider = provider.trim().to_lowercase();
    if !(provider.is_empty() || provider == "google") {
        return None;
    }
    crate::util::non_empty(key).map(|_| "google")
}

/// Build the configured places provider from an explicit key. Returns `None` — the tool
/// is simply not advertised — unless a supported provider is selected and a usable key is
/// present (Google Places has no keyless fallback, mirroring
/// [`crate::directions::from_token`]); see [`effective_backend`].
///
/// - `provider` selects the backend (`places.provider`; default/only: `google`; empty is
///   treated as `google`).
/// - `key` is the Google Places API key — a **secret**, seeded from `GOOGLE_PLACES_API_KEY`
///   but runtime-settable from the config page's Tools tab, so it's passed in here.
pub fn from_key(provider: &str, key: Option<&str>) -> Option<Arc<dyn PlacesProvider>> {
    if effective_backend(provider, Some("x")).is_none() {
        log::warn!(
            "places.provider='{}' is not supported (only 'google' in v1); \
             places_lookup disabled",
            provider.trim().to_lowercase()
        );
        return None;
    }
    let key = crate::util::non_empty(key)?;
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
        let out = candidates_from_search(serde_json::from_str(SEARCH).unwrap());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].place_id, "ChIJA");
        assert_eq!(out[0].name, "Blue Bottle Coffee");
        assert_eq!(out[0].address, "1 Main St, Austin, TX");
        assert_eq!(out[1].place_id, "ChIJB");
    }

    #[test]
    fn parses_details_body() {
        let value: PlaceDetails = serde_json::from_str(DETAILS).unwrap();
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
        let value: PlaceDetails = serde_json::from_str(r#"{"displayName":{"text":"X"}}"#).unwrap();
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
        let out = candidates_from_search(serde_json::from_str(SEARCH).unwrap());
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
        let out = candidates_from_search(serde_json::from_value(body).unwrap());
        assert_eq!(out.len(), 1, "the address point must be filtered out");
        assert_eq!(out[0].place_id, "gym");
    }

    #[tokio::test]
    async fn search_then_details_over_a_loopback_server() {
        // Serve the search body, then the details body, then a photo-media body.
        let (base, server) = crate::http::test_support::serve_sequence(vec![
            SEARCH.to_string(),
            DETAILS.to_string(),
            r#"{"name":"x","photoUri":"https://lh3.googleusercontent.com/keyless"}"#.to_string(),
        ]);

        let provider = GooglePlaces::with_base_url(base, "test-key");
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

    #[test]
    fn display_name_tolerates_a_bare_string_and_odd_shapes() {
        let v: PlaceDetails = serde_json::from_str(
            r#"{"displayName":"Plain","primaryTypeDisplayName":42,"photos":[{}]}"#,
        )
        .unwrap();
        let r = report_from_details(&v);
        assert_eq!(r.name, "Plain");
        assert_eq!(r.category, "");
        assert_eq!(first_photo_name(&v), None);
    }

    #[test]
    fn open_now_prefers_current_hours_even_without_the_flag() {
        // Present current hours without openNow ⇒ unknown (the old precedence).
        let v: PlaceDetails = serde_json::from_str(
            r#"{"currentOpeningHours":{},"regularOpeningHours":{"openNow":true}}"#,
        )
        .unwrap();
        assert_eq!(report_from_details(&v).open_now, None);
    }

    #[test]
    fn effective_backend_mirrors_from_key() {
        assert_eq!(effective_backend("google", Some("k")), Some("google"));
        assert_eq!(effective_backend("", Some("k")), Some("google"));
        assert_eq!(effective_backend("google", None), None);
        assert_eq!(effective_backend("yelp", Some("k")), None);
    }
}
