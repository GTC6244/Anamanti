//! Persistent music now-playing channel — the twin of [`super::weather`].
//!
//! The device dials the orchestrator's Wyoming endpoint, sends an `anamanti-hello`
//! `role=music` frame to register, then holds the socket open reading `anamanti-music`
//! push frames (the orchestrator's periodic now-playing broadcast) — reconnecting with
//! capped exponential backoff whenever the connection drops. Separate from the per-turn
//! voice socket and the notify/weather channels, so the music screen stays fresh without
//! a voice turn.
//!
//! The driver is transport-only: it forwards each pushed snapshot (the raw `music` JSON
//! object, as a string) — or an **empty string** for a `dismiss` (nothing is playing) —
//! to a callback that the FRB layer turns into a stream event for Flutter.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::BufReader;
use tokio::net::TcpStream;

use super::discovery::{resolve, EndpointCache};
use super::protocol::{self, WyomingEvent};

/// Shortest / longest wait between reconnect attempts.
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// How long a blocking read waits before it yields so the loop can re-check `running`.
const READ_POLL: Duration = Duration::from_secs(2);

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
/// away. Each push's JSON (or an empty string for dismiss) is handed to `on_push`, which
/// returns `false` when the downstream consumer (the FRB sink) has been dropped — that
/// ends the loop. Reconnects with capped exponential backoff on any drop or failure.
pub async fn run<F>(
    cache: &EndpointCache,
    discovery_timeout: Duration,
    orchestrator_key: Option<String>,
    device_id: String,
    running: Arc<AtomicBool>,
    mut on_push: F,
) where
    F: FnMut(String) -> bool,
{
    let mut backoff = MIN_BACKOFF;
    while running.load(Ordering::SeqCst) {
        match connect_and_listen(
            cache,
            discovery_timeout,
            orchestrator_key.as_deref(),
            &device_id,
            &running,
            &mut on_push,
        )
        .await
        {
            Ok(ListenEnd::ConsumerGone) => return,
            Ok(ListenEnd::Closed) => backoff = MIN_BACKOFF,
            Err(e) => {
                log::debug!("music channel: {e:#}; retrying in {backoff:?}");
                cache.clear();
            }
        }
        let mut waited = Duration::ZERO;
        while running.load(Ordering::SeqCst) && waited < backoff {
            let step = READ_POLL.min(backoff - waited);
            tokio::time::sleep(step).await;
            waited += step;
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Why a single connection ended.
enum ListenEnd {
    Closed,
    ConsumerGone,
}

/// One connection lifecycle: resolve → dial → register (`role=music`) → read pushes
/// until the socket closes, we're stopped, or the consumer goes away.
async fn connect_and_listen<F>(
    cache: &EndpointCache,
    discovery_timeout: Duration,
    orchestrator_key: Option<&str>,
    device_id: &str,
    running: &Arc<AtomicBool>,
    on_push: &mut F,
) -> Result<ListenEnd>
where
    F: FnMut(String) -> bool,
{
    let endpoint = resolve(cache, discovery_timeout, orchestrator_key)
        .await
        .context("resolving orchestrator for music channel")?;
    let stream = TcpStream::connect(endpoint.socket_addr())
        .await
        .with_context(|| format!("dialing {endpoint} for music channel"))?;
    stream.set_nodelay(true).ok();
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut writer = write_half;

    // Register this connection as the device's music channel (role=music).
    protocol::write_event(&mut writer, &WyomingEvent::hello_music(device_id, ""))
        .await
        .context("sending anamanti-hello (music)")?;
    log::info!("music channel connected to {endpoint}");

    loop {
        if !running.load(Ordering::SeqCst) {
            return Ok(ListenEnd::Closed);
        }
        match tokio::time::timeout(READ_POLL, protocol::read_event(&mut reader)).await {
            Err(_elapsed) => continue,
            Ok(read) => match read.context("reading music frame")? {
                None => return Ok(ListenEnd::Closed),
                Some(ev) => {
                    if let Some(json) = push_json(&ev) {
                        if !on_push(json) {
                            return Ok(ListenEnd::ConsumerGone);
                        }
                    }
                }
            },
        }
    }
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
