//! Project-local settings + memory control handler (Plan.MD Phase 6).
//!
//! The on-device settings screen manages the orchestrator's runtime LLM backend +
//! TTS voice and the persistent memory list. Those requests ride the same Wyoming
//! framing as a voice turn (device↔orchestrator hop only — see
//! `wyoming::protocol` `ambient-*` types) so the device reuses one discovered
//! endpoint and one codec. This module maps a control **request** event to a
//! **response** event; the server ([`crate::server`]) routes control frames here
//! and streams the response back.
//!
//! [`respond`] is a pure function of `(request, memory, settings)` so the whole
//! control surface is unit-testable without a socket.

use anyhow::Result;
use serde_json::{json, Value};

use crate::memory::MemoryStore;
use crate::settings::{SettingsUpdate, SharedSettings};
use crate::wyoming::protocol::{types, WyomingEvent};
use crate::wyoming::DynConnection;

/// Whether an incoming event is a Phase-6 control request the orchestrator should
/// answer (as opposed to the start of a voice turn).
pub fn is_control_request(event_type: &str) -> bool {
    matches!(
        event_type,
        types::DESCRIBE_SETTINGS
            | types::SET_SETTINGS
            | types::LIST_MEMORIES
            | types::DELETE_MEMORY
            | types::CLEAR_MEMORIES
    )
}

/// Handle one control request over `device`: build the response and send it.
pub async fn handle_control(
    device: &mut DynConnection,
    request: &WyomingEvent,
    memory: &MemoryStore,
    settings: &SharedSettings,
) -> Result<()> {
    let response = respond(request, memory, settings);
    device.send(&response).await
}

/// Build the response event for a control `request`. Never fails: a bad request or
/// a rejected settings change is reported in the response's `ok`/`message` fields
/// rather than raised, so a control error never drops the device connection.
pub fn respond(
    request: &WyomingEvent,
    memory: &MemoryStore,
    settings: &SharedSettings,
) -> WyomingEvent {
    match request.event_type.as_str() {
        types::DESCRIBE_SETTINGS => settings_response(settings, true, "current settings"),
        types::SET_SETTINGS => match settings.apply(&parse_update(&request.data)) {
            Ok(_) => settings_response(settings, true, "settings applied"),
            Err(e) => settings_response(settings, false, &format!("{e:#}")),
        },
        types::LIST_MEMORIES => memories_response(memory),
        types::DELETE_MEMORY => {
            let id = request.data.get("id").and_then(Value::as_i64);
            match id {
                Some(id) => match memory.delete(id) {
                    Ok(removed) => memory_result(removed, usize::from(removed)),
                    Err(e) => memory_error(&format!("{e:#}")),
                },
                None => memory_error("delete-memory request missing integer `id`"),
            }
        }
        types::CLEAR_MEMORIES => match memory.clear() {
            Ok(n) => memory_result(true, n),
            Err(e) => memory_error(&format!("{e:#}")),
        },
        other => memory_error(&format!("unknown control request `{other}`")),
    }
}

/// Parse a `SettingsUpdate` from the `ambient-set-settings` data block. An absent
/// key leaves that setting unchanged; a present `tts_voice: null` clears the voice.
fn parse_update(data: &Value) -> SettingsUpdate {
    let string_field = |key: &str| {
        data.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let tts_voice = match data.get("tts_voice") {
        None => None,                    // unchanged
        Some(Value::Null) => Some(None), // clear
        Some(v) => Some(v.as_str().filter(|s| !s.is_empty()).map(str::to_string)),
    };
    SettingsUpdate {
        llm_backend: string_field("llm_backend"),
        llm_model: string_field("llm_model"),
        tts_voice,
    }
}

fn settings_response(settings: &SharedSettings, ok: bool, message: &str) -> WyomingEvent {
    let v = settings.view();
    WyomingEvent::with_data(
        types::SETTINGS,
        json!({
            "ok": ok,
            "message": message,
            "llm_backend": v.llm_backend,
            "llm_model": v.llm_model,
            "tts_voice": v.tts_voice,
        }),
    )
}

fn memories_response(memory: &MemoryStore) -> WyomingEvent {
    match memory.list() {
        Ok(entries) => {
            let entries: Vec<Value> = entries
                .into_iter()
                .map(|m| {
                    json!({
                        "id": m.id,
                        "kind": m.kind.as_str(),
                        "content": m.content,
                        "source": m.source.as_str(),
                        "created_at": m.created_at,
                    })
                })
                .collect();
            WyomingEvent::with_data(types::MEMORIES, json!({ "ok": true, "entries": entries }))
        }
        Err(e) => WyomingEvent::with_data(
            types::MEMORIES,
            json!({ "ok": false, "message": format!("{e:#}"), "entries": [] }),
        ),
    }
}

fn memory_result(ok: bool, count: usize) -> WyomingEvent {
    WyomingEvent::with_data(
        types::MEMORY_RESULT,
        json!({ "ok": ok, "count": count as i64 }),
    )
}

fn memory_error(message: &str) -> WyomingEvent {
    WyomingEvent::with_data(
        types::MEMORY_RESULT,
        json!({ "ok": false, "count": 0, "message": message }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryKind, MemorySource, MemoryStore};
    use crate::settings::{RuntimeSettings, SharedSettings};
    use std::sync::Arc;

    fn settings() -> Arc<SharedSettings> {
        SharedSettings::fixed(Arc::new(crate::llm::mock::MockLlm::default()), "mock", None)
    }

    fn req(event_type: &str, data: Value) -> WyomingEvent {
        WyomingEvent::with_data(event_type, data)
    }

    #[test]
    fn describe_reports_current_settings() {
        let s = SharedSettings::new(
            crate::settings::LlmFactory {
                engine: crate::settings::LlmEngine::Native,
                web_search: false,
                ollama_url: "http://x".into(),
                anthropic_base_url: "http://y".into(),
                anthropic_api_key: None,
                anthropic_max_tokens: 10,
            },
            RuntimeSettings {
                llm: Arc::new(crate::llm::mock::MockLlm::default()),
                llm_backend: "ollama".into(),
                llm_model: Some("llama3.2".into()),
                tts_voice: Some("amy".into()),
            },
        );
        let mem = MemoryStore::open_in_memory().unwrap();
        let resp = respond(&WyomingEvent::new(types::DESCRIBE_SETTINGS), &mem, &s);
        assert_eq!(resp.event_type, types::SETTINGS);
        assert_eq!(resp.data["ok"], json!(true));
        assert_eq!(resp.data["llm_backend"], json!("ollama"));
        assert_eq!(resp.data["llm_model"], json!("llama3.2"));
        assert_eq!(resp.data["tts_voice"], json!("amy"));
    }

    #[test]
    fn set_settings_applies_tts_voice() {
        let s = settings();
        let mem = MemoryStore::open_in_memory().unwrap();
        let resp = respond(
            &req(
                types::SET_SETTINGS,
                json!({ "tts_voice": "en_US-amy-medium" }),
            ),
            &mem,
            &s,
        );
        assert_eq!(resp.data["ok"], json!(true));
        assert_eq!(resp.data["tts_voice"], json!("en_US-amy-medium"));
        assert_eq!(s.view().tts_voice.as_deref(), Some("en_US-amy-medium"));
    }

    #[test]
    fn set_settings_reports_rejected_backend_without_dropping() {
        let s = settings();
        let mem = MemoryStore::open_in_memory().unwrap();
        // `fixed` has no anthropic key, so selecting it must be rejected in-band.
        let resp = respond(
            &req(types::SET_SETTINGS, json!({ "llm_backend": "anthropic" })),
            &mem,
            &s,
        );
        assert_eq!(resp.event_type, types::SETTINGS);
        assert_eq!(resp.data["ok"], json!(false));
        assert!(resp.data["message"]
            .as_str()
            .unwrap()
            .contains("ANTHROPIC_API_KEY"));
        assert_eq!(
            s.view().llm_backend,
            "mock",
            "settings unchanged after reject"
        );
    }

    #[test]
    fn list_delete_and_clear_memories() {
        let s = settings();
        let mem = MemoryStore::open_in_memory().unwrap();
        let id = mem
            .add(
                MemoryKind::Fact,
                "the user likes tea",
                MemorySource::Explicit,
            )
            .unwrap();
        mem.add(MemoryKind::Preference, "likes jazz", MemorySource::Inferred)
            .unwrap();

        let listed = respond(&WyomingEvent::new(types::LIST_MEMORIES), &mem, &s);
        assert_eq!(listed.event_type, types::MEMORIES);
        assert_eq!(listed.data["entries"].as_array().unwrap().len(), 2);
        assert_eq!(
            listed.data["entries"][1]["content"],
            json!("the user likes tea")
        );

        let deleted = respond(&req(types::DELETE_MEMORY, json!({ "id": id })), &mem, &s);
        assert_eq!(deleted.event_type, types::MEMORY_RESULT);
        assert_eq!(deleted.data["ok"], json!(true));
        assert_eq!(deleted.data["count"], json!(1));

        let cleared = respond(&WyomingEvent::new(types::CLEAR_MEMORIES), &mem, &s);
        assert_eq!(cleared.data["ok"], json!(true));
        assert_eq!(cleared.data["count"], json!(1));
        assert_eq!(mem.count().unwrap(), 0);
    }

    #[test]
    fn delete_without_id_is_an_in_band_error() {
        let s = settings();
        let mem = MemoryStore::open_in_memory().unwrap();
        let resp = respond(&WyomingEvent::new(types::DELETE_MEMORY), &mem, &s);
        assert_eq!(resp.event_type, types::MEMORY_RESULT);
        assert_eq!(resp.data["ok"], json!(false));
    }

    #[test]
    fn control_requests_are_recognized() {
        assert!(is_control_request(types::DESCRIBE_SETTINGS));
        assert!(is_control_request(types::SET_SETTINGS));
        assert!(is_control_request(types::LIST_MEMORIES));
        assert!(!is_control_request(types::AUDIO_START));
        assert!(!is_control_request(types::TRANSCRIPT));
    }
}
