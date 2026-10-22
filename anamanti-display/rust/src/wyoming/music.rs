//! Persistent music now-playing channel (`role=music`): reads the orchestrator's
//! periodic `anamanti-music` now-playing broadcast so the music screen stays fresh
//! without a voice turn. The connection, registration and reconnect logic is the shared
//! [`super::push_channel`] driver; this module only picks the snapshot out of each frame
//! (the raw `music` JSON object as a string — or an **empty string** for a `dismiss`,
//! nothing is playing).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use super::discovery::EndpointCache;
use super::protocol::{self, WyomingEvent};
use super::push_channel::{self, Channel};

/// Extract the serialized now-playing `music` object from an `anamanti-music` frame as a
/// string, an **empty string** for a `dismiss` (nothing playing — clear the screen), or
/// `None` if `ev` is a different frame.
pub fn push_json(ev: &WyomingEvent) -> Option<String> {
    match ev.music_command()? {
        protocol::MusicCommand::NowPlaying(v) => Some(v.to_string()),
        protocol::MusicCommand::Dismiss => Some(String::new()),
        // `screen` commands ride the per-turn socket (voice `music_screen` tool), not this
        // persistent push channel — ignore if one ever arrives here.
        protocol::MusicCommand::Screen(_) => None,
    }
}

/// Run the persistent music channel until `running` is cleared or the consumer goes
/// away (see [`push_channel::run`]). Each push's JSON (or an empty string for dismiss) is
/// handed to `on_push`.
pub async fn run<F>(
    cache: &EndpointCache,
    discovery_timeout: Duration,
    orchestrator_key: Option<String>,
    device_id: String,
    device_name: String,
    running: Arc<AtomicBool>,
    on_push: F,
) where
    F: FnMut(String) -> bool,
{
    let channel = Channel {
        label: "music",
        hello: WyomingEvent::hello_music(device_id, "", device_name),
        outbound: None,
    };
    push_channel::run(
        cache,
        discovery_timeout,
        orchestrator_key,
        channel,
        running,
        push_json,
        on_push,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_snapshot_from_now_playing() {
        let music = json!({ "track_title": "Paranoid Android", "artist": "Radiohead" });
        let ev = WyomingEvent::music_now_playing(music);
        assert!(push_json(&ev).unwrap().contains("Paranoid Android"));
    }

    #[test]
    fn dismiss_yields_an_empty_string_and_other_frames_yield_none() {
        assert_eq!(
            push_json(&WyomingEvent::music_dismiss()),
            Some(String::new())
        );
        assert_eq!(push_json(&WyomingEvent::interrupt()), None);
    }
}
