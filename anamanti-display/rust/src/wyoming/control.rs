//! Device-side client for the Phase-6 settings + memory control protocol
//! (Plan.MD §3, Phase 6; architecture.md §3–5).
//!
//! Each function resolves the orchestrator's Wyoming host over mDNS (with the
//! cached fallback, symmetric with a voice turn), opens a short-lived TCP
//! connection, sends one project-local `anamanti-*` control request, and parses the
//! single response frame. The requests carry small JSON `data` blocks and never
//! stream audio, so they reuse the plain codec directly rather than the audio-
//! oriented [`crate::wyoming::WyomingConnection`].
//!
//! The parsed results are the FRB structs in [`crate::api::settings`], so there is
//! one definition of each shape across the bridge.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Map, Value};
use tokio::io::BufReader;
use tokio::net::TcpStream;

use crate::api::settings::{
    CameraStream, DriveToken, EntityInfo, EntityRef, FunctionStatus, MemoryEntry, ModelInfo,
    OrchestratorSettings, SettingsUpdate, SpeakerClipInfo, SpeakerInfo, TurnInfo, VoiceInfo,
};

use super::discovery::{resolve, EndpointCache, WyomingEndpoint};
use super::protocol::{self, types, WyomingEvent};

/// Hard cap on a single control round trip. A responsive orchestrator answers in
/// milliseconds (a catalog fetch a few seconds); this only bounds a peer that
/// connects but never replies — e.g. an **older orchestrator** that doesn't
/// recognize a newer `anamanti-*` frame and holds the socket open. Without this a
/// control call (notably the boot-time Drive-token sync) would hang forever.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

/// One control request/response round trip over a fresh connection to `endpoint`,
/// bounded by [`CONTROL_TIMEOUT`] so a non-responsive peer can never wedge the
/// caller (e.g. the boot sequence).
async fn round_trip(endpoint: &WyomingEndpoint, request: WyomingEvent) -> Result<WyomingEvent> {
    tokio::time::timeout(CONTROL_TIMEOUT, async {
        let stream = TcpStream::connect(endpoint.socket_addr())
            .await
            .with_context(|| format!("connecting to orchestrator {endpoint}"))?;
        stream.set_nodelay(true).ok();
        let (read_half, write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut writer = write_half;
        protocol::write_event(&mut writer, &request)
            .await
            .context("sending control request")?;
        protocol::read_event(&mut reader)
            .await
            .context("reading control response")?
            .ok_or_else(|| anyhow!("orchestrator closed the connection without responding"))
    })
    .await
    .map_err(|_| {
        anyhow!("orchestrator {endpoint} did not respond within {CONTROL_TIMEOUT:?} (is it an older build?)")
    })?
}

fn opt_str(data: &Value, key: &str) -> Option<String> {
    data.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn parse_settings(ev: &WyomingEvent) -> OrchestratorSettings {
    OrchestratorSettings {
        ok: ev.data.get("ok").and_then(Value::as_bool).unwrap_or(true),
        message: ev
            .data
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        llm_backend: opt_str(&ev.data, "llm_backend").unwrap_or_default(),
        llm_model: opt_str(&ev.data, "llm_model"),
        anthropic_auth: opt_str(&ev.data, "anthropic_auth").unwrap_or_else(|| "apikey".to_string()),
        tts_voice: opt_str(&ev.data, "tts_voice"),
        end_silence_ms: ev
            .data
            .get("end_silence_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        voice_rms_threshold: ev
            .data
            .get("voice_rms_threshold")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        vad_engine: opt_str(&ev.data, "vad_engine").unwrap_or_default(),
        silero_threshold: ev
            .data
            .get("silero_threshold")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
    }
}

fn parse_model(v: &Value) -> ModelInfo {
    let id = v
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let label = v
        .get("label")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(&id)
        .to_string();
    ModelInfo {
        provider: v
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        id,
        label,
    }
}

fn parse_voice(v: &Value) -> VoiceInfo {
    let name = v
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let label = v
        .get("label")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(&name)
        .to_string();
    VoiceInfo {
        language: v
            .get("language")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        name,
        label,
    }
}

fn parse_drive_token(ev: &WyomingEvent) -> DriveToken {
    let s = |k: &str| {
        ev.data
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let folder_ids = ev
        .data
        .get("folder_ids")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    DriveToken {
        linked: ev
            .data
            .get("linked")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        configured: ev
            .data
            .get("configured")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        client_id: s("client_id"),
        client_secret: s("client_secret"),
        refresh_token: s("refresh_token"),
        folder_ids,
        scope: s("scope"),
    }
}

/// Redacting `Debug` for [`CameraStream`]: its `url` embeds the NVR credentials, so
/// a stray `{:?}` must never print it. (Lives here, outside the FRB-scanned `api`
/// module, so codegen never sees the trait impl.)
impl std::fmt::Debug for CameraStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CameraStream")
            .field("ok", &self.ok)
            .field("camera", &self.camera)
            .field(
                "url",
                &if self.url.is_empty() {
                    ""
                } else {
                    "<redacted>"
                },
            )
            .field("message", &self.message)
            .finish()
    }
}

/// Parse an `anamanti-camera-stream` reply. A success with no `url` is treated as a
/// failure (there is nothing to play), so callers only ever see `ok` with a URL.
fn parse_camera_stream(ev: &WyomingEvent) -> CameraStream {
    let s = |k: &str| {
        ev.data
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let ok = ev.data.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let url = s("url");
    if ok && !url.is_empty() {
        CameraStream {
            ok: true,
            camera: s("camera"),
            url,
            message: String::new(),
        }
    } else {
        let message = s("message");
        CameraStream {
            ok: false,
            camera: String::new(),
            url: String::new(),
            message: if message.is_empty() {
                "The camera isn't available right now.".to_string()
            } else {
                message
            },
        }
    }
}

fn parse_function(v: &Value) -> FunctionStatus {
    let s = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let b = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
    FunctionStatus {
        category: s("category"),
        name: s("name"),
        mode: s("mode"),
        provider: s("provider"),
        model: s("model"),
        key_required: b("key_required"),
        key_set: b("key_set"),
        note: s("note"),
    }
}

fn parse_entry(v: &Value) -> MemoryEntry {
    let s = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    MemoryEntry {
        id: v.get("id").and_then(Value::as_i64).unwrap_or(0),
        kind: s("kind"),
        content: s("content"),
        source: s("source"),
        created_at: v.get("created_at").and_then(Value::as_i64).unwrap_or(0),
    }
}

fn parse_turn(v: &Value) -> TurnInfo {
    let s = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let entities = v
        .get("entities")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|e| EntityRef {
                    name: e
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    kind: e
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    TurnInfo {
        id: v.get("id").and_then(Value::as_i64).unwrap_or(0),
        ext_id: s("ext_id"),
        session: s("session"),
        ts: v.get("ts").and_then(Value::as_i64).unwrap_or(0),
        transcript: s("transcript"),
        reply: s("reply"),
        speaker_id: s("speaker_id"),
        entities,
    }
}

fn parse_entity(v: &Value) -> EntityInfo {
    EntityInfo {
        id: v.get("id").and_then(Value::as_i64).unwrap_or(0),
        name: v
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        kind: v
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}

/// Fetch the orchestrator's current runtime settings.
pub async fn describe_settings(
    cache: &EndpointCache,
    timeout: Duration,
    preferred: Option<&str>,
) -> Result<OrchestratorSettings> {
    let endpoint = resolve(cache, timeout, preferred).await?;
    let resp = round_trip(&endpoint, WyomingEvent::new(types::DESCRIBE_SETTINGS)).await?;
    Ok(parse_settings(&resp))
}

/// Apply a settings change and return the resulting settings.
pub async fn update_settings(
    cache: &EndpointCache,
    timeout: Duration,
    preferred: Option<&str>,
    update: &SettingsUpdate,
) -> Result<OrchestratorSettings> {
    let endpoint = resolve(cache, timeout, preferred).await?;

    let mut data = Map::new();
    if let Some(backend) = update.llm_backend.as_deref().filter(|s| !s.is_empty()) {
        data.insert("llm_backend".into(), json!(backend));
    }
    if let Some(model) = update.llm_model.as_deref().filter(|s| !s.is_empty()) {
        data.insert("llm_model".into(), json!(model));
    }
    if let Some(auth) = update.anthropic_auth.as_deref().filter(|s| !s.is_empty()) {
        data.insert("anthropic_auth".into(), json!(auth));
    }
    if update.set_tts_voice {
        // A present `tts_voice` key drives the change: a non-empty string sets the
        // voice, `null` clears it (matching the orchestrator's parse_update).
        match update.tts_voice.as_deref().filter(|s| !s.is_empty()) {
            Some(voice) => data.insert("tts_voice".into(), json!(voice)),
            None => data.insert("tts_voice".into(), Value::Null),
        };
    }
    if let Some(ms) = update.end_silence_ms {
        data.insert("end_silence_ms".into(), json!(ms));
    }
    if let Some(thr) = update.voice_rms_threshold {
        data.insert("voice_rms_threshold".into(), json!(thr));
    }
    if let Some(engine) = update.vad_engine.as_deref().filter(|s| !s.is_empty()) {
        data.insert("vad_engine".into(), json!(engine));
    }
    if let Some(thr) = update.silero_threshold {
        data.insert("silero_threshold".into(), json!(thr));
    }

    let request = WyomingEvent::with_data(types::SET_SETTINGS, Value::Object(data));
    let resp = round_trip(&endpoint, request).await?;
    Ok(parse_settings(&resp))
}

/// List the orchestrator's selectable LLM models for the settings model dropdown.
pub async fn list_models(
    cache: &EndpointCache,
    timeout: Duration,
    preferred: Option<&str>,
) -> Result<Vec<ModelInfo>> {
    let endpoint = resolve(cache, timeout, preferred).await?;
    let resp = round_trip(&endpoint, WyomingEvent::new(types::LIST_MODELS)).await?;
    if !resp.data.get("ok").and_then(Value::as_bool).unwrap_or(true) {
        return Err(anyhow!(
            "{}",
            resp.data
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("listing models failed")
        ));
    }
    let models = resp
        .data
        .get("models")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(parse_model).collect())
        .unwrap_or_default();
    Ok(models)
}

/// List the installed Piper voices for the settings TTS voice dropdown.
pub async fn list_voices(
    cache: &EndpointCache,
    timeout: Duration,
    preferred: Option<&str>,
) -> Result<Vec<VoiceInfo>> {
    let endpoint = resolve(cache, timeout, preferred).await?;
    let resp = round_trip(&endpoint, WyomingEvent::new(types::LIST_VOICES)).await?;
    if !resp.data.get("ok").and_then(Value::as_bool).unwrap_or(true) {
        return Err(anyhow!(
            "{}",
            resp.data
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("listing voices failed")
        ));
    }
    let voices = resp
        .data
        .get("voices")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(parse_voice).collect())
        .unwrap_or_default();
    Ok(voices)
}

/// Fetch the Google Drive photo-slideshow bundle (client creds + refresh token +
/// folder ids) the orchestrator owns. The device mints Drive access tokens on-device
/// from this, so the APK ships credential-free. An unlinked orchestrator answers
/// with empty fields (`linked: false`), which the caller treats as "not configured".
pub async fn get_drive_token(
    cache: &EndpointCache,
    timeout: Duration,
    preferred: Option<&str>,
) -> Result<DriveToken> {
    let endpoint = resolve(cache, timeout, preferred).await?;
    let resp = round_trip(&endpoint, WyomingEvent::new(types::GET_DRIVE_TOKEN)).await?;
    if !resp.data.get("ok").and_then(Value::as_bool).unwrap_or(true) {
        return Err(anyhow!(
            "{}",
            resp.data
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("fetching the Drive token failed")
        ));
    }
    Ok(parse_drive_token(&resp))
}

/// Fetch the live stream URL for the camera named `camera` (blank = the Core's
/// first configured camera), each time the camera view opens (`plans/CameraPlan.md`).
/// A Core-side refusal comes back as `ok: false` with a speakable `message` (not an
/// `Err`); a wrong reply type is an `Err`. **The URL embeds the NVR credentials** —
/// only `ok`/`camera`/`message` are ever logged.
pub async fn get_camera_stream(
    cache: &EndpointCache,
    timeout: Duration,
    preferred: Option<&str>,
    camera: &str,
) -> Result<CameraStream> {
    let endpoint = resolve(cache, timeout, preferred).await?;
    let resp = round_trip(&endpoint, WyomingEvent::get_camera_stream(camera)).await?;
    if resp.event_type != types::CAMERA_STREAM {
        return Err(anyhow!(
            "unexpected reply to a camera stream request: {}",
            resp.event_type
        ));
    }
    let stream = parse_camera_stream(&resp);
    log::info!(
        "camera stream: ok={} camera={:?} message={:?}",
        stream.ok,
        stream.camera,
        stream.message
    );
    Ok(stream)
}

/// Fetch the Core's read-only "functions" snapshot (one row per configured
/// capability: cloud/local, model, and whether a required key is set). Used by the
/// device Settings screen. An `ok: false` body surfaces as an error.
pub async fn get_functions(
    cache: &EndpointCache,
    timeout: Duration,
    preferred: Option<&str>,
) -> Result<Vec<FunctionStatus>> {
    let endpoint = resolve(cache, timeout, preferred).await?;
    let resp = round_trip(&endpoint, WyomingEvent::new(types::GET_FUNCTIONS)).await?;
    if !resp.data.get("ok").and_then(Value::as_bool).unwrap_or(true) {
        return Err(anyhow!(
            "{}",
            resp.data
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("fetching the functions snapshot failed")
        ));
    }
    let functions = resp
        .data
        .get("functions")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(parse_function).collect())
        .unwrap_or_default();
    Ok(functions)
}

/// List every persistent memory entry.
pub async fn list_memories(
    cache: &EndpointCache,
    timeout: Duration,
    preferred: Option<&str>,
) -> Result<Vec<MemoryEntry>> {
    let endpoint = resolve(cache, timeout, preferred).await?;
    let resp = round_trip(&endpoint, WyomingEvent::new(types::LIST_MEMORIES)).await?;
    if !resp.data.get("ok").and_then(Value::as_bool).unwrap_or(true) {
        return Err(anyhow!(
            "{}",
            resp.data
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("listing memories failed")
        ));
    }
    let entries = resp
        .data
        .get("entries")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(parse_entry).collect())
        .unwrap_or_default();
    Ok(entries)
}

/// Delete one entry by id; returns whether a row was removed.
pub async fn delete_memory(
    cache: &EndpointCache,
    timeout: Duration,
    preferred: Option<&str>,
    id: i64,
) -> Result<bool> {
    let endpoint = resolve(cache, timeout, preferred).await?;
    let request = WyomingEvent::with_data(types::DELETE_MEMORY, json!({ "id": id }));
    let resp = round_trip(&endpoint, request).await?;
    let ok = resp
        .data
        .get("ok")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let count = resp.data.get("count").and_then(Value::as_i64).unwrap_or(0);
    Ok(ok && count > 0)
}

/// Delete every entry; returns the number removed.
pub async fn clear_memories(
    cache: &EndpointCache,
    timeout: Duration,
    preferred: Option<&str>,
) -> Result<u32> {
    let endpoint = resolve(cache, timeout, preferred).await?;
    let resp = round_trip(&endpoint, WyomingEvent::new(types::CLEAR_MEMORIES)).await?;
    Ok(resp
        .data
        .get("count")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0) as u32)
}

/// Browse stored conversation turns from the GraphRAG graph, newest first. Empty
/// filter strings are omitted from the request (treated as "no filter"). An
/// `ok: false` body (older Core, or SQLite recall backend) surfaces as an error.
pub async fn list_turns(
    cache: &EndpointCache,
    timeout: Duration,
    preferred: Option<&str>,
    query: &str,
    speaker_id: &str,
    entity: &str,
    limit: u32,
) -> Result<Vec<TurnInfo>> {
    let endpoint = resolve(cache, timeout, preferred).await?;
    let mut data = Map::new();
    if !query.trim().is_empty() {
        data.insert("query".into(), json!(query.trim()));
    }
    if !speaker_id.trim().is_empty() {
        data.insert("speaker_id".into(), json!(speaker_id.trim()));
    }
    if !entity.trim().is_empty() {
        data.insert("entity".into(), json!(entity.trim()));
    }
    if limit > 0 {
        data.insert("limit".into(), json!(limit));
    }
    let request = WyomingEvent::with_data(types::LIST_TURNS, Value::Object(data));
    let resp = round_trip(&endpoint, request).await?;
    if !resp.data.get("ok").and_then(Value::as_bool).unwrap_or(true) {
        return Err(anyhow!(
            "{}",
            resp.data
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("listing turns failed")
        ));
    }
    Ok(resp
        .data
        .get("turns")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(parse_turn).collect())
        .unwrap_or_default())
}

/// Browse stored entities / "keywords" from the GraphRAG graph. An `ok: false` body
/// (older Core, or SQLite recall backend) surfaces as an error.
pub async fn list_entities(
    cache: &EndpointCache,
    timeout: Duration,
    preferred: Option<&str>,
    query: &str,
    limit: u32,
) -> Result<Vec<EntityInfo>> {
    let endpoint = resolve(cache, timeout, preferred).await?;
    let mut data = Map::new();
    if !query.trim().is_empty() {
        data.insert("query".into(), json!(query.trim()));
    }
    if limit > 0 {
        data.insert("limit".into(), json!(limit));
    }
    let request = WyomingEvent::with_data(types::LIST_ENTITIES, Value::Object(data));
    let resp = round_trip(&endpoint, request).await?;
    if !resp.data.get("ok").and_then(Value::as_bool).unwrap_or(true) {
        return Err(anyhow!(
            "{}",
            resp.data
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("listing entities failed")
        ));
    }
    Ok(resp
        .data
        .get("entities")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(parse_entity).collect())
        .unwrap_or_default())
}

/// Drop one graph node by id (a turn or entity). Surfaces an in-band `message` as an
/// error when the Core rejected the delete.
pub async fn delete_graph_node(
    cache: &EndpointCache,
    timeout: Duration,
    preferred: Option<&str>,
    id: i64,
) -> Result<bool> {
    let endpoint = resolve(cache, timeout, preferred).await?;
    let request = WyomingEvent::with_data(types::DELETE_GRAPH_NODE, json!({ "id": id }));
    let resp = round_trip(&endpoint, request).await?;
    let ok = resp
        .data
        .get("ok")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !ok {
        if let Some(msg) = resp.data.get("message").and_then(Value::as_str) {
            if !msg.is_empty() {
                return Err(anyhow!("{msg}"));
            }
        }
    }
    Ok(ok)
}

fn parse_speaker(v: &Value) -> SpeakerInfo {
    SpeakerInfo {
        id: v
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        name: opt_str(v, "name"),
        labeled: v.get("labeled").and_then(Value::as_bool).unwrap_or(false),
        samples: v.get("samples").and_then(Value::as_i64).unwrap_or(0),
        created_at: v.get("created_at").and_then(Value::as_i64).unwrap_or(0),
    }
}

/// The `ok` flag from a `SPEAKER_RESULT`, surfacing the in-band `message` as an
/// error when the orchestrator rejected the request.
fn speaker_ok(resp: &WyomingEvent) -> Result<bool> {
    let ok = resp
        .data
        .get("ok")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !ok {
        if let Some(msg) = resp.data.get("message").and_then(Value::as_str) {
            if !msg.is_empty() {
                return Err(anyhow!("{msg}"));
            }
        }
    }
    Ok(ok)
}

/// List the identified speakers (settings "People" view).
pub async fn list_speakers(
    cache: &EndpointCache,
    timeout: Duration,
    preferred: Option<&str>,
) -> Result<Vec<SpeakerInfo>> {
    let endpoint = resolve(cache, timeout, preferred).await?;
    let resp = round_trip(&endpoint, WyomingEvent::new(types::LIST_SPEAKERS)).await?;
    if !resp.data.get("ok").and_then(Value::as_bool).unwrap_or(true) {
        return Err(anyhow!(
            "{}",
            resp.data
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("listing speakers failed")
        ));
    }
    Ok(resp
        .data
        .get("speakers")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(parse_speaker).collect())
        .unwrap_or_default())
}

/// The household roster's member names (Core → Household), for the enrollment
/// flow's "existing person" picker. Rides the `list-speakers` round trip — the Core
/// attaches `members` (names only) to that response — and is read even when speaker
/// ID is off. An older Core that sends no `members` yields an empty list.
pub async fn list_household_members(
    cache: &EndpointCache,
    timeout: Duration,
    preferred: Option<&str>,
) -> Result<Vec<String>> {
    let endpoint = resolve(cache, timeout, preferred).await?;
    let resp = round_trip(&endpoint, WyomingEvent::new(types::LIST_SPEAKERS)).await?;
    Ok(resp
        .data
        .get("members")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

/// Name (or rename) a speaker; returns whether it was applied.
pub async fn name_speaker(
    cache: &EndpointCache,
    timeout: Duration,
    preferred: Option<&str>,
    id: &str,
    name: &str,
) -> Result<bool> {
    let endpoint = resolve(cache, timeout, preferred).await?;
    let request = WyomingEvent::with_data(types::NAME_SPEAKER, json!({ "id": id, "name": name }));
    let resp = round_trip(&endpoint, request).await?;
    speaker_ok(&resp)
}

/// Link a family member's phone to AppSaid by redeeming the short pairing `code` shown
/// in their AppSaid app and storing it on the Core as a phone-message recipient under
/// `name`. Returns the Core's speakable confirmation on success; a rejected/expired
/// code (or an unreachable Core) is an `Err` carrying the reason to show the user.
pub async fn pair_phone(
    cache: &EndpointCache,
    timeout: Duration,
    preferred: Option<&str>,
    name: &str,
    code: &str,
) -> Result<String> {
    let endpoint = resolve(cache, timeout, preferred).await?;
    let request = WyomingEvent::with_data(types::PAIR_PHONE, json!({ "name": name, "code": code }));
    let resp = round_trip(&endpoint, request).await?;
    let ok = resp
        .data
        .get("ok")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let message = resp
        .data
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if ok {
        Ok(if message.is_empty() {
            "Linked.".to_string()
        } else {
            message
        })
    } else {
        Err(anyhow!(
            "{}",
            if message.is_empty() {
                "pairing failed — try a fresh code"
            } else {
                &message
            }
        ))
    }
}

/// Merge the `drop` speaker into `keep`; returns whether it was applied.
pub async fn merge_speakers(
    cache: &EndpointCache,
    timeout: Duration,
    preferred: Option<&str>,
    keep: &str,
    drop: &str,
) -> Result<bool> {
    let endpoint = resolve(cache, timeout, preferred).await?;
    let request =
        WyomingEvent::with_data(types::MERGE_SPEAKERS, json!({ "keep": keep, "drop": drop }));
    let resp = round_trip(&endpoint, request).await?;
    speaker_ok(&resp)
}

/// Link voice `id` to household member `member` on the Core: merged into that
/// member's existing voiceprint if they have one, otherwise named as them. Returns
/// the Core's confirmation message; an in-band rejection (not a household member,
/// speaker ID off) is an error carrying its reason.
pub async fn link_speaker(
    cache: &EndpointCache,
    timeout: Duration,
    preferred: Option<&str>,
    id: &str,
    member: &str,
) -> Result<String> {
    let endpoint = resolve(cache, timeout, preferred).await?;
    let request =
        WyomingEvent::with_data(types::LINK_SPEAKER, json!({ "id": id, "member": member }));
    let resp = round_trip(&endpoint, request).await?;
    ok_message(&resp, "linking failed")
}

/// Issue a music transport/volume command (`pause`/`resume`/`next`/`previous`/`volume`/
/// `skip_to`) from the display's on-screen controls. Returns the Core's short spoken
/// confirmation; an in-band failure (Spotify not linked, device asleep, unknown action) is
/// an `Err` carrying its reason. `value` is the 0–100 percent for `"volume"` and the
/// 0-based up-next index for `"skip_to"`. `device_id` (this display's id; blank = omitted) tells the Core which display pressed the button — the
/// `display_fullscreen` / `display_compact` toggle changes only that display.
pub async fn music_control(
    cache: &EndpointCache,
    timeout: Duration,
    preferred: Option<&str>,
    action: &str,
    value: Option<i64>,
    device_id: &str,
) -> Result<String> {
    let endpoint = resolve(cache, timeout, preferred).await?;
    let request = WyomingEvent::music_control(action, value).with_device_id(device_id);
    let resp = round_trip(&endpoint, request).await?;
    ok_message(&resp, "music control failed")
}

/// Delete a speaker profile; returns whether one was removed.
pub async fn delete_speaker(
    cache: &EndpointCache,
    timeout: Duration,
    preferred: Option<&str>,
    id: &str,
) -> Result<bool> {
    let endpoint = resolve(cache, timeout, preferred).await?;
    let request = WyomingEvent::with_data(types::DELETE_SPEAKER, json!({ "id": id }));
    let resp = round_trip(&endpoint, request).await?;
    speaker_ok(&resp)
}

fn parse_speaker_clip(v: &Value) -> SpeakerClipInfo {
    SpeakerClipInfo {
        name: v
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        created_ms: v.get("created_ms").and_then(Value::as_i64).unwrap_or(0),
        samples: v.get("samples").and_then(Value::as_i64).unwrap_or(0),
        sample_rate: v
            .get("sample_rate")
            .and_then(Value::as_u64)
            .unwrap_or(16_000) as u32,
    }
}

/// List a speaker's stored voice clips (newest first), for the People screen's
/// playback.
pub async fn list_speaker_clips(
    cache: &EndpointCache,
    timeout: Duration,
    preferred: Option<&str>,
    id: &str,
) -> Result<Vec<SpeakerClipInfo>> {
    let endpoint = resolve(cache, timeout, preferred).await?;
    let request = WyomingEvent::with_data(types::LIST_SPEAKER_CLIPS, json!({ "id": id }));
    let resp = round_trip(&endpoint, request).await?;
    if !resp.data.get("ok").and_then(Value::as_bool).unwrap_or(true) {
        return Err(anyhow!(
            "{}",
            resp.data
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("listing voice clips failed")
        ));
    }
    Ok(resp
        .data
        .get("clips")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(parse_speaker_clip).collect())
        .unwrap_or_default())
}

/// Fetch one clip's raw little-endian `i16` mono PCM bytes (the `payload` of the
/// `anamanti-speaker-clip` response). Errors if the Core reports failure or sends no
/// audio.
pub async fn fetch_speaker_clip(
    cache: &EndpointCache,
    timeout: Duration,
    preferred: Option<&str>,
    id: &str,
    clip: &str,
) -> Result<Vec<u8>> {
    let endpoint = resolve(cache, timeout, preferred).await?;
    let request =
        WyomingEvent::with_data(types::GET_SPEAKER_CLIP, json!({ "id": id, "clip": clip }));
    let resp = round_trip(&endpoint, request).await?;
    if !resp.data.get("ok").and_then(Value::as_bool).unwrap_or(true) {
        return Err(anyhow!(
            "{}",
            resp.data
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("fetching the voice clip failed")
        ));
    }
    resp.payload
        .ok_or_else(|| anyhow!("the orchestrator returned no audio for this clip"))
}

/// The `message` from an `ENROLL_RESULT`, surfacing a rejected request's in-band
/// message as an error (parallels [`speaker_ok`]).
fn enroll_message(resp: &WyomingEvent) -> Result<String> {
    ok_message(resp, "enrollment failed")
}

/// The `message` from an in-band `{ok, message}` response: `Ok(message)` when `ok`,
/// else an `Err` carrying the message (or `fallback` when the Core sent none).
fn ok_message(resp: &WyomingEvent, fallback: &str) -> Result<String> {
    let ok = resp
        .data
        .get("ok")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let message = resp
        .data
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    match (ok, message.is_empty()) {
        (true, _) => Ok(message),
        (false, true) => Err(anyhow!("{fallback}")),
        (false, false) => Err(anyhow!("{message}")),
    }
}

/// Commit the current guided-enrollment session into a named profile
/// (SpeakerEnrollmentPlan.md Phase 2); returns the orchestrator's confirmation
/// message, or an error carrying the in-band reason (e.g. too few samples).
pub async fn enroll_finish(
    cache: &EndpointCache,
    timeout: Duration,
    preferred: Option<&str>,
    session: &str,
    name: &str,
    speaker_id: Option<&str>,
) -> Result<String> {
    let endpoint = resolve(cache, timeout, preferred).await?;
    let mut data = json!({ "session": session, "name": name });
    if let Some(id) = speaker_id {
        data["speaker_id"] = json!(id);
    }
    let request = WyomingEvent::with_data(types::ENROLL_FINISH, data);
    let resp = round_trip(&endpoint, request).await?;
    enroll_message(&resp)
}

/// Discard an in-flight guided-enrollment session (the user backed out).
pub async fn enroll_cancel(
    cache: &EndpointCache,
    timeout: Duration,
    preferred: Option<&str>,
    session: &str,
) -> Result<()> {
    let endpoint = resolve(cache, timeout, preferred).await?;
    let request = WyomingEvent::with_data(types::ENROLL_CANCEL, json!({ "session": session }));
    round_trip(&endpoint, request).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use tokio::io::BufReader as TokioBufReader;
    use tokio::net::TcpListener;

    /// A cache pre-seeded with a loopback endpoint. `resolve(_, 0)` fails the browse
    /// immediately and falls back to this, so tests never depend on real mDNS.
    fn cache_for(addr: std::net::SocketAddr) -> EndpointCache {
        cache_for_keyed(addr, "test-orch")
    }

    fn cache_for_keyed(addr: std::net::SocketAddr, key: &str) -> EndpointCache {
        let cache = EndpointCache::new();
        cache.set(WyomingEndpoint {
            address: IpAddr::from([127, 0, 0, 1]),
            port: addr.port(),
            hostname: "localhost.".to_string(),
            key: key.to_string(),
            name: "Test Orchestrator".to_string(),
        });
        cache
    }

    /// Spawn a one-shot mock orchestrator that reads a single control request and
    /// replies with `response`, returning the request it saw.
    async fn serve_once(
        listener: TcpListener,
        response: WyomingEvent,
    ) -> tokio::task::JoinHandle<WyomingEvent> {
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, w) = stream.into_split();
            let mut reader = TokioBufReader::new(r);
            let mut writer = w;
            let request = protocol::read_event(&mut reader).await.unwrap().unwrap();
            protocol::write_event(&mut writer, &response).await.unwrap();
            request
        })
    }

    #[tokio::test]
    async fn get_drive_token_parses_the_bundle() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = WyomingEvent::with_data(
            types::DRIVE_TOKEN,
            json!({
                "ok": true,
                "linked": true,
                "configured": true,
                "client_id": "cid.apps",
                "client_secret": "gocspx-secret",
                "refresh_token": "1//refresh",
                "folder_ids": ["1AbC", "1XyZ"],
                "scope": "scope-x",
            }),
        );
        let server = serve_once(listener, response).await;

        let cache = cache_for(addr);
        let token = get_drive_token(&cache, Duration::from_millis(0), None)
            .await
            .unwrap();
        assert!(token.linked && token.configured);
        assert_eq!(token.client_id, "cid.apps");
        assert_eq!(token.client_secret, "gocspx-secret");
        assert_eq!(token.refresh_token, "1//refresh");
        assert_eq!(
            token.folder_ids,
            vec!["1AbC".to_string(), "1XyZ".to_string()]
        );
        assert_eq!(token.scope, "scope-x");

        let request = server.await.unwrap();
        assert_eq!(request.event_type, types::GET_DRIVE_TOKEN);
    }

    #[tokio::test]
    async fn get_camera_stream_sends_the_name_and_parses_the_url() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = WyomingEvent::with_data(
            types::CAMERA_STREAM,
            json!({
                "ok": true,
                "camera": "Front Door",
                "url": "rtsp://user:pass@192.168.1.29:554/h264Preview_05_sub",
            }),
        );
        let server = serve_once(listener, response).await;

        let cache = cache_for(addr);
        let stream = get_camera_stream(&cache, Duration::from_millis(0), None, "Front Door")
            .await
            .unwrap();
        assert!(stream.ok);
        assert_eq!(stream.camera, "Front Door");
        assert_eq!(
            stream.url,
            "rtsp://user:pass@192.168.1.29:554/h264Preview_05_sub"
        );
        assert!(stream.message.is_empty());

        let request = server.await.unwrap();
        assert_eq!(request.event_type, types::GET_CAMERA_STREAM);
        assert_eq!(
            request.data.get("camera").and_then(Value::as_str),
            Some("Front Door")
        );
    }

    #[tokio::test]
    async fn get_camera_stream_surfaces_an_in_band_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = WyomingEvent::with_data(
            types::CAMERA_STREAM,
            json!({ "ok": false, "message": "The camera isn't set up yet." }),
        );
        let server = serve_once(listener, response).await;

        let cache = cache_for(addr);
        let stream = get_camera_stream(&cache, Duration::from_millis(0), None, "")
            .await
            .unwrap();
        assert!(!stream.ok);
        assert!(stream.url.is_empty());
        assert_eq!(stream.message, "The camera isn't set up yet.");
        server.await.unwrap();
    }

    #[test]
    fn camera_stream_ok_without_a_url_is_a_failure() {
        let ev = WyomingEvent::with_data(
            types::CAMERA_STREAM,
            json!({ "ok": true, "camera": "Front Door" }),
        );
        let stream = parse_camera_stream(&ev);
        assert!(!stream.ok);
        assert!(!stream.message.is_empty());
    }

    #[test]
    fn camera_stream_debug_never_prints_the_url() {
        let stream = CameraStream {
            ok: true,
            camera: "Front Door".to_string(),
            url: "rtsp://user:secret@192.168.1.29:554/h264Preview_05_sub".to_string(),
            message: String::new(),
        };
        let shown = format!("{stream:?}");
        assert!(!shown.contains("secret"), "{shown}");
        assert!(!shown.contains("rtsp://"), "{shown}");
        assert!(shown.contains("<redacted>"));
        assert!(shown.contains("Front Door"));
    }

    #[tokio::test]
    async fn pair_phone_sends_name_and_code_and_returns_the_confirmation() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = WyomingEvent::with_data(
            types::PAIR_RESULT,
            json!({ "ok": true, "name": "Mom", "message": "Linked Mom's phone." }),
        );
        let server = serve_once(listener, response).await;

        let cache = cache_for(addr);
        let msg = pair_phone(&cache, Duration::from_millis(0), None, "Mom", "AB2K")
            .await
            .unwrap();
        assert_eq!(msg, "Linked Mom's phone.");

        let request = server.await.unwrap();
        assert_eq!(request.event_type, types::PAIR_PHONE);
        assert_eq!(
            request.data.get("name").and_then(Value::as_str),
            Some("Mom")
        );
        assert_eq!(
            request.data.get("code").and_then(Value::as_str),
            Some("AB2K")
        );
    }

    #[tokio::test]
    async fn pair_phone_turns_an_ok_false_into_an_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = WyomingEvent::with_data(
            types::PAIR_RESULT,
            json!({ "ok": false, "message": "that pairing code is invalid, already used, or expired" }),
        );
        let _server = serve_once(listener, response).await;

        let cache = cache_for(addr);
        let err = pair_phone(&cache, Duration::from_millis(0), None, "Mom", "ZZZZ")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("expired"), "{err}");
    }

    #[tokio::test]
    async fn list_turns_parses_turns_and_sends_filters() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = WyomingEvent::with_data(
            types::TURNS,
            json!({
                "ok": true,
                "turns": [{
                    "id": 7,
                    "ext_id": "r1",
                    "session": "sess",
                    "ts": 123,
                    "transcript": "where is the library?",
                    "reply": "On Main Street.",
                    "speaker_id": "household",
                    "entities": [{ "name": "library", "kind": "place" }],
                }],
            }),
        );
        let server = serve_once(listener, response).await;

        let cache = cache_for(addr);
        let turns = list_turns(
            &cache,
            Duration::from_millis(0),
            None,
            "library",
            "household",
            "library",
            50,
        )
        .await
        .unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].id, 7);
        assert_eq!(turns[0].transcript, "where is the library?");
        assert_eq!(turns[0].reply, "On Main Street.");
        assert_eq!(turns[0].entities.len(), 1);
        assert_eq!(turns[0].entities[0].name, "library");
        assert_eq!(turns[0].entities[0].kind, "place");

        let request = server.await.unwrap();
        assert_eq!(request.event_type, types::LIST_TURNS);
        assert_eq!(
            request.data.get("query").and_then(Value::as_str),
            Some("library")
        );
        assert_eq!(
            request.data.get("speaker_id").and_then(Value::as_str),
            Some("household")
        );
        assert_eq!(
            request.data.get("entity").and_then(Value::as_str),
            Some("library")
        );
        assert_eq!(request.data.get("limit").and_then(Value::as_u64), Some(50));
    }

    #[tokio::test]
    async fn list_turns_surfaces_an_error_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = WyomingEvent::with_data(
            types::TURNS,
            json!({ "ok": false, "message": "graph memory is unavailable", "turns": [] }),
        );
        let _server = serve_once(listener, response).await;

        let cache = cache_for(addr);
        let err = list_turns(&cache, Duration::from_millis(0), None, "", "", "", 0)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unavailable"));
    }

    #[tokio::test]
    async fn list_entities_parses_entities() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = WyomingEvent::with_data(
            types::ENTITIES,
            json!({
                "ok": true,
                "entities": [
                    { "id": 1, "name": "jazz", "kind": "topic" },
                    { "id": 2, "name": "Portland", "kind": "place" },
                ],
            }),
        );
        let server = serve_once(listener, response).await;

        let cache = cache_for(addr);
        let entities = list_entities(&cache, Duration::from_millis(0), None, "", 0)
            .await
            .unwrap();
        assert_eq!(entities.len(), 2);
        assert_eq!(entities[0].name, "jazz");
        assert_eq!(entities[1].kind, "place");

        let request = server.await.unwrap();
        assert_eq!(request.event_type, types::LIST_ENTITIES);
    }

    #[tokio::test]
    async fn delete_graph_node_sends_id_and_reads_ok() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = WyomingEvent::with_data(types::GRAPH_RESULT, json!({ "ok": true }));
        let server = serve_once(listener, response).await;

        let cache = cache_for(addr);
        let ok = delete_graph_node(&cache, Duration::from_millis(0), None, 42)
            .await
            .unwrap();
        assert!(ok);

        let request = server.await.unwrap();
        assert_eq!(request.event_type, types::DELETE_GRAPH_NODE);
        assert_eq!(request.data.get("id").and_then(Value::as_i64), Some(42));
    }

    #[tokio::test]
    async fn describe_parses_settings_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = WyomingEvent::with_data(
            types::SETTINGS,
            json!({
                "ok": true,
                "message": "current settings",
                "llm_backend": "ollama",
                "llm_model": "llama3.2",
                "tts_voice": null,
            }),
        );
        let server = serve_once(listener, response).await;

        let cache = cache_for(addr);
        let settings = describe_settings(&cache, Duration::from_millis(0), None)
            .await
            .unwrap();
        assert!(settings.ok);
        assert_eq!(settings.llm_backend, "ollama");
        assert_eq!(settings.llm_model.as_deref(), Some("llama3.2"));
        assert_eq!(settings.tts_voice, None);

        let request = server.await.unwrap();
        assert_eq!(request.event_type, types::DESCRIBE_SETTINGS);
    }

    #[tokio::test]
    async fn update_sends_only_the_changed_fields() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = WyomingEvent::with_data(
            types::SETTINGS,
            json!({ "ok": true, "message": "applied", "llm_backend": "ollama",
                    "llm_model": "llama3.2", "tts_voice": "en_US-amy-medium" }),
        );
        let server = serve_once(listener, response).await;

        let cache = cache_for(addr);
        let update = SettingsUpdate {
            llm_backend: None,
            llm_model: None,
            anthropic_auth: None,
            set_tts_voice: true,
            tts_voice: Some("en_US-amy-medium".to_string()),
            end_silence_ms: None,
            voice_rms_threshold: None,
            vad_engine: None,
            silero_threshold: None,
        };
        let out = update_settings(&cache, Duration::from_millis(0), None, &update)
            .await
            .unwrap();
        assert_eq!(out.tts_voice.as_deref(), Some("en_US-amy-medium"));

        let request = server.await.unwrap();
        assert_eq!(request.event_type, types::SET_SETTINGS);
        // Only tts_voice was requested; the backend/model keys are absent.
        assert!(request.data.get("llm_backend").is_none());
        assert!(request.data.get("llm_model").is_none());
        assert_eq!(request.data["tts_voice"], json!("en_US-amy-medium"));
    }

    #[tokio::test]
    async fn update_sends_anthropic_auth_mode() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = WyomingEvent::with_data(
            types::SETTINGS,
            json!({ "ok": true, "llm_backend": "anthropic", "anthropic_auth": "subscription" }),
        );
        let server = serve_once(listener, response).await;

        let cache = cache_for(addr);
        let update = SettingsUpdate {
            llm_backend: Some("anthropic".to_string()),
            llm_model: None,
            anthropic_auth: Some("subscription".to_string()),
            set_tts_voice: false,
            tts_voice: None,
            end_silence_ms: None,
            voice_rms_threshold: None,
            vad_engine: None,
            silero_threshold: None,
        };
        let out = update_settings(&cache, Duration::from_millis(0), None, &update)
            .await
            .unwrap();
        assert_eq!(out.anthropic_auth, "subscription");

        let request = server.await.unwrap();
        assert_eq!(request.data["anthropic_auth"], json!("subscription"));
    }

    #[tokio::test]
    async fn describe_parses_and_update_sends_vad_fields() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = WyomingEvent::with_data(
            types::SETTINGS,
            json!({ "ok": true, "llm_backend": "ollama",
                    "end_silence_ms": 550, "voice_rms_threshold": 80.0,
                    "vad_engine": "silero", "silero_threshold": 0.4 }),
        );
        let server = serve_once(listener, response).await;

        let cache = cache_for(addr);
        let update = SettingsUpdate {
            llm_backend: None,
            llm_model: None,
            anthropic_auth: None,
            set_tts_voice: false,
            tts_voice: None,
            end_silence_ms: Some(550),
            voice_rms_threshold: Some(80.0),
            vad_engine: Some("silero".to_string()),
            silero_threshold: Some(0.4),
        };
        let out = update_settings(&cache, Duration::from_millis(0), None, &update)
            .await
            .unwrap();
        // Response parsing surfaces the VAD values to the settings screen.
        assert_eq!(out.end_silence_ms, 550);
        assert_eq!(out.voice_rms_threshold, 80.0);
        assert_eq!(out.vad_engine, "silero");
        assert_eq!(out.silero_threshold, 0.4);

        // The request carried the VAD keys the orchestrator's parse_update reads.
        let request = server.await.unwrap();
        assert_eq!(request.data["end_silence_ms"], json!(550));
        assert_eq!(request.data["voice_rms_threshold"], json!(80.0));
        assert_eq!(request.data["vad_engine"], json!("silero"));
        assert_eq!(request.data["silero_threshold"], json!(0.4));
    }

    #[tokio::test]
    async fn clearing_tts_voice_sends_explicit_null() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = WyomingEvent::with_data(
            types::SETTINGS,
            json!({ "ok": true, "llm_backend": "ollama", "tts_voice": null }),
        );
        let server = serve_once(listener, response).await;

        let cache = cache_for(addr);
        let update = SettingsUpdate {
            llm_backend: None,
            llm_model: None,
            anthropic_auth: None,
            set_tts_voice: true,
            tts_voice: None,
            end_silence_ms: None,
            voice_rms_threshold: None,
            vad_engine: None,
            silero_threshold: None,
        };
        update_settings(&cache, Duration::from_millis(0), None, &update)
            .await
            .unwrap();

        let request = server.await.unwrap();
        assert_eq!(request.data["tts_voice"], Value::Null);
    }

    #[tokio::test]
    async fn list_parses_entries() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = WyomingEvent::with_data(
            types::MEMORIES,
            json!({ "ok": true, "entries": [
                { "id": 2, "kind": "preference", "content": "likes jazz", "source": "inferred", "created_at": 100 },
                { "id": 1, "kind": "fact", "content": "name is Sam", "source": "explicit", "created_at": 90 },
            ]}),
        );
        let server = serve_once(listener, response).await;

        let cache = cache_for(addr);
        let entries = list_memories(&cache, Duration::from_millis(0), None)
            .await
            .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, 2);
        assert_eq!(entries[0].kind, "preference");
        assert_eq!(entries[1].content, "name is Sam");

        server.await.unwrap();
    }

    #[tokio::test]
    async fn list_models_parses_catalog() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = WyomingEvent::with_data(
            types::MODELS,
            json!({ "ok": true, "models": [
                { "provider": "anthropic", "id": "claude-opus-5", "label": "Claude Opus 5" },
                { "provider": "openai", "id": "gpt-4o-mini", "label": "gpt-4o-mini" },
            ]}),
        );
        let server = serve_once(listener, response).await;

        let cache = cache_for(addr);
        let models = list_models(&cache, Duration::from_millis(0), None)
            .await
            .unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].provider, "anthropic");
        assert_eq!(models[0].id, "claude-opus-5");
        assert_eq!(models[0].label, "Claude Opus 5");
        assert_eq!(models[1].provider, "openai");

        let request = server.await.unwrap();
        assert_eq!(request.event_type, types::LIST_MODELS);
    }

    #[tokio::test]
    async fn list_voices_parses_catalog() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = WyomingEvent::with_data(
            types::VOICES,
            json!({ "ok": true, "voices": [
                { "name": "en_US-amy-medium", "language": "en_US", "label": "amy (medium)" },
                { "name": "en_US-lessac-medium", "language": "en_US", "label": "lessac (medium)" },
                { "name": "bare" }, // no language/label → language None, label falls back to name
            ]}),
        );
        let server = serve_once(listener, response).await;

        let cache = cache_for(addr);
        let voices = list_voices(&cache, Duration::from_millis(0), None)
            .await
            .unwrap();
        assert_eq!(voices.len(), 3);
        assert_eq!(voices[0].name, "en_US-amy-medium");
        assert_eq!(voices[0].language.as_deref(), Some("en_US"));
        assert_eq!(voices[0].label, "amy (medium)");
        assert_eq!(voices[2].name, "bare");
        assert_eq!(voices[2].language, None);
        assert_eq!(voices[2].label, "bare");

        let request = server.await.unwrap();
        assert_eq!(request.event_type, types::LIST_VOICES);
    }

    #[tokio::test]
    async fn list_voices_surfaces_in_band_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = WyomingEvent::with_data(
            types::VOICES,
            json!({ "ok": false, "message": "piper unreachable", "voices": [] }),
        );
        serve_once(listener, response).await;

        let cache = cache_for(addr);
        let err = list_voices(&cache, Duration::from_millis(0), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("piper unreachable"));
    }

    #[tokio::test]
    async fn delete_reports_removal() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response =
            WyomingEvent::with_data(types::MEMORY_RESULT, json!({ "ok": true, "count": 1 }));
        let server = serve_once(listener, response).await;

        let cache = cache_for(addr);
        let removed = delete_memory(&cache, Duration::from_millis(0), None, 7)
            .await
            .unwrap();
        assert!(removed);

        let request = server.await.unwrap();
        assert_eq!(request.event_type, types::DELETE_MEMORY);
        assert_eq!(request.data["id"], json!(7));
    }

    #[tokio::test]
    async fn list_speakers_parses_people() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = WyomingEvent::with_data(
            types::SPEAKERS,
            json!({ "ok": true, "speakers": [
                { "id": "spk-1", "name": "Sam", "labeled": true, "samples": 4, "created_at": 100 },
                { "id": "spk-2", "name": null, "labeled": false, "samples": 1, "created_at": 200 },
            ]}),
        );
        let server = serve_once(listener, response).await;

        let cache = cache_for(addr);
        let people = list_speakers(&cache, Duration::from_millis(0), None)
            .await
            .unwrap();
        assert_eq!(people.len(), 2);
        assert_eq!(people[0].id, "spk-1");
        assert_eq!(people[0].name.as_deref(), Some("Sam"));
        assert!(people[0].labeled);
        assert_eq!(people[1].name, None);

        let request = server.await.unwrap();
        assert_eq!(request.event_type, types::LIST_SPEAKERS);
    }

    #[tokio::test]
    async fn list_household_members_reads_roster_names() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Speaker ID off on the Core still lists the roster.
        let response = WyomingEvent::with_data(
            types::SPEAKERS,
            json!({ "ok": false, "message": "disabled", "speakers": [],
                    "members": ["Dana", "  ", "Sam"] }),
        );
        let server = serve_once(listener, response).await;

        let cache = cache_for(addr);
        let members = list_household_members(&cache, Duration::from_millis(0), None)
            .await
            .unwrap();
        assert_eq!(members, vec!["Dana".to_string(), "Sam".to_string()]);
        assert_eq!(server.await.unwrap().event_type, types::LIST_SPEAKERS);
    }

    #[tokio::test]
    async fn enroll_finish_targets_a_speaker_id_when_given() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = WyomingEvent::with_data(
            types::ENROLL_RESULT,
            json!({ "ok": true, "phase": "finish", "message": "added 3 samples to Sam's voiceprint" }),
        );
        let server = serve_once(listener, response).await;

        let cache = cache_for(addr);
        let msg = enroll_finish(
            &cache,
            Duration::from_millis(0),
            None,
            "s",
            "Sam",
            Some("spk-1"),
        )
        .await
        .unwrap();
        assert_eq!(msg, "added 3 samples to Sam's voiceprint");
        let request = server.await.unwrap();
        assert_eq!(request.event_type, types::ENROLL_FINISH);
        assert_eq!(request.data["speaker_id"], json!("spk-1"));
        assert_eq!(request.data["session"], json!("s"));
    }

    #[tokio::test]
    async fn link_speaker_sends_id_and_member_and_surfaces_rejection() {
        for (resp, want_ok) in [
            (
                json!({ "ok": true, "message": "merged into Christina (2 memories moved)",
                     "speaker_id": "spk-1", "merged": true }),
                true,
            ),
            (
                json!({ "ok": false, "message": "Zed isn't in the household" }),
                false,
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = serve_once(
                listener,
                WyomingEvent::with_data(types::SPEAKER_RESULT, resp),
            )
            .await;
            let cache = cache_for(addr);
            let out =
                link_speaker(&cache, Duration::from_millis(0), None, "spk-2", "Christina").await;
            if want_ok {
                assert_eq!(out.unwrap(), "merged into Christina (2 memories moved)");
            } else {
                assert_eq!(out.unwrap_err().to_string(), "Zed isn't in the household");
            }
            let request = server.await.unwrap();
            assert_eq!(request.event_type, types::LINK_SPEAKER);
            assert_eq!(request.data["id"], json!("spk-2"));
            assert_eq!(request.data["member"], json!("Christina"));
        }
    }

    #[tokio::test]
    async fn name_speaker_sends_id_and_name() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = WyomingEvent::with_data(
            types::SPEAKER_RESULT,
            json!({ "ok": true, "message": "named" }),
        );
        let server = serve_once(listener, response).await;

        let cache = cache_for(addr);
        let ok = name_speaker(&cache, Duration::from_millis(0), None, "spk-2", "Dana")
            .await
            .unwrap();
        assert!(ok);

        let request = server.await.unwrap();
        assert_eq!(request.event_type, types::NAME_SPEAKER);
        assert_eq!(request.data["id"], json!("spk-2"));
        assert_eq!(request.data["name"], json!("Dana"));
    }

    #[tokio::test]
    async fn music_control_sends_action_and_value_and_returns_message() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = WyomingEvent::music_result(true, "Set the volume to 40 percent.");
        let server = serve_once(listener, response).await;

        let cache = cache_for(addr);
        let msg = music_control(
            &cache,
            Duration::from_millis(0),
            None,
            "volume",
            Some(40),
            "den",
        )
        .await
        .unwrap();
        assert_eq!(msg, "Set the volume to 40 percent.");

        let request = server.await.unwrap();
        assert_eq!(request.event_type, types::MUSIC_CONTROL);
        assert_eq!(request.data["action"], json!("volume"));
        assert_eq!(request.data["value"], json!(40));
        assert_eq!(request.data["device_id"], json!("den"));
    }

    #[tokio::test]
    async fn music_control_surfaces_in_band_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response =
            WyomingEvent::music_result(false, "Spotify isn't linked on the Anamanti Core");
        serve_once(listener, response).await;

        let cache = cache_for(addr);
        let err = music_control(&cache, Duration::from_millis(0), None, "pause", None, "")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("linked"));
    }

    #[tokio::test]
    async fn speaker_action_surfaces_in_band_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = WyomingEvent::with_data(
            types::SPEAKER_RESULT,
            json!({ "ok": false, "message": "no such speaker" }),
        );
        serve_once(listener, response).await;

        let cache = cache_for(addr);
        let err = delete_speaker(&cache, Duration::from_millis(0), None, "nope")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no such speaker"));
    }

    #[tokio::test]
    async fn resolve_failure_surfaces_as_error() {
        // Empty cache + 0 s browse → resolve fails, so the call errors rather than
        // hanging or panicking.
        let cache = EndpointCache::new();
        let err = describe_settings(&cache, Duration::from_millis(0), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no cached endpoint"));
    }

    #[tokio::test]
    async fn preferred_key_mismatch_never_round_trips_to_the_wrong_host() {
        // A cache seeded with orchestrator "mac-a" must NOT be used to satisfy a
        // control call pinned to "mac-b": strict selection means the settings path
        // stays offline rather than talking to a different Mac.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cache = cache_for_keyed(addr, "mac-a");
        let err = describe_settings(&cache, Duration::from_millis(0), Some("mac-b"))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("mac-b"),
            "error names the selected orchestrator, not the cached one: {err}"
        );
    }
}
