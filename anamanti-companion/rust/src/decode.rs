//! Decoding of the Core's JSON documents into the FRB structs in
//! [`crate::api::companion`]. Private serde mirrors with lenient defaults keep the
//! FRB-scanned API types free of serde attributes, and let an older/newer Core add or
//! omit fields without breaking the phone.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::api::companion::{
    CoreTimer, DisplayTimers, MemoryEntry, NowPlayingStatus, NowPlayingTrack, TimersSnapshot,
};
use crate::http::check_ok;

// ---- /music/nowplaying.json ----------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawNowPlaying {
    playing: bool,
    track_title: String,
    artist: String,
    album: String,
    artwork_uri: String,
    position_secs: i64,
    duration_secs: i64,
    volume_percent: i64,
    source: String,
    station_id: String,
    station_name: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawNowPlayingStatus {
    linked: bool,
    radio: bool,
    now_playing: Option<RawNowPlaying>,
}

/// Decode `GET /music/nowplaying.json` → `{ok, linked, radio, now_playing}`.
pub fn now_playing(body: &str) -> Result<NowPlayingStatus> {
    let v: Value = serde_json::from_str(body).context("now-playing is not JSON")?;
    check_ok(&v, "now playing")?;
    let raw: RawNowPlayingStatus = serde_json::from_value(v).context("decoding now-playing")?;
    Ok(NowPlayingStatus {
        linked: raw.linked,
        radio: raw.radio,
        now_playing: raw.now_playing.map(|n| NowPlayingTrack {
            playing: n.playing,
            track_title: n.track_title,
            artist: n.artist,
            album: n.album,
            artwork_uri: n.artwork_uri,
            position_secs: n.position_secs.max(0),
            duration_secs: n.duration_secs.max(0),
            volume_percent: n.volume_percent.clamp(0, 100) as u8,
            source: n.source,
            station_id: n.station_id,
            station_name: n.station_name,
        }),
    })
}

// ---- /timers.json --------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawTimer {
    id: i64,
    label: String,
    duration_secs: i64,
    remaining_secs: i64,
    ringing: bool,
    ends_at_ms: i64,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawDisplay {
    device_id: String,
    name: String,
    reported_at_ms: i64,
    timers: Vec<RawTimer>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawTimers {
    now_ms: i64,
    displays: Vec<RawDisplay>,
}

/// Decode `GET /timers.json`. Negative `remaining_secs` are clamped to 0.
pub fn timers(body: &str) -> Result<TimersSnapshot> {
    let v: Value = serde_json::from_str(body).context("timers is not JSON")?;
    check_ok(&v, "timers")?;
    let raw: RawTimers = serde_json::from_value(v).context("decoding timers")?;
    Ok(TimersSnapshot {
        now_ms: raw.now_ms,
        displays: raw
            .displays
            .into_iter()
            .map(|d| DisplayTimers {
                device_id: d.device_id,
                name: d.name,
                reported_at_ms: d.reported_at_ms,
                timers: d
                    .timers
                    .into_iter()
                    .map(|t| CoreTimer {
                        id: t.id,
                        label: t.label,
                        duration_secs: t.duration_secs.max(0),
                        remaining_secs: t.remaining_secs.max(0),
                        ringing: t.ringing,
                        ends_at_ms: t.ends_at_ms,
                    })
                    .collect(),
            })
            .collect(),
    })
}

// ---- anamanti-memories ---------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawMemory {
    id: i64,
    kind: String,
    content: String,
    source: String,
    created_at: i64,
}

/// Decode the `data` of an `anamanti-memories` reply (`{ok, entries:[…]}`).
pub fn memories(data: &Value) -> Result<Vec<MemoryEntry>> {
    check_ok(data, "listing memories")?;
    let entries = match data.get("entries") {
        Some(e) => serde_json::from_value::<Vec<RawMemory>>(e.clone())
            .context("decoding memory entries")?,
        None => Vec::new(),
    };
    Ok(entries
        .into_iter()
        .map(|m| MemoryEntry {
            id: m.id,
            kind: m.kind,
            content: m.content,
            source: m.source,
            created_at: m.created_at,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_spotify_now_playing() {
        let body = json!({
            "ok": true, "linked": true, "radio": false,
            "now_playing": {
                "playing": true, "track_title": "So What", "artist": "Miles Davis",
                "album": "Kind of Blue", "artwork_uri": "https://i.scdn.co/x.jpg",
                "position_secs": 61, "duration_secs": 562, "volume_percent": 40,
                "next_up": [{"track_title": "Freddie"}], "source": "spotify"
            }
        })
        .to_string();
        let s = now_playing(&body).unwrap();
        assert!(s.linked && !s.radio);
        let np = s.now_playing.unwrap();
        assert_eq!(np.track_title, "So What");
        assert_eq!(np.position_secs, 61);
        assert_eq!(np.duration_secs, 562);
        assert_eq!(np.volume_percent, 40);
        assert_eq!(np.station_name, "");
    }

    #[test]
    fn decodes_radio_and_nothing_playing() {
        let radio = json!({
            "ok": true, "linked": false, "radio": true,
            "now_playing": { "playing": true, "track_title": "CBC Music", "source": "radio",
                             "station_id": "CBCM", "station_name": "CBC Music" }
        })
        .to_string();
        let np = now_playing(&radio).unwrap().now_playing.unwrap();
        assert_eq!(np.source, "radio");
        assert_eq!(np.station_name, "CBC Music");
        assert_eq!(np.duration_secs, 0);

        let idle = r#"{"ok":true,"linked":true,"radio":false,"now_playing":null}"#;
        assert!(now_playing(idle).unwrap().now_playing.is_none());

        let err = now_playing(r#"{"ok":false,"message":"token expired"}"#).unwrap_err();
        assert!(err.to_string().contains("token expired"));
    }

    #[test]
    fn decodes_timers_grouped_by_display() {
        let body = json!({
            "ok": true, "now_ms": 1_760_000_000_000i64,
            "displays": [
                { "device_id": "anamanti-aa", "name": "Kitchen", "reported_at_ms": 1_759_999_999_000i64,
                  "timers": [
                    { "id": 1, "label": "pasta", "duration_secs": 600, "remaining_secs": 412,
                      "ringing": false, "ends_at_ms": 1_760_000_412_000i64 },
                    { "id": 2, "label": "", "duration_secs": 60, "remaining_secs": -3,
                      "ringing": true, "ends_at_ms": 1_759_999_997_000i64 }
                  ] },
                { "device_id": "anamanti-bb", "name": "", "reported_at_ms": 0, "timers": [] }
            ]
        })
        .to_string();
        let t = timers(&body).unwrap();
        assert_eq!(t.now_ms, 1_760_000_000_000);
        assert_eq!(t.displays.len(), 2);
        let k = &t.displays[0];
        assert_eq!(k.name, "Kitchen");
        assert_eq!(k.timers[0].label, "pasta");
        assert_eq!(k.timers[0].remaining_secs, 412);
        assert!(k.timers[1].ringing);
        assert_eq!(
            k.timers[1].remaining_secs, 0,
            "negative remaining clamps to 0"
        );
        assert_eq!(t.displays[1].device_id, "anamanti-bb");
    }

    #[test]
    fn timers_tolerates_missing_fields_and_rejects_ok_false() {
        let t = timers(r#"{"ok":true}"#).unwrap();
        assert!(t.displays.is_empty());
        assert!(timers(r#"{"ok":false,"message":"nope"}"#).is_err());
        assert!(timers("<html>").is_err());
    }

    #[test]
    fn decodes_memory_entries() {
        let data = json!({ "ok": true, "entries": [
            { "id": 2, "kind": "preference", "content": "likes jazz", "source": "inferred", "created_at": 100 },
            { "id": 1, "kind": "fact", "content": "name is Sam", "source": "explicit", "created_at": 90 },
        ]});
        let m = memories(&data).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].id, 2);
        assert_eq!(m[0].kind, "preference");
        assert_eq!(m[1].content, "name is Sam");
        assert_eq!(m[1].created_at, 90);

        let err =
            memories(&json!({ "ok": false, "message": "db locked", "entries": [] })).unwrap_err();
        assert!(err.to_string().contains("db locked"));
    }
}
