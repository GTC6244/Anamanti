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
    /// Search for a track and add it to the up-next queue.
    Queue {
        query: String,
    },
    /// Set the Connect device volume (0–100).
    SetVolume {
        percent: u8,
    },
}

/// The control seam the `spotify_control` rig tool depends on. Returns a short,
/// speakable confirmation the model relays to the user.
#[async_trait]
pub trait SpotifyController: Send + Sync {
    async fn command(&self, cmd: SpotifyCommand) -> Result<String>;
}

/// A resolved search hit: the URI to play and a speakable label.
#[derive(Debug, Clone)]
struct SearchHit {
    uri: String,
    label: String,
    is_context: bool,
}

/// Live Spotify Web API implementation of [`SpotifyController`].
///
/// Holds long-lived credentials (client id/secret + refresh token) and caches a
/// short-lived access token, refreshing it on demand. Every playback call targets
/// the librespot Connect device resolved by name.
pub struct SpotifyWebApi {
    http: reqwest::Client,
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
}

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
            http: reqwest::Client::new(),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            refresh_token: refresh_token.into(),
            device_name: device_name.into(),
            accounts_base: accounts_base.into().trim_end_matches('/').to_string(),
            api_base: api_base.into().trim_end_matches('/').to_string(),
            token: Mutex::new(None),
        }
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
        let resp: Value = self
            .http
            .post(format!("{}/api/token", self.accounts_base))
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", self.refresh_token.as_str()),
            ])
            .send()
            .await
            .context("requesting Spotify access token")?
            .error_for_status()
            .context(
                "Spotify token endpoint returned an error (check client id/secret/refresh token)",
            )?
            .json()
            .await
            .context("parsing Spotify token response")?;

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
        let resp: Value = self
            .http
            .get(format!("{}/v1/me/player/devices", self.api_base))
            .bearer_auth(token)
            .send()
            .await
            .context("listing Spotify devices")?
            .error_for_status()
            .context("Spotify devices endpoint returned an error")?
            .json()
            .await
            .context("parsing Spotify devices response")?;

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
        let resp: Value = self
            .http
            .get(format!("{}/v1/search", self.api_base))
            .bearer_auth(token)
            .query(&[("q", query), ("type", kind.as_query()), ("limit", "1")])
            .send()
            .await
            .context("searching Spotify")?
            .error_for_status()
            .context("Spotify search returned an error")?
            .json()
            .await
            .context("parsing Spotify search response")?;

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
        let mut req = self
            .http
            .request(method, format!("{}/v1/me/player{path}", self.api_base))
            .bearer_auth(token)
            .query(&query);
        if let Some(b) = body {
            req = req.json(&b);
        } else {
            // Spotify's play/pause endpoints want a JSON content-type even empty.
            req = req.header(reqwest::header::CONTENT_LENGTH, "0");
        }
        req.send()
            .await
            .with_context(|| format!("Spotify player call {path}"))?
            .error_for_status()
            .with_context(|| format!("Spotify player endpoint {path} returned an error"))?;
        Ok(())
    }
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

#[async_trait]
impl SpotifyController for SpotifyWebApi {
    async fn command(&self, cmd: SpotifyCommand) -> Result<String> {
        let token = self.access_token().await?;
        let device = self.device_id(&token).await?;
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
                    } else if first.starts_with("GET /v1/search") {
                        r#"{"tracks":{"items":[{"uri":"spotify:track:abc","name":"Karma Police","artists":[{"name":"Radiohead"}]}]}}"#.to_string()
                    } else {
                        // player control endpoints: 204-style, empty body
                        String::new()
                    };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
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
}
