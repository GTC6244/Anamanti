//! Where the household lives: the home address, the live handle the location-aware
//! tools read it through, and the household's unit preference.
//!
//! These belong to the household record (edited on the config page's Household tab and
//! persisted with the settings), not to any one tool: directions and nearby-places
//! geocode the full address, while weather, System-1 and the prompt's "here" grounding
//! use only City + State/Prov. Keeping them here means weather and places do not depend
//! on the directions module.

use std::sync::{Arc, RwLock};

/// Trim an optional field and treat blank as unset, borrowing from the input.
fn field(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// The home's physical location captured as **discrete fields** rather than a single
/// free-text string, so each consumer can use the right granularity: driving directions
/// and nearby-places search geocode the whole address, while weather and the prompt's
/// "here" grounding use only City + State/Prov. All fields are optional; a blank field is
/// treated as unset. Serde field names match the config/web payload keys.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HomeAddress {
    /// Street line 1 (e.g. "123 Main St").
    #[serde(default)]
    pub address1: Option<String>,
    /// Street line 2 (e.g. "Apt 4"), optional.
    #[serde(default)]
    pub address2: Option<String>,
    /// City / town.
    #[serde(default)]
    pub city: Option<String>,
    /// State / province.
    #[serde(default)]
    pub state: Option<String>,
    /// Zip / postal code.
    #[serde(default)]
    pub postal: Option<String>,
    /// Country.
    #[serde(default)]
    pub country: Option<String>,
}

impl HomeAddress {
    /// True when every field is blank/unset.
    pub fn is_empty(&self) -> bool {
        [
            &self.address1,
            &self.address2,
            &self.city,
            &self.state,
            &self.postal,
            &self.country,
        ]
        .iter()
        .all(|f| f.as_deref().map(str::trim).unwrap_or("").is_empty())
    }

    /// A cleaned copy: every field trimmed, blanks dropped to `None`.
    pub fn sanitized(&self) -> HomeAddress {
        let clean = |v: &Option<String>| field(v).map(str::to_string);
        HomeAddress {
            address1: clean(&self.address1),
            address2: clean(&self.address2),
            city: clean(&self.city),
            state: clean(&self.state),
            postal: clean(&self.postal),
            country: clean(&self.country),
        }
    }

    /// The full combined address as one geocodable string, e.g.
    /// `"123 Main St, Apt 4, Austin, Texas 78701, USA"`. Used for driving directions and
    /// the nearby-places search bias. `None` when every field is blank.
    pub fn full_address(&self) -> Option<String> {
        // "<state> <postal>" collapses to just one when the other is absent.
        let state_postal = match (field(&self.state), field(&self.postal)) {
            (Some(s), Some(p)) => Some(format!("{s} {p}")),
            (Some(s), None) => Some(s.to_string()),
            (None, Some(p)) => Some(p.to_string()),
            (None, None) => None,
        };
        let segments: Vec<String> = [
            field(&self.address1).map(str::to_string),
            field(&self.address2).map(str::to_string),
            field(&self.city).map(str::to_string),
            state_postal,
            field(&self.country).map(str::to_string),
        ]
        .into_iter()
        .flatten()
        .collect();
        if segments.is_empty() {
            None
        } else {
            Some(segments.join(", "))
        }
    }

    /// The weather-scoped location — City + State/Prov only, e.g. `"Austin, Texas"`. Used
    /// for the weather tool's home fallback, the ambient weather push, System-1, and the
    /// prompt's "here" grounding line. `None` when both City and State are blank.
    pub fn weather_location(&self) -> Option<String> {
        let parts: Vec<&str> = [field(&self.city), field(&self.state)]
            .into_iter()
            .flatten()
            .collect();
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(", "))
        }
    }
}

/// A cheaply-cloneable, thread-safe handle to the current [`HomeAddress`], used by the
/// location-aware tools. It reads **live** so an edit to the household record (config page
/// Household tab) changes every consumer without rebuilding the LLM backend: the
/// orchestrator updates the same cell every tool clone shares. Seeds from the config
/// file's home address at boot (via the `Household` record). Expose two formatted views —
/// [`full`](Self::full) (combined address, for directions/places) and
/// [`weather`](Self::weather) (City + State/Prov).
#[derive(Clone, Default)]
pub struct LiveHomeLocation(Arc<RwLock<HomeAddress>>);

impl LiveHomeLocation {
    /// A handle initialized to `addr` (sanitized on store).
    pub fn new(addr: HomeAddress) -> Self {
        let this = Self::default();
        this.set(addr);
        this
    }

    /// The full combined address string, or `None` when unset. See
    /// [`HomeAddress::full_address`]. Used by directions + nearby-places.
    pub fn full(&self) -> Option<String> {
        self.0.read().unwrap().full_address()
    }

    /// The weather-scoped City + State/Prov string, or `None` when unset. See
    /// [`HomeAddress::weather_location`]. Used by weather + System-1 + prompt grounding.
    pub fn weather(&self) -> Option<String> {
        self.0.read().unwrap().weather_location()
    }

    /// Replace the current home address (stored sanitized: trimmed, blanks → `None`).
    pub fn set(&self, addr: HomeAddress) {
        *self.0.write().unwrap() = addr.sanitized();
    }
}

/// Whether the household's units preference is imperial. Recognizes `imperial` / `us` /
/// `miles` (and the `imp`/`mi` prefixes); everything else — including unset — defaults
/// to metric. Drives distances (directions) and temperatures (weather).
pub fn units_are_imperial(units: Option<&str>) -> bool {
    match units.map(str::trim).map(str::to_lowercase) {
        Some(u) => u.starts_with("imp") || u == "us" || u.starts_with("mile") || u == "mi",
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(
        a1: &str,
        a2: &str,
        city: &str,
        state: &str,
        postal: &str,
        country: &str,
    ) -> HomeAddress {
        let f = |s: &str| (!s.is_empty()).then(|| s.to_string());
        HomeAddress {
            address1: f(a1),
            address2: f(a2),
            city: f(city),
            state: f(state),
            postal: f(postal),
            country: f(country),
        }
    }

    #[test]
    fn home_address_full_address_joins_present_fields() {
        let a = addr("123 Main St", "Apt 4", "Austin", "Texas", "78701", "USA");
        assert_eq!(
            a.full_address().as_deref(),
            Some("123 Main St, Apt 4, Austin, Texas 78701, USA")
        );
        // Sparse: city + state only.
        let a = addr("", "", "Austin", "Texas", "", "");
        assert_eq!(a.full_address().as_deref(), Some("Austin, Texas"));
        // State with no city, postal with no state.
        let a = addr("", "", "", "Texas", "78701", "");
        assert_eq!(a.full_address().as_deref(), Some("Texas 78701"));
        let a = addr("", "", "", "", "78701", "");
        assert_eq!(a.full_address().as_deref(), Some("78701"));
        assert_eq!(HomeAddress::default().full_address(), None);
    }

    #[test]
    fn home_address_weather_location_is_city_and_state_only() {
        let a = addr("123 Main St", "Apt 4", "Austin", "Texas", "78701", "USA");
        // Street / postal / country are dropped for weather.
        assert_eq!(a.weather_location().as_deref(), Some("Austin, Texas"));
        let a = addr("", "", "Austin", "", "", "");
        assert_eq!(a.weather_location().as_deref(), Some("Austin"));
        let a = addr("", "", "", "Texas", "", "");
        assert_eq!(a.weather_location().as_deref(), Some("Texas"));
        // No city/state ⇒ nothing, even if a street is present.
        let a = addr("123 Main St", "", "", "", "78701", "USA");
        assert_eq!(a.weather_location(), None);
    }

    #[test]
    fn home_address_sanitize_trims_and_drops_blanks() {
        let a = addr("  123 Main St ", "   ", " Austin ", "Texas", "", "").sanitized();
        assert_eq!(a.address1.as_deref(), Some("123 Main St"));
        assert_eq!(a.address2, None);
        assert_eq!(a.city.as_deref(), Some("Austin"));
        assert!(!a.is_empty());
        assert!(HomeAddress::default().is_empty());
    }

    #[test]
    fn live_home_location_exposes_full_and_weather_views() {
        let live = LiveHomeLocation::new(addr(
            "123 Main St",
            "Apt 4",
            "Austin",
            "Texas",
            "78701",
            "USA",
        ));
        assert_eq!(
            live.full().as_deref(),
            Some("123 Main St, Apt 4, Austin, Texas 78701, USA")
        );
        assert_eq!(live.weather().as_deref(), Some("Austin, Texas"));
        // A live edit is reflected immediately through the shared cell.
        let live2 = live.clone();
        live.set(addr("", "", "Boston", "MA", "", ""));
        assert_eq!(live2.weather().as_deref(), Some("Boston, MA"));
        assert_eq!(live2.full().as_deref(), Some("Boston, MA"));
    }

    #[test]
    fn imperial_detection() {
        assert!(units_are_imperial(Some("imperial")));
        assert!(units_are_imperial(Some("US")));
        assert!(units_are_imperial(Some("miles")));
        assert!(!units_are_imperial(Some("metric")));
        assert!(!units_are_imperial(None));
    }
}
