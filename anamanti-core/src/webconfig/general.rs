//! The Config page's own endpoints (`/config`, `/models`, `/voices`), the About tab,
//! and the Notify tab.

use serde_json::{json, Value};

use super::{parse_json_body, str_field, view_json, Reply, Request};
use crate::app::AppServices;
use crate::llm::catalog::ModelCatalog;
use crate::notify::{Notification, NotificationService};
use crate::settings::{SettingsUpdate, SharedSettings};

// Build metadata for the About tab, captured at compile time by `build.rs` so a
// running orchestrator can report exactly which build it is.
const VERSION: &str = env!("CARGO_PKG_VERSION");
const GIT_SHA: &str = env!("ANAMANTI_GIT_SHA");
const GIT_BRANCH: &str = env!("ANAMANTI_GIT_BRANCH");
const BUILD_TIME: &str = env!("ANAMANTI_BUILD_TIME");

/// `/about.json` — machine-readable build metadata the About page fetches + renders.
pub(super) fn about_json() -> String {
    json!({
        "ok": true,
        "version": VERSION,
        "git_branch": GIT_BRANCH,
        "git_sha": GIT_SHA,
        "build_time": BUILD_TIME,
    })
    .to_string()
}

/// `POST /config` — apply a `{llm_backend?, llm_model?, tts_voice?, …}` change (same
/// JSON shape as the `ambient-set-settings` control frame; a `tts_voice` of `null`
/// clears the voice) and return the resulting settings. Unlike the page handlers, a
/// malformed body is a `400`.
pub(super) fn config_save(settings: &SharedSettings, req: &Request<'_>) -> Reply {
    match parse_json_body(req.body) {
        // The loopback page is the one place provider secrets may be entered.
        Ok(data) => Reply::json(
            match settings.apply(&SettingsUpdate::from_json(&data, true)) {
                Ok(_) => view_json(settings, true, Some("settings applied")),
                Err(e) => view_json(settings, false, Some(&format!("{e:#}"))),
            },
        ),
        Err(invalid) => Reply::json(invalid).with_status("400 Bad Request"),
    }
}

/// The selectable models as a JSON string (`{ "ok": true, "models": [...] }`).
pub(super) async fn models_json(catalog: &ModelCatalog) -> String {
    let models: Vec<Value> = catalog
        .models()
        .await
        .into_iter()
        .map(|m| json!({ "provider": m.provider, "id": m.id, "label": m.label }))
        .collect();
    json!({ "ok": true, "models": models }).to_string()
}

/// `GET /voices` — the exact device-facing logic: Piper's catalog intersected with the
/// installed voices when a voices dir is configured.
pub(super) async fn voices_json(services: &AppServices) -> String {
    crate::control::voices_response(services.connector.as_ref(), services.voices_dir.as_deref())
        .await
        .data
        .to_string()
}

/// `GET /notifications/status.json` — how many device notify channels are connected.
pub(super) fn notifications_status_json(notify: &NotificationService) -> String {
    json!({
        "connected": notify.connected(),
        "devices": notify.connected_devices(),
    })
    .to_string()
}

/// `POST /notifications/test` — push a proactive notification to every connected
/// device notify channel. Body: `{ title?, body?, priority? }` (all optional; sane
/// defaults; a malformed body is treated as empty). Returns `{ ok, delivered }` — the
/// number of channels it reached (0 if the device isn't currently holding a notify
/// channel open).
pub(super) fn notifications_test_json(notify: &NotificationService, body: &[u8]) -> String {
    let data: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let note = Notification {
        id: notify.new_id(),
        priority: str_field(&data, "priority").unwrap_or_else(|| "info".to_string()),
        title: str_field(&data, "title").unwrap_or_else(|| "Test notification".to_string()),
        body: str_field(&data, "body")
            .unwrap_or_else(|| "Hello from the orchestrator.".to_string()),
    };
    let delivered = notify.notify(&note);
    json!({ "ok": true, "delivered": delivered }).to_string()
}
