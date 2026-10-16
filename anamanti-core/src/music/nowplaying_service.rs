//! The always-on music now-playing push: a registry of connected `role="music"`
//! channels plus a periodic task that reads the current Spotify playback (now-playing
//! track + up-next queue) and fans it out as `anamanti-music` frames — so the display's
//! music screen stays fresh without a voice turn.
//!
//! Structurally a twin of [`crate::weather::WeatherService`] — both wrap a
//! [`ChannelRegistry`]: the socket is owned by the per-connection task in
//! [`crate::server`]; the registry only holds an mpsc *sender* into that task. A
//! newly-registered channel is immediately replayed the last-known snapshot so the
//! screen appears at once rather than after the next tick. When nothing is playing the
//! push is a `dismiss` frame (and the cache is cleared) so the screen closes.
//!
//! The service also owns the one live [`SpotifyController`] (rebuilt only when the
//! Spotify credentials change), shared by the periodic push and the display's on-screen
//! transport controls ([`crate::control::music_control_response`]) so both reuse the
//! same access-token cache.
//!
//! **Radio** (`plans/RadioPlan.md`): when a [`RadioPlayer`] is attached and a station
//! is on, the push carries the radio snapshot instead of Spotify's, and a station change
//! wakes the loop at once (no 5 s wait). If Spotify starts playing while the radio is on
//! (e.g. from a phone via Spotify Connect) the radio stops and Spotify takes over.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{to_value as json_to_value, Value};
use tokio::sync::mpsc;

use crate::music::{NowPlaying, RadioPlayer, RoutedSpotify, SpotifyCommand, SpotifyController};
use crate::notify::ChannelRegistry;
use crate::settings::{SharedSettings, SpotifyConfig};
use crate::wyoming::protocol::WyomingEvent;

/// Serialize a snapshot to the JSON `music` payload carried in the frame; an empty
/// object on the impossible serialize failure keeps the push infallible.
fn to_value(np: &NowPlaying) -> Value {
    json_to_value(np).unwrap_or_default()
}

/// The `anamanti-music` frame for a snapshot: `now_playing`, or `dismiss` when nothing
/// is playing.
fn to_event(snapshot: Option<&NowPlaying>) -> WyomingEvent {
    match snapshot {
        Some(np) => WyomingEvent::music_now_playing(to_value(np)),
        None => WyomingEvent::music_dismiss(),
    }
}

/// Registry of connected music channels + the last-known snapshot + the cached Spotify
/// controller. Cheap to share behind an `Arc`; construct once at boot and hand a clone
/// to the device-facing server (which registers live channels) and the periodic push.
#[derive(Default)]
pub struct NowPlayingService {
    channels: ChannelRegistry<()>,
    /// The most recent now-playing snapshot (`None` = nothing playing), replayed to a
    /// channel the moment it connects.
    last: Mutex<Option<NowPlaying>>,
    /// The live controller and the credentials it was built from; rebuilt only when
    /// [`SharedSettings::spotify`] changes, so the access-token cache survives.
    controller: Mutex<Option<(SpotifyConfig, Arc<dyn SpotifyController>)>>,
    /// The internet-radio player, when music routing is on.
    radio: Option<Arc<RadioPlayer>>,
}

impl NowPlayingService {
    pub fn new() -> Self {
        Self::default()
    }

    /// A service that also reports (and hands off to/from) the radio player.
    pub fn with_radio(radio: Option<Arc<RadioPlayer>>) -> Self {
        Self {
            radio,
            ..Self::default()
        }
    }

    /// The attached radio player, if any.
    pub fn radio(&self) -> Option<&Arc<RadioPlayer>> {
        self.radio.as_ref()
    }

    /// Register a newly-opened music channel. Returns a connection handle (used to
    /// [`deregister`](Self::deregister) on close) and the receiver the connection task
    /// drains to write pushes out. If a snapshot is already cached it is queued
    /// immediately so the device shows now-playing without waiting for the next tick.
    pub fn register(&self) -> (u64, mpsc::UnboundedReceiver<WyomingEvent>) {
        let initial = self
            .last
            .lock()
            .unwrap()
            .as_ref()
            .map(|np| to_event(Some(np)));
        self.channels.register((), initial)
    }

    /// Drop a channel from the registry (its task is ending / the device closed).
    pub fn deregister(&self, conn_id: u64) {
        self.channels.deregister(conn_id);
    }

    /// How many music channels are currently connected.
    pub fn connected(&self) -> usize {
        self.channels.len()
    }

    /// Cache `snapshot` and push it to every connected device. `Some` fans out a
    /// `now_playing` frame; `None` fans out a `dismiss` frame (and clears the cache so a
    /// late joiner doesn't replay a stale track). Dead channels are pruned. Returns the
    /// delivery count.
    pub fn broadcast(&self, snapshot: Option<NowPlaying>) -> usize {
        let event = to_event(snapshot.as_ref());
        *self.last.lock().unwrap() = snapshot;
        self.channels.broadcast(&event)
    }

    /// The live Spotify controller for the *current* credentials in `settings`, or `None`
    /// when Spotify isn't linked. Cached and only rebuilt when the credentials change
    /// (so a config-page relink takes effect at once without losing the token cache).
    pub fn controller(&self, settings: &SharedSettings) -> Option<Arc<dyn SpotifyController>> {
        let cfg = settings.spotify();
        let mut cached = self.controller.lock().unwrap();
        if let Some((c, ctrl)) = cached.as_ref() {
            if *c == cfg {
                return Some(ctrl.clone());
            }
        }
        let ctrl = cfg.controller().map(|c| match &self.radio {
            Some(radio) => RoutedSpotify::wrap(c, radio.clone()),
            None => c,
        });
        *cached = ctrl.map(|ctrl| (cfg, ctrl));
        cached.as_ref().map(|(_, ctrl)| ctrl.clone())
    }

    /// Apply one on-screen transport action (`pause`/`resume`/`next`/`previous`/
    /// `volume`, as [`SpotifyCommand::transport`] parses them) to whatever is audible:
    /// while a radio station is on it drives the radio (pause = stop, next/previous =
    /// change station, volume = player volume) — even with Spotify unlinked — else
    /// Spotify. Shared by the display's controls ([`crate::control`]) and the config
    /// page. `Ok` carries the speakable confirmation; `Err` is a speakable reason.
    pub async fn transport(
        &self,
        settings: &SharedSettings,
        action: &str,
        value: Option<i64>,
    ) -> anyhow::Result<String> {
        let cmd = SpotifyCommand::transport(action, value)?
            .ok_or_else(|| anyhow::anyhow!("unknown music action `{action}`"))?;
        if let Some(radio) = &self.radio {
            if let Some(res) = radio.transport(&cmd).await {
                return res;
            }
        }
        let controller = self
            .controller(settings)
            .ok_or_else(|| anyhow::anyhow!("Spotify isn't linked on the Anamanti Core"))?;
        controller.command(cmd).await
    }

    /// What is audible right now, read on demand (no hand-off side effects, unlike the
    /// periodic [`Self::tick`]): the radio snapshot when a station is on, else Spotify's
    /// playback. `Ok(None)` = nothing loaded or Spotify not linked.
    pub async fn current(&self, settings: &SharedSettings) -> anyhow::Result<Option<NowPlaying>> {
        if let Some(radio) = self.radio.as_ref().filter(|r| r.is_playing()) {
            return Ok(radio.now_playing().await);
        }
        match self.controller(settings) {
            Some(ctrl) => ctrl.now_playing().await,
            None => Ok(None),
        }
    }

    /// One tick's snapshot: the radio when a station is on, else Spotify. `spotify_was_playing`
    /// carries Spotify's last-seen play state between ticks so a *new* Spotify start while
    /// the radio is on (a rising edge) hands the speakers back to Spotify. `Err` = the
    /// Spotify read failed (skip the tick, keep the screen); `Ok(None)` from Spotify with
    /// no controller means "not linked" and is also skipped (`None` outer).
    async fn tick(
        &self,
        settings: &SharedSettings,
        spotify_was_playing: &mut bool,
    ) -> Option<anyhow::Result<Option<NowPlaying>>> {
        let ctrl = self.controller(settings);
        if let Some(radio) = self.radio.as_ref().filter(|r| r.is_playing()) {
            if let Some(ctrl) = &ctrl {
                if let Ok(Some(sp)) = ctrl.now_playing().await {
                    let rising = sp.playing && !*spotify_was_playing;
                    *spotify_was_playing = sp.playing;
                    if rising {
                        log::info!(
                            "music push: Spotify started while the radio was on; handing off"
                        );
                        radio.release_for_spotify().await;
                        return Some(Ok(Some(sp)));
                    }
                }
            }
            return Some(Ok(radio.now_playing().await));
        }
        let Some(ctrl) = ctrl else {
            // Spotify isn't linked. If the radio just stopped, dismiss its screen;
            // otherwise there is nothing to report.
            let was_radio = self
                .last
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|np| np.source == crate::music::radio::SOURCE_RADIO);
            return was_radio.then_some(Ok(None));
        };
        let res = ctrl.now_playing().await;
        if let Ok(snap) = &res {
            *spotify_was_playing = snap.as_ref().is_some_and(|s| s.playing);
        }
        Some(res)
    }
}

/// Spawn the periodic now-playing push. Every `interval` it reads the current playback
/// through [`NowPlayingService::controller`] and broadcasts it to all connected music
/// channels. No-ops on a tick when no display has a music channel open (so an unwatched
/// Core makes no Spotify calls), Spotify isn't linked (the previous snapshot stays on
/// screen), or the read fails. Returns immediately; the task runs for the process
/// lifetime.
pub fn spawn_periodic(
    service: Arc<NowPlayingService>,
    settings: Arc<SharedSettings>,
    interval: Duration,
) {
    tokio::spawn(async move {
        // A tiny initial delay lets the device dial its channel before the first push.
        tokio::time::sleep(Duration::from_secs(2)).await;
        // A station change (play/stop/step) wakes the loop early so the screen updates
        // at once; with no radio this is a Notify nobody signals.
        let wake = service.radio().map(|r| r.changed()).unwrap_or_default();
        let mut spotify_was_playing = false;
        loop {
            if service.connected() > 0 {
                match service.tick(&settings, &mut spotify_was_playing).await {
                    Some(Ok(snapshot)) => {
                        let playing = snapshot.is_some();
                        let n = service.broadcast(snapshot);
                        log::debug!("music push: playing={playing} → {n} device(s)");
                    }
                    Some(Err(e)) => log::warn!("music push: now-playing read failed: {e:#}"),
                    None => log::debug!("music push: nothing linked/playing; skipping tick"),
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = wake.notified() => {}
            }
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

        let (id_a, mut rx_a) = svc.register();
        let (_id_b, rx_b) = svc.register();
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
        let (_id, mut rx) = svc.register();
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
        let (_id, mut rx) = svc.register();
        // Nothing cached → nothing replayed on connect.
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn dismiss_frame_is_pushed_to_live_channels() {
        let svc = NowPlayingService::new();
        let (_id, mut rx) = svc.register();
        svc.broadcast(None);
        let got = rx.try_recv().expect("live channel received the dismiss");
        assert_eq!(got.event_type, types::MUSIC);
        assert_eq!(got.data["action"], serde_json::json!("dismiss"));
    }
}
