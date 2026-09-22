//! Music routing control plane (Snapcast + web-URL player).
//!
//! This module is the orchestrator's **control-only** side of the house-wide
//! music feature (`plans/snapcast_routing_plan.md`). It never touches audio PCM —
//! music is produced by external sibling processes (librespot for Spotify, an
//! `mpv`/`ffmpeg` player for web URLs) that write raw PCM into snapfifos read by a
//! **snapserver** on the Mac. Here we only:
//!
//! * talk to snapserver over its **JSON-RPC control API** (TCP :1705,
//!   newline-delimited JSON-RPC 2.0) to **duck** the music while the assistant
//!   speaks and to select the active stream, and
//! * drive the web-URL player over **mpv's JSON IPC** (a Unix socket) to
//!   play/stop/volume a URL.
//!
//! Everything here is **best-effort and inert unless `AMBIENT_MUSIC` is on**: a
//! missing/unreachable snapserver or mpv never breaks a voice turn — the caller
//! logs and moves on. See `crate::config::MusicConfig` for the env wiring and
//! `crate::server` for where ducking hooks the turn lifecycle
//! (`TurnEvent::Speaking` → duck, `TurnEvent::Finished` → restore).

pub mod supervisor;
pub use supervisor::{ManagedProc, MusicHub, MusicSupervisor, ProcSpec, ProcStatus};

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

/// Which snapserver group the ducker acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupSelector {
    /// The first group reported by the server.
    Auto,
    /// A specific group id.
    GroupId(String),
    /// The group currently playing this stream id (e.g. `"Spotify"`).
    StreamId(String),
}

impl GroupSelector {
    /// Parse the `AMBIENT_MUSIC_GROUP` / `AMBIENT_MUSIC_STREAM` pair. A non-empty
    /// stream id wins (most specific); then a group id; else `Auto`.
    pub fn from_parts(group_id: Option<&str>, stream_id: Option<&str>) -> Self {
        if let Some(s) = stream_id.map(str::trim).filter(|s| !s.is_empty()) {
            GroupSelector::StreamId(s.to_string())
        } else if let Some(g) = group_id.map(str::trim).filter(|g| !g.is_empty()) {
            match g.to_lowercase().as_str() {
                "auto" | "" => GroupSelector::Auto,
                _ => GroupSelector::GroupId(g.to_string()),
            }
        } else {
            GroupSelector::Auto
        }
    }
}

// ---------------------------------------------------------------------------
// snapserver JSON-RPC control client
// ---------------------------------------------------------------------------

/// A thin snapserver JSON-RPC client. Opens a fresh short-lived TCP connection
/// per call (ducking happens a handful of times per turn, so there is no reason
/// to hold a socket open or manage reconnects). Robust to interleaved
/// notifications: it reads lines until the response whose `id` matches the
/// request.
#[derive(Debug, Clone)]
pub struct SnapcastClient {
    addr: SocketAddr,
}

impl SnapcastClient {
    pub fn new(addr: SocketAddr) -> Self {
        Self { addr }
    }

    /// Issue one JSON-RPC method call and return its `result` value.
    pub async fn call(&self, method: &str, params: Option<Value>) -> Result<Value> {
        let stream = TcpStream::connect(self.addr)
            .await
            .with_context(|| format!("connecting to snapserver control at {}", self.addr))?;
        let (read_half, mut write_half) = stream.into_split();

        let mut req = serde_json::Map::new();
        req.insert("id".into(), json!(1));
        req.insert("jsonrpc".into(), json!("2.0"));
        req.insert("method".into(), json!(method));
        if let Some(p) = params {
            req.insert("params".into(), p);
        }
        let mut line = serde_json::to_string(&Value::Object(req))?;
        line.push('\n');
        write_half
            .write_all(line.as_bytes())
            .await
            .context("writing snapserver request")?;
        write_half.flush().await.ok();

        let mut reader = BufReader::new(read_half);
        let mut buf = String::new();
        loop {
            buf.clear();
            let n = reader
                .read_line(&mut buf)
                .await
                .context("reading snapserver response")?;
            if n == 0 {
                bail!("snapserver closed the connection before responding to {method}");
            }
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                continue;
            }
            let msg: Value =
                serde_json::from_str(trimmed).context("parsing snapserver JSON-RPC message")?;
            // Skip server-pushed notifications (they carry `method`, not `id`).
            if msg.get("id") != Some(&json!(1)) {
                continue;
            }
            if let Some(err) = msg.get("error") {
                bail!("snapserver {method} error: {err}");
            }
            return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// `Server.GetStatus` → the parsed group/stream/client tree.
    pub async fn get_status(&self) -> Result<ServerStatus> {
        let result = self.call("Server.GetStatus", None).await?;
        let status: StatusResult =
            serde_json::from_value(result).context("decoding Server.GetStatus result")?;
        Ok(status.server)
    }

    /// `Client.SetVolume` — set one client's volume percent (0–100) and mute.
    pub async fn set_client_volume(&self, client_id: &str, percent: u8, muted: bool) -> Result<()> {
        self.call(
            "Client.SetVolume",
            Some(json!({
                "id": client_id,
                "volume": { "muted": muted, "percent": percent.min(100) },
            })),
        )
        .await
        .map(|_| ())
    }

    /// `Group.SetMute` — mute/unmute an entire group.
    pub async fn set_group_muted(&self, group_id: &str, mute: bool) -> Result<()> {
        self.call(
            "Group.SetMute",
            Some(json!({ "id": group_id, "mute": mute })),
        )
        .await
        .map(|_| ())
    }

    /// `Group.SetStream` — point a group at a different stream id.
    pub async fn set_group_stream(&self, group_id: &str, stream_id: &str) -> Result<()> {
        self.call(
            "Group.SetStream",
            Some(json!({ "id": group_id, "stream_id": stream_id })),
        )
        .await
        .map(|_| ())
    }

    /// Resolve the group a selector points at from a fresh status snapshot.
    pub async fn resolve_group(&self, sel: &GroupSelector) -> Result<Group> {
        let status = self.get_status().await?;
        select_group(&status, sel)
            .cloned()
            .with_context(|| format!("no snapserver group matched {sel:?}"))
    }
}

/// Pick the group a selector refers to from an already-fetched status.
fn select_group<'a>(status: &'a ServerStatus, sel: &GroupSelector) -> Option<&'a Group> {
    match sel {
        GroupSelector::Auto => status.groups.first(),
        GroupSelector::GroupId(id) => status.groups.iter().find(|g| &g.id == id),
        GroupSelector::StreamId(s) => status.groups.iter().find(|g| &g.stream_id == s),
    }
}

#[derive(Debug, Deserialize)]
struct StatusResult {
    #[serde(default)]
    server: ServerStatus,
}

/// The subset of `Server.GetStatus` this module needs.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ServerStatus {
    #[serde(default)]
    pub groups: Vec<Group>,
    #[serde(default)]
    pub streams: Vec<Stream>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Group {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub stream_id: String,
    #[serde(default)]
    pub clients: Vec<Client>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Client {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub config: ClientConfig,
    #[serde(default)]
    pub host: ClientHost,
}

impl Client {
    pub fn volume_percent(&self) -> u8 {
        self.config.volume.percent
    }
    pub fn volume_muted(&self) -> bool {
        self.config.volume.muted
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ClientConfig {
    #[serde(default)]
    pub volume: Volume,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ClientHost {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Volume {
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub percent: u8,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Stream {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub status: String,
}

// ---------------------------------------------------------------------------
// Ducker
// ---------------------------------------------------------------------------

/// What we changed on the last `duck()`, so `restore()` can put it back exactly.
#[derive(Debug, Clone)]
struct DuckRecord {
    /// (client_id, prior_percent, prior_muted) for each client we attenuated.
    clients: Vec<(String, u8, bool)>,
}

/// Lowers the music group's volume while the assistant speaks, then restores it.
///
/// Attenuates each client in the target group to `duck_percent` (remembering the
/// prior per-client volume so `restore()` is exact), which keeps a spoken reply
/// audible over quieter music. Idempotent: a second `duck()` while already ducked
/// is a no-op, and `restore()` with nothing ducked is a no-op — so it is safe to
/// wire to `TurnEvent::Speaking` (which can fire several times per turn) and
/// `TurnEvent::Finished`.
#[derive(Debug)]
pub struct MusicDucker {
    client: SnapcastClient,
    selector: GroupSelector,
    duck_percent: u8,
    active: Mutex<Option<DuckRecord>>,
}

impl MusicDucker {
    pub fn new(addr: SocketAddr, selector: GroupSelector, duck_percent: u8) -> Self {
        Self {
            client: SnapcastClient::new(addr),
            selector,
            duck_percent: duck_percent.min(100),
            active: Mutex::new(None),
        }
    }

    /// Attenuate the target group. Best-effort: returns an error only for logging;
    /// callers must not let it break a turn.
    pub async fn duck(&self) -> Result<()> {
        let mut active = self.active.lock().await;
        if active.is_some() {
            return Ok(()); // already ducked
        }
        let group = self.client.resolve_group(&self.selector).await?;
        let mut record = DuckRecord {
            clients: Vec::with_capacity(group.clients.len()),
        };
        for c in &group.clients {
            record
                .clients
                .push((c.id.clone(), c.volume_percent(), c.volume_muted()));
        }
        // Only mutate after snapshotting, so a mid-loop failure still lets restore
        // put back whatever we managed to change.
        *active = Some(record.clone());
        for (id, _, _) in &record.clients {
            self.client
                .set_client_volume(id, self.duck_percent, false)
                .await
                .with_context(|| format!("ducking client {id}"))?;
        }
        log::debug!(
            "music ducked group {} ({} clients) to {}%",
            group.id,
            record.clients.len(),
            self.duck_percent
        );
        Ok(())
    }

    /// Restore the volumes captured by the last `duck()`.
    pub async fn restore(&self) -> Result<()> {
        let record = {
            let mut active = self.active.lock().await;
            active.take()
        };
        let Some(record) = record else {
            return Ok(()); // nothing ducked
        };
        for (id, percent, muted) in &record.clients {
            self.client
                .set_client_volume(id, *percent, *muted)
                .await
                .with_context(|| format!("restoring client {id}"))?;
        }
        log::debug!("music restored {} clients", record.clients.len());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// mpv JSON IPC (web-URL / radio player control)
// ---------------------------------------------------------------------------

/// Controls a sibling `mpv` process over its JSON IPC Unix socket
/// (`mpv --input-ipc-server=<path> --idle --no-video`). The mpv process decodes a
/// URL to PCM and writes it to the web snapfifo; this only sends control commands
/// (load/stop/volume/pause) — never audio.
#[derive(Debug, Clone)]
pub struct MpvControl {
    socket: PathBuf,
}

impl MpvControl {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    /// Send one mpv IPC command array and return its `data` (if any).
    pub async fn command(&self, args: Vec<Value>) -> Result<Value> {
        use tokio::net::UnixStream;
        let stream = UnixStream::connect(&self.socket)
            .await
            .with_context(|| format!("connecting to mpv IPC at {}", self.socket.display()))?;
        let (read_half, mut write_half) = stream.into_split();

        let mut line = serde_json::to_string(&json!({ "command": args }))?;
        line.push('\n');
        write_half
            .write_all(line.as_bytes())
            .await
            .context("writing mpv command")?;
        write_half.flush().await.ok();

        let mut reader = BufReader::new(read_half);
        let mut buf = String::new();
        loop {
            buf.clear();
            let n = reader
                .read_line(&mut buf)
                .await
                .context("reading mpv response")?;
            if n == 0 {
                bail!("mpv closed the IPC socket before responding");
            }
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                continue;
            }
            let msg: Value = serde_json::from_str(trimmed).context("parsing mpv IPC message")?;
            // mpv interleaves async `event` messages; the command reply carries
            // `error`. Skip everything until we see it.
            match msg.get("error").and_then(Value::as_str) {
                None => continue, // an event, not our reply
                Some("success") => return Ok(msg.get("data").cloned().unwrap_or(Value::Null)),
                Some(other) => bail!("mpv command failed: {other}"),
            }
        }
    }

    /// Replace the current playlist with `url` and start playing.
    pub async fn load(&self, url: &str) -> Result<()> {
        self.command(vec![json!("loadfile"), json!(url), json!("replace")])
            .await
            .map(|_| ())
    }

    /// Stop playback and clear the playlist.
    pub async fn stop(&self) -> Result<()> {
        self.command(vec![json!("stop")]).await.map(|_| ())
    }

    /// Set the player volume (0–100+; mpv allows >100 but we clamp to 100).
    pub async fn set_volume(&self, percent: u8) -> Result<()> {
        self.command(vec![
            json!("set_property"),
            json!("volume"),
            json!(f64::from(percent.min(100))),
        ])
        .await
        .map(|_| ())
    }

    /// Pause or resume playback.
    pub async fn set_paused(&self, paused: bool) -> Result<()> {
        self.command(vec![json!("set_property"), json!("pause"), json!(paused)])
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex as TokioMutex;

    /// A fake snapserver: accepts connections, records each request, and replies
    /// per method. `Server.GetStatus` returns one group ("g1") on stream
    /// "Spotify" with two clients at 80% and 60%.
    async fn spawn_fake_snapserver() -> (SocketAddr, Arc<TokioMutex<Vec<Value>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(TokioMutex::new(Vec::new()));
        let reqs = requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let reqs = reqs.clone();
                tokio::spawn(async move {
                    let (r, mut w) = stream.into_split();
                    let mut reader = BufReader::new(r);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let req: Value = serde_json::from_str(line.trim()).unwrap();
                    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
                    reqs.lock().await.push(req.clone());
                    let result = match method {
                        "Server.GetStatus" => json!({
                            "server": {
                                "groups": [{
                                    "id": "g1",
                                    "muted": false,
                                    "stream_id": "Spotify",
                                    "clients": [
                                        {"id": "a", "config": {"volume": {"muted": false, "percent": 80}}, "host": {"name": "living"}},
                                        {"id": "b", "config": {"volume": {"muted": false, "percent": 60}}, "host": {"name": "kitchen"}}
                                    ]
                                }],
                                "streams": [{"id": "Spotify", "status": "playing"}]
                            }
                        }),
                        "Client.SetVolume" => {
                            let v = req.get("params").and_then(|p| p.get("volume")).cloned();
                            json!({ "volume": v })
                        }
                        _ => json!({}),
                    };
                    let mut resp = serde_json::to_string(
                        &json!({"id": 1, "jsonrpc": "2.0", "result": result}),
                    )
                    .unwrap();
                    resp.push('\n');
                    let _ = w.write_all(resp.as_bytes()).await;
                });
            }
        });
        (addr, requests)
    }

    #[tokio::test]
    async fn get_status_parses_groups_and_clients() {
        let (addr, _reqs) = spawn_fake_snapserver().await;
        let client = SnapcastClient::new(addr);
        let status = client.get_status().await.unwrap();
        assert_eq!(status.groups.len(), 1);
        let g = &status.groups[0];
        assert_eq!(g.id, "g1");
        assert_eq!(g.stream_id, "Spotify");
        assert_eq!(g.clients.len(), 2);
        assert_eq!(g.clients[0].volume_percent(), 80);
        assert_eq!(g.clients[1].host.name, "kitchen");
        assert_eq!(status.streams[0].status, "playing");
    }

    #[test]
    fn group_selector_precedence() {
        assert_eq!(
            GroupSelector::from_parts(Some("g9"), Some("Spotify")),
            GroupSelector::StreamId("Spotify".into())
        );
        assert_eq!(
            GroupSelector::from_parts(Some("g9"), None),
            GroupSelector::GroupId("g9".into())
        );
        assert_eq!(
            GroupSelector::from_parts(Some("auto"), None),
            GroupSelector::Auto
        );
        assert_eq!(GroupSelector::from_parts(None, None), GroupSelector::Auto);
    }

    #[test]
    fn select_group_matches_by_stream_and_id() {
        let status = ServerStatus {
            groups: vec![
                Group {
                    id: "g1".into(),
                    stream_id: "Spotify".into(),
                    ..Default::default()
                },
                Group {
                    id: "g2".into(),
                    stream_id: "Web".into(),
                    ..Default::default()
                },
            ],
            streams: vec![],
        };
        assert_eq!(
            select_group(&status, &GroupSelector::StreamId("Web".into()))
                .unwrap()
                .id,
            "g2"
        );
        assert_eq!(
            select_group(&status, &GroupSelector::GroupId("g1".into()))
                .unwrap()
                .id,
            "g1"
        );
        assert_eq!(
            select_group(&status, &GroupSelector::Auto).unwrap().id,
            "g1"
        );
        assert!(select_group(&status, &GroupSelector::StreamId("Nope".into())).is_none());
    }

    #[tokio::test]
    async fn duck_then_restore_sets_and_reverts_volumes() {
        let (addr, reqs) = spawn_fake_snapserver().await;
        let ducker = MusicDucker::new(addr, GroupSelector::StreamId("Spotify".into()), 25);

        ducker.duck().await.unwrap();
        // A second duck while ducked is a no-op (no extra requests beyond the first).
        ducker.duck().await.unwrap();
        ducker.restore().await.unwrap();

        let requests = reqs.lock().await;
        let set_vols: Vec<&Value> = requests
            .iter()
            .filter(|r| r.get("method").and_then(Value::as_str) == Some("Client.SetVolume"))
            .collect();
        // duck: 2 clients set to 25; restore: 2 clients set back to 80/60 → 4 total.
        assert_eq!(
            set_vols.len(),
            4,
            "expected 2 duck + 2 restore SetVolume calls"
        );

        let percent = |r: &Value| {
            r.get("params")
                .and_then(|p| p.get("volume"))
                .and_then(|v| v.get("percent"))
                .and_then(Value::as_u64)
                .unwrap()
        };
        // First two are the duck (both → 25).
        assert_eq!(percent(set_vols[0]), 25);
        assert_eq!(percent(set_vols[1]), 25);
        // Last two are the restore (→ original 80 and 60, order preserved).
        assert_eq!(percent(set_vols[2]), 80);
        assert_eq!(percent(set_vols[3]), 60);
    }

    #[tokio::test]
    async fn restore_without_duck_is_noop() {
        let (addr, reqs) = spawn_fake_snapserver().await;
        let ducker = MusicDucker::new(addr, GroupSelector::Auto, 25);
        ducker.restore().await.unwrap();
        assert!(reqs.lock().await.is_empty());
    }

    #[tokio::test]
    async fn mpv_load_and_volume_roundtrip() {
        use tokio::net::UnixListener;
        let dir = std::env::temp_dir();
        let sock = dir.join(format!("ambient-mpv-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        let received = Arc::new(TokioMutex::new(Vec::<Value>::new()));
        let rx = received.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let rx = rx.clone();
                tokio::spawn(async move {
                    let (r, mut w) = stream.into_split();
                    let mut reader = BufReader::new(r);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let msg: Value = serde_json::from_str(line.trim()).unwrap();
                    rx.lock().await.push(msg);
                    // mpv interleaves an event before the reply; the client must skip it.
                    let _ = w.write_all(b"{\"event\":\"file-loaded\"}\n").await;
                    let _ = w.write_all(b"{\"error\":\"success\"}\n").await;
                });
            }
        });

        let mpv = MpvControl::new(&sock);
        mpv.load("http://example.com/stream.mp3").await.unwrap();
        mpv.set_volume(55).await.unwrap();

        let got = received.lock().await;
        assert_eq!(got.len(), 2);
        assert_eq!(got[0]["command"][0], json!("loadfile"));
        assert_eq!(got[0]["command"][1], json!("http://example.com/stream.mp3"));
        assert_eq!(got[1]["command"], json!(["set_property", "volume", 55.0]));
        let _ = std::fs::remove_file(&sock);
    }
}
