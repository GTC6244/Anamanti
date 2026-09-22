//! Device-side client for the Phase-6 settings + memory control protocol
//! (Plan.MD §3, Phase 6; architecture.md §3–5).
//!
//! Each function resolves the orchestrator's Wyoming host over mDNS (with the
//! cached fallback, symmetric with a voice turn), opens a short-lived TCP
//! connection, sends one project-local `ambient-*` control request, and parses the
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
    DriveToken, MemoryEntry, ModelInfo, OrchestratorSettings, SettingsUpdate, SpeakerInfo,
    VoiceInfo,
};

use super::discovery::{resolve, EndpointCache, WyomingEndpoint};
use super::protocol::{self, types, WyomingEvent};

/// Hard cap on a single control round trip. A responsive orchestrator answers in
/// milliseconds (a catalog fetch a few seconds); this only bounds a peer that
/// connects but never replies — e.g. an **older orchestrator** that doesn't
/// recognize a newer `ambient-*` frame and holds the socket open. Without this a
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
        linked: ev.data.get("linked").and_then(Value::as_bool).unwrap_or(false),
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
    let ok = resp.data.get("ok").and_then(Value::as_bool).unwrap_or(false);
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
        assert_eq!(token.folder_ids, vec!["1AbC".to_string(), "1XyZ".to_string()]);
        assert_eq!(token.scope, "scope-x");

        let request = server.await.unwrap();
        assert_eq!(request.event_type, types::GET_DRIVE_TOKEN);
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
                    "end_silence_ms": 550, "voice_rms_threshold": 80.0 }),
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
        };
        let out = update_settings(&cache, Duration::from_millis(0), None, &update)
            .await
            .unwrap();
        // Response parsing surfaces the VAD values to the settings screen.
        assert_eq!(out.end_silence_ms, 550);
        assert_eq!(out.voice_rms_threshold, 80.0);

        // The request carried the VAD keys the orchestrator's parse_update reads.
        let request = server.await.unwrap();
        assert_eq!(request.data["end_silence_ms"], json!(550));
        assert_eq!(request.data["voice_rms_threshold"], json!(80.0));
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
        let models = list_models(&cache, Duration::from_millis(0), None).await.unwrap();
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
        let voices = list_voices(&cache, Duration::from_millis(0), None).await.unwrap();
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
        let err = list_voices(&cache, Duration::from_millis(0), None).await.unwrap_err();
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
        let people = list_speakers(&cache, Duration::from_millis(0), None).await.unwrap();
        assert_eq!(people.len(), 2);
        assert_eq!(people[0].id, "spk-1");
        assert_eq!(people[0].name.as_deref(), Some("Sam"));
        assert!(people[0].labeled);
        assert_eq!(people[1].name, None);

        let request = server.await.unwrap();
        assert_eq!(request.event_type, types::LIST_SPEAKERS);
    }

    #[tokio::test]
    async fn name_speaker_sends_id_and_name() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response =
            WyomingEvent::with_data(types::SPEAKER_RESULT, json!({ "ok": true, "message": "named" }));
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
