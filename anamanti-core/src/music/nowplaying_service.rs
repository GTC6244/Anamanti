//! The always-on music now-playing push: a registry of connected `role="music"`
//! channels plus a periodic task that reads the current Spotify playback (now-playing
//! track + up-next queue) and fans it out as `anamanti-music` frames — so the display's
//! music screen stays fresh without a voice turn.
//!
//! Structurally a twin of [`crate::weather::WeatherService`]: the socket is owned by the
//! per-connection task in [`crate::server`]; this registry only holds an mpsc *sender*
//! into that task. A newly-registered channel is immediately replayed the last-known
//! snapshot so the screen appears at once rather than after the next tick. When nothing
//! is playing the push is a `dismiss` frame (and the cache is cleared) so the screen
//! closes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{to_value as json_to_value, Value};
use tokio::sync::mpsc;

use crate::music::{NowPlaying, SpotifyController};
use crate::settings::{SharedSettings, SpotifyConfig};
use crate::wyoming::protocol::WyomingEvent;

/// Serialize a snapshot to the JSON `music` payload carried in the frame; an empty
/// object on the impossible serialize failure keeps the push infallible.
fn to_value(np: &NowPlaying) -> Value {
    json_to_value(np).unwrap_or_default()
}

/// Registry of connected music channels + the last-known snapshot. Cheap to share
/// behind an `Arc`; construct once at boot and hand a clone to the device-facing server
/// (which registers live channels) and the periodic push task.
#[derive(Default)]
pub struct NowPlayingService {
    /// conn_id → the sender feeding that connection's write pump.
    conns: Mutex<HashMap<u64, mpsc::UnboundedSender<WyomingEvent>>>,
    /// Monotonic connection-handle allocator.
    next_conn: AtomicU64,
    /// The most recent now-playing snapshot (`None` = nothing playing), replayed to a
    /// channel the moment it connects.
    last: Mutex<Option<NowPlaying>>,
}

impl NowPlayingService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a newly-opened music channel. Returns a connection handle (used to
    /// [`deregister`](Self::deregister) on close) and the receiver the connection task
    /// drains to write pushes out. If a snapshot is already cached it is queued
    /// immediately so the device shows now-playing without waiting for the next tick.
    pub fn register(&self, _device_id: &str) -> (u64, mpsc::UnboundedReceiver<WyomingEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        if let Some(np) = self.last.lock().unwrap().as_ref() {
            let _ = tx.send(WyomingEvent::music_now_playing(to_value(np)));
        }
        let conn_id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        self.conns.lock().unwrap().insert(conn_id, tx);
        (conn_id, rx)
    }

    /// Drop a channel from the registry (its task is ending / the device closed).
    pub fn deregister(&self, conn_id: u64) {
        self.conns.lock().unwrap().remove(&conn_id);
    }

    /// How many music channels are currently connected.
    pub fn connected(&self) -> usize {
        self.conns.lock().unwrap().len()
    }

    /// Cache `snapshot` and push it to every connected device. `Some` fans out a
    /// `now_playing` frame; `None` fans out a `dismiss` frame (and clears the cache so a
    /// late joiner doesn't replay a stale track). Dead channels are pruned. Returns the
    /// delivery count.
    pub fn broadcast(&self, snapshot: Option<NowPlaying>) -> usize {
        let event = match &snapshot {
            Some(np) => WyomingEvent::music_now_playing(to_value(np)),
            None => WyomingEvent::music_dismiss(),
        };
        *self.last.lock().unwrap() = snapshot;
        let mut conns = self.conns.lock().unwrap();
        let mut delivered = 0usize;
        conns.retain(|_, tx| match tx.send(event.clone()) {
            Ok(()) => {
                delivered += 1;
                true
            }
            Err(_) => false,
        });
        delivered
    }
}

/// Spawn the periodic now-playing push. Every `interval` it reads the live Spotify
/// controller from `settings` (so a config-page relink activates the push without a
/// restart), fetches the current playback, and broadcasts it to all connected music
/// channels. The controller is cached and only rebuilt when the Spotify credentials
/// change, so the access-token cache survives across ticks. No-ops on a tick when
/// Spotify isn't linked (the previous snapshot stays on screen) or the read fails.
/// Returns immediately; the task runs for the process lifetime.
pub fn spawn_periodic(
    service: Arc<NowPlayingService>,
    settings: Arc<SharedSettings>,
    interval: Duration,
) {
    tokio::spawn(async move {
        // A tiny initial delay lets the device dial its channel before the first push.
        tokio::time::sleep(Duration::from_secs(2)).await;
        // Cache (credentials, controller) so the token cache survives; rebuild only when
        // the config changes (SpotifyConfig: PartialEq).
        let mut cached: Option<(SpotifyConfig, Arc<dyn SpotifyController>)> = None;
        loop {
            let cfg = settings.spotify();
            let controller = match &cached {
                Some((c, ctrl)) if *c == cfg => Some(ctrl.clone()),
                _ => match cfg.controller() {
                    Some(ctrl) => {
                        cached = Some((cfg.clone(), ctrl.clone()));
                        Some(ctrl)
                    }
                    None => {
                        cached = None;
                        None
                    }
                },
            };
            match controller {
                Some(ctrl) => match ctrl.now_playing().await {
                    Ok(snapshot) => {
                        let playing = snapshot.is_some();
                        let n = service.broadcast(snapshot);
                        log::debug!("music push: playing={playing} → {n} device(s)");
                    }
                    Err(e) => log::warn!("music push: now-playing read failed: {e:#}"),
                },
                None => log::debug!("music push: Spotify not linked; skipping tick"),
            }
            tokio::time::sleep(interval).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wyoming::protocol::types;

    fn snapshot() -> NowPlaying {
        NowPlaying {
            playing: true,
            track_title: "Paranoid Android".into(),
            artist: "Radiohead".into(),
            album: "OK Computer".into(),
            duration_secs: 383,
            position_secs: 73,
            volume_percent: 65,
            ..Default::default()
        }
    }

    #[test]
    fn broadcasts_and_prunes_dead_channels() {
        let svc = NowPlayingService::new();
        assert_eq!(svc.connected(), 0);

        let (id_a, mut rx_a) = svc.register("dev-a");
        let (_id_b, rx_b) = svc.register("dev-b");
        assert_eq!(svc.connected(), 2);

        drop(rx_b);
        assert_eq!(svc.broadcast(Some(snapshot())), 1);
        assert_eq!(svc.connected(), 1);

        let got = rx_a.try_recv().expect("live channel received the push");
        assert_eq!(got.event_type, types::MUSIC);
        assert_eq!(got.data["action"], serde_json::json!("now_playing"));
        assert_eq!(
            got.data["music"]["track_title"],
            serde_json::json!("Paranoid Android")
        );

        svc.deregister(id_a);
        assert_eq!(svc.connected(), 0);
    }

    #[test]
    fn a_new_channel_gets_the_last_snapshot_immediately() {
        let svc = NowPlayingService::new();
        svc.broadcast(Some(snapshot())); // caches, nobody connected yet
        let (_id, mut rx) = svc.register("late");
        let got = rx
            .try_recv()
            .expect("late joiner replayed the cached snapshot");
        assert_eq!(got.event_type, types::MUSIC);
        assert_eq!(got.data["action"], serde_json::json!("now_playing"));
    }

    #[test]
    fn dismiss_clears_the_cache_so_late_joiners_see_nothing() {
        let svc = NowPlayingService::new();
        svc.broadcast(Some(snapshot()));
        svc.broadcast(None); // playback stopped
        let (_id, mut rx) = svc.register("late");
        // Nothing cached → nothing replayed on connect.
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn dismiss_frame_is_pushed_to_live_channels() {
        let svc = NowPlayingService::new();
        let (_id, mut rx) = svc.register("dev");
        svc.broadcast(None);
        let got = rx.try_recv().expect("live channel received the dismiss");
        assert_eq!(got.event_type, types::MUSIC);
        assert_eq!(got.data["action"], serde_json::json!("dismiss"));
    }
}
