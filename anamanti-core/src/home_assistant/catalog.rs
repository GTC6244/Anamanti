//! The cached device catalog and the pure target resolver for the Home Assistant
//! fast path (`plans/HomeAssistantPlan.md` §A2).
//!
//! The catalog is a flat list of HA's controllable entities (id, name, room, state),
//! fetched in one `/api/template` call. [`resolve`] turns a spoken target phrase into
//! the entities it means — a room's lights, every light, the display's own room, or a
//! named device — and **refuses to guess**: an unknown or ambiguous phrase returns
//! `None` so the turn falls through to the LLM.

use serde::Deserialize;

use super::matcher::normalize;

/// One controllable Home Assistant entity, as rendered by the catalog template.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Entity {
    /// `domain.object_id`, e.g. `light.office`.
    #[serde(rename = "id")]
    pub entity_id: String,
    /// HA's friendly name.
    pub name: String,
    /// The HA area (room) name, empty when unassigned.
    #[serde(default)]
    pub area: String,
    /// The current state string (`on` / `off` / `unavailable` / `playing` / …).
    pub state: String,
}

/// Names so generic that, spoken back, they need the room ("Lights" → "Office lights").
const GENERIC_NAMES: &[&str] = &["light", "lamp", "switch", "plug", "fan"];

impl Entity {
    /// The HA domain (`light`, `switch`, `scene`, …).
    pub fn domain(&self) -> &str {
        self.entity_id.split('.').next().unwrap_or_default()
    }

    /// Whether HA can currently reach the device.
    pub fn available(&self) -> bool {
        !matches!(self.state.as_str(), "unavailable" | "unknown")
    }

    /// A light for room-wide / "all lights" purposes: the `light` domain, plus switches
    /// whose name says light or lamp (many wall switches are modelled as `switch`).
    pub fn is_light_like(&self) -> bool {
        match self.domain() {
            "light" => true,
            "switch" => {
                let k = key(&self.name);
                k.split(' ').any(|w| w == "light" || w == "lamp")
            }
            _ => false,
        }
    }

    /// How to say this device's name: its HA name, prefixed with the room when the name
    /// alone is generic ("Lights" in Office → "Office lights").
    pub fn label(&self) -> String {
        if !self.area.is_empty() && GENERIC_NAMES.contains(&key(&self.name).as_str()) {
            capitalize(&format!("{} {}", self.area, self.name.to_lowercase()))
        } else {
            capitalize(&self.name)
        }
    }

    /// Whether this entity is one that must be named exactly (never swept up by a
    /// room-wide or "all" command) — e.g. a fireplace.
    pub fn is_exact_only(&self, patterns: &[String]) -> bool {
        let name = key(&self.name);
        let id = self.entity_id.to_lowercase();
        patterns.iter().any(|p| {
            let p = key(p);
            !p.is_empty() && (name.contains(&p) || id.contains(&p.replace(' ', "_")))
        })
    }
}

/// The house's controllable entities at one moment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    pub entities: Vec<Entity>,
}

impl Catalog {
    /// Parse the catalog template's JSON output.
    pub fn from_json(text: &str) -> serde_json::Result<Self> {
        Ok(Self {
            entities: serde_json::from_str(text)?,
        })
    }

    /// The canonical spelling of a room, matched by its [`key`].
    fn area_named(&self, k: &str) -> Option<&str> {
        self.entities
            .iter()
            .map(|e| e.area.as_str())
            .find(|a| !a.is_empty() && key(a) == k)
    }

    /// Every distinct room name, sorted (for the LLM tool's "known rooms" hint).
    pub fn areas(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .entities
            .iter()
            .filter(|e| !e.area.is_empty())
            .map(|e| e.area.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    }
}

/// The comparison key for names and phrases: [`normalize`]d, articles dropped, and each
/// word singularized ("the Deck Lights" → "deck light"), applied identically to both
/// sides so plural/singular and "the" never block a match.
pub fn key(s: &str) -> String {
    normalize(s)
        .split(' ')
        .filter(|w| !matches!(*w, "the" | "my" | "our" | "a"))
        .map(singular)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Strip one plural `s` (not from short words or `ss` endings).
fn singular(w: &str) -> &str {
    if w.len() > 3 && w.ends_with('s') && !w.ends_with("ss") {
        &w[..w.len() - 1]
    } else {
        w
    }
}

/// Uppercase the first character.
fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// What a target phrase resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// How to say it ("Office lights", "All lights", "Porch lights").
    pub label: String,
    /// The entities it covers (one device, or a same-name pair in one room, or a group).
    pub entities: Vec<Entity>,
    /// A room-wide / "all" group (spoken in the plural) rather than one named device.
    pub group: bool,
}

/// Resolve a spoken target phrase against the catalog.
///
/// Order (first hit wins): `<room> lights` / `lights in <room>` → that room's lights;
/// `all lights` / `every light` → the whole house's lights; bare `lights` → the
/// display's `room`; an exact device name (or `<room> <name>`); then a unique
/// trailing-word match ("tv" → "Samsung TV"). Groups never include `exact_only`
/// entities. Same-named devices in different rooms are narrowed to `room`; if that still
/// leaves more than one room, this returns `None` rather than guess.
pub fn resolve(
    catalog: &Catalog,
    phrase: &str,
    room: Option<&str>,
    exact_only: &[String],
) -> Option<Target> {
    let t = key(phrase);
    if t.is_empty() {
        return None;
    }

    // A room-wide / whole-house light group.
    let lights_in = |area: Option<&str>, label: String| -> Option<Target> {
        let entities: Vec<Entity> = catalog
            .entities
            .iter()
            .filter(|e| e.is_light_like() && !e.is_exact_only(exact_only))
            .filter(|e| area.is_none_or(|a| key(&e.area) == key(a)))
            .cloned()
            .collect();
        (!entities.is_empty()).then_some(Target {
            label,
            entities,
            group: true,
        })
    };

    // 1. "<room> light(s)" / "light(s) in <room>"
    let room_phrase = t
        .strip_suffix(" light")
        .or_else(|| t.strip_prefix("light in "))
        .or_else(|| t.strip_prefix("light on "));
    if let Some(area) = room_phrase.and_then(|r| catalog.area_named(r)) {
        return lights_in(Some(area), format!("{} lights", capitalize(area)));
    }
    // 2. every light in the house
    if matches!(
        t.as_str(),
        "all light" | "all of light" | "every light" | "all"
    ) {
        return lights_in(None, "All lights".to_string());
    }
    // 3. bare "lights" → this display's room
    if t == "light" {
        let area = catalog.area_named(&key(room?))?;
        return lights_in(Some(area), format!("{} lights", capitalize(area)));
    }

    // 4. an exact device name, or "<room> <name>"; 5. a unique trailing-word match.
    let exact = |e: &&Entity| {
        key(&e.name) == t || (!e.area.is_empty() && key(&format!("{} {}", e.area, e.name)) == t)
    };
    let suffix = |e: &&Entity| {
        !GENERIC_NAMES.contains(&t.as_str()) && key(&e.name).ends_with(&format!(" {t}"))
    };
    for matcher in [&exact as &dyn Fn(&&Entity) -> bool, &suffix] {
        let hits: Vec<&Entity> = catalog.entities.iter().filter(matcher).collect();
        if let Some(target) = narrow(hits, room) {
            return Some(target);
        }
    }
    None
}

/// Pick one device from name matches: a single room's matches are one target (a
/// same-named pair in one room — e.g. a light and its switch — is operated together);
/// matches spread over rooms are narrowed to the display's `room`, else ambiguous.
fn narrow(hits: Vec<&Entity>, room: Option<&str>) -> Option<Target> {
    if hits.is_empty() {
        return None;
    }
    let mut areas: Vec<String> = hits.iter().map(|e| key(&e.area)).collect();
    areas.sort();
    areas.dedup();
    let chosen: Vec<&Entity> = if areas.len() == 1 {
        hits
    } else {
        let r = key(room?);
        let in_room: Vec<&Entity> = hits.into_iter().filter(|e| key(&e.area) == r).collect();
        if in_room.is_empty() {
            return None;
        }
        in_room
    };
    Some(Target {
        label: chosen[0].label(),
        entities: chosen.into_iter().cloned().collect(),
        group: false,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn e(id: &str, name: &str, area: &str, state: &str) -> Entity {
        Entity {
            entity_id: id.into(),
            name: name.into(),
            area: area.into(),
            state: state.into(),
        }
    }

    /// A trimmed copy of the real household catalog (2026-10-15), duplicates and all.
    pub(crate) fn house() -> Catalog {
        Catalog {
            entities: vec![
                e("light.lights_office", "Lights", "Office", "on"),
                e("light.pot_lights_office", "Pot lights", "Office", "on"),
                e("switch.lights_office", "Lights", "Office", "off"),
                e("light.lights_dining", "Lights", "Dining", "on"),
                e("light.pot_lights_dining", "Pot LIghts", "Dining", "on"),
                e("switch.lights_bathroom", "Lights", "Bathroom", "off"),
                e("switch.porch_lights", "Porch lights", "Outside", "off"),
                e("switch.deck_lights", "Deck Lights", "Outside", "off"),
                e("switch.track_lights", "Track lights", "Kitchen", "off"),
                e(
                    "switch.brendons_light",
                    "Brendon's light",
                    "Master Bedroom",
                    "off",
                ),
                e(
                    "light.furnace_room_light",
                    "Furnace room lights",
                    "Utility Room",
                    "off",
                ),
                e(
                    "switch.furnace_room_lights",
                    "Furnace room lights",
                    "Utility Room",
                    "off",
                ),
                e("switch.fireplace", "Fireplace", "Living Room", "off"),
                e(
                    "switch.fireplace_relay",
                    "Fireplace Relay",
                    "Living Room",
                    "off",
                ),
                e(
                    "switch.fireplace_lights",
                    "Fireplace lights",
                    "Living Room",
                    "off",
                ),
                e("light.living_room_lamp", "Lamp", "Living Room", "off"),
                e(
                    "switch.decora_smart_plug",
                    "Decora Smart Plug",
                    "Office",
                    "off",
                ),
                e("light.bathroom_light", "Bathroom Light", "", "unavailable"),
                e("light.bedroom_lights", "Bedroom Lights", "", "unavailable"),
                e("media_player.samsung_tv", "Samsung TV", "Media room", "off"),
                e("remote.samsung_tv", "Samsung TV", "Media room", "off"),
                e(
                    "scene.media_room_tv_on",
                    "Media Room TV On",
                    "Media room",
                    "2025-10-13",
                ),
                e(
                    "scene.main_floor_lights_off",
                    "Main Floor lights off",
                    "",
                    "2024-12-06",
                ),
                e(
                    "script.lights_off_basement",
                    "Lights off in the Basement",
                    "",
                    "off",
                ),
            ],
        }
    }

    fn fire() -> Vec<String> {
        vec!["fireplace".to_string()]
    }

    fn ids(t: &Target) -> Vec<&str> {
        t.entities.iter().map(|e| e.entity_id.as_str()).collect()
    }

    #[test]
    fn key_singularizes_and_drops_articles() {
        assert_eq!(key("the Deck Lights"), "deck light");
        assert_eq!(key("Brendon's light"), "brendon light");
        assert_eq!(key("Glass"), "glass");
    }

    #[test]
    fn room_lights_cover_lights_and_light_switches() {
        let t = resolve(&house(), "office lights", None, &fire()).unwrap();
        assert!(t.group);
        assert_eq!(t.label, "Office lights");
        assert_eq!(
            ids(&t),
            [
                "light.lights_office",
                "light.pot_lights_office",
                "switch.lights_office"
            ]
        );
        // The smart plug is not light-like, so it is not swept up.
        let t = resolve(&house(), "lights in office", None, &fire()).unwrap();
        assert!(!ids(&t).contains(&"switch.decora_smart_plug"));
    }

    #[test]
    fn groups_never_include_the_fireplace() {
        // "Fireplace lights" is light-like, so only the exact-only rule keeps it out.
        let all = resolve(&house(), "all lights", None, &fire()).unwrap();
        assert!(all.group);
        assert!(all
            .entities
            .iter()
            .all(|e| !e.entity_id.contains("fireplace")));
        let room = resolve(&house(), "living room lights", None, &fire()).unwrap();
        assert_eq!(ids(&room), ["light.living_room_lamp"]);
        // Without the rule it would be swept up.
        let unguarded = resolve(&house(), "living room lights", None, &[]).unwrap();
        assert!(ids(&unguarded).contains(&"switch.fireplace_lights"));
        // …but naming it exactly works.
        let t = resolve(&house(), "fireplace", None, &fire()).unwrap();
        assert_eq!(ids(&t), ["switch.fireplace"]);
    }

    #[test]
    fn bare_lights_need_the_display_room() {
        assert_eq!(resolve(&house(), "lights", None, &fire()), None);
        let t = resolve(&house(), "lights", Some("dining"), &fire()).unwrap();
        assert_eq!(t.label, "Dining lights");
        assert_eq!(ids(&t), ["light.lights_dining", "light.pot_lights_dining"]);
    }

    #[test]
    fn named_devices_resolve_exactly() {
        let t = resolve(&house(), "porch lights", None, &fire()).unwrap();
        assert_eq!((t.label.as_str(), t.group), ("Porch lights", false));
        let t = resolve(&house(), "brendons light", None, &fire()).unwrap();
        assert_eq!(ids(&t), ["switch.brendons_light"]);
        // "<room> <name>" disambiguates a duplicate name.
        let t = resolve(&house(), "office pot lights", None, &fire()).unwrap();
        assert_eq!(ids(&t), ["light.pot_lights_office"]);
    }

    #[test]
    fn duplicates_across_rooms_need_the_room_or_fall_through() {
        assert_eq!(resolve(&house(), "pot lights", None, &fire()), None);
        let t = resolve(&house(), "pot lights", Some("Office"), &fire()).unwrap();
        assert_eq!(ids(&t), ["light.pot_lights_office"]);
    }

    #[test]
    fn same_name_same_room_is_operated_together() {
        let t = resolve(&house(), "furnace room lights", None, &fire()).unwrap();
        assert_eq!(t.entities.len(), 2);
        let t = resolve(&house(), "tv", None, &fire()).unwrap();
        assert_eq!(ids(&t), ["media_player.samsung_tv", "remote.samsung_tv"]);
        assert_eq!(t.label, "Samsung TV");
    }

    #[test]
    fn unknown_targets_fall_through() {
        for phrase in [
            "timer for ten minutes",
            "garage",
            "kitchen lights in the attic",
            "it",
        ] {
            assert_eq!(resolve(&house(), phrase, None, &fire()), None, "{phrase:?}");
        }
    }

    #[test]
    fn generic_names_are_labelled_with_their_room() {
        let lights = &house().entities[0];
        assert_eq!(lights.label(), "Office lights");
        assert_eq!(house().entities[6].label(), "Porch lights");
    }

    #[test]
    fn parses_the_template_json() {
        let c = Catalog::from_json(
            r#"[{"id":"light.x","name":"X","area":"Office","state":"on"},
                {"id":"scene.y","name":"Y","area":"","state":"2024-01-01"}]"#,
        )
        .unwrap();
        assert_eq!(c.entities.len(), 2);
        assert_eq!(c.entities[1].domain(), "scene");
        assert_eq!(c.areas(), ["Office"]);
    }
}
