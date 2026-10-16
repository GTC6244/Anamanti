//! The Music tab: the managed music processes, the Snapcast status, the mpv web-URL
//! player, the Spotify voice-control link, and the internet-radio station list
//! (`plans/RadioPlan.md`).

use serde_json::{json, Value};

use super::{fail, keep_or_set, parse_json_body};
use crate::music::{ManagedProc, MusicDisplayStyle, MusicHub, NowPlayingService, RadioCatalog};
use crate::settings::{SharedSettings, SpotifyUpdate};

const MUSIC_DISABLED: &str = "music routing is disabled";
const NO_MPV: &str = "no mpv IPC configured (music.web_ipc)";
const NO_RADIO: &str =
    "radio needs music routing and the mpv web player (music.enabled + music.web_ipc)";

/// `{ ok: true }`, or the error in-band.
fn done(res: anyhow::Result<()>) -> String {
    match res {
        Ok(()) => json!({ "ok": true }).to_string(),
        Err(e) => fail(format!("{e:#}")),
    }
}

/// `GET /music/status.json` — supervised process state + live snapserver status.
/// `{ ok, enabled, snapserver_addr, procs: [...], snapserver: { reachable, groups } }`.
pub(super) async fn music_status_json(music: Option<&MusicHub>) -> String {
    let Some(hub) = music else {
        return json!({ "ok": true, "enabled": false }).to_string();
    };
    let procs = serde_json::to_value(hub.supervisor.status().await).unwrap_or_else(|_| json!([]));
    let snapserver = match hub.snapcast.get_status().await {
        Ok(status) => {
            let groups: Vec<Value> = status
                .groups
                .iter()
                .map(|g| {
                    let clients: Vec<Value> = g
                        .clients
                        .iter()
                        .map(|c| {
                            json!({
                                "id": c.id,
                                "name": c.host.name,
                                "volume": c.volume_percent(),
                                "muted": c.volume_muted(),
                            })
                        })
                        .collect();
                    json!({ "id": g.id, "stream": g.stream_id, "muted": g.muted, "clients": clients })
                })
                .collect();
            json!({ "reachable": true, "groups": groups })
        }
        Err(_) => json!({ "reachable": false }),
    };
    json!({
        "ok": true,
        "enabled": true,
        "snapserver_addr": hub.snapserver_addr,
        "procs": procs,
        "snapserver": snapserver,
    })
    .to_string()
}

/// `POST /music/proc` — body `{ proc, action }` starts/stops a managed process.
pub(super) async fn music_proc_json(music: Option<&MusicHub>, body: &[u8]) -> String {
    let Some(hub) = music else {
        return fail(MUSIC_DISABLED);
    };
    let data = match parse_json_body(body) {
        Ok(v) => v,
        Err(invalid) => return invalid,
    };
    let Some(proc) = data
        .get("proc")
        .and_then(Value::as_str)
        .and_then(ManagedProc::from_key)
    else {
        return fail("unknown proc");
    };
    let action = data.get("action").and_then(Value::as_str).unwrap_or("");
    done(match action {
        "start" => hub.supervisor.start(proc).await,
        "stop" => hub.supervisor.stop(proc).await,
        other => Err(anyhow::anyhow!("unknown action {other:?}")),
    })
}

/// `POST /music/play` — body `{ url }` loads a URL in the mpv web player.
pub(super) async fn music_play_json(music: Option<&MusicHub>, body: &[u8]) -> String {
    let Some(hub) = music else {
        return fail(MUSIC_DISABLED);
    };
    let Some(mpv) = &hub.mpv else {
        return fail(NO_MPV);
    };
    let data = match parse_json_body(body) {
        Ok(v) => v,
        Err(invalid) => return invalid,
    };
    let url = data.get("url").and_then(Value::as_str).unwrap_or("").trim();
    if url.is_empty() {
        return fail("empty url");
    }
    done(mpv.load(url).await)
}

/// `POST /music/stopweb` — stop the mpv web player.
pub(super) async fn music_stopweb_json(music: Option<&MusicHub>) -> String {
    let Some(hub) = music else {
        return fail(MUSIC_DISABLED);
    };
    let Some(mpv) = &hub.mpv else {
        return fail(NO_MPV);
    };
    done(mpv.stop().await)
}

/// `POST /music/volume` — body `{ client, volume?, muted? }` sets one snapclient's
/// volume (0–100) and/or mute. Replies `{ ok, deferred }`: `deferred` means the
/// assistant is speaking (music ducked) and the new level applies when it finishes.
pub(super) async fn music_volume_json(music: Option<&MusicHub>, body: &[u8]) -> String {
    let Some(hub) = music else {
        return fail(MUSIC_DISABLED);
    };
    let data = match parse_json_body(body) {
        Ok(v) => v,
        Err(invalid) => return invalid,
    };
    let client = data
        .get("client")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if client.is_empty() {
        return fail("missing client");
    }
    let percent = data
        .get("volume")
        .and_then(Value::as_f64)
        .map(|v| v.round().clamp(0.0, 100.0) as u8);
    let muted = data.get("muted").and_then(Value::as_bool);
    if percent.is_none() && muted.is_none() {
        return fail("nothing to set (volume or muted)");
    }
    match hub.set_client_volume(client, percent, muted).await {
        Ok(deferred) => json!({ "ok": true, "deferred": deferred }).to_string(),
        Err(e) => fail(format!("{e:#}")),
    }
}

/// `POST /music/startall` — start all managed processes (skips a snapserver that is
/// already running).
pub(super) async fn music_startall_json(music: Option<&MusicHub>) -> String {
    match music {
        Some(hub) => {
            hub.start_all().await;
            json!({ "ok": true }).to_string()
        }
        None => fail(MUSIC_DISABLED),
    }
}

/// `POST /music/stopall` — stop every process this orchestrator started.
pub(super) async fn music_stopall_json(music: Option<&MusicHub>) -> String {
    match music {
        Some(hub) => {
            hub.stop_all().await;
            json!({ "ok": true }).to_string()
        }
        None => fail(MUSIC_DISABLED),
    }
}

/// `GET /spotify/status.json` — the Spotify voice-control linkage state for the
/// `/music` page. Reports only booleans + the device name; the client secret and
/// refresh token are never included.
pub(super) fn spotify_status_json(settings: &SharedSettings) -> String {
    let s = settings.spotify();
    json!({
        "ok": true,
        "configured": s.configured(),
        "linked": s.linked(),
        "client_id_set": s.client_id.as_deref().is_some_and(|v| !v.is_empty()),
        "client_secret_set": s.client_secret.as_deref().is_some_and(|v| !v.is_empty()),
        "has_refresh_token": s.refresh_token.as_deref().is_some_and(|v| !v.is_empty()),
        "device_name": s.device_label(),
        "redirect_url": s.redirect_url_or_default(),
    })
    .to_string()
}

/// `POST /spotify/save` — set the Spotify app client id/secret, device name, and OAuth
/// redirect URL. A blank or absent value is left unchanged (a page reload never wipes a
/// stored secret). Applying rebuilds the LLM so the `spotify_control` tool tracks
/// linkage.
pub(super) fn spotify_save_json(settings: &SharedSettings, data: &Value) -> String {
    settings.apply_spotify(&SpotifyUpdate {
        client_id: keep_or_set(data, "client_id"),
        client_secret: keep_or_set(data, "client_secret"),
        device_name: keep_or_set(data, "device_name"),
        redirect_url: keep_or_set(data, "redirect_url"),
        ..Default::default()
    });
    spotify_status_json(settings)
}

/// `POST /spotify/link` — run the one-time Spotify OAuth consent using the stored
/// client credentials (opens a browser on the Mac) and store the refresh token.
/// Requires the configured redirect URL (default `http://127.0.0.1:8888/callback`) to
/// be registered in the Spotify app. On success the `spotify_control` tool activates
/// immediately. Blocks this connection until consent completes or times out.
pub(super) async fn spotify_link_json(settings: &SharedSettings) -> String {
    let s = settings.spotify();
    let (Some(cid), Some(secret)) = (
        s.client_id.clone().filter(|v| !v.is_empty()),
        s.client_secret.clone().filter(|v| !v.is_empty()),
    ) else {
        return fail("Set the Spotify client id and secret first (Save), then Connect.");
    };
    let redirect_url = s.redirect_url_or_default();
    let outcome = match crate::spotify_consent::run_consent(
        &cid,
        &secret,
        &redirect_url,
        crate::spotify_consent::SPOTIFY_SCOPE,
        std::time::Duration::from_secs(180),
    )
    .await
    {
        Ok(o) => o,
        Err(e) => return fail(format!("{e:#}")),
    };
    settings.apply_spotify(&SpotifyUpdate {
        refresh_token: Some(Some(outcome.refresh_token)),
        scope: Some(Some(outcome.scope)),
        ..Default::default()
    });
    let now = settings.spotify();
    json!({
        "ok": true,
        "message": "Spotify linked — the voice tool is live. Try \"play some Radiohead\".",
        "linked": now.linked(),
        "device_name": now.device_label(),
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Internet radio
// ---------------------------------------------------------------------------

/// `GET /radio/stations.json` — `{ ok, enabled, stations: [...], current }`. `enabled` is
/// whether the player exists (music routing + mpv); the list is editable either way.
pub(super) fn radio_stations_json(settings: &SharedSettings) -> String {
    let player = settings.radio_player();
    json!({
        "ok": true,
        "enabled": player.is_some(),
        "stations": settings.radio().stations,
        "current": player.and_then(|p| p.current()).map(|s| s.id),
    })
    .to_string()
}

/// `POST /radio/save` — body `{ stations: [...] }`, a full-record save (sanitized:
/// trimmed, ids uppercased + de-duplicated, entries without an id or http(s) URL
/// dropped). Rebuilds the LLM so `radio_control` lists the new stations.
pub(super) fn radio_save_json(settings: &SharedSettings, data: &Value) -> String {
    let catalog: RadioCatalog = match serde_json::from_value(json!({
        "stations": data.get("stations").cloned().unwrap_or_else(|| json!([])),
    })) {
        Ok(c) => c,
        Err(e) => return fail(format!("invalid stations: {e}")),
    };
    settings.apply_radio(&catalog);
    radio_stations_json(settings)
}

/// `POST /radio/lookup` — body `{ url, name }`: candidate stations (name, logo,
/// suggested id…) from the public Radio Browser directory.
pub(super) async fn radio_lookup_json(body: &[u8]) -> String {
    let data = match parse_json_body(body) {
        Ok(v) => v,
        Err(invalid) => return invalid,
    };
    let field = |k: &str| {
        data.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    match crate::music::radio_browser::lookup(&field("url"), &field("name")).await {
        Ok(matches) => json!({ "ok": true, "matches": matches }).to_string(),
        Err(e) => fail(format!("{e:#}")),
    }
}

/// `GET /music/nowplaying.json` — what is audible right now (the radio when a station
/// is on, else Spotify), read on demand for the Music page's now-playing strip:
/// `{ ok, linked, radio, now_playing }` where `now_playing` is the same snapshot the
/// display's music screen receives, or `null` when nothing is loaded.
pub(super) async fn music_nowplaying_json(
    settings: &SharedSettings,
    now_playing: &NowPlayingService,
) -> String {
    let linked = now_playing.controller(settings).is_some();
    let radio = now_playing.radio().is_some();
    match now_playing.current(settings).await {
        Ok(np) => {
            json!({ "ok": true, "linked": linked, "radio": radio, "now_playing": np }).to_string()
        }
        Err(e) => fail(format!("{e:#}")),
    }
}

/// `POST /music/transport` — body `{ action, value? }` (`pause`/`resume`/`next`/
/// `previous`/`volume` + 0–100). The same path as the display's on-screen controls:
/// it drives the radio while a station is on, else Spotify.
pub(super) async fn music_transport_json(
    settings: &SharedSettings,
    now_playing: &NowPlayingService,
    body: &[u8],
) -> String {
    let data = match parse_json_body(body) {
        Ok(v) => v,
        Err(invalid) => return invalid,
    };
    let action = data.get("action").and_then(Value::as_str).unwrap_or("");
    let value = data
        .get("value")
        .and_then(Value::as_f64)
        .map(|v| v.round() as i64);
    match now_playing.transport(settings, action, value).await {
        Ok(msg) => json!({ "ok": true, "message": msg }).to_string(),
        Err(e) => fail(format!("{e:#}")),
    }
}

/// `POST /radio/play` — body `{ id }`: play that station on the house speakers
/// (pausing Spotify first, best-effort).
pub(super) async fn radio_play_json(
    settings: &SharedSettings,
    now_playing: &NowPlayingService,
    body: &[u8],
) -> String {
    let Some(player) = settings.radio_player() else {
        return fail(NO_RADIO);
    };
    let data = match parse_json_body(body) {
        Ok(v) => v,
        Err(invalid) => return invalid,
    };
    let id = data.get("id").and_then(Value::as_str).unwrap_or("");
    let spotify = now_playing.controller(settings);
    match player.play_id(id, spotify.as_deref()).await {
        Ok(msg) => json!({ "ok": true, "message": msg, "current": player.current().map(|s| s.id) })
            .to_string(),
        Err(e) => fail(format!("{e:#}")),
    }
}

/// `POST /radio/stop` — stop the radio and hand the speakers back to Spotify.
pub(super) async fn radio_stop_json(settings: &SharedSettings) -> String {
    let Some(player) = settings.radio_player() else {
        return fail(NO_RADIO);
    };
    match player.stop().await {
        Ok(msg) => json!({ "ok": true, "message": msg, "current": null }).to_string(),
        Err(e) => fail(format!("{e:#}")),
    }
}

// ---------------------------------------------------------------------------
// Display style (compact bar / full-screen now-playing)
// ---------------------------------------------------------------------------

/// `GET /music/display.json` — `{ ok, style }`: which music widget the displays show
/// while Spotify or the radio plays (`"compact"` / `"fullscreen"`).
pub(super) fn music_display_json(settings: &SharedSettings) -> String {
    json!({ "ok": true, "style": settings.music_display_style().as_str() }).to_string()
}

/// `POST /music/display` — body `{ style }`: set the music-widget preference, persist it,
/// and re-push now-playing so every connected display switches at once.
pub(super) fn music_display_save_json(
    settings: &SharedSettings,
    now_playing: &NowPlayingService,
    data: &Value,
) -> String {
    let raw = data.get("style").and_then(Value::as_str).unwrap_or("");
    let Some(style) = MusicDisplayStyle::parse(raw) else {
        return fail(format!("unknown style {raw:?} (use compact or fullscreen)"));
    };
    let pushed = now_playing.set_display_style(settings, style);
    json!({ "ok": true, "style": style.as_str(), "pushed": pushed }).to_string()
}
