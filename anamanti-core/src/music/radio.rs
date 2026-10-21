//! Internet radio (`plans/RadioPlan.md`): a curated station catalog played through the
//! **existing** mpv web player → snapserver's `Web` stream, i.e. the same Snapcast
//! speakers Spotify plays on. Still control-only: the Core loads a URL into mpv and
//! points the snapserver groups at the `Web` stream; it never touches PCM.
//!
//! * [`RadioStation`] / [`RadioCatalog`] — the GUI-edited station list (persisted in the
//!   settings overlay), with sanitize + spoken-name resolution.
//! * [`RadioPlayer`] — play/stop/next/previous/volume, the Snapcast stream switch, and
//!   the now-playing snapshot (station + the stream's ICY song title).
//! * [`RoutedSpotify`] — wraps the Spotify controller so the two sources hand off
//!   cleanly: transport while radio is on drives the radio; starting Spotify stops it.
//! * [`RadioOutput`] — the mpv + snapserver seam, faked in tests.

use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Notify;

use super::{MpvControl, NowPlaying, SnapcastClient, SpotifyCommand, SpotifyController};

/// [`NowPlaying::source`] for a radio snapshot.
pub const SOURCE_RADIO: &str = "radio";
/// The snapserver stream radio plays on (`deploy/snapcast/snapserver.conf`).
pub const WEB_STREAM: &str = "Web";
/// The snapserver stream Spotify (librespot) plays on.
pub const SPOTIFY_STREAM: &str = "Spotify";
/// Don't retry a dropped stream more often than this.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

/// One curated station. `id` is the short stable key (the call sign, e.g. `CJDV`) the
/// display, the voice tool and the web UI select by; `name` is what is shown/spoken.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RadioStation {
    // Every field is defaulted so a partial entry from the page reaches
    // `RadioCatalog::sanitized` (which drops it) instead of failing the whole save.
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// The stream URL mpv plays (MP3/AAC/HLS — anything mpv decodes).
    #[serde(default)]
    pub url: String,
    /// Logo image URL for the now-playing screen. Empty ⇒ the display's music-note
    /// placeholder.
    #[serde(default)]
    pub logo_url: String,
    /// Extra spoken names ("Dave", "Dave Rocks", "one oh seven five").
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// The editable station list, persisted inside `anamanti_settings.json` (`radio`).
/// Pure runtime/GUI state with no `anamanti.json` seed (like the personality catalog).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RadioCatalog {
    #[serde(default)]
    pub stations: Vec<RadioStation>,
}

/// Lowercase alphanumerics only, so "107.5 Dave-Rocks" and "1075 dave rocks" compare
/// equal.
fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

impl RadioStation {
    /// Every name this station answers to, normalized ([`norm`]).
    fn names(&self) -> impl Iterator<Item = String> + '_ {
        [&self.id, &self.name]
            .into_iter()
            .chain(self.aliases.iter())
            .map(|s| norm(s))
            .filter(|s| !s.is_empty())
    }
}

impl RadioCatalog {
    /// Clean a GUI-submitted catalog: trim fields, uppercase ids, drop entries without
    /// an id or an `http(s)` URL, default a blank name to the id, drop blank/duplicate
    /// aliases, and de-duplicate ids (case-insensitive, first wins).
    pub fn sanitized(&self) -> Self {
        let mut seen = std::collections::HashSet::new();
        let stations = self
            .stations
            .iter()
            .map(|s| {
                let id = s.id.trim().to_uppercase();
                let name = match s.name.trim() {
                    "" => id.clone(),
                    n => n.to_string(),
                };
                let mut alias_seen = std::collections::HashSet::new();
                let aliases = s
                    .aliases
                    .iter()
                    .map(|a| a.trim().to_string())
                    .filter(|a| !a.is_empty() && alias_seen.insert(a.to_lowercase()))
                    .collect();
                RadioStation {
                    id,
                    name,
                    url: s.url.trim().to_string(),
                    logo_url: s.logo_url.trim().to_string(),
                    aliases,
                }
            })
            .filter(|s| !s.id.is_empty() && is_http_url(&s.url))
            .filter(|s| seen.insert(s.id.clone()))
            .collect();
        Self { stations }
    }
}

/// Whether `url` is an absolute `http`/`https` URL (what mpv should be handed).
fn is_http_url(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https"))
}

/// Resolve a spoken/typed station name against `stations`: an exact id/name/alias match
/// (normalized), else a unique substring match either way round. `Err` (speakable)
/// lists the options when nothing or several match.
pub fn resolve<'a>(stations: &'a [RadioStation], query: &str) -> Result<&'a RadioStation, String> {
    let q = norm(query);
    let options = || {
        stations
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    if stations.is_empty() {
        return Err("no radio stations are set up yet".to_string());
    }
    if q.is_empty() {
        return Err(format!("which station? The options are: {}", options()));
    }
    if let Some(s) = stations.iter().find(|s| s.names().any(|n| n == q)) {
        return Ok(s);
    }
    let partial: Vec<&RadioStation> = stations
        .iter()
        .filter(|s| s.names().any(|n| n.contains(&q) || q.contains(&n)))
        .collect();
    match partial.as_slice() {
        [one] => Ok(one),
        [] => Err(format!(
            "I don't have a station called {query}. The options are: {}",
            options()
        )),
        many => Err(format!(
            "{query} could be {}",
            many.iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(" or ")
        )),
    }
}

/// Split a raw ICY `StreamTitle` into `(artist, title)`. Broadcasters pad it with junk
/// (`". - Big Sugar - Better Get Used To It ~ "`), so leading/trailing punctuation
/// fragments are dropped before splitting on the first `" - "`. Returns `None` when
/// nothing meaningful remains; an un-splittable string is all title.
pub fn parse_icy_title(raw: &str) -> Option<(String, String)> {
    let meaningful = |s: &str| s.chars().any(char::is_alphanumeric);
    let parts: Vec<&str> = raw
        .split(" - ")
        .map(|p| p.trim().trim_matches(|c: char| c == '~' || c == '|').trim())
        .collect();
    // Drop leading/trailing fragments with no letters/digits ("." / "~" / "").
    let start = parts.iter().position(|p| meaningful(p))?;
    let end = parts.iter().rposition(|p| meaningful(p))?;
    let parts = &parts[start..=end];
    match parts {
        [title] => Some((String::new(), title.to_string())),
        [artist, rest @ ..] => Some((artist.to_string(), rest.join(" - "))),
        [] => None,
    }
}

// ---------------------------------------------------------------------------
// Output seam (mpv + snapserver)
// ---------------------------------------------------------------------------

/// What the player needs from the outside world. The real implementation is
/// [`SnapMpvOutput`]; tests use a recording fake.
#[async_trait]
pub trait RadioOutput: Send + Sync {
    /// Load `url` into the player and start it.
    async fn load(&self, url: &str) -> Result<()>;
    /// Stop the player (silence on the `Web` stream).
    async fn stop(&self) -> Result<()>;
    /// Whether the player is idle (nothing loaded — e.g. the stream dropped).
    async fn is_idle(&self) -> Result<bool>;
    /// The raw ICY `icy-title` of the current stream, if any.
    async fn icy_title(&self) -> Result<Option<String>>;
    /// Player volume, 0–100.
    async fn volume(&self) -> Result<u8>;
    async fn set_volume(&self, percent: u8) -> Result<()>;
    /// Point every snapserver group at `stream_id`.
    async fn route(&self, stream_id: &str) -> Result<()>;
}

/// The production [`RadioOutput`]: mpv JSON IPC + snapserver JSON-RPC.
pub struct SnapMpvOutput {
    pub mpv: MpvControl,
    pub snapcast: SnapcastClient,
}

#[async_trait]
impl RadioOutput for SnapMpvOutput {
    async fn load(&self, url: &str) -> Result<()> {
        self.mpv.load(url).await
    }
    async fn stop(&self) -> Result<()> {
        self.mpv.stop().await
    }
    async fn is_idle(&self) -> Result<bool> {
        Ok(self.mpv.get_property("idle-active").await?.as_bool() == Some(true))
    }
    async fn icy_title(&self) -> Result<Option<String>> {
        // `metadata` is unavailable (an error) while idle/loading — treat as none.
        let Ok(meta) = self.mpv.get_property("metadata").await else {
            return Ok(None);
        };
        Ok(meta
            .get("icy-title")
            .and_then(Value::as_str)
            .map(str::to_string))
    }
    async fn volume(&self) -> Result<u8> {
        let v = self.mpv.get_property("volume").await?;
        Ok(v.as_f64().unwrap_or(100.0).round().clamp(0.0, 100.0) as u8)
    }
    async fn set_volume(&self, percent: u8) -> Result<()> {
        self.mpv
            .set_property("volume", json!(percent.min(100)))
            .await
    }
    async fn route(&self, stream_id: &str) -> Result<()> {
        let status = self.snapcast.get_status().await?;
        for g in status.groups.iter().filter(|g| g.stream_id != stream_id) {
            self.snapcast
                .call(
                    "Group.SetStream",
                    Some(json!({ "id": g.id, "stream_id": stream_id })),
                )
                .await
                .with_context(|| format!("pointing group {} at {stream_id}", g.id))?;
            log::info!("radio: group {} → stream {stream_id}", g.id);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Player
// ---------------------------------------------------------------------------

/// The live radio player: the catalog cell (updated by `SharedSettings::apply_radio`),
/// the current station, and the operations the tool, the web UI and the display's
/// on-screen controls share. One instance per Core, built at boot when music routing +
/// the mpv IPC are configured. Cheap to share behind an `Arc`.
pub struct RadioPlayer {
    output: Arc<dyn RadioOutput>,
    catalog: RwLock<Vec<RadioStation>>,
    current: Mutex<Option<RadioStation>>,
    /// Serializes play/stop/step so two callers can't interleave mpv + routing calls.
    op: tokio::sync::Mutex<()>,
    /// Last automatic reload of a dropped stream (rate-limits reconnects).
    last_reconnect: Mutex<Option<Instant>>,
    /// Signalled on every station change so the now-playing push updates at once.
    changed: Arc<Notify>,
}

impl RadioPlayer {
    pub fn new(output: Arc<dyn RadioOutput>) -> Self {
        Self {
            output,
            catalog: RwLock::new(Vec::new()),
            current: Mutex::new(None),
            op: tokio::sync::Mutex::new(()),
            last_reconnect: Mutex::new(None),
            changed: Arc::new(Notify::new()),
        }
    }

    /// The production player over mpv IPC + snapserver.
    pub fn over(mpv: MpvControl, snapcast: SnapcastClient) -> Self {
        Self::new(Arc::new(SnapMpvOutput { mpv, snapcast }))
    }

    /// Replace the live catalog (boot seed + every config-page save). If the playing
    /// station was edited its details refresh; if it was removed it keeps playing until
    /// stopped.
    pub fn set_catalog(&self, stations: Vec<RadioStation>) {
        if let Some(cur) = self.current.lock().unwrap().as_mut() {
            if let Some(updated) = stations.iter().find(|s| s.id == cur.id) {
                *cur = updated.clone();
            }
        }
        *self.catalog.write().unwrap() = stations;
        self.changed.notify_one();
    }

    pub fn stations(&self) -> Vec<RadioStation> {
        self.catalog.read().unwrap().clone()
    }

    pub fn has_stations(&self) -> bool {
        !self.catalog.read().unwrap().is_empty()
    }

    /// The station currently playing, if any.
    pub fn current(&self) -> Option<RadioStation> {
        self.current.lock().unwrap().clone()
    }

    pub fn is_playing(&self) -> bool {
        self.current.lock().unwrap().is_some()
    }

    /// Notified on every station change (play/stop/step/catalog edit).
    pub fn changed(&self) -> Arc<Notify> {
        self.changed.clone()
    }

    fn set_current(&self, station: Option<RadioStation>) {
        *self.current.lock().unwrap() = station;
        self.changed.notify_one();
    }

    /// Resolve `query` against the catalog and play it.
    pub async fn play_named(
        &self,
        query: &str,
        spotify: Option<&dyn SpotifyController>,
    ) -> Result<String> {
        let station = {
            let catalog = self.catalog.read().unwrap();
            resolve(&catalog, query)
                .map_err(anyhow::Error::msg)?
                .clone()
        };
        self.play(station, spotify).await
    }

    /// Play `station` by its exact id (web UI).
    pub async fn play_id(
        &self,
        id: &str,
        spotify: Option<&dyn SpotifyController>,
    ) -> Result<String> {
        let station = self
            .stations()
            .into_iter()
            .find(|s| s.id.eq_ignore_ascii_case(id.trim()))
            .with_context(|| format!("no station with id {id}"))?;
        self.play(station, spotify).await
    }

    /// Start `station`: pause Spotify (best-effort, so it isn't playing silently into
    /// its fifo), load the stream into mpv, and point the speakers at the `Web` stream.
    /// A routing failure is logged, not fatal — the stream is loaded either way.
    ///
    /// Spotify is only paused when the radio is *off* (switching stations leaves it
    /// paused already), and before taking the op lock: `spotify` may be a
    /// [`RoutedSpotify`], whose `Pause` would route back into [`Self::stop`].
    pub async fn play(
        &self,
        station: RadioStation,
        spotify: Option<&dyn SpotifyController>,
    ) -> Result<String> {
        if !self.is_playing() {
            if let Some(sp) = spotify {
                if let Err(e) = sp.command(SpotifyCommand::Pause).await {
                    log::debug!("radio: pausing Spotify before {}: {e:#}", station.id);
                }
            }
        }
        let _op = self.op.lock().await;
        self.output
            .load(&station.url)
            .await
            .context("couldn't start the radio player (is mpv running?)")?;
        if let Err(e) = self.output.route(WEB_STREAM).await {
            log::warn!("radio: could not switch the speakers to the {WEB_STREAM} stream: {e:#}");
        }
        log::info!("radio: playing {} ({})", station.id, station.url);
        let reply = format!("Playing {}.", station.name);
        self.set_current(Some(station));
        Ok(reply)
    }

    /// Step `delta` stations through the catalog from the current one (wrapping) and
    /// play it. Spotify is already paused while radio plays, so it isn't touched.
    pub async fn step(&self, delta: isize) -> Result<String> {
        let stations = self.stations();
        anyhow::ensure!(!stations.is_empty(), "no radio stations are set up yet");
        let cur = self.current().map(|c| c.id);
        let idx = cur
            .and_then(|id| stations.iter().position(|s| s.id == id))
            .map(|i| (i as isize + delta).rem_euclid(stations.len() as isize) as usize)
            .unwrap_or(0);
        self.play(stations[idx].clone(), None).await
    }

    /// Stop the radio and point the speakers back at the Spotify stream. Best-effort:
    /// the station is cleared and the speakers re-routed **even if mpv fails** (a hung or
    /// dead player must not leave the house stuck on `Web` with "playing" state). An mpv
    /// failure is still reported as the `Err`, after that cleanup.
    pub async fn stop(&self) -> Result<String> {
        let _op = self.op.lock().await;
        let was = self.current().map(|s| s.name);
        let stopped = self.output.stop().await;
        self.set_current(None);
        if let Err(e) = self.output.route(SPOTIFY_STREAM).await {
            log::warn!("radio: could not switch the speakers back to {SPOTIFY_STREAM}: {e:#}");
        }
        if let Err(e) = stopped {
            log::warn!("radio: mpv stop failed (state cleared, speakers re-routed): {e:#}");
            return Err(e.context("the radio player didn't respond to stop"));
        }
        Ok(match was {
            Some(name) => format!("Stopped {name}."),
            None => "The radio wasn't playing.".to_string(),
        })
    }

    /// Hand the speakers to Spotify: stop the radio if it is on, and (always) make sure
    /// every group is on the Spotify stream — a previous run may have left them on
    /// `Web`. Best-effort; failures are logged.
    pub async fn release_for_spotify(&self) {
        let _op = self.op.lock().await;
        if self.is_playing() {
            if let Err(e) = self.output.stop().await {
                log::warn!("radio: stopping for Spotify: {e:#}");
            }
            self.set_current(None);
            log::info!("radio: stopped — Spotify is taking over");
        }
        if let Err(e) = self.output.route(SPOTIFY_STREAM).await {
            log::debug!("radio: routing back to {SPOTIFY_STREAM}: {e:#}");
        }
    }

    /// Boot-time reset: if no station is on, make sure every group is on the Spotify
    /// stream (a previous run may have left them on `Web`). Never stops a station that
    /// was started meanwhile. Best-effort.
    pub async fn reset_routing(&self) {
        let _op = self.op.lock().await;
        if self.is_playing() {
            return;
        }
        if let Err(e) = self.output.route(SPOTIFY_STREAM).await {
            log::debug!("radio: boot routing reset to {SPOTIFY_STREAM}: {e:#}");
        }
    }

    /// Apply a transport command **while the radio is playing**: pause/stop → stop,
    /// resume → no-op, next/previous → step stations, volume → player volume. `None`
    /// when the radio isn't playing or the command isn't a transport one (play/queue
    /// a Spotify search), so the caller falls through to Spotify.
    pub async fn transport(&self, cmd: &SpotifyCommand) -> Option<Result<String>> {
        if !self.is_playing() {
            return None;
        }
        Some(match cmd {
            SpotifyCommand::Pause => self.stop().await,
            SpotifyCommand::Resume => Ok("The radio is already playing.".to_string()),
            SpotifyCommand::Next => self.step(1).await,
            SpotifyCommand::Previous => self.step(-1).await,
            // Live radio has no up-next queue (the display hides it); never fall through
            // to skipping a paused Spotify queue underneath the radio.
            SpotifyCommand::SkipTo { .. } => {
                Err(anyhow::anyhow!("the radio doesn't have an up-next queue"))
            }
            SpotifyCommand::SetVolume { percent } => self
                .output
                .set_volume(*percent)
                .await
                .map(|()| format!("Radio volume set to {percent} percent.")),
            SpotifyCommand::Play { .. } | SpotifyCommand::Queue { .. } => return None,
        })
    }

    /// The now-playing snapshot for the display, or `None` when the radio is off. The
    /// song comes from the stream's ICY title; with no song the station name is the
    /// title (`track_title` is never empty, which older displays require). If mpv went
    /// idle (the stream dropped) it is reloaded, at most once per [`RECONNECT_BACKOFF`].
    pub async fn now_playing(&self) -> Option<NowPlaying> {
        let station = self.current()?;
        if self.output.is_idle().await.unwrap_or(false) {
            self.reconnect(&station).await;
        }
        let song = match self.output.icy_title().await {
            Ok(Some(raw)) => parse_icy_title(&raw),
            _ => None,
        };
        let (artist, title) = song.unwrap_or_default();
        let has_song = !title.is_empty();
        Some(NowPlaying {
            playing: true,
            track_title: if has_song {
                title
            } else {
                station.name.clone()
            },
            artist,
            album: if has_song {
                station.name.clone()
            } else {
                String::new()
            },
            artwork_uri: station.logo_url.clone(),
            volume_percent: self.output.volume().await.unwrap_or(0),
            source: SOURCE_RADIO.to_string(),
            station_id: station.id.clone(),
            station_name: station.name.clone(),
            ..Default::default()
        })
    }

    async fn reconnect(&self, station: &RadioStation) {
        {
            let mut last = self.last_reconnect.lock().unwrap();
            if last.is_some_and(|t| t.elapsed() < RECONNECT_BACKOFF) {
                return;
            }
            *last = Some(Instant::now());
        }
        log::warn!("radio: {} stream dropped; reconnecting", station.id);
        if let Err(e) = self.output.load(&station.url).await {
            log::warn!("radio: reconnecting {}: {e:#}", station.id);
        }
    }
}

// ---------------------------------------------------------------------------
// Spotify hand-off
// ---------------------------------------------------------------------------

/// A [`SpotifyController`] that keeps radio and Spotify from talking over each other:
/// while the radio plays, transport (pause/next/previous/volume) drives the radio — so
/// "turn it down" or the display's controls act on what is actually audible — and any
/// command that starts Spotify playback stops the radio and routes the speakers back to
/// the Spotify stream first.
pub struct RoutedSpotify {
    inner: Arc<dyn SpotifyController>,
    radio: Arc<RadioPlayer>,
}

impl RoutedSpotify {
    pub fn wrap(
        inner: Arc<dyn SpotifyController>,
        radio: Arc<RadioPlayer>,
    ) -> Arc<dyn SpotifyController> {
        Arc::new(Self { inner, radio })
    }
}

#[async_trait]
impl SpotifyController for RoutedSpotify {
    async fn command(&self, cmd: SpotifyCommand) -> Result<String> {
        if let Some(radio_result) = self.radio.transport(&cmd).await {
            return radio_result;
        }
        if matches!(
            cmd,
            SpotifyCommand::Play { .. }
                | SpotifyCommand::Resume
                | SpotifyCommand::Next
                | SpotifyCommand::Previous
                | SpotifyCommand::SkipTo { .. }
        ) {
            self.radio.release_for_spotify().await;
        }
        self.inner.command(cmd).await
    }

    async fn now_playing(&self) -> Result<Option<NowPlaying>> {
        self.inner.now_playing().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::music::SearchKind;

    /// Records every output call as a short string.
    #[derive(Default)]
    struct FakeOutput {
        calls: Mutex<Vec<String>>,
        icy: Mutex<Option<String>>,
        idle: Mutex<bool>,
        /// Make `stop` fail (a hung/dead mpv).
        stop_fails: Mutex<bool>,
    }

    impl FakeOutput {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn log(&self, s: String) {
            self.calls.lock().unwrap().push(s);
        }
    }

    #[async_trait]
    impl RadioOutput for FakeOutput {
        async fn load(&self, url: &str) -> Result<()> {
            *self.idle.lock().unwrap() = false;
            self.log(format!("load {url}"));
            Ok(())
        }
        async fn stop(&self) -> Result<()> {
            self.log("stop".into());
            if *self.stop_fails.lock().unwrap() {
                anyhow::bail!("mpv IPC timed out");
            }
            Ok(())
        }
        async fn is_idle(&self) -> Result<bool> {
            Ok(*self.idle.lock().unwrap())
        }
        async fn icy_title(&self) -> Result<Option<String>> {
            Ok(self.icy.lock().unwrap().clone())
        }
        async fn volume(&self) -> Result<u8> {
            Ok(80)
        }
        async fn set_volume(&self, percent: u8) -> Result<()> {
            self.log(format!("volume {percent}"));
            Ok(())
        }
        async fn route(&self, stream_id: &str) -> Result<()> {
            self.log(format!("route {stream_id}"));
            Ok(())
        }
    }

    /// A Spotify fake that records commands.
    #[derive(Default)]
    struct FakeSpotify(Mutex<Vec<SpotifyCommand>>);

    #[async_trait]
    impl SpotifyController for FakeSpotify {
        async fn command(&self, cmd: SpotifyCommand) -> Result<String> {
            self.0.lock().unwrap().push(cmd);
            Ok("spotify ok".into())
        }
    }

    fn station(id: &str, name: &str, aliases: &[&str]) -> RadioStation {
        RadioStation {
            id: id.into(),
            name: name.into(),
            url: format!("https://example.com/{id}"),
            logo_url: format!("https://example.com/{id}.png"),
            aliases: aliases.iter().map(|a| a.to_string()).collect(),
        }
    }

    fn catalog() -> Vec<RadioStation> {
        vec![
            station("CJDV", "107.5 Dave Rocks", &["Dave", "Dave FM"]),
            station("CHYM", "570 News", &[]),
            station("CKGL", "Kitchener 570", &[]),
        ]
    }

    fn player() -> (Arc<FakeOutput>, RadioPlayer) {
        let out = Arc::new(FakeOutput::default());
        let p = RadioPlayer::new(out.clone());
        p.set_catalog(catalog());
        (out, p)
    }

    #[test]
    fn icy_titles_are_cleaned_and_split() {
        assert_eq!(
            parse_icy_title(". - Big Sugar - Better Get Used To It ~ "),
            Some(("Big Sugar".into(), "Better Get Used To It".into()))
        );
        assert_eq!(
            parse_icy_title("Our Lady Peace - Naveed"),
            Some(("Our Lady Peace".into(), "Naveed".into()))
        );
        // A title containing " - " keeps the remainder joined.
        assert_eq!(
            parse_icy_title("Artist - Song - Live"),
            Some(("Artist".into(), "Song - Live".into()))
        );
        assert_eq!(
            parse_icy_title("Station Jingle"),
            Some((String::new(), "Station Jingle".into()))
        );
        assert_eq!(parse_icy_title(" . - ~ "), None);
        assert_eq!(parse_icy_title(""), None);
    }

    #[test]
    fn sanitize_cleans_and_dedupes() {
        let cat = RadioCatalog {
            stations: vec![
                RadioStation {
                    id: " cjdv ".into(),
                    name: "  ".into(),
                    url: " https://corus.leanstream.co/CJDVFM-MP3 ".into(),
                    logo_url: " ".into(),
                    aliases: vec!["Dave".into(), " dave ".into(), "".into()],
                },
                // Duplicate id (case-insensitive) — dropped.
                station("CJDV", "Other", &[]),
                // No id / non-http URL — dropped.
                RadioStation {
                    url: "https://x".into(),
                    ..Default::default()
                },
                RadioStation {
                    id: "BAD".into(),
                    url: "file:///etc/passwd".into(),
                    ..Default::default()
                },
            ],
        }
        .sanitized();
        assert_eq!(cat.stations.len(), 1);
        let s = &cat.stations[0];
        assert_eq!(s.id, "CJDV");
        assert_eq!(s.name, "CJDV");
        assert_eq!(s.url, "https://corus.leanstream.co/CJDVFM-MP3");
        assert_eq!(s.logo_url, "");
        assert_eq!(s.aliases, vec!["Dave".to_string()]);
    }

    #[test]
    fn resolve_matches_ids_names_aliases_and_partials() {
        let c = catalog();
        assert_eq!(resolve(&c, "cjdv").unwrap().id, "CJDV");
        assert_eq!(resolve(&c, "Dave FM").unwrap().id, "CJDV");
        assert_eq!(resolve(&c, "107.5 dave rocks").unwrap().id, "CJDV");
        assert_eq!(resolve(&c, "dave rocks").unwrap().id, "CJDV"); // partial
        assert_eq!(resolve(&c, "news").unwrap().id, "CHYM");
        // "570" is ambiguous.
        let err = resolve(&c, "570").unwrap_err();
        assert!(
            err.contains("570 News") && err.contains("Kitchener 570"),
            "{err}"
        );
        let err = resolve(&c, "jazz fm").unwrap_err();
        assert!(
            err.starts_with("I don't have a station called jazz fm"),
            "{err}"
        );
        assert!(resolve(&[], "dave").is_err());
    }

    #[tokio::test]
    async fn play_pauses_spotify_loads_and_routes_to_web() {
        let (out, p) = player();
        let spotify = FakeSpotify::default();
        let reply = p.play_named("dave", Some(&spotify)).await.unwrap();
        assert_eq!(reply, "Playing 107.5 Dave Rocks.");
        assert_eq!(*spotify.0.lock().unwrap(), vec![SpotifyCommand::Pause]);
        assert_eq!(
            out.calls(),
            vec!["load https://example.com/CJDV", "route Web"]
        );
        assert_eq!(p.current().unwrap().id, "CJDV");
    }

    #[tokio::test]
    async fn stop_routes_back_to_spotify() {
        let (out, p) = player();
        p.play_id("cjdv", None).await.unwrap();
        assert_eq!(p.stop().await.unwrap(), "Stopped 107.5 Dave Rocks.");
        assert!(!p.is_playing());
        assert_eq!(out.calls()[2..], ["stop", "route Spotify"]);
    }

    #[tokio::test]
    async fn stop_clears_and_reroutes_even_when_mpv_fails() {
        let (out, p) = player();
        p.play_id("cjdv", None).await.unwrap();
        *out.stop_fails.lock().unwrap() = true;
        let err = p.stop().await.unwrap_err();
        assert!(format!("{err:#}").contains("mpv IPC timed out"), "{err:#}");
        // Still cleaned up: no station, speakers back on Spotify.
        assert!(!p.is_playing());
        assert!(p.now_playing().await.is_none());
        assert_eq!(out.calls()[2..], ["stop", "route Spotify"]);
        // The op lock was released: a fresh play goes through.
        *out.stop_fails.lock().unwrap() = false;
        p.play_id("CHYM", None).await.unwrap();
        assert_eq!(p.current().unwrap().id, "CHYM");
    }

    #[tokio::test]
    async fn step_wraps_through_the_catalog() {
        let (_out, p) = player();
        p.play_id("CKGL", None).await.unwrap();
        p.step(1).await.unwrap();
        assert_eq!(p.current().unwrap().id, "CJDV"); // wrapped forward
        p.step(-1).await.unwrap();
        assert_eq!(p.current().unwrap().id, "CKGL"); // wrapped back
    }

    #[tokio::test]
    async fn now_playing_carries_station_and_song() {
        let (out, p) = player();
        assert!(p.now_playing().await.is_none());
        p.play_id("CJDV", None).await.unwrap();

        // No ICY title yet → the station is the title.
        let np = p.now_playing().await.unwrap();
        assert_eq!(np.source, "radio");
        assert_eq!(np.track_title, "107.5 Dave Rocks");
        assert_eq!(np.artist, "");
        assert_eq!(np.station_id, "CJDV");
        assert_eq!(np.artwork_uri, "https://example.com/CJDV.png");
        assert_eq!(np.duration_secs, 0);

        *out.icy.lock().unwrap() = Some(". - Big Sugar - Better Get Used To It ~ ".into());
        let np = p.now_playing().await.unwrap();
        assert_eq!(np.track_title, "Better Get Used To It");
        assert_eq!(np.artist, "Big Sugar");
        assert_eq!(np.album, "107.5 Dave Rocks");
        assert_eq!(np.volume_percent, 80);
        let json = serde_json::to_value(&np).unwrap();
        assert_eq!(json["station_name"], "107.5 Dave Rocks");
    }

    #[tokio::test]
    async fn a_dropped_stream_is_reloaded_with_backoff() {
        let (out, p) = player();
        p.play_id("CJDV", None).await.unwrap();
        *out.idle.lock().unwrap() = true;
        p.now_playing().await.unwrap();
        *out.idle.lock().unwrap() = true;
        p.now_playing().await.unwrap(); // within the backoff: no second reload
        let loads = out.calls().iter().filter(|c| c.starts_with("load")).count();
        assert_eq!(loads, 2); // the initial play + one reconnect
    }

    #[tokio::test]
    async fn routed_spotify_drives_radio_transport_and_hands_off_on_play() {
        let (out, p) = player();
        let radio = Arc::new(p);
        let inner = Arc::new(FakeSpotify::default());
        let routed = RoutedSpotify::wrap(inner.clone(), radio.clone());

        // Radio off: everything goes to Spotify (a resume also re-routes to Spotify).
        assert_eq!(
            routed.command(SpotifyCommand::Resume).await.unwrap(),
            "spotify ok"
        );
        assert_eq!(out.calls(), vec!["route Spotify"]);

        radio.play_id("CJDV", None).await.unwrap();
        // Volume and next act on the radio, not Spotify.
        assert_eq!(
            routed
                .command(SpotifyCommand::SetVolume { percent: 40 })
                .await
                .unwrap(),
            "Radio volume set to 40 percent."
        );
        routed.command(SpotifyCommand::Next).await.unwrap();
        assert_eq!(radio.current().unwrap().id, "CHYM");
        // The radio has no queue: a skip-to is refused, never skipping Spotify underneath.
        assert!(routed
            .command(SpotifyCommand::SkipTo { index: 0 })
            .await
            .is_err());
        assert_eq!(inner.0.lock().unwrap().len(), 1); // still just the Resume

        // Playing a Spotify search stops the radio and routes back first.
        routed
            .command(SpotifyCommand::Play {
                query: Some("radiohead".into()),
                kind: SearchKind::Artist,
            })
            .await
            .unwrap();
        assert!(!radio.is_playing());
        let calls = out.calls();
        assert_eq!(calls[calls.len() - 2..], ["stop", "route Spotify"]);
        assert_eq!(inner.0.lock().unwrap().len(), 2);

        // Switching stations through the routed controller must not deadlock (its
        // Pause would route into stop) and must not pause Spotify again.
        radio.play_id("CJDV", Some(routed.as_ref())).await.unwrap();
        radio.play_id("CHYM", Some(routed.as_ref())).await.unwrap();
        assert_eq!(radio.current().unwrap().id, "CHYM");

        // Pause while radio plays stops the radio.
        assert_eq!(
            routed.command(SpotifyCommand::Pause).await.unwrap(),
            "Stopped 570 News."
        );
    }

    #[tokio::test]
    async fn boot_reset_routes_only_when_idle() {
        let (out, p) = player();
        p.reset_routing().await;
        assert_eq!(out.calls(), vec!["route Spotify"]);
        p.play_id("CJDV", None).await.unwrap();
        p.reset_routing().await; // a station started meanwhile: left alone
        assert!(p.is_playing());
        assert_eq!(out.calls().last().unwrap(), "route Web");
    }

    #[tokio::test]
    async fn editing_the_playing_station_refreshes_it() {
        let (_out, p) = player();
        p.play_id("CJDV", None).await.unwrap();
        let mut edited = catalog();
        edited[0].name = "Dave Rocks 107.5".into();
        p.set_catalog(edited);
        assert_eq!(p.current().unwrap().name, "Dave Rocks 107.5");
    }
}
