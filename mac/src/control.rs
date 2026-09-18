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
use crate::speaker::SpeakerRegistry;
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
            | types::LIST_SPEAKERS
            | types::NAME_SPEAKER
            | types::MERGE_SPEAKERS
            | types::DELETE_SPEAKER
    )
}

/// Handle one control request over `device`: build the response and send it.
pub async fn handle_control(
    device: &mut DynConnection,
    request: &WyomingEvent,
    memory: &MemoryStore,
    settings: &SharedSettings,
    speaker: Option<&SpeakerRegistry>,
) -> Result<()> {
    let response = respond(request, memory, settings, speaker);
    device.send(&response).await
}

/// Build the response event for a control `request`. Never fails: a bad request or
/// a rejected settings change is reported in the response's `ok`/`message` fields
/// rather than raised, so a control error never drops the device connection.
pub fn respond(
    request: &WyomingEvent,
    memory: &MemoryStore,
    settings: &SharedSettings,
    speaker: Option<&SpeakerRegistry>,
) -> WyomingEvent {
    match request.event_type.as_str() {
        types::LIST_SPEAKERS => speakers_response(speaker),
        types::NAME_SPEAKER => name_speaker(request, speaker),
        types::MERGE_SPEAKERS => merge_speakers(request, memory, speaker),
        types::DELETE_SPEAKER => delete_speaker(request, speaker),
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

// ---- speaker identification control (speaker_id_plan.md Phase C) ----

/// The `ambient-speakers` response listing identified speakers.
fn speakers_response(speaker: Option<&SpeakerRegistry>) -> WyomingEvent {
    let Some(reg) = speaker else {
        return WyomingEvent::with_data(
            types::SPEAKERS,
            json!({ "ok": false, "message": SPEAKER_DISABLED, "speakers": [] }),
        );
    };
    match reg.list() {
        Ok(list) => {
            let speakers: Vec<Value> = list
                .into_iter()
                .map(|p| {
                    json!({
                        "id": p.id,
                        "name": p.name,
                        "labeled": p.labeled,
                        "samples": p.samples as i64,
                        "created_at": p.created_at,
                    })
                })
                .collect();
            WyomingEvent::with_data(types::SPEAKERS, json!({ "ok": true, "speakers": speakers }))
        }
        Err(e) => WyomingEvent::with_data(
            types::SPEAKERS,
            json!({ "ok": false, "message": format!("{e:#}"), "speakers": [] }),
        ),
    }
}

fn name_speaker(request: &WyomingEvent, speaker: Option<&SpeakerRegistry>) -> WyomingEvent {
    let Some(reg) = speaker else {
        return speaker_result(false, SPEAKER_DISABLED);
    };
    let id = request.data.get("id").and_then(Value::as_str);
    let name = request.data.get("name").and_then(Value::as_str);
    match (id, name) {
        (Some(id), Some(name)) if !name.trim().is_empty() => match reg.rename(id, name) {
            Ok(true) => speaker_result(true, &format!("named {id} \"{}\"", name.trim())),
            Ok(false) => speaker_result(false, "no such speaker"),
            Err(e) => speaker_result(false, &format!("{e:#}")),
        },
        _ => speaker_result(false, "name-speaker requires `id` and a non-empty `name`"),
    }
}

fn merge_speakers(
    request: &WyomingEvent,
    memory: &MemoryStore,
    speaker: Option<&SpeakerRegistry>,
) -> WyomingEvent {
    let Some(reg) = speaker else {
        return speaker_result(false, SPEAKER_DISABLED);
    };
    let keep = request.data.get("keep").and_then(Value::as_str);
    let drop = request.data.get("drop").and_then(Value::as_str);
    match (keep, drop) {
        (Some(keep), Some(drop)) => match reg.merge(keep, drop) {
            Ok(true) => {
                // Move the dropped speaker's memories onto the kept profile.
                let moved = memory.reassign_speaker(drop, keep).unwrap_or(0);
                speaker_result(true, &format!("merged {drop} into {keep} ({moved} memories moved)"))
            }
            Ok(false) => speaker_result(false, "merge failed (unknown id, or same id)"),
            Err(e) => speaker_result(false, &format!("{e:#}")),
        },
        _ => speaker_result(false, "merge-speakers requires `keep` and `drop`"),
    }
}

fn delete_speaker(request: &WyomingEvent, speaker: Option<&SpeakerRegistry>) -> WyomingEvent {
    let Some(reg) = speaker else {
        return speaker_result(false, SPEAKER_DISABLED);
    };
    match request.data.get("id").and_then(Value::as_str) {
        Some(id) => match reg.delete(id) {
            Ok(true) => speaker_result(true, "deleted"),
            Ok(false) => speaker_result(false, "no such speaker"),
            Err(e) => speaker_result(false, &format!("{e:#}")),
        },
        None => speaker_result(false, "delete-speaker requires `id`"),
    }
}

fn speaker_result(ok: bool, message: &str) -> WyomingEvent {
    WyomingEvent::with_data(types::SPEAKER_RESULT, json!({ "ok": ok, "message": message }))
}

const SPEAKER_DISABLED: &str = "speaker identification is disabled on the orchestrator";

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
        let resp = respond(&WyomingEvent::new(types::DESCRIBE_SETTINGS), &mem, &s, None);
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
            None,
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
            None,
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

        let listed = respond(&WyomingEvent::new(types::LIST_MEMORIES), &mem, &s, None);
        assert_eq!(listed.event_type, types::MEMORIES);
        assert_eq!(listed.data["entries"].as_array().unwrap().len(), 2);
        assert_eq!(
            listed.data["entries"][1]["content"],
            json!("the user likes tea")
        );

        let deleted = respond(&req(types::DELETE_MEMORY, json!({ "id": id })), &mem, &s, None);
        assert_eq!(deleted.event_type, types::MEMORY_RESULT);
        assert_eq!(deleted.data["ok"], json!(true));
        assert_eq!(deleted.data["count"], json!(1));

        let cleared = respond(&WyomingEvent::new(types::CLEAR_MEMORIES), &mem, &s, None);
        assert_eq!(cleared.data["ok"], json!(true));
        assert_eq!(cleared.data["count"], json!(1));
        assert_eq!(mem.count().unwrap(), 0);
    }

    #[test]
    fn delete_without_id_is_an_in_band_error() {
        let s = settings();
        let mem = MemoryStore::open_in_memory().unwrap();
        let resp = respond(&WyomingEvent::new(types::DELETE_MEMORY), &mem, &s, None);
        assert_eq!(resp.event_type, types::MEMORY_RESULT);
        assert_eq!(resp.data["ok"], json!(false));
    }

    #[test]
    fn control_requests_are_recognized() {
        assert!(is_control_request(types::DESCRIBE_SETTINGS));
        assert!(is_control_request(types::SET_SETTINGS));
        assert!(is_control_request(types::LIST_MEMORIES));
        assert!(is_control_request(types::LIST_SPEAKERS));
        assert!(is_control_request(types::NAME_SPEAKER));
        assert!(is_control_request(types::MERGE_SPEAKERS));
        assert!(is_control_request(types::DELETE_SPEAKER));
        assert!(!is_control_request(types::AUDIO_START));
        assert!(!is_control_request(types::TRANSCRIPT));
    }

    #[test]
    fn speaker_requests_report_disabled_without_a_registry() {
        let s = settings();
        let mem = MemoryStore::open_in_memory().unwrap();
        let resp = respond(&WyomingEvent::new(types::LIST_SPEAKERS), &mem, &s, None);
        assert_eq!(resp.event_type, types::SPEAKERS);
        assert_eq!(resp.data["ok"], json!(false));
        assert_eq!(resp.data["speakers"].as_array().unwrap().len(), 0);

        let resp = respond(
            &req(types::NAME_SPEAKER, json!({ "id": "spk-1", "name": "Sam" })),
            &mem,
            &s,
            None,
        );
        assert_eq!(resp.event_type, types::SPEAKER_RESULT);
        assert_eq!(resp.data["ok"], json!(false));
    }

    #[test]
    fn list_name_and_delete_speakers() {
        use crate::speaker::{MockSpeakerEmbedder, SpeakerEmbedder, SpeakerRegistry};
        let s = settings();
        let mem = MemoryStore::open_in_memory().unwrap();
        let reg = SpeakerRegistry::open_in_memory().unwrap();
        let e = MockSpeakerEmbedder::default();
        let id = reg
            .create_cluster(&e.embed(&vec![1000i16; 20_000]).unwrap())
            .unwrap();

        // List shows the anonymous cluster.
        let listed = respond(&WyomingEvent::new(types::LIST_SPEAKERS), &mem, &s, Some(&reg));
        assert_eq!(listed.event_type, types::SPEAKERS);
        assert_eq!(listed.data["ok"], json!(true));
        let arr = listed.data["speakers"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], json!(id));
        assert_eq!(arr[0]["labeled"], json!(false));

        // Name it.
        let named = respond(
            &req(types::NAME_SPEAKER, json!({ "id": id, "name": "Sam" })),
            &mem,
            &s,
            Some(&reg),
        );
        assert_eq!(named.event_type, types::SPEAKER_RESULT);
        assert_eq!(named.data["ok"], json!(true));
        assert_eq!(reg.get(&id).unwrap().unwrap().name.as_deref(), Some("Sam"));

        // Naming an unknown speaker fails in-band.
        let bad = respond(
            &req(types::NAME_SPEAKER, json!({ "id": "nope", "name": "X" })),
            &mem,
            &s,
            Some(&reg),
        );
        assert_eq!(bad.data["ok"], json!(false));

        // Delete it.
        let del = respond(
            &req(types::DELETE_SPEAKER, json!({ "id": id })),
            &mem,
            &s,
            Some(&reg),
        );
        assert_eq!(del.data["ok"], json!(true));
        assert_eq!(reg.count().unwrap(), 0);
    }

    #[test]
    fn merge_speakers_moves_memories() {
        use crate::speaker::{MockSpeakerEmbedder, SpeakerEmbedder, SpeakerRegistry};
        use crate::memory::{MemoryKind, MemorySource};
        let s = settings();
        let mem = MemoryStore::open_in_memory().unwrap();
        let reg = SpeakerRegistry::open_in_memory().unwrap();
        let e = MockSpeakerEmbedder::default();
        let keep = reg.create_cluster(&e.embed(&vec![1000i16; 20_000]).unwrap()).unwrap();
        let drop = reg.create_cluster(&e.embed(&vec![500i16; 20_000]).unwrap()).unwrap();
        mem.add_scoped(MemoryKind::Fact, "likes tea", MemorySource::Explicit, Some(&drop))
            .unwrap();

        let resp = respond(
            &req(types::MERGE_SPEAKERS, json!({ "keep": keep, "drop": drop })),
            &mem,
            &s,
            Some(&reg),
        );
        assert_eq!(resp.data["ok"], json!(true));
        assert_eq!(reg.count().unwrap(), 1, "dropped profile removed");
        // The dropped speaker's memory now belongs to the kept speaker.
        let hits = mem.search_scoped("tea", Some(&keep), 10).unwrap();
        assert!(hits.iter().any(|m| m.content == "likes tea"));
    }
}
