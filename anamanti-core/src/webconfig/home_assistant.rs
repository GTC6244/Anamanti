//! Integrations → Home Assistant (`plans/HomeAssistantPlan.md`): the base URL, the
//! secret long-lived access token (write-only — never echoed back) and the default
//! display room, plus a read-only "Test connection" that fetches the device catalog.

use serde_json::{json, Value};

use super::{keep_or_set, set_or_clear};
use crate::settings::{HomeAssistantUpdate, SharedSettings};

/// `GET /home-assistant/status.json` — the connection settings (never the token) and
/// whether the fast path + `home_control` tool are live.
pub(super) fn ha_status_json(settings: &SharedSettings) -> String {
    let ha = settings.home_assistant_settings();
    let active = settings
        .home_assistant()
        .is_some_and(|hub| hub.is_configured());
    json!({
        "ok": true,
        "base_url": ha.base_url.clone().unwrap_or_default(),
        "default_room": ha.default_room.clone().unwrap_or_default(),
        "token_set": ha.token_set(),
        "configured": ha.configured(),
        "active": active,
    })
    .to_string()
}

/// `POST /home-assistant/save` — set the base URL and/or default room. A blank value
/// clears the field; an absent field is left unchanged. Reconfigures the hub live.
pub(super) fn ha_save_json(settings: &SharedSettings, data: &Value) -> String {
    settings.apply_home_assistant(&HomeAssistantUpdate {
        base_url: set_or_clear(data, "base_url"),
        default_room: set_or_clear(data, "default_room"),
        ..Default::default()
    });
    ha_status_json(settings)
}

/// `POST /home-assistant/token` — set the secret token. A blank/absent token is left
/// unchanged (a page reload never wipes it); `{"clear": true}` removes it.
pub(super) fn ha_token_json(settings: &SharedSettings, data: &Value) -> String {
    let token = if data.get("clear").and_then(Value::as_bool) == Some(true) {
        Some(None)
    } else {
        keep_or_set(data, "token")
    };
    settings.apply_home_assistant(&HomeAssistantUpdate {
        token,
        ..Default::default()
    });
    ha_status_json(settings)
}

/// `POST /home-assistant/test` — fetch the device catalog now (read-only; never
/// switches anything) and report the entity / room counts and latency.
pub(super) async fn ha_test_json(settings: &SharedSettings) -> String {
    let Some(hub) = settings.home_assistant().filter(|h| h.is_configured()) else {
        return json!({ "ok": false, "error": "Set the URL and token first." }).to_string();
    };
    match hub.test_connection().await {
        Ok((entities, rooms, ms)) => json!({
            "ok": true,
            "entities": entities,
            "rooms": rooms,
            "ms": ms,
            "message": format!("Connected: {entities} devices in {rooms} rooms ({ms} ms)."),
        })
        .to_string(),
        Err(e) => json!({ "ok": false, "error": format!("{e:#}") }).to_string(),
    }
}
