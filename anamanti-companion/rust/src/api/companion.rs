//! FRB-exposed API for Anamanti Companion (the phone app).
//!
//! Rust owns all networking: mDNS discovery, the companion HTTP API (GET-only, plain
//! HTTP on the LAN) and the Wyoming memory control frames. Dart holds the selected
//! [`CoreInfo`] (as resolved by mDNS — never typed in by a user) and passes it back
//! into each call, so this layer is stateless.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use crate::{decode, discovery, http, memory};

/// A Core found by mDNS (`_wyoming._tcp`, TXT `role=core`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreInfo {
    /// Stable selection key: TXT `instance_id` (falls back to the mDNS instance name).
    pub key: String,
    /// Human-friendly name: TXT `name` (falls back to the instance name).
    pub name: String,
    /// The advertised mDNS hostname (informational).
    pub host: String,
    /// The chosen reachable IP literal (IPv4 preferred).
    pub address: String,
    /// The Wyoming port (the mDNS service port) — memories go here.
    pub wyoming_port: u16,
    /// TXT `companion_port`; `None` ⇒ the Core's companion HTTP API is off/too old.
    pub companion_port: Option<u16>,
}

/// The current playback, as `/music/nowplaying.json`'s `now_playing` snapshot.
#[derive(Debug, Clone)]
pub struct NowPlayingTrack {
    pub playing: bool,
    pub track_title: String,
    pub artist: String,
    pub album: String,
    /// Artwork URL (Spotify album art or a radio logo), or empty.
    pub artwork_uri: String,
    pub position_secs: i64,
    /// 0 for radio (no seek bar).
    pub duration_secs: i64,
    pub volume_percent: u8,
    /// `spotify` or `radio`.
    pub source: String,
    pub station_id: String,
    pub station_name: String,
}

/// `GET /music/nowplaying.json`.
#[derive(Debug, Clone)]
pub struct NowPlayingStatus {
    /// Spotify is linked on the Core.
    pub linked: bool,
    /// A radio station is on.
    pub radio: bool,
    /// `None` when nothing is loaded.
    pub now_playing: Option<NowPlayingTrack>,
}

/// One timer on one display.
#[derive(Debug, Clone)]
pub struct CoreTimer {
    pub id: i64,
    pub label: String,
    pub duration_secs: i64,
    /// Seconds left as of the Core's response (clamped ≥ 0). Count down from here
    /// using the phone's monotonic clock — never compare phone vs Mac wall clocks.
    pub remaining_secs: i64,
    pub ringing: bool,
    /// Informational only (Core wall clock).
    pub ends_at_ms: i64,
}

/// One display's timers.
#[derive(Debug, Clone)]
pub struct DisplayTimers {
    pub device_id: String,
    pub name: String,
    pub reported_at_ms: i64,
    pub timers: Vec<CoreTimer>,
}

/// `GET /timers.json`.
#[derive(Debug, Clone)]
pub struct TimersSnapshot {
    /// The Core's clock at response time (informational only).
    pub now_ms: i64,
    pub displays: Vec<DisplayTimers>,
}

/// One persistent memory entry (the Core's SQLite facts store).
#[derive(Debug, Clone)]
pub struct MemoryEntry {
    pub id: i64,
    /// `fact` or `preference`.
    pub kind: String,
    pub content: String,
    /// `explicit` (user asked) or `inferred` (auto-extracted).
    pub source: String,
    /// Unix seconds when the entry was stored.
    pub created_at: i64,
}

/// Version of the companion's Rust engine (the crate version).
#[flutter_rust_bridge::frb(sync)]
pub fn engine_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Browse mDNS for up to `timeout_ms` and return every Core found. When
/// `preferred_key` is given and that Core answers, the browse returns early.
pub async fn discover_cores(
    timeout_ms: u32,
    preferred_key: Option<String>,
) -> Result<Vec<CoreInfo>> {
    discovery::discover_all(
        Duration::from_millis(u64::from(timeout_ms)),
        preferred_key.as_deref(),
    )
    .await
}

/// `GET /overview.json` as raw JSON (rendered flexibly in Dart). `probe=false`
/// sends `?probe=0` so the Core skips its live service probes.
pub async fn fetch_overview(core: CoreInfo, probe: bool) -> Result<String> {
    let base = companion_base(&core)?;
    let path = if probe {
        "/overview.json"
    } else {
        "/overview.json?probe=0"
    };
    let v = http::get_json(&base, path).await?;
    Ok(v.to_string())
}

/// `GET /music/nowplaying.json`.
pub async fn fetch_now_playing(core: CoreInfo) -> Result<NowPlayingStatus> {
    let base = companion_base(&core)?;
    decode::now_playing(&http::get(&base, "/music/nowplaying.json").await?)
}

/// `GET /timers.json` — every timer on every display.
pub async fn fetch_timers(core: CoreInfo) -> Result<TimersSnapshot> {
    let base = companion_base(&core)?;
    decode::timers(&http::get(&base, "/timers.json").await?)
}

/// List memories over Wyoming (`anamanti-list-memories`).
pub async fn list_memories(core: CoreInfo) -> Result<Vec<MemoryEntry>> {
    memory::list(wyoming_addr(&core)?).await
}

/// Delete one memory over Wyoming; `true` if it was removed.
pub async fn delete_memory(core: CoreInfo, id: i64) -> Result<bool> {
    memory::delete(wyoming_addr(&core)?, id).await
}

/// Clear every memory over Wyoming; returns how many were removed.
pub async fn clear_memories(core: CoreInfo) -> Result<u32> {
    memory::clear(wyoming_addr(&core)?).await
}

fn companion_base(core: &CoreInfo) -> Result<String> {
    let port = core
        .companion_port
        .ok_or_else(|| anyhow!("This Core doesn't have the companion API turned on"))?;
    http::base_url(&core.address, port)
}

fn wyoming_addr(core: &CoreInfo) -> Result<SocketAddr> {
    let ip: IpAddr = core
        .address
        .parse()
        .with_context(|| format!("invalid Core address `{}`", core.address))?;
    Ok(SocketAddr::new(ip, core.wyoming_port))
}

#[flutter_rust_bridge::frb(init)]
pub fn init_app() {
    flutter_rust_bridge::setup_default_user_utils();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::test_server::serve_once;

    fn core(port: Option<u16>) -> CoreInfo {
        CoreInfo {
            key: "k".into(),
            name: "n".into(),
            host: "h.local.".into(),
            address: "127.0.0.1".into(),
            wyoming_port: 10700,
            companion_port: port,
        }
    }

    fn port_of(base: &str) -> u16 {
        base.rsplit(':').next().unwrap().parse().unwrap()
    }

    #[tokio::test]
    async fn missing_companion_port_is_a_clear_error() {
        let err = fetch_timers(core(None)).await.unwrap_err();
        assert!(err.to_string().contains("companion API"));
    }

    #[tokio::test]
    async fn fetch_overview_uses_probe_query() {
        let (base, server) = serve_once("200 OK", r#"{"ok":true,"stages":[]}"#).await;
        let json = fetch_overview(core(Some(port_of(&base))), false)
            .await
            .unwrap();
        assert!(json.contains("stages"));
        assert!(server
            .await
            .unwrap()
            .starts_with("GET /overview.json?probe=0 "));
    }

    #[tokio::test]
    async fn fetch_timers_end_to_end() {
        let body = r#"{"ok":true,"now_ms":5,"displays":[{"device_id":"d","name":"Kitchen","reported_at_ms":1,"timers":[{"id":1,"label":"eggs","duration_secs":300,"remaining_secs":120,"ringing":false,"ends_at_ms":9}]}]}"#;
        let (base, server) = serve_once("200 OK", body).await;
        let t = fetch_timers(core(Some(port_of(&base)))).await.unwrap();
        assert_eq!(t.displays[0].timers[0].label, "eggs");
        assert!(server.await.unwrap().starts_with("GET /timers.json "));
    }

    #[tokio::test]
    async fn fetch_now_playing_end_to_end() {
        let (base, _s) = serve_once(
            "200 OK",
            r#"{"ok":true,"linked":false,"radio":false,"now_playing":null}"#,
        )
        .await;
        let s = fetch_now_playing(core(Some(port_of(&base)))).await.unwrap();
        assert!(!s.linked && s.now_playing.is_none());
    }
}
