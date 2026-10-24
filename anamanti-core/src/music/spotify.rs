//! Spotify **Web API** control client — the voice control plane for house-wide
//! music (`plans/MusicPlan.md`).
//!
//! This is control-only, exactly like the rest of `crate::music`: it never
//! touches audio PCM. The music itself is produced by the **librespot** sibling
//! process (the Spotify Connect device named by `music.spotify_device_name`,
//! default `"Ambient"`) whose PCM Snapcast routes to the speakers. Here we only
//! issue Web API calls — search, start/resume, pause, skip, queue, volume —
//! **targeting that librespot device** so a spoken "play some Radiohead" turns
//! into music on the house speakers.
//!
//! Requires **Spotify Premium**: the Web API's playback-control endpoints
//! (`/me/player/*`) are Premium-only. Credentials come from the JSON config file's
//! `spotify` block (or the config-page consent flow), held in
//! [`crate::settings::SpotifyConfig`]; the tool is simply not advertised when they
//! are absent.
//!
//! The [`SpotifyController`] trait is the seam the `spotify_control` rig tool
//! (`crate::llm::rig`) depends on, so the tool is unit-testable without hitting
//! the network — mirroring how `InternetSearch` holds a `SearchProvider`.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;

/// What kind of Spotify entity a `play`/`queue` search should resolve to. The
/// model picks this: a specific song is a `Track`; "some Radiohead" / a vibe is
/// an `Artist` or `Playlist` (which plays a whole context, not one song).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchKind {
    Track,
    Artist,
    Album,
    Playlist,
}

impl SearchKind {
    /// The Spotify `type` query value.
    fn as_query(self) -> &'static str {
        match self {
            SearchKind::Track => "track",
            SearchKind::Artist => "artist",
            SearchKind::Album => "album",
            SearchKind::Playlist => "playlist",
        }
    }

    /// The results container key in a `/v1/search` response.
    fn results_key(self) -> &'static str {
        match self {
            SearchKind::Track => "tracks",
            SearchKind::Artist => "artists",
            SearchKind::Album => "albums",
            SearchKind::Playlist => "playlists",
        }
    }

    /// A track plays as a `uris` list; everything else plays as a `context_uri`.
    fn is_context(self) -> bool {
        !matches!(self, SearchKind::Track)
    }

    /// Parse the tool's `kind` argument; defaults to [`SearchKind::Track`].
    pub fn from_arg(arg: Option<&str>) -> Self {
        match arg.map(str::to_lowercase).as_deref() {
            Some("artist") => SearchKind::Artist,
            Some("album") => SearchKind::Album,
            Some("playlist") => SearchKind::Playlist,
            _ => SearchKind::Track,
        }
    }
}

/// A single control operation the voice tool can request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpotifyCommand {
    /// Start playback. With a `query`, search for it and play; without one,
    /// resume whatever is loaded.
    Play {
        query: Option<String>,
        kind: SearchKind,
    },
    Pause,
    Resume,
    Next,
    Previous,
    /// Jump to the up-next track at `index` (0-based into [`NowPlaying::next_up`]) — the
    /// display's tappable Up Next list. The Web API has no "play queue item N", so when
    /// an album/playlist is playing this is **one** call that re-plays that context at
    /// the track (`offset.uri`), keeping the rest of the context after it; otherwise (an
    /// artist/no context, or the track isn't in the context — e.g. user-queued) it falls
    /// back to skipping forward `index + 1` times. Never a by-name search: playing a bare
    /// track URI would drop the rest of the context.
    SkipTo {
        index: usize,
    },
    /// Search for a track and add it to the up-next queue.
    Queue {
        query: String,
    },
    /// Set the Connect device volume (0–100).
    SetVolume {
        percent: u8,
    },
}

impl SpotifyCommand {
    /// Parse a bare **transport** action — `pause`/`stop`, `resume`/`unpause`,
    /// `next`/`skip`/`forward`, `previous`/`prev`/`back`, `volume`/`set_volume` (which
    /// needs `value` as the percent, clamped to 0–100), `skip_to` (which needs `value` as
    /// the 0-based up-next index). Case- and whitespace-insensitive.
    /// `Ok(None)` when `action` is not a transport action (e.g. `play`/`queue`, which
    /// need a search query); `Err` (speakable) for `volume` with no level or `skip_to`
    /// with no (or a negative) index. Shared by the
    /// `spotify_control` tool and the display's on-screen controls so both accept the
    /// same vocabulary.
    pub fn transport(action: &str, value: Option<i64>) -> Result<Option<Self>> {
        Ok(Some(match action.trim().to_lowercase().as_str() {
            "pause" | "stop" => SpotifyCommand::Pause,
            "resume" | "unpause" => SpotifyCommand::Resume,
            "next" | "skip" | "forward" => SpotifyCommand::Next,
            "previous" | "prev" | "back" => SpotifyCommand::Previous,
            "volume" | "set_volume" => SpotifyCommand::SetVolume {
                percent: value
                    .context("volume needs a level from 0 to 100")?
                    .clamp(0, 100) as u8,
            },
            "skip_to" => SpotifyCommand::SkipTo {
                index: value
                    .and_then(|i| usize::try_from(i).ok())
                    .context("skip_to needs the position of an up-next track")?,
            },
            _ => return Ok(None),
        }))
    }
}

/// One queued track ("up next"), as the display's Next Up list renders it. Field
/// names are the snake_case JSON keys the device's `QueueTrack.tryParse` expects.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct QueueItem {
    pub track_title: String,
    pub artist: String,
    pub album: String,
    pub artwork_uri: String,
    /// The track's Spotify URI (`spotify:track:…`), used Core-side to jump to it. Not
    /// sent to the display (it addresses rows by index).
    #[serde(skip)]
    pub uri: String,
}

/// A snapshot of the current playback, pushed to the display's now-playing screen.
/// Field names are the snake_case JSON keys the device's `MusicData.tryParse` expects.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct NowPlaying {
    /// Whether playback is currently running (vs. paused on a loaded track).
    pub playing: bool,
    pub track_title: String,
    pub artist: String,
    pub album: String,
    /// URL of the album artwork (largest image Spotify offers), or empty.
    pub artwork_uri: String,
    /// Playback position within the current track, in whole seconds.
    pub position_secs: u64,
    /// The current track's total length, in whole seconds.
    pub duration_secs: u64,
    /// The target Connect device's volume (0–100), or 0 when unknown.
    pub volume_percent: u8,
    /// The up-next queue (capped), oldest-first.
    pub next_up: Vec<QueueItem>,
    /// Which source this snapshot describes: [`SOURCE_SPOTIFY`] or
    /// [`super::radio::SOURCE_RADIO`]. The display hides the seek bar / queue for radio.
    pub source: String,
    /// Radio only: the station id (call sign, e.g. `CJDV`). Omitted for Spotify.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub station_id: String,
    /// Radio only: the station's display name. Omitted for Spotify.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub station_name: String,
}

/// [`NowPlaying::source`] for a Spotify snapshot.
pub const SOURCE_SPOTIFY: &str = "spotify";

/// The control seam the `spotify_control` rig tool depends on. Returns a short,
/// speakable confirmation the model relays to the user.
#[async_trait]
pub trait SpotifyController: Send + Sync {
    async fn command(&self, cmd: SpotifyCommand) -> Result<String>;

    /// Read the current playback snapshot (now-playing track + up-next queue) for the
    /// display's music screen. `Ok(None)` means nothing is loaded/playing. The default
    /// returns `Ok(None)` so test fakes that only exercise `command` need not implement
    /// it.
    async fn now_playing(&self) -> Result<Option<NowPlaying>> {
        Ok(None)
    }
}

/// A resolved search hit: the URI to play and a speakable label.
#[derive(Debug, Clone)]
struct SearchHit {
    uri: String,
    label: String,
    is_context: bool,
}

/// Per-request budget for every Spotify Web API / accounts call, so a stalled Spotify
/// can't hang a voice turn.
const SPOTIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// `User-Agent` component label for Spotify requests.
const COMPONENT: &str = "spotify";

/// Live Spotify Web API implementation of [`SpotifyController`].
///
/// Holds long-lived credentials (client id/secret + refresh token) and caches a
/// short-lived access token, refreshing it on demand. Every playback call targets
/// the librespot Connect device resolved by name. Requests ride the shared
/// [`crate::http::shared_client`] pool with a per-request timeout.
pub struct SpotifyWebApi {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    device_name: String,
    /// `https://accounts.spotify.com` (overridable for tests).
    accounts_base: String,
    /// `https://api.spotify.com` (overridable for tests).
    api_base: String,
    /// Cached (access_token, expires_at).
    token: Mutex<Option<(String, Instant)>>,
    /// Cached (Connect device id, resolved_at) for [`Self::device_name`], so a burst of
    /// commands (a volume drag) doesn't spend a `GET /devices` cloud round trip on every
    /// one. librespot's id is stable for a given name; a command that fails against a
    /// cached id drops it and retries once against a fresh lookup.
    device: Mutex<Option<(String, Instant)>>,
}

/// How long a resolved Connect device id is reused before it is looked up again.
const DEVICE_ID_TTL: Duration = Duration::from_secs(300);

impl SpotifyWebApi {
    pub fn new(
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        refresh_token: impl Into<String>,
        device_name: impl Into<String>,
    ) -> Self {
        Self::with_bases(
            client_id,
            client_secret,
            refresh_token,
            device_name,
            "https://accounts.spotify.com",
            "https://api.spotify.com",
        )
    }

    /// Construct with explicit endpoint bases (tests point these at a fake server).
    pub fn with_bases(
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        refresh_token: impl Into<String>,
        device_name: impl Into<String>,
        accounts_base: impl Into<String>,
        api_base: impl Into<String>,
    ) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            refresh_token: refresh_token.into(),
            device_name: device_name.into(),
            accounts_base: accounts_base.into().trim_end_matches('/').to_string(),
            api_base: api_base.into().trim_end_matches('/').to_string(),
            token: Mutex::new(None),
            device: Mutex::new(None),
        }
    }

    /// The target device id from the cache when fresh (`true` = it came from the
    /// cache), else from a live lookup that refreshes the cache.
    async fn cached_device_id(&self, token: &str) -> Result<(String, bool)> {
        if let Some((id, at)) = self.device.lock().await.as_ref() {
            if at.elapsed() < DEVICE_ID_TTL {
                return Ok((id.clone(), true));
            }
        }
        let id = self.device_id(token).await?;
        *self.device.lock().await = Some((id.clone(), Instant::now()));
        Ok((id, false))
    }

    /// A valid bearer token, refreshing (and caching) when the cache is empty or
    /// within 30s of expiry.
    async fn access_token(&self) -> Result<String> {
        let mut guard = self.token.lock().await;
        if let Some((tok, exp)) = guard.as_ref() {
            if *exp > Instant::now() + Duration::from_secs(30) {
                return Ok(tok.clone());
            }
        }
        let resp: Value = crate::http::send_json(
            crate::http::post(format!("{}/api/token", self.accounts_base), COMPONENT)
                .basic_auth(&self.client_id, Some(&self.client_secret))
                .form(&[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", self.refresh_token.as_str()),
                ]),
            SPOTIFY_TIMEOUT,
            "Spotify token endpoint (check client id/secret/refresh token)",
        )
        .await?;

        let access = resp
            .get("access_token")
            .and_then(Value::as_str)
            .context("Spotify token response had no access_token")?
            .to_string();
        let ttl = resp
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(3600);
        *guard = Some((access.clone(), Instant::now() + Duration::from_secs(ttl)));
        Ok(access)
    }

    /// Resolve the target Connect device's id by name (case-insensitive). Errors
    /// with a speakable message when the device is not present (e.g. librespot is
    /// down or the speaker is asleep).
    async fn device_id(&self, token: &str) -> Result<String> {
        let resp: Value = crate::http::send_json(
            crate::http::get(format!("{}/v1/me/player/devices", self.api_base), COMPONENT)
                .bearer_auth(token),
            SPOTIFY_TIMEOUT,
            "Spotify devices",
        )
        .await?;

        let devices = resp
            .get("devices")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let want = self.device_name.to_lowercase();
        devices
            .iter()
            .find(|d| {
                d.get("name")
                    .and_then(Value::as_str)
                    .map(|n| n.to_lowercase() == want)
                    .unwrap_or(false)
            })
            .and_then(|d| d.get("id").and_then(Value::as_str))
            .map(str::to_string)
            .with_context(|| {
                format!(
                    "the \"{}\" speaker isn't available right now — is it powered on?",
                    self.device_name
                )
            })
    }

    /// Search for the top matching entity of `kind`.
    async fn search(&self, token: &str, query: &str, kind: SearchKind) -> Result<SearchHit> {
        let resp: Value = crate::http::send_json(
            crate::http::get(format!("{}/v1/search", self.api_base), COMPONENT)
                .bearer_auth(token)
                .query(&[("q", query), ("type", kind.as_query()), ("limit", "1")]),
            SPOTIFY_TIMEOUT,
            "Spotify search",
        )
        .await?;

        let item = resp
            .get(kind.results_key())
            .and_then(|c| c.get("items"))
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .with_context(|| format!("I couldn't find \"{query}\" on Spotify."))?;

        let uri = item
            .get("uri")
            .and_then(Value::as_str)
            .context("Spotify search hit had no uri")?
            .to_string();
        Ok(SearchHit {
            uri,
            label: describe_hit(item, kind),
            is_context: kind.is_context(),
        })
    }

    /// PUT/POST a player-control endpoint that returns no body (204/202). `path`
    /// is appended to `/v1/me/player`. Adds `device_id` to every call.
    async fn player_call(
        &self,
        token: &str,
        method: reqwest::Method,
        path: &str,
        device_id: &str,
        extra_query: &[(&str, &str)],
        body: Option<Value>,
    ) -> Result<()> {
        let mut query: Vec<(&str, &str)> = vec![("device_id", device_id)];
        query.extend_from_slice(extra_query);
        let mut req = crate::http::request(
            method,
            format!("{}/v1/me/player{path}", self.api_base),
            COMPONENT,
        )
        .bearer_auth(token)
        .query(&query);
        if let Some(b) = body {
            req = req.json(&b);
        } else {
            // Spotify's play/pause endpoints want a JSON content-type even empty.
            req = req.header(reqwest::header::CONTENT_LENGTH, "0");
        }
        crate::http::send_ok(
            req,
            SPOTIFY_TIMEOUT,
            &format!("Spotify player endpoint {path}"),
        )
        .await?;
        Ok(())
    }

    /// GET a player endpoint, returning the parsed body — or `Ok(None)` for a `204 No
    /// Content` (nothing playing) or an empty body. `path` is appended to
    /// `/v1/me/player`. These reads are account-wide, so (unlike [`player_call`]) they
    /// take no `device_id`.
    async fn player_get(&self, token: &str, path: &str) -> Result<Option<Value>> {
        let what = format!("Spotify player read {path}");
        let body = crate::http::send_text(
            crate::http::get(format!("{}/v1/me/player{path}", self.api_base), COMPONENT)
                .bearer_auth(token),
            SPOTIFY_TIMEOUT,
            &what,
        )
        .await?;
        // A `204 No Content` (nothing playing) arrives as an empty body.
        if body.trim().is_empty() {
            return Ok(None);
        }
        let value: Value =
            serde_json::from_str(&body).with_context(|| format!("parsing the {what} response"))?;
        Ok(Some(value))
    }

    /// Fetch the up-next queue (capped at [`QUEUE_LIMIT`]); best-effort, so a failure
    /// yields an empty list rather than failing the whole now-playing read.
    async fn fetch_queue(&self, token: &str) -> Vec<QueueItem> {
        match self.player_get(token, "/queue").await {
            Ok(Some(v)) => v
                .get("queue")
                .and_then(Value::as_array)
                .map(|items| items.iter().take(QUEUE_LIMIT).map(track_fields).collect())
                .unwrap_or_default(),
            Ok(None) => Vec::new(),
            Err(e) => {
                log::debug!("spotify queue read failed: {e:#}");
                Vec::new()
            }
        }
    }
}

/// Maximum number of up-next tracks surfaced to the display.
const QUEUE_LIMIT: usize = 20;

/// Extract title / artist / album / artwork from a Spotify track object (the shape
/// shared by `/me/player`'s `item` and `/me/player/queue`'s entries).
fn track_fields(item: &Value) -> QueueItem {
    let title = item
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let artist = item
        .get("artists")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|a| a.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let album_obj = item.get("album");
    let album = album_obj
        .and_then(|a| a.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    // Spotify lists images largest-first; take the first url.
    let artwork_uri = album_obj
        .and_then(|a| a.get("images"))
        .and_then(Value::as_array)
        .and_then(|imgs| imgs.first())
        .and_then(|img| img.get("url"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let uri = item
        .get("uri")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    QueueItem {
        track_title: title,
        artist,
        album,
        artwork_uri,
        uri,
    }
}

/// The playing context's URI when Spotify can start it at a given track — an album or
/// playlist (`offset` is not supported for artist contexts). `None` otherwise.
fn offsetable_context(state: &Value) -> Option<String> {
    let ctx = state.get("context")?;
    let kind = ctx.get("type").and_then(Value::as_str)?;
    if !matches!(kind, "album" | "playlist") {
        return None;
    }
    ctx.get("uri")
        .and_then(Value::as_str)
        .filter(|u| !u.is_empty())
        .map(str::to_string)
}

/// Build a speakable label for a search hit ("Karma Police by Radiohead",
/// "Radiohead", "OK Computer by Radiohead", "This Is Radiohead").
fn describe_hit(item: &Value, kind: SearchKind) -> String {
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("that")
        .to_string();
    let artist = item
        .get("artists")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|a| a.get("name"))
        .and_then(Value::as_str);
    match (kind, artist) {
        (SearchKind::Track, Some(a)) | (SearchKind::Album, Some(a)) => format!("{name} by {a}"),
        _ => name,
    }
}

impl SpotifyWebApi {
    /// Run one command against an already-resolved Connect `device`.
    async fn command_on(&self, token: &str, device: &str, cmd: SpotifyCommand) -> Result<String> {
        let token = token.to_string();
        let device = device.to_string();
        match cmd {
            SpotifyCommand::Play { query, kind } => match query {
                Some(q) => {
                    let hit = self.search(&token, &q, kind).await?;
                    let body = if hit.is_context {
                        serde_json::json!({ "context_uri": hit.uri })
                    } else {
                        serde_json::json!({ "uris": [hit.uri] })
                    };
                    self.player_call(
                        &token,
                        reqwest::Method::PUT,
                        "/play",
                        &device,
                        &[],
                        Some(body),
                    )
                    .await?;
                    Ok(format!("Playing {}.", hit.label))
                }
                None => {
                    self.player_call(&token, reqwest::Method::PUT, "/play", &device, &[], None)
                        .await?;
                    Ok("Resuming playback.".to_string())
                }
            },
            SpotifyCommand::Resume => {
                self.player_call(&token, reqwest::Method::PUT, "/play", &device, &[], None)
                    .await?;
                Ok("Resuming playback.".to_string())
            }
            SpotifyCommand::Pause => {
                self.player_call(&token, reqwest::Method::PUT, "/pause", &device, &[], None)
                    .await?;
                Ok("Paused.".to_string())
            }
            SpotifyCommand::Next => {
                self.player_call(&token, reqwest::Method::POST, "/next", &device, &[], None)
                    .await?;
                Ok("Skipping to the next track.".to_string())
            }
            SpotifyCommand::Previous => {
                self.player_call(
                    &token,
                    reqwest::Method::POST,
                    "/previous",
                    &device,
                    &[],
                    None,
                )
                .await?;
                Ok("Going back to the previous track.".to_string())
            }
            SpotifyCommand::SkipTo { index } => {
                // Re-read the queue (so a stale tap — the queue moved since the display's
                // last push — can't over-skip, and to get the track's URI + name) and the
                // playing context, concurrently.
                let (queue, state) =
                    tokio::join!(self.fetch_queue(&token), self.player_get(&token, ""));
                let target = queue
                    .get(index)
                    .context("that track isn't in the up-next queue any more")?;
                let jumped = match state.ok().flatten().as_ref().and_then(offsetable_context) {
                    Some(context_uri) if !target.uri.is_empty() => {
                        let body = serde_json::json!({
                            "context_uri": context_uri,
                            "offset": { "uri": target.uri },
                        });
                        match self
                            .player_call(
                                &token,
                                reqwest::Method::PUT,
                                "/play",
                                &device,
                                &[],
                                Some(body),
                            )
                            .await
                        {
                            Ok(()) => true,
                            // e.g. a user-queued track that isn't in the context.
                            Err(e) => {
                                log::debug!(
                                    "spotify skip_to: offset play failed, skipping instead: {e:#}"
                                );
                                false
                            }
                        }
                    }
                    _ => false,
                };
                if !jumped {
                    // Sequential, in order: each skip must land before the next is issued.
                    for _ in 0..=index {
                        self.player_call(
                            &token,
                            reqwest::Method::POST,
                            "/next",
                            &device,
                            &[],
                            None,
                        )
                        .await?;
                    }
                }
                Ok(if target.artist.is_empty() {
                    format!("Playing {}.", target.track_title)
                } else {
                    format!("Playing {} by {}.", target.track_title, target.artist)
                })
            }
            SpotifyCommand::Queue { query } => {
                let hit = self.search(&token, &query, SearchKind::Track).await?;
                self.player_call(
                    &token,
                    reqwest::Method::POST,
                    "/queue",
                    &device,
                    &[("uri", hit.uri.as_str())],
                    None,
                )
                .await?;
                Ok(format!("Added {} to the queue.", hit.label))
            }
            SpotifyCommand::SetVolume { percent } => {
                let pct = percent.min(100);
                let pct_str = pct.to_string();
                self.player_call(
                    &token,
                    reqwest::Method::PUT,
                    "/volume",
                    &device,
                    &[("volume_percent", pct_str.as_str())],
                    None,
                )
                .await?;
                Ok(format!("Set the volume to {pct} percent."))
            }
        }
    }
}

#[async_trait]
impl SpotifyController for SpotifyWebApi {
    async fn command(&self, cmd: SpotifyCommand) -> Result<String> {
        let token = self.access_token().await?;
        let (device, cached) = self.cached_device_id(&token).await?;
        match self.command_on(&token, &device, cmd.clone()).await {
            // A 404 against a *cached* id = the device went away (librespot restarted
            // under a new id, or is down). Nothing was applied, so drop the cache and
            // retry once against a fresh lookup (which yields the speakable "isn't
            // available" error if it's really gone).
            Err(e) if cached && format!("{e:#}").contains("404") => {
                log::debug!("spotify: cached device id rejected ({e:#}); re-resolving");
                *self.device.lock().await = None;
                let (device, _) = self.cached_device_id(&token).await?;
                self.command_on(&token, &device, cmd).await
            }
            res => res,
        }
    }

    async fn now_playing(&self) -> Result<Option<NowPlaying>> {
        let token = self.access_token().await?;
        let Some(state) = self.player_get(&token, "").await? else {
            return Ok(None); // 204 / empty body → nothing loaded
        };
        let Some(item) = state.get("item").filter(|v| v.is_object()) else {
            return Ok(None); // playing an ad / local file with no track object
        };
        let QueueItem {
            track_title,
            artist,
            album,
            artwork_uri,
            ..
        } = track_fields(item);
        let playing = state
            .get("is_playing")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let position_secs = state
            .get("progress_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            / 1000;
        let duration_secs = item.get("duration_ms").and_then(Value::as_u64).unwrap_or(0) / 1000;
        let volume_percent = state
            .get("device")
            .and_then(|d| d.get("volume_percent"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(100) as u8;
        let next_up = self.fetch_queue(&token).await;
        Ok(Some(NowPlaying {
            playing,
            track_title,
            artist,
            album,
            artwork_uri,
            position_secs,
            duration_secs,
            volume_percent,
            next_up,
            source: SOURCE_SPOTIFY.to_string(),
            ..Default::default()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc as StdArc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex as TokioMutex;

    #[test]
    fn search_kind_parses_and_maps() {
        assert_eq!(SearchKind::from_arg(Some("artist")), SearchKind::Artist);
        assert_eq!(SearchKind::from_arg(Some("PLAYLIST")), SearchKind::Playlist);
        assert_eq!(SearchKind::from_arg(None), SearchKind::Track);
        assert_eq!(SearchKind::from_arg(Some("bogus")), SearchKind::Track);
        assert!(SearchKind::Artist.is_context());
        assert!(!SearchKind::Track.is_context());
    }

    #[test]
    fn describe_hit_reads_naturally() {
        let track = serde_json::json!({
            "name": "Karma Police",
            "artists": [{"name": "Radiohead"}]
        });
        assert_eq!(
            describe_hit(&track, SearchKind::Track),
            "Karma Police by Radiohead"
        );
        let artist = serde_json::json!({ "name": "Radiohead" });
        assert_eq!(describe_hit(&artist, SearchKind::Artist), "Radiohead");
    }

    /// A minimal fake Spotify server: replies to the token, devices, search, and
    /// player endpoints, recording each request line (METHOD + path?query) so a
    /// test can assert what the client sent.
    async fn spawn_fake_spotify() -> (String, StdArc<TokioMutex<Vec<String>>>) {
        spawn_fake_spotify_with(DEFAULT_PLAYER_STATE, false).await
    }

    const DEFAULT_PLAYER_STATE: &str = r#"{"is_playing":true,"progress_ms":73000,"device":{"volume_percent":65},"item":{"name":"Paranoid Android","duration_ms":383000,"artists":[{"name":"Radiohead"}],"album":{"name":"OK Computer","images":[{"url":"http://img/ok-computer.jpg"}]}}}"#;

    /// [`spawn_fake_spotify`] with a custom `GET /v1/me/player` body, and (when
    /// `fail_play`) a `PUT /v1/me/player/play` that answers 404.
    async fn spawn_fake_spotify_with(
        player_state: &'static str,
        fail_play: bool,
    ) -> (String, StdArc<TokioMutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = StdArc::new(TokioMutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let sink = sink.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let first = req.lines().next().unwrap_or_default().to_string();
                    // "METHOD /path?query HTTP/1.1" → "METHOD /path?query"
                    let request_line = first.rsplit_once(' ').map(|(a, _)| a).unwrap_or(&first);
                    sink.lock().await.push(request_line.to_string());

                    let body = if first.starts_with("POST /api/token") {
                        r#"{"access_token":"tok-123","expires_in":3600}"#.to_string()
                    } else if first.starts_with("GET /v1/me/player/devices") {
                        r#"{"devices":[{"id":"dev-1","name":"Ambient"},{"id":"other","name":"Phone"}]}"#.to_string()
                    } else if first.starts_with("GET /v1/me/player/queue") {
                        r#"{"queue":[{"uri":"spotify:track:letdown","name":"Let Down","artists":[{"name":"Radiohead"}],"album":{"name":"OK Computer","images":[{"url":"http://img/let-down.jpg"}]}}]}"#.to_string()
                    } else if first.starts_with("GET /v1/me/player ")
                        || first.starts_with("GET /v1/me/player?")
                    {
                        player_state.to_string()
                    } else if first.starts_with("GET /v1/search") {
                        r#"{"tracks":{"items":[{"uri":"spotify:track:abc","name":"Karma Police","artists":[{"name":"Radiohead"}]}]}}"#.to_string()
                    } else {
                        // player control endpoints: 204-style, empty body
                        String::new()
                    };
                    let status = if fail_play && first.starts_with("PUT /v1/me/player/play") {
                        "404 Not Found"
                    } else {
                        "200 OK"
                    };
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });
        (format!("http://{addr}"), seen)
    }

    #[tokio::test]
    async fn play_query_searches_then_starts_on_the_named_device() {
        let (base, seen) = spawn_fake_spotify().await;
        let api = SpotifyWebApi::with_bases("cid", "secret", "refresh", "Ambient", &base, &base);

        let msg = api
            .command(SpotifyCommand::Play {
                query: Some("karma police".to_string()),
                kind: SearchKind::Track,
            })
            .await
            .unwrap();
        assert_eq!(msg, "Playing Karma Police by Radiohead.");

        let calls = seen.lock().await;
        assert!(
            calls.iter().any(|c| c.starts_with("POST /api/token")),
            "refreshed token"
        );
        assert!(
            calls
                .iter()
                .any(|c| c.starts_with("GET /v1/me/player/devices")),
            "resolved device"
        );
        assert!(
            calls.iter().any(|c| c.starts_with("GET /v1/search")),
            "searched"
        );
        // Playback targeted the resolved device id, not the phone.
        assert!(
            calls
                .iter()
                .any(|c| c.starts_with("PUT /v1/me/player/play?device_id=dev-1")),
            "started playback on dev-1; calls were {calls:?}"
        );
    }

    #[tokio::test]
    async fn volume_is_clamped_and_targets_the_device() {
        let (base, seen) = spawn_fake_spotify().await;
        let api = SpotifyWebApi::with_bases("cid", "secret", "refresh", "Ambient", &base, &base);
        let msg = api
            .command(SpotifyCommand::SetVolume { percent: 250 })
            .await
            .unwrap();
        assert_eq!(msg, "Set the volume to 100 percent.");
        let calls = seen.lock().await;
        assert!(calls
            .iter()
            .any(|c| c.contains("/v1/me/player/volume?device_id=dev-1&volume_percent=100")));
    }

    #[tokio::test]
    async fn device_id_is_cached_across_commands() {
        // A volume drag is a burst of commands: only the first looks the device up.
        let (base, seen) = spawn_fake_spotify().await;
        let api = SpotifyWebApi::with_bases("cid", "secret", "refresh", "Ambient", &base, &base);
        for pct in [10, 20, 30] {
            api.command(SpotifyCommand::SetVolume { percent: pct })
                .await
                .unwrap();
        }
        let calls = seen.lock().await;
        let lookups = calls
            .iter()
            .filter(|c| c.starts_with("GET /v1/me/player/devices"))
            .count();
        assert_eq!(
            lookups, 1,
            "device resolved once and reused; calls were {calls:?}"
        );
        assert!(calls
            .iter()
            .any(|c| c.contains("volume?device_id=dev-1&volume_percent=30")));
    }

    #[tokio::test]
    async fn access_token_is_cached_across_commands() {
        let (base, seen) = spawn_fake_spotify().await;
        let api = SpotifyWebApi::with_bases("cid", "secret", "refresh", "Ambient", &base, &base);
        api.command(SpotifyCommand::Pause).await.unwrap();
        api.command(SpotifyCommand::Next).await.unwrap();
        let token_calls = seen
            .lock()
            .await
            .iter()
            .filter(|c| c.starts_with("POST /api/token"))
            .count();
        assert_eq!(token_calls, 1, "token fetched once and reused");
    }

    #[test]
    fn skip_to_parses_an_index_and_rejects_missing_or_negative() {
        assert_eq!(
            SpotifyCommand::transport("skip_to", Some(3)).unwrap(),
            Some(SpotifyCommand::SkipTo { index: 3 })
        );
        assert!(SpotifyCommand::transport("skip_to", None).is_err());
        assert!(SpotifyCommand::transport("skip_to", Some(-1)).is_err());
    }

    #[tokio::test]
    async fn skip_to_with_no_offsetable_context_skips_index_plus_one_times() {
        let (base, seen) = spawn_fake_spotify().await;
        let api = SpotifyWebApi::with_bases("cid", "secret", "refresh", "Ambient", &base, &base);
        let msg = api
            .command(SpotifyCommand::SkipTo { index: 0 })
            .await
            .unwrap();
        assert_eq!(msg, "Playing Let Down by Radiohead.");
        let calls = seen.lock().await;
        let nexts = calls
            .iter()
            .filter(|c| c.starts_with("POST /v1/me/player/next?device_id=dev-1"))
            .count();
        assert_eq!(nexts, 1, "calls were {calls:?}");
    }

    const PLAYLIST_STATE: &str = r#"{"is_playing":true,"context":{"type":"playlist","uri":"spotify:playlist:p1"},"item":{"name":"Paranoid Android","artists":[{"name":"Radiohead"}]}}"#;

    #[tokio::test]
    async fn skip_to_in_a_playlist_jumps_with_one_offset_play() {
        let (base, seen) = spawn_fake_spotify_with(PLAYLIST_STATE, false).await;
        let api = SpotifyWebApi::with_bases("cid", "secret", "refresh", "Ambient", &base, &base);
        let msg = api
            .command(SpotifyCommand::SkipTo { index: 0 })
            .await
            .unwrap();
        assert_eq!(msg, "Playing Let Down by Radiohead.");
        let calls = seen.lock().await;
        assert!(
            calls
                .iter()
                .any(|c| c.starts_with("PUT /v1/me/player/play?device_id=dev-1")),
            "calls were {calls:?}"
        );
        assert!(!calls
            .iter()
            .any(|c| c.starts_with("POST /v1/me/player/next")));
    }

    #[tokio::test]
    async fn skip_to_falls_back_to_skipping_when_the_offset_play_fails() {
        // e.g. a user-queued track that isn't part of the playlist.
        let (base, seen) = spawn_fake_spotify_with(PLAYLIST_STATE, true).await;
        let api = SpotifyWebApi::with_bases("cid", "secret", "refresh", "Ambient", &base, &base);
        api.command(SpotifyCommand::SkipTo { index: 0 })
            .await
            .unwrap();
        let calls = seen.lock().await;
        let nexts = calls
            .iter()
            .filter(|c| c.starts_with("POST /v1/me/player/next"))
            .count();
        assert_eq!(nexts, 1, "calls were {calls:?}");
    }

    #[test]
    fn only_album_and_playlist_contexts_take_an_offset() {
        let ctx = |t: &str| serde_json::json!({"context": {"type": t, "uri": "spotify:x:1"}});
        assert_eq!(
            offsetable_context(&ctx("playlist")).as_deref(),
            Some("spotify:x:1")
        );
        assert!(offsetable_context(&ctx("album")).is_some());
        assert!(offsetable_context(&ctx("artist")).is_none());
        assert!(offsetable_context(&serde_json::json!({"context": null})).is_none());
    }

    #[tokio::test]
    async fn skip_to_past_the_queue_end_errors_without_skipping() {
        let (base, seen) = spawn_fake_spotify().await;
        let api = SpotifyWebApi::with_bases("cid", "secret", "refresh", "Ambient", &base, &base);
        // The fake queue holds one track; a stale tap on a later row must not skip.
        let err = api
            .command(SpotifyCommand::SkipTo { index: 4 })
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("isn't in the up-next queue"));
        let calls = seen.lock().await;
        assert!(!calls
            .iter()
            .any(|c| c.starts_with("POST /v1/me/player/next")));
    }

    #[tokio::test]
    async fn now_playing_reads_track_and_queue() {
        let (base, seen) = spawn_fake_spotify().await;
        let api = SpotifyWebApi::with_bases("cid", "secret", "refresh", "Ambient", &base, &base);

        let np = api
            .now_playing()
            .await
            .unwrap()
            .expect("something is playing");
        assert!(np.playing);
        assert_eq!(np.track_title, "Paranoid Android");
        assert_eq!(np.artist, "Radiohead");
        assert_eq!(np.album, "OK Computer");
        assert_eq!(np.artwork_uri, "http://img/ok-computer.jpg");
        assert_eq!(np.position_secs, 73);
        assert_eq!(np.duration_secs, 383);
        assert_eq!(np.volume_percent, 65);
        assert_eq!(np.next_up.len(), 1);
        assert_eq!(np.next_up[0].track_title, "Let Down");
        assert_eq!(np.next_up[0].artwork_uri, "http://img/let-down.jpg");

        let calls = seen.lock().await;
        assert!(calls.iter().any(|c| c.starts_with("GET /v1/me/player ")
            || c.starts_with("GET /v1/me/player?")
            || c == "GET /v1/me/player"));
        assert!(calls
            .iter()
            .any(|c| c.starts_with("GET /v1/me/player/queue")));
    }
}
