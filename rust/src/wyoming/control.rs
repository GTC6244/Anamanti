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

use crate::api::settings::{MemoryEntry, OrchestratorSettings, SettingsUpdate, SpeakerInfo};

use super::discovery::{resolve, EndpointCache, WyomingEndpoint};
use super::protocol::{self, types, WyomingEvent};

/// One control request/response round trip over a fresh connection to `endpoint`.
async fn round_trip(endpoint: &WyomingEndpoint, request: WyomingEvent) -> Result<WyomingEvent> {
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
) -> Result<OrchestratorSettings> {
    let endpoint = resolve(cache, timeout).await?;
    let resp = round_trip(&endpoint, WyomingEvent::new(types::DESCRIBE_SETTINGS)).await?;
    Ok(parse_settings(&resp))
}

/// Apply a settings change and return the resulting settings.
pub async fn update_settings(
    cache: &EndpointCache,
    timeout: Duration,
    update: &SettingsUpdate,
) -> Result<OrchestratorSettings> {
    let endpoint = resolve(cache, timeout).await?;

    let mut data = Map::new();
    if let Some(backend) = update.llm_backend.as_deref().filter(|s| !s.is_empty()) {
        data.insert("llm_backend".into(), json!(backend));
    }
    if let Some(model) = update.llm_model.as_deref().filter(|s| !s.is_empty()) {
        data.insert("llm_model".into(), json!(model));
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

/// List every persistent memory entry.
pub async fn list_memories(cache: &EndpointCache, timeout: Duration) -> Result<Vec<MemoryEntry>> {
    let endpoint = resolve(cache, timeout).await?;
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
pub async fn delete_memory(cache: &EndpointCache, timeout: Duration, id: i64) -> Result<bool> {
    let endpoint = resolve(cache, timeout).await?;
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
pub async fn clear_memories(cache: &EndpointCache, timeout: Duration) -> Result<u32> {
    let endpoint = resolve(cache, timeout).await?;
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
pub async fn list_speakers(cache: &EndpointCache, timeout: Duration) -> Result<Vec<SpeakerInfo>> {
    let endpoint = resolve(cache, timeout).await?;
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
    id: &str,
    name: &str,
) -> Result<bool> {
    let endpoint = resolve(cache, timeout).await?;
    let request = WyomingEvent::with_data(types::NAME_SPEAKER, json!({ "id": id, "name": name }));
    let resp = round_trip(&endpoint, request).await?;
    speaker_ok(&resp)
}

/// Merge the `drop` speaker into `keep`; returns whether it was applied.
pub async fn merge_speakers(
    cache: &EndpointCache,
    timeout: Duration,
    keep: &str,
    drop: &str,
) -> Result<bool> {
    let endpoint = resolve(cache, timeout).await?;
    let request =
        WyomingEvent::with_data(types::MERGE_SPEAKERS, json!({ "keep": keep, "drop": drop }));
    let resp = round_trip(&endpoint, request).await?;
    speaker_ok(&resp)
}

/// Delete a speaker profile; returns whether one was removed.
pub async fn delete_speaker(cache: &EndpointCache, timeout: Duration, id: &str) -> Result<bool> {
    let endpoint = resolve(cache, timeout).await?;
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
        let cache = EndpointCache::new();
        cache.set(WyomingEndpoint {
            address: IpAddr::from([127, 0, 0, 1]),
            port: addr.port(),
            hostname: "localhost.".to_string(),
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
        let settings = describe_settings(&cache, Duration::from_millis(0))
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
            set_tts_voice: true,
            tts_voice: Some("en_US-amy-medium".to_string()),
            end_silence_ms: None,
            voice_rms_threshold: None,
        };
        let out = update_settings(&cache, Duration::from_millis(0), &update)
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
            set_tts_voice: false,
            tts_voice: None,
            end_silence_ms: Some(550),
            voice_rms_threshold: Some(80.0),
        };
        let out = update_settings(&cache, Duration::from_millis(0), &update)
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
            set_tts_voice: true,
            tts_voice: None,
            end_silence_ms: None,
            voice_rms_threshold: None,
        };
        update_settings(&cache, Duration::from_millis(0), &update)
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
        let entries = list_memories(&cache, Duration::from_millis(0))
            .await
            .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, 2);
        assert_eq!(entries[0].kind, "preference");
        assert_eq!(entries[1].content, "name is Sam");

        server.await.unwrap();
    }

    #[tokio::test]
    async fn delete_reports_removal() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response =
            WyomingEvent::with_data(types::MEMORY_RESULT, json!({ "ok": true, "count": 1 }));
        let server = serve_once(listener, response).await;

        let cache = cache_for(addr);
        let removed = delete_memory(&cache, Duration::from_millis(0), 7)
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
        let people = list_speakers(&cache, Duration::from_millis(0)).await.unwrap();
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
        let ok = name_speaker(&cache, Duration::from_millis(0), "spk-2", "Dana")
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
        let err = delete_speaker(&cache, Duration::from_millis(0), "nope")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no such speaker"));
    }

    #[tokio::test]
    async fn resolve_failure_surfaces_as_error() {
        // Empty cache + 0 s browse → resolve fails, so the call errors rather than
        // hanging or panicking.
        let cache = EndpointCache::new();
        let err = describe_settings(&cache, Duration::from_millis(0))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no cached endpoint"));
    }
}
