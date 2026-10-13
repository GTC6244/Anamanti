//! The Tools tab (directions, weather and places credentials), the Speech tab (STT
//! engine) and the System-1 tab. Status reads never include a secret, only whether
//! one is set.

use serde_json::{json, Value};

use super::{fail, keep_or_set, secret_update, str_field};
use crate::settings::{
    DirectionsUpdate, PlacesToolUpdate, SharedSettings, SttUpdate, System1Update, WeatherToolUpdate,
};

/// `GET /tools/status.json` — the directions tool state for the `/tools` page.
/// Reports only whether a Mapbox token is set (never the token) and whether the
/// `directions_lookup` tool is therefore active (v1 provider is always Mapbox).
pub(super) fn directions_status_json(settings: &SharedSettings) -> String {
    let token_set = settings.mapbox_token_set();
    json!({
        "ok": true,
        "token_set": token_set,
        "tool_active": token_set,
    })
    .to_string()
}

/// `POST /tools/save` — set the Mapbox token for the directions tool. A blank/absent
/// token is left unchanged (a page reload never wipes the stored token); a non-empty
/// value sets it and rebuilds the backend so `directions_lookup` activates live.
pub(super) fn directions_save_json(settings: &SharedSettings, data: &Value) -> String {
    settings.apply_directions(&DirectionsUpdate {
        mapbox_token: keep_or_set(data, "mapbox_token"),
    });
    directions_status_json(settings)
}

/// `GET /tools/weather/status.json` — the weather tool state for the `/tools` page.
/// Reports the selected provider, whether a Visual Crossing key is set (never the key),
/// whether weather is enabled, and whether Visual Crossing is effectively active (it is
/// selected and a key is present; otherwise the tool falls back to keyless Open-Meteo).
pub(super) fn weather_status_json(settings: &SharedSettings) -> String {
    let provider = settings.weather_provider_label();
    let key_set = settings.visualcrossing_key_set();
    let enabled = settings.weather_enabled();
    // Which backend actually serves requests — the same decision `weather::from_config`
    // builds from (an unsupported provider falls back to Open-Meteo even with a key).
    let effective = crate::weather::effective_backend(enabled, &provider, key_set).as_str();
    json!({
        "ok": true,
        "provider": provider,
        "key_set": key_set,
        "enabled": enabled,
        "effective": effective,
    })
    .to_string()
}

/// `POST /tools/weather/save` — set the weather provider and/or Visual Crossing key.
/// `provider` (when present) switches the backend; a blank/absent `visualcrossing_key` is
/// left unchanged (a page reload never wipes the stored key), while an explicit
/// `"clear": true` clears it. Rebuilds the tool + retargets the ambient push live.
pub(super) fn weather_save_json(settings: &SharedSettings, data: &Value) -> String {
    settings.apply_weather_tool(&WeatherToolUpdate {
        provider: str_field(data, "provider"),
        visualcrossing_key: secret_update(data, "visualcrossing_key"),
    });
    weather_status_json(settings)
}

/// `GET /tools/places/status.json` — the places tool state for the `/tools` page.
/// Reports only whether a Google Places key is set (never the key) and whether the
/// `places_lookup` tool is therefore active (no keyless fallback).
pub(super) fn places_status_json(settings: &SharedSettings) -> String {
    let key_set = settings.google_places_key_set();
    json!({
        "ok": true,
        "key_set": key_set,
        "tool_active": key_set,
    })
    .to_string()
}

/// `POST /tools/places/save` — set the Google Places key for the places tool. A
/// blank/absent key is left unchanged (a page reload never wipes the stored key), while an
/// explicit `"clear": true` clears it (withdrawing the tool). Rebuilds the backend so
/// `places_lookup` activates/withdraws live.
pub(super) fn places_save_json(settings: &SharedSettings, data: &Value) -> String {
    settings.apply_places_tool(&PlacesToolUpdate {
        google_places_key: secret_update(data, "google_places_key"),
    });
    places_status_json(settings)
}

/// `GET /stt/status.json` — the live STT selection for the `/stt` page. Reports the
/// engine, the ElevenLabs model/base, whether an ElevenLabs key is set (never the key),
/// and whether whisper-rs is available (feature compiled + model present) so the page can
/// disable that option.
pub(super) fn stt_status_json(settings: &SharedSettings) -> String {
    let v = settings.stt_view();
    json!({
        "ok": true,
        "engine": v.engine,
        "elevenlabs_model_id": v.elevenlabs_model_id,
        "elevenlabs_base_url": v.elevenlabs_base_url,
        "elevenlabs_key_set": v.elevenlabs_key_set,
        "whisper_available": v.whisper_available,
    })
    .to_string()
}

/// `POST /stt/save` — set the STT engine and/or ElevenLabs settings. `engine` (when
/// present) switches the live engine, applied on the next turn (no restart). A
/// blank/absent `elevenlabs_api_key` is left unchanged (a page reload never wipes the
/// stored key), while an explicit `"clear": true` clears it. Blank model/base are left
/// unchanged. Returns the refreshed status, or an error on an unknown engine label.
pub(super) fn stt_save_json(settings: &SharedSettings, data: &Value) -> String {
    match settings.apply_stt(&SttUpdate {
        engine: str_field(data, "engine"),
        elevenlabs_model_id: str_field(data, "elevenlabs_model_id"),
        elevenlabs_base_url: str_field(data, "elevenlabs_base_url"),
        elevenlabs_api_key: secret_update(data, "elevenlabs_api_key"),
    }) {
        Ok(_) => stt_status_json(settings),
        Err(e) => fail(e),
    }
}

/// Current System-1 selection for the config page. Never returns the OpenRouter key,
/// only whether one is set.
pub(super) fn system1_status_json(settings: &SharedSettings) -> String {
    let v = settings.system1_view();
    json!({
        "ok": true,
        "backend": v.backend,
        "base_url": v.base_url,
        "model": v.model,
        "min_confidence": v.min_confidence,
        "openrouter_key_set": v.openrouter_key_set,
        "intents": v.intents,
        "active": v.backend != "none",
    })
    .to_string()
}

/// Apply a System-1 selection from the config page (rebuilds the engine live + persists).
/// A blank `openrouter_api_key` means "leave unchanged" (never shown back). On a build
/// failure (e.g. unknown backend) the current engine is left untouched and the error is
/// returned.
pub(super) fn system1_save_json(settings: &SharedSettings, data: &Value) -> String {
    let update = System1Update {
        backend: str_field(data, "backend"),
        base_url: str_field(data, "base_url"),
        model: str_field(data, "model"),
        min_confidence: data.get("min_confidence").and_then(Value::as_f64),
        // Blank = keep the current key (it is never echoed back to the page).
        openrouter_api_key: keep_or_set(data, "openrouter_api_key"),
        intents: None,
    };
    match settings.apply_system1(&update) {
        Ok(_) => system1_status_json(settings),
        Err(e) => fail(format!("{e:#}")),
    }
}
