//! Which music widget the display shows while something is playing
//! (`plans/MusicDisplayStylePlan.md`): the **compact** transport bar riding the ambient
//! screen, or the **full-screen** now-playing view. One household-wide preference shared
//! by Spotify and the radio (they render through the same widgets), owned by the Core so
//! the config page, the display's on-screen toggle, and the voice `music_screen` tool all
//! edit the same value. It is persisted in the settings overlay and stamped onto every
//! `anamanti-music` now-playing push (`display_style`) so every display follows it.

use serde::{Deserialize, Deserializer, Serialize};

/// The music widget preference. `Compact` (the small bar) is the default.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_labels_and_synonyms() {
        assert_eq!(
            MusicDisplayStyle::parse("compact"),
            Some(MusicDisplayStyle::Compact)
        );
        assert_eq!(
            MusicDisplayStyle::parse(" Full Screen "),
            Some(MusicDisplayStyle::Fullscreen)
        );
        assert_eq!(
            MusicDisplayStyle::parse("full-screen"),
            Some(MusicDisplayStyle::Fullscreen)
        );
        assert_eq!(
            MusicDisplayStyle::parse("small"),
            Some(MusicDisplayStyle::Compact)
        );
        assert_eq!(MusicDisplayStyle::parse("huge"), None);
    }

    #[test]
    fn serde_roundtrips_and_reads_leniently() {
        let json = serde_json::to_string(&MusicDisplayStyle::Fullscreen).unwrap();
        assert_eq!(json, "\"fullscreen\"");
        let back: MusicDisplayStyle = serde_json::from_str(&json).unwrap();
        assert_eq!(back, MusicDisplayStyle::Fullscreen);
        let junk: MusicDisplayStyle = serde_json::from_str("\"sideways\"").unwrap();
        assert_eq!(junk, MusicDisplayStyle::Compact);
        let null: MusicDisplayStyle = serde_json::from_str("null").unwrap();
        assert_eq!(null, MusicDisplayStyle::Compact);
    }
}
