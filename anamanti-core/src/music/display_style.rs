//! Which music widget each display shows while something is playing
//! (`plans/MusicDisplayStylePlan.md`): the **compact** transport bar riding the ambient
//! screen, or the **full-screen** now-playing view. Spotify and the radio render through
//! the same widgets, so one style applies to both.
//!
//! The style is chosen **per display**, in this order (first match wins):
//!
//! 1. **This session's choice for that display** — the display's own expand / minimize
//!    button, or a voice "make the music full screen" spoken *to that display*. Lasts
//!    until the music stops or a new play request starts a new session.
//! 2. **The display's pinned preference** — set on the config page (Music & Devices →
//!    Displays): "always full screen" or "always the small bar". Persisted in the settings
//!    overlay ([`MusicDisplayPins`]); a display with no pin is *automatic*.
//! 3. **Automatic** — full screen on the display the play request came from (the session's
//!    *initiator*), the compact bar on every other display. Music started from outside any
//!    display (the Spotify app, the config page) has no initiator, so it is compact
//!    everywhere.
//!
//! [`MusicDisplayRouter`] holds the session state (initiator + per-display choices); the
//! now-playing push asks it for each display's style and stamps it into that display's
//! `anamanti-music` push (`display_style`).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Deserializer, Serialize};
use tokio::sync::Notify;

/// The music widget a display shows. `Compact` (the small bar) is the default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MusicDisplayStyle {
    /// The small transport bar at the bottom of the ambient screen.
    #[default]
    Compact,
    /// The full-screen now-playing view.
    Fullscreen,
}

impl MusicDisplayStyle {
    /// The wire / JSON label (`"compact"` / `"fullscreen"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Fullscreen => "fullscreen",
        }
    }

    /// Parse a label tolerantly (case/whitespace-insensitive, a few synonyms); `None`
    /// for anything unrecognized.
    pub fn parse(label: &str) -> Option<Self> {
        match label
            .trim()
            .to_ascii_lowercase()
            .replace(['-', ' '], "_")
            .as_str()
        {
            "compact" | "small" | "mini" | "bar" | "overlay" => Some(Self::Compact),
            "fullscreen" | "full_screen" | "full" | "large" | "big" => Some(Self::Fullscreen),
            _ => None,
        }
    }
}

/// Lenient on read: an unknown/garbled persisted value falls back to the default rather
/// than failing the whole settings file (which would be renamed aside as corrupt).
impl<'de> Deserialize<'de> for MusicDisplayStyle {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = Option::<String>::deserialize(d)?;
        Ok(raw.as_deref().and_then(Self::parse).unwrap_or_default())
    }
}

/// Per-display pinned styles, keyed by the display's `device_id`. A display that is not
/// in the map is **automatic** (full screen where the play request came from, compact
/// elsewhere). Persisted in the settings overlay.
pub type MusicDisplayPins = BTreeMap<String, MusicDisplayStyle>;

/// Lenient reader for [`MusicDisplayPins`]: anything but an object of label strings reads
/// as "no pins" (unknown entries are dropped) instead of failing the settings file.
pub fn deserialize_pins<'de, D: Deserializer<'de>>(d: D) -> Result<MusicDisplayPins, D::Error> {
    let raw = Option::<serde_json::Value>::deserialize(d)?;
    let Some(serde_json::Value::Object(map)) = raw else {
        return Ok(MusicDisplayPins::new());
    };
    Ok(map
        .into_iter()
        .filter(|(id, _)| !id.trim().is_empty())
        .filter_map(|(id, v)| {
            v.as_str()
                .and_then(MusicDisplayStyle::parse)
                .map(|s| (id, s))
        })
        .collect())
}

/// What a play request means for the current session (see [`MusicDisplayRouter::play_requested`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayRequest {
    /// A new play (a song / artist / station): always starts a new session.
    Play,
    /// A resume: starts a new session only when nothing is loaded (music that is merely
    /// paused keeps its session and its per-display choices).
    Resume,
}

#[derive(Debug, Default)]
struct Session {
    /// Whether music is loaded (the last push was a snapshot, not a dismiss).
    active: bool,
    /// The display the play request came from (shown full screen by default).
    initiator: Option<String>,
    /// This session's explicit per-display choices (button / voice).
    chosen: HashMap<String, MusicDisplayStyle>,
}

/// The session half of the per-display music style: who started the music and what each
/// display chose since. Shared (behind an `Arc`) by the voice tools that start music, the
/// display's on-screen toggle, the config page, and the now-playing push, which reads
/// [`Self::style_for`] for every connected display.
///
/// Every change signals [`Self::changed`] so the now-playing push re-sends at once.
#[derive(Debug, Default)]
pub struct MusicDisplayRouter {
    session: Mutex<Session>,
    changed: Arc<Notify>,
}

/// Normalize a device id: trimmed, `None` when blank (an older display sends none).
fn device_key(device_id: Option<&str>) -> Option<String> {
    device_id
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(str::to_string)
}

impl MusicDisplayRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Signalled whenever a display's style may have changed, so the now-playing push can
    /// re-send without waiting for its next tick.
    pub fn changed(&self) -> Arc<Notify> {
        self.changed.clone()
    }

    /// A play request from `device_id` (`None` = not from a display, e.g. an older display
    /// that doesn't identify itself). Call it **before** issuing the play so the first push
    /// that shows the new music already carries the right styles. A [`PlayRequest::Play`]
    /// always starts a new session (the requester becomes the initiator and every display's
    /// session choice is forgotten); a [`PlayRequest::Resume`] does so only when nothing is
    /// loaded.
    pub fn play_requested(&self, device_id: Option<&str>, kind: PlayRequest) {
        {
            let mut s = self.session.lock().unwrap();
            if kind == PlayRequest::Resume && s.active {
                return;
            }
            s.initiator = device_key(device_id);
            s.chosen.clear();
        }
        self.changed.notify_one();
    }

    /// A display explicitly chose a style for this session (its expand / minimize button,
    /// or a voice request spoken to it). Ignored for a blank `device_id`.
    pub fn choose(&self, device_id: &str, style: MusicDisplayStyle) {
        let Some(id) = device_key(Some(device_id)) else {
            return;
        };
        self.session.lock().unwrap().chosen.insert(id, style);
        self.changed.notify_one();
    }

    /// Forget `device_id`'s session choice so its pin / the automatic rule applies again
    /// (used when the config page changes that display's pin).
    pub fn forget(&self, device_id: &str) {
        self.session.lock().unwrap().chosen.remove(device_id.trim());
        self.changed.notify_one();
    }

    /// Record what the push is about to send: `true` for a now-playing snapshot, `false`
    /// for a dismiss. Music going from loaded to nothing ends the session (the initiator
    /// and every choice are forgotten). A dismiss while nothing was loaded changes nothing,
    /// so the idle push between a play request and the music actually starting can't wipe
    /// the request's initiator.
    pub fn set_active(&self, active: bool) {
        let mut s = self.session.lock().unwrap();
        if s.active && !active {
            s.initiator = None;
            s.chosen.clear();
        }
        s.active = active;
    }

    /// The style `device_id` should show now: its session choice, else its pin, else full
    /// screen when it started the music and compact otherwise.
    pub fn style_for(&self, device_id: &str, pins: &MusicDisplayPins) -> MusicDisplayStyle {
        let id = device_id.trim();
        let s = self.session.lock().unwrap();
        if let Some(style) = s.chosen.get(id) {
            return *style;
        }
        if let Some(style) = pins.get(id) {
            return *style;
        }
        if !id.is_empty() && s.initiator.as_deref() == Some(id) {
            MusicDisplayStyle::Fullscreen
        } else {
            MusicDisplayStyle::Compact
        }
    }

    /// The display that started the current session, if any (for the config page).
    pub fn initiator(&self) -> Option<String> {
        self.session.lock().unwrap().initiator.clone()
    }

    /// Whether `device_id` made an explicit choice this session (for the config page).
    pub fn has_choice(&self, device_id: &str) -> bool {
        self.session
            .lock()
            .unwrap()
            .chosen
            .contains_key(device_id.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use MusicDisplayStyle::{Compact, Fullscreen};

    #[test]
    fn parses_labels_and_synonyms() {
        assert_eq!(MusicDisplayStyle::parse("compact"), Some(Compact));
        assert_eq!(MusicDisplayStyle::parse(" Full Screen "), Some(Fullscreen));
        assert_eq!(MusicDisplayStyle::parse("full-screen"), Some(Fullscreen));
        assert_eq!(MusicDisplayStyle::parse("small"), Some(Compact));
        assert_eq!(MusicDisplayStyle::parse("huge"), None);
    }

    #[test]
    fn serde_roundtrips_and_reads_leniently() {
        let json = serde_json::to_string(&Fullscreen).unwrap();
        assert_eq!(json, "\"fullscreen\"");
        let back: MusicDisplayStyle = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Fullscreen);
        let junk: MusicDisplayStyle = serde_json::from_str("\"sideways\"").unwrap();
        assert_eq!(junk, Compact);
        let null: MusicDisplayStyle = serde_json::from_str("null").unwrap();
        assert_eq!(null, Compact);
    }

    #[test]
    fn pins_read_leniently() {
        #[derive(Deserialize)]
        struct W {
            #[serde(default, deserialize_with = "deserialize_pins")]
            pins: MusicDisplayPins,
        }
        let w: W = serde_json::from_str(
            r#"{"pins":{"kitchen":"fullscreen","den":"small","x":"sideways","":"compact","y":3}}"#,
        )
        .unwrap();
        assert_eq!(
            w.pins,
            MusicDisplayPins::from([("den".into(), Compact), ("kitchen".into(), Fullscreen)])
        );
        // A legacy / garbled value is "no pins", never a failed settings file.
        let w: W = serde_json::from_str(r#"{"pins":"fullscreen"}"#).unwrap();
        assert!(w.pins.is_empty());
        let w: W = serde_json::from_str("{}").unwrap();
        assert!(w.pins.is_empty());
    }

    #[test]
    fn the_requesting_display_is_full_screen_and_the_others_compact() {
        let r = MusicDisplayRouter::new();
        let pins = MusicDisplayPins::new();
        r.play_requested(Some("kitchen"), PlayRequest::Play);
        r.set_active(true);
        assert_eq!(r.style_for("kitchen", &pins), Fullscreen);
        assert_eq!(r.style_for("den", &pins), Compact);
        assert_eq!(r.style_for("", &pins), Compact);
    }

    #[test]
    fn music_started_elsewhere_is_compact_everywhere() {
        let r = MusicDisplayRouter::new();
        let pins = MusicDisplayPins::new();
        r.set_active(true); // e.g. started from the Spotify app
        assert_eq!(r.style_for("kitchen", &pins), Compact);
        r.play_requested(None, PlayRequest::Play); // an older display: no id
        assert_eq!(r.style_for("kitchen", &pins), Compact);
    }

    #[test]
    fn each_display_chooses_separately_for_the_session() {
        let r = MusicDisplayRouter::new();
        let pins = MusicDisplayPins::new();
        r.play_requested(Some("kitchen"), PlayRequest::Play);
        r.set_active(true);
        r.choose("kitchen", Compact);
        r.choose("den", Fullscreen);
        assert_eq!(r.style_for("kitchen", &pins), Compact);
        assert_eq!(r.style_for("den", &pins), Fullscreen);
        assert_eq!(r.style_for("bedroom", &pins), Compact);
        assert!(r.has_choice("den") && !r.has_choice("bedroom"));
        r.forget("den");
        assert_eq!(r.style_for("den", &pins), Compact);
    }

    #[test]
    fn a_choice_beats_a_pin_which_beats_the_automatic_rule() {
        let r = MusicDisplayRouter::new();
        let pins =
            MusicDisplayPins::from([("kitchen".into(), Compact), ("den".into(), Fullscreen)]);
        r.play_requested(Some("kitchen"), PlayRequest::Play);
        r.set_active(true);
        assert_eq!(
            r.style_for("kitchen", &pins),
            Compact,
            "pin beats initiator"
        );
        assert_eq!(r.style_for("den", &pins), Fullscreen, "pin beats default");
        r.choose("kitchen", Fullscreen);
        assert_eq!(
            r.style_for("kitchen", &pins),
            Fullscreen,
            "choice beats pin"
        );
    }

    #[test]
    fn a_new_play_request_starts_a_new_session() {
        let r = MusicDisplayRouter::new();
        let pins = MusicDisplayPins::new();
        r.play_requested(Some("kitchen"), PlayRequest::Play);
        r.set_active(true);
        r.choose("den", Fullscreen);
        r.play_requested(Some("den"), PlayRequest::Play);
        assert_eq!(r.style_for("kitchen", &pins), Compact);
        assert_eq!(r.style_for("den", &pins), Fullscreen);
        assert!(!r.has_choice("den"), "choices reset with the new session");
    }

    #[test]
    fn resume_only_claims_the_music_when_nothing_is_loaded() {
        let r = MusicDisplayRouter::new();
        let pins = MusicDisplayPins::new();
        r.play_requested(Some("kitchen"), PlayRequest::Play);
        r.set_active(true);
        r.choose("den", Fullscreen);
        // Paused music resumed from the den keeps the session.
        r.play_requested(Some("den"), PlayRequest::Resume);
        assert_eq!(r.initiator().as_deref(), Some("kitchen"));
        assert!(r.has_choice("den"));
        // Once the music has stopped, a resume starts a new session.
        r.set_active(false);
        r.play_requested(Some("den"), PlayRequest::Resume);
        assert_eq!(r.initiator().as_deref(), Some("den"));
        assert_eq!(r.style_for("den", &pins), Fullscreen);
    }

    #[test]
    fn the_session_ends_when_the_music_stops_but_not_before_it_starts() {
        let r = MusicDisplayRouter::new();
        let pins = MusicDisplayPins::new();
        r.play_requested(Some("kitchen"), PlayRequest::Play);
        // The idle push between the request and the music starting is a dismiss.
        r.set_active(false);
        assert_eq!(r.style_for("kitchen", &pins), Fullscreen);
        r.set_active(true);
        r.choose("den", Fullscreen);
        r.set_active(false); // music stopped
        assert_eq!(r.initiator(), None);
        assert_eq!(r.style_for("kitchen", &pins), Compact);
        assert_eq!(r.style_for("den", &pins), Compact);
    }
}
