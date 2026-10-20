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

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::app::AppServices;
use crate::llm::anthropic_auth::AnthropicAuth;
use crate::llm::catalog::ModelCatalog;
use crate::memory::graphview::NodeId;
use crate::memory::{EmbedStatus, GraphRagController, MemoryStore};
use crate::music::NowPlayingService;
use crate::orchestrator::ServiceConnector;
use crate::settings::{SettingsUpdate, SharedSettings};
use crate::speaker::{SpeakerRegistry, SpeakerService};
use crate::wyoming::protocol::{types, WyomingEvent};
use crate::wyoming::tts::{describe_voices, VoiceEntry};
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
            | types::LIST_TURNS
            | types::LIST_ENTITIES
            | types::DELETE_GRAPH_NODE
            | types::LIST_SPEAKERS
            | types::NAME_SPEAKER
            | types::MERGE_SPEAKERS
            | types::DELETE_SPEAKER
            | types::LINK_SPEAKER
            | types::LIST_SPEAKER_CLIPS
            | types::GET_SPEAKER_CLIP
            | types::ENROLL_FINISH
            | types::ENROLL_CANCEL
            | types::LIST_MODELS
            | types::LIST_VOICES
            | types::GET_DRIVE_TOKEN
            | types::GET_CAMERA_STREAM
            | types::MUSIC_CONTROL
            | types::GET_FUNCTIONS
            | types::PAIR_PHONE
    )
}

/// Handle one control request over `device`: build the response and send it. Most
/// requests are answered synchronously by [`respond`]; `ambient-list-models` needs
/// an async catalog fetch and `ambient-list-voices` an async Piper `describe`, so
/// those are handled here.
pub async fn handle_control(
    device: &mut DynConnection,
    request: &WyomingEvent,
    services: &AppServices,
) -> Result<()> {
    let memory = services.memory().as_ref();
    let settings = services.settings().as_ref();
    let speaker = services.speaker();
    let graphrag = services.graphrag.as_deref();
    let response = if request.event_type == types::LIST_MODELS {
        models_response(&services.catalog).await
    } else if request.event_type == types::LIST_VOICES {
        voices_response(services.connector.as_ref(), services.voices_dir.as_deref()).await
    } else if request.event_type == types::MUSIC_CONTROL {
        music_control_response(request, settings, &services.now_playing).await
    } else if request.event_type == types::GET_FUNCTIONS {
        // The embeddings entry needs the live GraphRAG backend status (async), so this
        // request is answered here rather than in the pure `respond`.
        let embed = match graphrag {
            Some(g) => Some(g.status().await),
            None => None,
        };
        functions_response(settings, embed.as_ref())
    } else if request.event_type == types::PAIR_PHONE {
        // Linking a phone makes an outbound AppSaid claim call AND mutates settings, so
        // it is answered here (async) rather than in the pure `respond`.
        pair_phone_response(request, settings).await
    } else if request.event_type == types::LIST_TURNS
        || request.event_type == types::LIST_ENTITIES
        || request.event_type == types::DELETE_GRAPH_NODE
    {
        // GraphRAG browse/delete needs async access to the live HelixDB store (via the
        // graph-view proxy), so these are answered here rather than in the pure `respond`.
        graph_response(request, graphrag).await
    } else if request.event_type == types::ENROLL_FINISH {
        // Enrollment commit/cancel touch the SpeakerService's in-flight session map
        // (not just the registry), so they are handled here rather than in `respond`.
        with_pending_unknown(enroll_finish(request, speaker, settings), memory)
    } else if request.event_type == types::ENROLL_CANCEL {
        enroll_cancel(request, speaker)
    } else if request.event_type == types::LIST_SPEAKER_CLIPS {
        // Clip list/fetch read the SpeakerService's clip store (not the registry), so
        // they are answered here with the full service rather than in `respond`.
        speaker_clips_response(speaker, request)
    } else if request.event_type == types::GET_SPEAKER_CLIP {
        speaker_clip_response(speaker, request)
    } else if request.event_type == types::LINK_SPEAKER {
        // Linking to a household member reads the roster (settings) and may run a full
        // merge (registry + memory + clips + graph), so it is coordinated here.
        let graph = graphrag.map(|g| g.recall());
        link_speaker_response(request, memory, speaker, graph.as_deref(), settings).await
    } else if request.event_type == types::MERGE_SPEAKERS
        || request.event_type == types::DELETE_SPEAKER
    {
        // Merging/forgetting a person spans the registry, SQLite memory, voice clips
        // and the (async) GraphRAG graph, so the SpeakerService coordinates it here.
        let graph = graphrag.map(|g| g.recall());
        speaker_change_response(request, memory, speaker, graph.as_deref()).await
    } else {
        respond(request, memory, settings, speaker.map(|s| s.registry()))
    };
    device.send(&response).await
}

/// Handle an `anamanti-music-control` command from the display's on-screen transport
/// controls: map the action to a [`SpotifyCommand`] and issue it against the live
/// controller. Answered in-band (`ok`/`message`) so a failure (not linked, device
/// asleep, bad action) never drops the device connection — mirroring the other control
/// responses. The spoken confirmation `command` returns is surfaced as the `message`.
pub async fn music_control_response(
    request: &WyomingEvent,
    settings: &SharedSettings,
    now_playing: &NowPlayingService,
) -> WyomingEvent {
    let Some((action, value)) = request.music_control_command() else {
        return WyomingEvent::music_result(false, "music-control request missing `action`");
    };
    // The on-screen widget toggle (`display_compact` / `display_fullscreen`): that
    // display's choice for the current music, not transport — record it for the display
    // that sent it (only that one changes) and re-push so its next push agrees.
    if let Some(style) = action
        .strip_prefix("display_")
        .and_then(crate::music::MusicDisplayStyle::parse)
    {
        let Some(device_id) = request.music_control_device_id() else {
            // An older display that doesn't identify itself: nothing to attach the choice
            // to. Its own screen has already switched locally.
            return WyomingEvent::music_result(
                false,
                "this display didn't send its device id; update the display app",
            );
        };
        now_playing.display().choose(device_id, style);
        now_playing.repush(settings);
        return WyomingEvent::music_result(true, format!("Music player: {}.", style.as_str()));
    }
    // Radio while a station is on, else Spotify (shared with the config page).
    match now_playing.transport(settings, &action, value).await {
        Ok(msg) => WyomingEvent::music_result(true, msg),
        Err(e) => WyomingEvent::music_result(false, format!("{e:#}")),
    }
}

/// Build the `anamanti-pair-result` response for a device phone-pairing request.
///
/// Redeems the AppSaid pairing `code` (shown on the recipient's AppSaid app) against
/// the Core's configured worker, then stores the returned `user_key` as a
/// `send_phone_message` recipient under `name`. Every failure — a missing field, no
/// worker URL configured on the Core, a bad/used/expired code — is reported in-band
/// (`ok: false` + a speakable `message`) so a bad pairing never drops the device
/// connection. The claimed `user_key` is never echoed back to the device.
async fn pair_phone_response(request: &WyomingEvent, settings: &SharedSettings) -> WyomingEvent {
    let name = request
        .data
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    let code = request
        .data
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if name.is_empty() {
        return pair_result(false, "enter the person's name".to_string(), None);
    }

    let worker_url = settings.appsaid().worker_url.unwrap_or_default();
    match crate::appsaid::claim_pairing_code(&worker_url, code).await {
        Ok(user_key) => {
            let cfg = settings.add_appsaid_recipient(name, &user_key);
            // The `send_phone_message` tool only activates with a worker URL + app token
            // + at least one recipient; tell the user if a token is still missing so a
            // "linked" phone isn't silently unusable.
            let message = if cfg.active() {
                format!("Linked {name}'s phone.")
            } else if !cfg.token_set() {
                format!(
                    "Linked {name}, but set the AppSaid app token on the Core to send messages."
                )
            } else {
                format!("Linked {name}.")
            };
            pair_result(true, message, Some(name.to_string()))
        }
        Err(e) => pair_result(false, format!("{e:#}"), None),
    }
}

/// Build an `anamanti-pair-result` frame. On success `name` echoes the linked contact
/// (for the device confirmation); it is omitted on failure. Never carries the
/// `user_key`.
fn pair_result(ok: bool, message: String, name: Option<String>) -> WyomingEvent {
    let mut data = json!({ "ok": ok, "message": message });
    if let Some(name) = name {
        data["name"] = json!(name);
    }
    WyomingEvent::with_data(types::PAIR_RESULT, data)
}

/// The `anamanti-speaker-clips` response listing one speaker's stored voice clips.
fn speaker_clips_response(
    speaker: Option<&SpeakerService>,
    request: &WyomingEvent,
) -> WyomingEvent {
    let id = request.data.get("id").and_then(Value::as_str).unwrap_or("");
    let Some(svc) = speaker else {
        return WyomingEvent::with_data(
            types::SPEAKER_CLIPS,
            json!({ "ok": false, "message": SPEAKER_DISABLED, "clips": [] }),
        );
    };
    match svc.list_clips(id) {
        Ok(clips) => {
            let clips: Vec<Value> = clips
                .into_iter()
                .map(|c| {
                    json!({
                        "name": c.name,
                        "created_ms": c.created_ms as i64,
                        "samples": c.samples as i64,
                        "sample_rate": crate::speaker::embed::SAMPLE_RATE,
                    })
                })
                .collect();
            WyomingEvent::with_data(
                types::SPEAKER_CLIPS,
                json!({ "ok": true, "id": id, "clips": clips }),
            )
        }
        Err(e) => WyomingEvent::with_data(
            types::SPEAKER_CLIPS,
            json!({ "ok": false, "message": format!("{e:#}"), "clips": [] }),
        ),
    }
}

/// The `anamanti-speaker-clip` response carrying one clip's raw PCM in the payload.
fn speaker_clip_response(speaker: Option<&SpeakerService>, request: &WyomingEvent) -> WyomingEvent {
    let id = request.data.get("id").and_then(Value::as_str).unwrap_or("");
    let clip = request
        .data
        .get("clip")
        .and_then(Value::as_str)
        .unwrap_or("");
    let Some(svc) = speaker else {
        return WyomingEvent::with_data(
            types::SPEAKER_CLIP,
            json!({ "ok": false, "message": SPEAKER_DISABLED }),
        );
    };
    match svc.load_clip(id, clip) {
        Ok(bytes) => {
            let samples = (bytes.len() / 2) as i64;
            let mut event = WyomingEvent::with_data(
                types::SPEAKER_CLIP,
                json!({
                    "ok": true,
                    "id": id,
                    "clip": clip,
                    "sample_rate": crate::speaker::embed::SAMPLE_RATE,
                    "samples": samples,
                }),
            );
            event.payload = Some(bytes);
            event
        }
        Err(e) => WyomingEvent::with_data(
            types::SPEAKER_CLIP,
            json!({ "ok": false, "message": format!("{e:#}") }),
        ),
    }
}

/// Build the `ambient-voices` response: Piper's advertised voice catalog, filtered
/// to the voices actually present on disk when a voices dir is configured. A
/// failure to reach Piper is reported in-band (`ok: false`) so it never drops the
/// device connection.
pub async fn voices_response(
    connector: &dyn ServiceConnector,
    voices_dir: Option<&Path>,
) -> WyomingEvent {
    match list_installed_voices(connector, voices_dir).await {
        Ok(voices) => {
            let voices: Vec<Value> = voices
                .into_iter()
                .map(|v| json!({ "name": v.name, "language": v.language, "label": v.label }))
                .collect();
            WyomingEvent::with_data(types::VOICES, json!({ "ok": true, "voices": voices }))
        }
        Err(e) => WyomingEvent::with_data(
            types::VOICES,
            json!({ "ok": false, "message": format!("{e:#}"), "voices": [] }),
        ),
    }
}

/// Fetch Piper's advertised voices and, when `voices_dir` is set, keep only those
/// whose `<name>.onnx` model is present there. Sorted by name, deduplicated.
async fn list_installed_voices(
    connector: &dyn ServiceConnector,
    voices_dir: Option<&Path>,
) -> Result<Vec<VoiceEntry>> {
    let conn = connector.connect_tts().await?;
    let mut catalog = describe_voices(conn).await?;
    if let Some(dir) = voices_dir {
        let installed = installed_voice_names(dir)?;
        catalog.retain(|v| installed.contains(&v.name));
    }
    catalog.sort_by(|a, b| a.name.cmp(&b.name));
    catalog.dedup_by(|a, b| a.name == b.name);
    Ok(catalog)
}

/// The set of voice names present in `dir`: the file stem of every `<name>.onnx`
/// (so `en_US-amy-medium.onnx` → `en_US-amy-medium`; the sibling `.onnx.json` is
/// ignored).
fn installed_voice_names(dir: &Path) -> Result<HashSet<String>> {
    let mut names = HashSet::new();
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("scanning Piper voices dir {}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("onnx") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                names.insert(stem.to_string());
            }
        }
    }
    Ok(names)
}

/// Build the `ambient-models` response from the selectable-model catalog.
pub async fn models_response(catalog: &ModelCatalog) -> WyomingEvent {
    let models: Vec<Value> = catalog
        .models()
        .await
        .into_iter()
        .map(|m| json!({ "provider": m.provider, "id": m.id, "label": m.label }))
        .collect();
    WyomingEvent::with_data(types::MODELS, json!({ "ok": true, "models": models }))
}

/// Build the `ambient-drive-token` response: the Google Drive photo-slideshow
/// bundle the orchestrator owns (client creds + refresh token + folder ids). The
/// device pulls this and mints Drive access tokens on-device. The client secret and
/// refresh token ride the device↔orchestrator LAN hop only (never an off-the-shelf
/// Wyoming server); an unlinked orchestrator still answers `ok: true` with empty
/// fields so the device can degrade to local gradients.
pub fn drive_token_response(settings: &SharedSettings) -> WyomingEvent {
    let d = settings.drive();
    WyomingEvent::with_data(
        types::DRIVE_TOKEN,
        json!({
            "ok": true,
            "linked": d.linked(),
            "configured": d.configured(),
            "client_id": d.client_id.unwrap_or_default(),
            "client_secret": d.client_secret.unwrap_or_default(),
            "refresh_token": d.refresh_token.unwrap_or_default(),
            "folder_ids": d.folder_ids,
            "scope": d.scope.unwrap_or_default(),
        }),
    )
}

/// Build the `anamanti-camera-stream` response (`plans/CameraPlan.md`): the RTSP
/// sub-stream URL — **credential embedded** — for the requested camera. Like the Drive
/// bundle it rides the device↔Core LAN hop only; the device holds it in memory. Never
/// log the URL. An unconfigured camera or an unknown name answers `ok: false` with a
/// speakable `message`.
pub fn camera_stream_response(
    request: &WyomingEvent,
    hub: Option<&crate::camera::CameraHub>,
) -> WyomingEvent {
    let fail = |message: &str| {
        WyomingEvent::with_data(
            types::CAMERA_STREAM,
            json!({ "ok": false, "message": message }),
        )
    };
    let Some(hub) = hub else {
        return fail("The camera isn't set up on Anamanti Core.");
    };
    let want = request
        .data
        .get("camera")
        .and_then(Value::as_str)
        .unwrap_or("");
    match hub.resolve(want) {
        Some(cam) => {
            log::info!("camera stream: serving {:?} to a display", cam.name);
            WyomingEvent::with_data(
                types::CAMERA_STREAM,
                json!({ "ok": true, "camera": cam.name, "url": hub.stream_url(cam) }),
            )
        }
        None => fail(&format!("There's no camera called {}.", want.trim())),
    }
}

/// One capability row in the `anamanti-functions` snapshot. `mode` is the device's
/// cloud/local/off chip; `key_required && !key_set` drives the red "API key missing"
/// warning. Secret values are never included — only whether each required key is set,
/// exactly like the loopback config page.
#[allow(clippy::too_many_arguments)]
fn func_entry(
    category: &str,
    name: &str,
    mode: &str,
    provider: Option<&str>,
    model: Option<String>,
    key_required: bool,
    key_set: bool,
    note: Option<&str>,
) -> Value {
    json!({
        "category": category,
        "name": name,
        "mode": mode,
        "provider": provider.unwrap_or_default(),
        "model": model.unwrap_or_default(),
        "key_required": key_required,
        "key_set": key_set,
        "note": note.unwrap_or_default(),
    })
}

/// Build the `anamanti-functions` response: a read-only snapshot of the Core's
/// configured capabilities (cloud vs local, model, and whether a required API key is
/// set) for the device Settings screen. Mirrors what the loopback config page's Config,
/// Tools, System1, Speech, and Memory→Embedding tabs show, aggregated into one frame.
/// Never exposes secret values. `embed` is the live GraphRAG embedding status when the
/// GraphRAG backend is running (`None` ⇒ memory is on SQLite FTS).
pub fn functions_response(settings: &SharedSettings, embed: Option<&EmbedStatus>) -> WyomingEvent {
    let v = settings.view();
    let mut fns: Vec<Value> = Vec::new();

    // ---- Config: LLM ----
    let llm = match v.llm_backend.as_str() {
        "anthropic" => {
            let key_set = match v.anthropic_auth {
                AnthropicAuth::Subscription => v.anthropic_oauth_token_set,
                AnthropicAuth::ApiKey => v.anthropic_key_set,
            };
            func_entry(
                "Config",
                "LLM",
                "cloud",
                Some("Anthropic (Claude)"),
                v.llm_model.clone(),
                true,
                key_set,
                None,
            )
        }
        "openai" => func_entry(
            "Config",
            "LLM",
            "cloud",
            Some("OpenAI"),
            v.llm_model.clone(),
            true,
            v.openai_key_set,
            None,
        ),
        "ollama" => func_entry(
            "Config",
            "LLM",
            "local",
            Some("Ollama"),
            v.llm_model.clone(),
            false,
            false,
            None,
        ),
        _ => func_entry(
            "Config",
            "LLM",
            "local",
            Some("Mock (offline echo)"),
            v.llm_model.clone(),
            false,
            false,
            None,
        ),
    };
    fns.push(llm);

    // ---- Config: web search ----
    if v.web_search {
        let tavily = v.search_provider.eq_ignore_ascii_case("tavily");
        fns.push(func_entry(
            "Config",
            "Web search",
            "cloud",
            Some(&v.search_provider),
            None,
            tavily, // DuckDuckGo is keyless
            v.search_key_set,
            None,
        ));
    }

    // ---- Config: text-to-speech (Piper, always local) ----
    fns.push(func_entry(
        "Config",
        "Text-to-speech",
        "local",
        Some("Piper"),
        v.tts_voice.clone(),
        false,
        false,
        None,
    ));

    // ---- Speech: speech-to-text ----
    let stt = settings.stt_view();
    let stt_entry = match stt.engine.as_str() {
        "elevenlabs" => func_entry(
            "Speech",
            "Speech-to-text",
            "cloud",
            Some("ElevenLabs Scribe"),
            Some(stt.elevenlabs_model_id.clone()),
            true,
            stt.elevenlabs_key_set,
            None,
        ),
        "whisper-rs" => func_entry(
            "Speech",
            "Speech-to-text",
            "local",
            Some("Whisper (in-process)"),
            None,
            false,
            false,
            None,
        ),
        _ => func_entry(
            "Speech",
            "Speech-to-text",
            "local",
            Some("Whisper (Wyoming)"),
            None,
            false,
            false,
            None,
        ),
    };
    fns.push(stt_entry);

    // ---- Tools: directions (Mapbox) ----
    // Optional add-on tools (directions/places/Spotify/shopping list/phone/calendar)
    // are "on iff configured": a missing credential just means the tool is off, not a
    // broken setup, so they never raise the red key warning — only the pipeline
    // essentials above do. An off tool carries a note saying what would enable it.
    let mapbox_set = v.mapbox_token_set;
    fns.push(func_entry(
        "Tools",
        "Directions",
        if mapbox_set { "cloud" } else { "off" },
        Some("Mapbox"),
        None,
        false,
        mapbox_set,
        (!mapbox_set).then_some("Needs a Mapbox token"),
    ));

    // ---- Tools: weather ----
    let weather_provider = settings.weather_provider_label();
    let vc_key_set = settings.visualcrossing_key_set();
    let weather_enabled = settings.weather_enabled();
    // Visual Crossing needs a key; without one it silently falls back to keyless
    // Open-Meteo, so the key is never strictly *required* — surface the fallback as a
    // note instead of a red warning. The backend itself is `weather::from_config`'s own
    // decision (an unsupported provider is Open-Meteo even with a key).
    let (weather_mode, weather_provider_label, weather_note): (&str, &str, Option<&str>) =
        match crate::weather::effective_backend(weather_enabled, &weather_provider, vc_key_set) {
            crate::weather::WeatherBackend::Disabled => ("off", "Weather", None),
            crate::weather::WeatherBackend::VisualCrossing => ("cloud", "Visual Crossing", None),
            crate::weather::WeatherBackend::OpenMeteo
                if crate::weather::selects_visualcrossing(&weather_provider) =>
            {
                (
                    "local",
                    "Open-Meteo",
                    Some("No Visual Crossing key — using Open-Meteo"),
                )
            }
            crate::weather::WeatherBackend::OpenMeteo => ("local", "Open-Meteo", None),
        };
    fns.push(func_entry(
        "Tools",
        "Weather",
        weather_mode,
        Some(weather_provider_label),
        None,
        false,
        vc_key_set,
        weather_note,
    ));

    // ---- Tools: places (Google, no keyless fallback) ----
    let places_set = settings.google_places_key_set();
    fns.push(func_entry(
        "Tools",
        "Places",
        if places_set { "cloud" } else { "off" },
        Some("Google Places"),
        None,
        false,
        places_set,
        (!places_set).then_some("Needs a Google Places key"),
    ));

    // ---- Tools: Spotify ----
    let spotify = settings.spotify();
    let spotify_linked = spotify.linked();
    fns.push(func_entry(
        "Tools",
        "Spotify",
        if spotify_linked { "cloud" } else { "off" },
        Some("Spotify"),
        None,
        false,
        spotify_linked,
        (!spotify_linked).then_some(if spotify.configured() {
            "Not linked"
        } else {
            "Not configured"
        }),
    ));

    // ---- Tools: shopping list (Cadora) ----
    let cadora = settings.cadora();
    let cadora_linked = cadora.linked();
    fns.push(func_entry(
        "Tools",
        "Shopping list",
        if cadora_linked { "cloud" } else { "off" },
        Some("Cadora"),
        None,
        false,
        cadora_linked,
        (!cadora_linked).then_some("Not linked"),
    ));

    // ---- Tools: phone messages (AppSaid) ----
    let appsaid = settings.appsaid();
    let appsaid_active = appsaid.active();
    fns.push(func_entry(
        "Tools",
        "Phone messages",
        if appsaid_active { "cloud" } else { "off" },
        Some("AppSaid"),
        None,
        false,
        appsaid.token_set(),
        (!appsaid_active).then_some(if appsaid.token_set() {
            "No worker URL or recipient"
        } else {
            "Not configured"
        }),
    ));

    // ---- Tools: calendar (read-only web .ics, keyless) ----
    let calendar_active = settings.calendar_active();
    fns.push(func_entry(
        "Tools",
        "Calendar",
        if calendar_active { "cloud" } else { "off" },
        Some("Web calendar"),
        None,
        false,
        calendar_active,
        (!calendar_active).then_some("No subscriptions"),
    ));

    // ---- System1: fast decisions ----
    let s1 = settings.system1_view();
    let s1_entry = match s1.backend.as_str() {
        "jev" => func_entry(
            "System1",
            "Fast decisions",
            "cloud",
            Some("OpenRouter"),
            Some(s1.model.clone()),
            true,
            s1.openrouter_key_set,
            None,
        ),
        "laya-serve" => func_entry(
            "System1",
            "Fast decisions",
            "local",
            Some("Laya-Decision"),
            None,
            false,
            false,
            None,
        ),
        _ => func_entry(
            "System1",
            "Fast decisions",
            "off",
            Some("Disabled"),
            None,
            false,
            false,
            None,
        ),
    };
    fns.push(s1_entry);

    // ---- Memory: embeddings (GraphRAG) ----
    let embed_entry = match embed {
        Some(e) if e.active == "openai" => func_entry(
            "Memory",
            "Embeddings",
            "cloud",
            Some("OpenAI (text-embedding-3-small)"),
            Some(format!("{}-dim", e.dims)),
            true,
            e.openai_available,
            None,
        ),
        Some(e) => func_entry(
            "Memory",
            "Embeddings",
            "local",
            Some("nomic-embed-text"),
            Some(format!("{}-dim", e.dims)),
            false,
            false,
            None,
        ),
        None => func_entry(
            "Memory",
            "Embeddings",
            "local",
            Some("SQLite FTS"),
            None,
            false,
            false,
            Some("GraphRAG disabled"),
        ),
    };
    fns.push(embed_entry);

    WyomingEvent::with_data(types::FUNCTIONS, json!({ "ok": true, "functions": fns }))
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
        types::LIST_SPEAKERS => speakers_response(speaker, settings),
        types::NAME_SPEAKER => name_speaker(request, speaker),
        types::GET_DRIVE_TOKEN => drive_token_response(settings),
        types::GET_CAMERA_STREAM => camera_stream_response(request, settings.camera().as_deref()),
        types::DESCRIBE_SETTINGS => settings_response(settings, true, "current settings"),
        // The device path never accepts secrets (`allow_secrets = false`): provider keys
        // are entered only on the orchestrator's loopback config page, so cloud secrets
        // never travel from or live on the shared Echo Show screen.
        types::SET_SETTINGS => {
            match settings.apply(&SettingsUpdate::from_json(&request.data, false)) {
                Ok(_) => settings_response(settings, true, "settings applied"),
                Err(e) => settings_response(settings, false, &format!("{e:#}")),
            }
        }
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

/// Build the `ambient-settings` response: the shared settings JSON view
/// ([`crate::settings::SettingsView::to_json`], the same payload the config page
/// serves) with an `ok`/`message` envelope. Read-only on the device: whether a provider
/// key is configured, never the key itself.
fn settings_response(settings: &SharedSettings, ok: bool, message: &str) -> WyomingEvent {
    WyomingEvent::with_data(types::SETTINGS, settings.view().to_json(ok, Some(message)))
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

// ---- GraphRAG (HelixDB) memory browsing ----

/// Answer a `LIST_TURNS` / `LIST_ENTITIES` / `DELETE_GRAPH_NODE` request against the live
/// GraphRAG graph. Requires the HelixDB backend: when `graphrag` is `None` (SQLite recall
/// backend) every graph request replies `ok:false` with an explanation on the matching
/// reply frame, so the device shows a clean "unavailable" state instead of hanging.
async fn graph_response(
    request: &WyomingEvent,
    graphrag: Option<&GraphRagController>,
) -> WyomingEvent {
    // Pick the reply frame that pairs with this request so an error still round-trips.
    let reply_type = match request.event_type.as_str() {
        types::LIST_TURNS => types::TURNS,
        types::LIST_ENTITIES => types::ENTITIES,
        _ => types::GRAPH_RESULT,
    };
    let Some(ctrl) = graphrag else {
        return WyomingEvent::with_data(
            reply_type,
            json!({
                "ok": false,
                "message": "graph memory is unavailable on this backend (SQLite recall)",
            }),
        );
    };
    let graph = ctrl.graph_view();
    let data = &request.data;
    let str_arg = |key: &str| -> Option<String> {
        data.get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };

    match request.event_type.as_str() {
        types::LIST_TURNS => {
            let query = str_arg("query");
            let speaker_id = str_arg("speaker_id");
            let entity = str_arg("entity");
            let limit = data.get("limit").and_then(|v| v.as_u64()).unwrap_or(100) as usize;
            let offset = data.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            match graph
                .list_turns(
                    query.as_deref(),
                    speaker_id.as_deref(),
                    entity.as_deref(),
                    limit,
                    offset,
                )
                .await
            {
                Ok(list) => WyomingEvent::with_data(
                    types::TURNS,
                    json!({ "ok": true, "turns": list.turns }),
                ),
                Err(e) => WyomingEvent::with_data(
                    types::TURNS,
                    json!({ "ok": false, "message": format!("{e:#}"), "turns": [] }),
                ),
            }
        }
        types::LIST_ENTITIES => {
            let query = str_arg("query");
            let limit = data.get("limit").and_then(|v| v.as_u64()).unwrap_or(200) as usize;
            match graph.list_entities(query.as_deref(), limit).await {
                Ok(list) => WyomingEvent::with_data(
                    types::ENTITIES,
                    json!({ "ok": true, "entities": list.entities }),
                ),
                Err(e) => WyomingEvent::with_data(
                    types::ENTITIES,
                    json!({ "ok": false, "message": format!("{e:#}"), "entities": [] }),
                ),
            }
        }
        // DELETE_GRAPH_NODE
        _ => match data
            .get("id")
            .and_then(|v| v.as_u64())
            .map(NodeId::from_raw)
        {
            Some(id) => match graph.delete_node(id).await {
                Ok(_) => WyomingEvent::with_data(types::GRAPH_RESULT, json!({ "ok": true })),
                Err(e) => WyomingEvent::with_data(
                    types::GRAPH_RESULT,
                    json!({ "ok": false, "message": format!("{e:#}") }),
                ),
            },
            None => WyomingEvent::with_data(
                types::GRAPH_RESULT,
                json!({ "ok": false, "message": "delete-graph-node requires a numeric `id`" }),
            ),
        },
    }
}

// ---- speaker identification control (speaker_id_plan.md Phase C) ----

/// After a successful enrollment, add `pending_unknown`: how many unattributed
/// memories name the enrolled voiceprint as their best guess, so the device can say
/// "I also have N things someone told me that might be yours — review them on the Mac"
/// (SpeakerScopedMemoryPlan.md §4a). Additive: an older device ignores it.
fn with_pending_unknown(mut result: WyomingEvent, memory: &MemoryStore) -> WyomingEvent {
    let ok = result.data.get("ok").and_then(Value::as_bool) == Some(true);
    let id = result
        .data
        .get("speaker_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let (true, Some(id)) = (ok, id) {
        let n = memory.unknown_count_for_candidate(&id).unwrap_or(0);
        if let Value::Object(map) = &mut result.data {
            map.insert("pending_unknown".to_string(), json!(n));
        }
    }
    result
}

/// The `ambient-speakers` response listing identified speakers, plus `members`: the
/// household roster's **names only** (no contact PII leaves the Mac), so the device's
/// enrollment flow can attach a voiceprint to an existing home member.
fn speakers_response(speaker: Option<&SpeakerRegistry>, settings: &SharedSettings) -> WyomingEvent {
    let members: Vec<String> = settings
        .household()
        .members
        .into_iter()
        .map(|m| m.name)
        .collect();
    let Some(reg) = speaker else {
        return WyomingEvent::with_data(
            types::SPEAKERS,
            json!({ "ok": false, "message": SPEAKER_DISABLED, "speakers": [], "members": members }),
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
                        "name_source": p.name_source.map(|s| s.as_str()),
                        "labeled": p.labeled,
                        "samples": p.samples as i64,
                        "created_at": p.created_at,
                    })
                })
                .collect();
            WyomingEvent::with_data(
                types::SPEAKERS,
                json!({ "ok": true, "speakers": speakers, "members": members }),
            )
        }
        Err(e) => WyomingEvent::with_data(
            types::SPEAKERS,
            json!({ "ok": false, "message": format!("{e:#}"), "speakers": [], "members": members }),
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

/// Answer `MERGE_SPEAKERS` (`{keep, drop}`) or `DELETE_SPEAKER` (`{id}`) through
/// [`SpeakerService::merge`] / [`SpeakerService::delete`], which update the registry,
/// SQLite memory, voice clips and the GraphRAG graph (`graph`: the live recall
/// backend, `None` without GraphRAG) together. Failures are reported in-band.
async fn speaker_change_response(
    request: &WyomingEvent,
    memory: &MemoryStore,
    speaker: Option<&SpeakerService>,
    graph: Option<&dyn crate::memory::Recall>,
) -> WyomingEvent {
    let Some(svc) = speaker else {
        return speaker_result(false, SPEAKER_DISABLED);
    };
    let arg = |key: &str| request.data.get(key).and_then(Value::as_str);
    if request.event_type == types::MERGE_SPEAKERS {
        let (Some(keep), Some(drop)) = (arg("keep"), arg("drop")) else {
            return speaker_result(false, "merge-speakers requires `keep` and `drop`");
        };
        match svc.merge(keep, drop, memory, graph).await {
            Ok(Some(change)) => speaker_result(
                true,
                &format!(
                    "merged {drop} into {keep} ({} memories moved){}",
                    change.memories,
                    warnings_suffix(&change.warnings)
                ),
            ),
            Ok(None) => speaker_result(false, "merge failed (unknown id, or same id)"),
            Err(e) => speaker_result(false, &format!("{e:#}")),
        }
    } else {
        let Some(id) = arg("id") else {
            return speaker_result(false, "delete-speaker requires `id`");
        };
        match svc.delete(id, memory, graph).await {
            Ok(Some(change)) => speaker_result(
                true,
                &format!("deleted{}", warnings_suffix(&change.warnings)),
            ),
            Ok(None) => speaker_result(false, "no such speaker"),
            Err(e) => speaker_result(false, &format!("{e:#}")),
        }
    }
}

/// Link voice `id` to the household member `member` (the People screen's "Link to
/// household member"). The Core's roster is the authority: `member` must match a
/// household member (case-insensitive). If another voiceprint already carries that
/// member's name, `id` is **merged into it** (voiceprint, memories, clips, graph
/// history — the same coordinated merge as "Merge with…"), so a person ends up with one
/// voiceprint; otherwise `id` is simply named as the member. The resulting profile is
/// labeled with the member's canonical spelling.
async fn link_speaker_response(
    request: &WyomingEvent,
    memory: &MemoryStore,
    speaker: Option<&SpeakerService>,
    graph: Option<&dyn crate::memory::Recall>,
    settings: &SharedSettings,
) -> WyomingEvent {
    let Some(svc) = speaker else {
        return speaker_result(false, SPEAKER_DISABLED);
    };
    let arg = |key: &str| {
        request
            .data
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let (Some(id), Some(member)) = (arg("id"), arg("member")) else {
        return speaker_result(false, "link-speaker requires `id` and `member`");
    };
    let Some(canonical) = settings
        .household()
        .member_matching(member)
        .map(|m| m.name.clone())
    else {
        return speaker_result(false, &format!("{member} isn't in the household"));
    };
    let reg = svc.registry();
    let profiles = match reg.list() {
        Ok(p) => p,
        Err(e) => return speaker_result(false, &format!("{e:#}")),
    };
    if !profiles.iter().any(|p| p.id == id) {
        return speaker_result(false, "no such speaker");
    }
    // The member's existing voiceprint (oldest first), other than this one.
    let target = profiles
        .iter()
        .find(|p| {
            p.id != id
                && p.name
                    .as_deref()
                    .is_some_and(|n| n.trim().eq_ignore_ascii_case(&canonical))
        })
        .map(|p| p.id.clone());

    let link_result = |ok: bool, message: String, speaker_id: &str, merged: bool| {
        WyomingEvent::with_data(
            types::SPEAKER_RESULT,
            json!({
                "ok": ok, "message": message, "speaker_id": speaker_id,
                "name": canonical, "merged": merged,
            }),
        )
    };
    match target {
        Some(keep) => match svc.merge(&keep, id, memory, graph).await {
            Ok(Some(change)) => {
                // Normalize the kept profile to the roster's spelling.
                if let Err(e) = reg.rename(&keep, &canonical) {
                    log::warn!("link-speaker: renaming {keep} to {canonical}: {e:#}");
                }
                link_result(
                    true,
                    format!(
                        "merged into {canonical} ({} memories moved){}",
                        change.memories,
                        warnings_suffix(&change.warnings)
                    ),
                    &keep,
                    true,
                )
            }
            Ok(None) => speaker_result(false, "merge failed (unknown id)"),
            Err(e) => speaker_result(false, &format!("{e:#}")),
        },
        None => match reg.rename(id, &canonical) {
            Ok(true) => link_result(true, format!("linked to {canonical}"), id, false),
            Ok(false) => speaker_result(false, "no such speaker"),
            Err(e) => speaker_result(false, &format!("{e:#}")),
        },
    }
}

/// `"; warnings: a; b"` for a speaker change whose follow-up steps partly failed.
fn warnings_suffix(warnings: &[String]) -> String {
    if warnings.is_empty() {
        String::new()
    } else {
        format!("; warnings: {}", warnings.join("; "))
    }
}

fn speaker_result(ok: bool, message: &str) -> WyomingEvent {
    WyomingEvent::with_data(
        types::SPEAKER_RESULT,
        json!({ "ok": ok, "message": message }),
    )
}

/// Commit the current guided-enrollment session (SpeakerEnrollmentPlan.md Phase 2)
/// into a named, labeled voiceprint. The session's samples were accumulated by the
/// enroll-context capture turns; this averages them into a centroid.
///
/// The voiceprint is tied to the **household roster**: a `name` matching an existing
/// member (case-insensitive) is saved under that member's canonical spelling; a new
/// name is added to the roster once the commit succeeds. A person who already has a
/// voiceprint under that name gets the samples folded into it (no duplicate).
fn enroll_finish(
    request: &WyomingEvent,
    speaker: Option<&SpeakerService>,
    settings: &SharedSettings,
) -> WyomingEvent {
    let session = request
        .data
        .get("session")
        .and_then(Value::as_str)
        .unwrap_or("");
    let Some(svc) = speaker else {
        return enroll_result(json!({
            "ok": false, "phase": "finish", "session": session, "message": SPEAKER_DISABLED,
        }));
    };
    // "Add voice samples" for a person already on the People list: fold into that
    // exact profile by id (keeps its name; never touches the household roster).
    let speaker_id = request
        .data
        .get("speaker_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(id) = speaker_id {
        return match svc.enroll_into(session, id) {
            Ok(out) => {
                let who = if out.name.is_empty() {
                    "this person's".to_string()
                } else {
                    format!("{}'s", out.name)
                };
                enroll_result(json!({
                    "ok": true,
                    "phase": "finish",
                    "session": session,
                    "speaker_id": out.speaker_id,
                    "name": out.name,
                    "samples": out.samples,
                    "updated": true,
                    "existing_member": false,
                    "message": format!("added {} samples to {who} voiceprint", out.samples),
                }))
            }
            Err(e) => enroll_result(json!({
                "ok": false, "phase": "finish", "session": session, "message": format!("{e:#}"),
            })),
        };
    }
    let name = request
        .data
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if name.is_empty() {
        return enroll_result(json!({
            "ok": false, "phase": "finish", "session": session,
            "message": "a name is required to finish enrollment",
        }));
    }
    let existing_member = settings
        .household()
        .member_matching(name)
        .map(|m| m.name.clone());
    let name = existing_member.as_deref().unwrap_or(name);
    match svc.enroll_finish(session, name) {
        Ok(out) => {
            if existing_member.is_none() {
                settings.ensure_household_member(&out.name);
            }
            let message = if out.updated {
                format!("added {} samples to {}'s voiceprint", out.samples, out.name)
            } else {
                format!("enrolled {} from {} samples", out.name, out.samples)
            };
            enroll_result(json!({
                "ok": true,
                "phase": "finish",
                "session": session,
                "speaker_id": out.speaker_id,
                "name": out.name,
                "samples": out.samples,
                "updated": out.updated,
                "existing_member": existing_member.is_some(),
                "message": message,
            }))
        }
        Err(e) => enroll_result(json!({
            "ok": false, "phase": "finish", "session": session, "message": format!("{e:#}"),
        })),
    }
}

/// Discard an in-flight guided-enrollment session (the user backed out).
fn enroll_cancel(request: &WyomingEvent, speaker: Option<&SpeakerService>) -> WyomingEvent {
    let session = request
        .data
        .get("session")
        .and_then(Value::as_str)
        .unwrap_or("");
    if let Some(svc) = speaker {
        svc.enroll_cancel(session);
    }
    enroll_result(json!({
        "ok": true, "phase": "cancel", "session": session, "message": "cancelled",
    }))
}

fn enroll_result(data: Value) -> WyomingEvent {
    WyomingEvent::with_data(types::ENROLL_RESULT, data)
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
                openai_base_url: "http://z".into(),
                openai_api_key: None,
                openai_max_tokens: 10,
                anthropic_token: None,
                home_location: crate::household::LiveHomeLocation::default(),
                spotify: None,
                cadora: None,
                appsaid: None,
                calendar: None,
                directions: None,
                directions_provider: String::new(),
                directions_imperial: false,
                weather: None,
                weather_imperial: false,
                weather_enabled: false,
                places: None,
                places_enabled: true,
                places_provider: "google".to_string(),
                radio: None,
                home_assistant: None,
                camera: None,
                music_display: Default::default(),
            },
            RuntimeSettings {
                llm: Arc::new(crate::llm::mock::MockLlm::default()),
                engine: crate::settings::LlmEngine::Native,
                web_search: false,
                search_provider: "duckduckgo".into(),
                search_api_key: None,
                llm_backend: "ollama".into(),
                llm_model: Some("llama3.2".into()),
                anthropic_api_key: None,
                openai_api_key: None,
                anthropic_oauth_token: None,
                mapbox_token: None,
                anthropic_auth: crate::llm::anthropic_auth::AnthropicAuth::ApiKey,
                tts_voice: Some("amy".into()),
                end_silence_ms: crate::settings::DEFAULT_END_SILENCE_MS,
                voice_rms_threshold: crate::settings::DEFAULT_VOICE_RMS_THRESHOLD,
                silero_threshold: crate::settings::DEFAULT_SILERO_THRESHOLD,
                vad_engine: crate::config::VadEngineKind::Energy,
                stt: crate::settings::SttRuntime::default(),
                drive: crate::settings::DriveConfig::default(),
                household: crate::settings::Household::default(),
                spotify: crate::settings::SpotifyConfig::default(),
                cadora: crate::settings::CadoraConfig::default(),
                appsaid: crate::settings::AppSaidConfig::default(),
                home_assistant: Default::default(),
                weather_provider: "visualcrossing".into(),
                visualcrossing_key: None,
                google_places_key: None,
                system1: crate::settings::System1Runtime::default(),
                personality: crate::settings::Personality::default(),
                pronunciations: Vec::new(),
                music_display_pins: Default::default(),
                radio: Default::default(),
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
    fn get_drive_token_is_a_control_request_and_reports_the_bundle() {
        use crate::settings::DriveUpdate;
        assert!(is_control_request(types::GET_DRIVE_TOKEN));
        let s = settings();
        // Unlinked orchestrator: ok with empty fields so the device degrades cleanly.
        let mem = MemoryStore::open_in_memory().unwrap();
        let resp = respond(&WyomingEvent::new(types::GET_DRIVE_TOKEN), &mem, &s, None);
        assert_eq!(resp.event_type, types::DRIVE_TOKEN);
        assert_eq!(resp.data["ok"], json!(true));
        assert_eq!(resp.data["linked"], json!(false));
        assert_eq!(resp.data["configured"], json!(false));
        assert_eq!(resp.data["refresh_token"], json!(""));

        // Fully linked: the bundle carries the creds + token for the device.
        s.apply_drive(&DriveUpdate {
            client_id: Some(Some("cid.apps".into())),
            client_secret: Some(Some("gocspx-secret".into())),
            refresh_token: Some(Some("1//refresh".into())),
            folder_ids: Some(vec!["1AbC".into()]),
            scope: Some(Some("scope-x".into())),
        });
        let resp = respond(&WyomingEvent::new(types::GET_DRIVE_TOKEN), &mem, &s, None);
        assert_eq!(resp.data["linked"], json!(true));
        assert_eq!(resp.data["configured"], json!(true));
        assert_eq!(resp.data["client_id"], json!("cid.apps"));
        assert_eq!(resp.data["client_secret"], json!("gocspx-secret"));
        assert_eq!(resp.data["refresh_token"], json!("1//refresh"));
        assert_eq!(resp.data["folder_ids"][0], json!("1AbC"));
        assert_eq!(resp.data["scope"], json!("scope-x"));
    }

    #[test]
    fn get_camera_stream_serves_the_url_or_a_speakable_failure() {
        assert!(is_control_request(types::GET_CAMERA_STREAM));
        // Unconfigured Core (the default test settings): ok=false + message, no URL.
        let s = settings();
        let mem = MemoryStore::open_in_memory().unwrap();
        let resp = respond(
            &WyomingEvent::get_camera_stream("Front Door"),
            &mem,
            &s,
            None,
        );
        assert_eq!(resp.event_type, types::CAMERA_STREAM);
        assert_eq!(resp.data["ok"], json!(false));
        assert!(resp.data.get("url").is_none());
        assert!(resp.data["message"]
            .as_str()
            .unwrap()
            .contains("isn't set up"));

        let hub = crate::camera::tests::hub();
        let ok = camera_stream_response(&WyomingEvent::get_camera_stream("front door"), Some(&hub));
        assert_eq!(ok.data["ok"], json!(true));
        assert_eq!(ok.data["camera"], json!("Front Door"));
        assert_eq!(
            ok.data["url"],
            json!("rtsp://anamanti:p%40ss%3Aw%2Frd@192.168.1.29:554/h264Preview_05_sub")
        );
        // Blank → the first camera.
        let first = camera_stream_response(&WyomingEvent::get_camera_stream(""), Some(&hub));
        assert_eq!(first.data["camera"], json!("Front Door"));
        let unknown =
            camera_stream_response(&WyomingEvent::get_camera_stream("garage"), Some(&hub));
        assert_eq!(unknown.data["ok"], json!(false));
        assert!(unknown.data.get("url").is_none());
    }

    #[test]
    fn get_functions_is_a_control_request_and_reports_a_snapshot() {
        assert!(is_control_request(types::GET_FUNCTIONS));
        let s = settings(); // mock LLM, no provider keys, nothing linked
        let resp = functions_response(&s, None);
        assert_eq!(resp.event_type, types::FUNCTIONS);
        assert_eq!(resp.data["ok"], json!(true));
        let fns = resp.data["functions"].as_array().unwrap();
        assert!(!fns.is_empty());

        let find = |name: &str| fns.iter().find(|f| f["name"] == json!(name)).unwrap();

        // Mock LLM → local, no key required, so no red warning.
        let llm = find("LLM");
        assert_eq!(llm["mode"], json!("local"));
        assert_eq!(llm["key_required"], json!(false));

        // Places is an optional add-on tool: with no key it's simply off (not a
        // broken setup), so it never raises the red warning — it shows "off" with a
        // note, not key_required.
        let places = find("Places");
        assert_eq!(places["category"], json!("Tools"));
        assert_eq!(places["key_required"], json!(false));
        assert_eq!(places["key_set"], json!(false));
        assert_eq!(places["mode"], json!("off"));
        assert_eq!(places["note"], json!("Needs a Google Places key"));

        // Embeddings with no GraphRAG controller → SQLite FTS, local, keyless.
        let embed = find("Embeddings");
        assert_eq!(embed["category"], json!("Memory"));
        assert_eq!(embed["mode"], json!("local"));
        assert_eq!(embed["note"], json!("GraphRAG disabled"));

        // Every category we promised is present.
        for cat in ["Config", "Tools", "System1", "Memory", "Speech"] {
            assert!(
                fns.iter().any(|f| f["category"] == json!(cat)),
                "missing category {cat}"
            );
        }
    }

    /// Spawn a one-shot fake AppSaid worker that replies to `/v1/pair/claim` with
    /// `status`/`body`, and return its `http://127.0.0.1:PORT` base URL.
    async fn fake_worker(status: &'static str, body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = sock.read(&mut buf).await.unwrap();
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn pair_phone_claims_a_code_and_stores_the_recipient() {
        use crate::settings::AppSaidUpdate;
        assert!(is_control_request(types::PAIR_PHONE));
        let base = fake_worker("200 OK", r#"{"user_key":"gdksxryuunome6dp"}"#).await;
        let s = settings();
        s.apply_appsaid(&AppSaidUpdate {
            worker_url: Some(Some(base)),
            app_token: Some(Some("app-tok".into())),
            ..Default::default()
        });

        let resp = pair_phone_response(
            &req(types::PAIR_PHONE, json!({ "name": "Mom", "code": "ab-2k" })),
            &s,
        )
        .await;
        assert_eq!(resp.event_type, types::PAIR_RESULT);
        assert_eq!(resp.data["ok"], json!(true));
        assert_eq!(resp.data["name"], json!("Mom"));

        // The recipient is stored and the send tool is now fully configured.
        let cfg = s.appsaid();
        assert_eq!(
            cfg.recipients.get("Mom").map(String::as_str),
            Some("gdksxryuunome6dp")
        );
        assert!(cfg.active(), "worker + token + recipient ⇒ tool active");

        // Pairing auto-adds the person to the household with their AppSaid address.
        let member = s.household().member_matching("Mom").cloned();
        assert_eq!(
            member.and_then(|m| m.appsaid).as_deref(),
            Some("gdksxryuunome6dp"),
            "paired person is in the household"
        );

        // The claimed user_key must never ride back down to the device.
        let wire = serde_json::to_string(&resp.data).unwrap();
        assert!(
            !wire.contains("gdksxryuunome6dp"),
            "leaked user_key: {wire}"
        );
    }

    #[tokio::test]
    async fn pair_phone_without_a_name_is_an_in_band_error() {
        let s = settings();
        let resp =
            pair_phone_response(&req(types::PAIR_PHONE, json!({ "code": "ab2k" })), &s).await;
        assert_eq!(resp.data["ok"], json!(false));
        assert!(s.appsaid().recipients.is_empty());
    }

    #[tokio::test]
    async fn pair_phone_surfaces_a_bad_code_without_dropping() {
        use crate::settings::AppSaidUpdate;
        let base = fake_worker("404 Not Found", r#"{"error":"not_found"}"#).await;
        let s = settings();
        s.apply_appsaid(&AppSaidUpdate {
            worker_url: Some(Some(base)),
            app_token: Some(Some("app-tok".into())),
            ..Default::default()
        });
        let resp = pair_phone_response(
            &req(types::PAIR_PHONE, json!({ "name": "Mom", "code": "ZZZZ" })),
            &s,
        )
        .await;
        assert_eq!(resp.event_type, types::PAIR_RESULT);
        assert_eq!(resp.data["ok"], json!(false));
        assert!(
            s.appsaid().recipients.is_empty(),
            "nothing stored on failure"
        );
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

        let deleted = respond(
            &req(types::DELETE_MEMORY, json!({ "id": id })),
            &mem,
            &s,
            None,
        );
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
        assert!(is_control_request(types::ENROLL_FINISH));
        assert!(is_control_request(types::ENROLL_CANCEL));
        assert!(is_control_request(types::LIST_MODELS));
        assert!(is_control_request(types::LIST_VOICES));
        assert!(is_control_request(types::MUSIC_CONTROL));
        assert!(!is_control_request(types::AUDIO_START));
        assert!(!is_control_request(types::TRANSCRIPT));
    }

    #[tokio::test]
    async fn music_control_reports_unlinked_in_band() {
        // The fixture settings have no Spotify link, so any valid action is answered
        // ok:false with a speakable reason rather than dropping the connection.
        let s = settings();
        let np = NowPlayingService::new();
        let resp =
            music_control_response(&WyomingEvent::music_control("pause", None), &s, &np).await;
        assert_eq!(resp.event_type, types::MUSIC_RESULT);
        assert_eq!(resp.data["ok"], json!(false));
        assert!(resp.data["message"].as_str().unwrap().contains("linked"));
    }

    #[tokio::test]
    async fn music_control_display_toggle_is_that_displays_choice_without_spotify() {
        // The on-screen widget toggle is a per-display choice: it succeeds even with
        // Spotify unlinked and changes only the display that pressed it.
        use crate::music::MusicDisplayStyle::{Compact, Fullscreen};
        let s = settings();
        let np = NowPlayingService::new();
        let toggle = |action: &str, device: &str| {
            WyomingEvent::music_control(action, None).with_device_id(device)
        };
        let resp = music_control_response(&toggle("display_fullscreen", "den"), &s, &np).await;
        assert_eq!(resp.data["ok"], json!(true));
        assert_eq!(np.style_for("den"), Fullscreen);
        assert_eq!(
            np.style_for("kitchen"),
            Compact,
            "other displays unaffected"
        );
        let resp = music_control_response(&toggle("display_compact", "den"), &s, &np).await;
        assert_eq!(resp.data["ok"], json!(true));
        assert_eq!(np.style_for("den"), Compact);
        // Nothing household-wide is persisted.
        assert!(s.music_display_pins().is_empty());
        // An older display that sends no id is answered in-band, changing nothing.
        let resp = music_control_response(
            &WyomingEvent::music_control("display_fullscreen", None),
            &s,
            &np,
        )
        .await;
        assert_eq!(resp.data["ok"], json!(false));
        assert!(!np.display().has_choice(""));
        // An unknown style falls through to transport and is rejected as unknown.
        let resp = music_control_response(&toggle("display_sideways", "den"), &s, &np).await;
        assert_eq!(resp.data["ok"], json!(false));
    }

    #[tokio::test]
    async fn music_control_rejects_missing_and_unknown_actions() {
        let s = settings();
        let np = NowPlayingService::new();
        // Missing action.
        let resp = music_control_response(
            &WyomingEvent::with_data(types::MUSIC_CONTROL, json!({ "value": 50 })),
            &s,
            &np,
        )
        .await;
        assert_eq!(resp.data["ok"], json!(false));
        assert!(resp.data["message"].as_str().unwrap().contains("action"));

        // Unknown action (recognized before the link check, so it reports the action).
        let resp =
            music_control_response(&WyomingEvent::music_control("teleport", None), &s, &np).await;
        assert_eq!(resp.data["ok"], json!(false));
        assert!(resp.data["message"].as_str().unwrap().contains("teleport"));

        // `volume` with no level is rejected rather than silently muting.
        let resp =
            music_control_response(&WyomingEvent::music_control("volume", None), &s, &np).await;
        assert_eq!(resp.data["ok"], json!(false));
        assert!(resp.data["message"].as_str().unwrap().contains("level"));
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

    /// A speaker service over an in-memory registry (no clips).
    fn speaker_service() -> crate::speaker::SpeakerService {
        use crate::speaker::{MockSpeakerEmbedder, SpeakerService, SpeakerThresholds};
        SpeakerService::new(
            Arc::new(MockSpeakerEmbedder::default()),
            SpeakerRegistry::open_in_memory().unwrap(),
            SpeakerThresholds::default(),
        )
    }

    #[tokio::test]
    async fn list_name_and_delete_speakers() {
        use crate::speaker::{MockSpeakerEmbedder, SpeakerEmbedder};
        let s = settings();
        let mem = MemoryStore::open_in_memory().unwrap();
        let svc = speaker_service();
        let reg = svc.registry();
        let e = MockSpeakerEmbedder::default();
        let id = reg
            .create_cluster(&e.embed(&vec![1000i16; 20_000]).unwrap())
            .unwrap();

        // List shows the anonymous cluster.
        let listed = respond(
            &WyomingEvent::new(types::LIST_SPEAKERS),
            &mem,
            &s,
            Some(reg),
        );
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
            Some(reg),
        );
        assert_eq!(named.event_type, types::SPEAKER_RESULT);
        assert_eq!(named.data["ok"], json!(true));
        assert_eq!(reg.get(&id).unwrap().unwrap().name.as_deref(), Some("Sam"));

        // Naming an unknown speaker fails in-band.
        let bad = respond(
            &req(types::NAME_SPEAKER, json!({ "id": "nope", "name": "X" })),
            &mem,
            &s,
            Some(reg),
        );
        assert_eq!(bad.data["ok"], json!(false));

        // Delete it (through the SpeakerService, which also detaches memories).
        mem.add_scoped(
            MemoryKind::Fact,
            "likes tea",
            MemorySource::Explicit,
            Some(&id),
        )
        .unwrap();
        let del = speaker_change_response(
            &req(types::DELETE_SPEAKER, json!({ "id": id })),
            &mem,
            Some(&svc),
            None,
        )
        .await;
        assert_eq!(del.event_type, types::SPEAKER_RESULT);
        assert_eq!(del.data["ok"], json!(true));
        assert_eq!(reg.count().unwrap(), 0);
        assert_eq!(
            mem.list().unwrap()[0].speaker_id,
            None,
            "memory kept, now shared"
        );

        // Deleting again, or without speaker ID, fails in-band.
        let again = speaker_change_response(
            &req(types::DELETE_SPEAKER, json!({ "id": id })),
            &mem,
            Some(&svc),
            None,
        )
        .await;
        assert_eq!(again.data["ok"], json!(false));
        let disabled = speaker_change_response(
            &req(types::DELETE_SPEAKER, json!({ "id": id })),
            &mem,
            None,
            None,
        )
        .await;
        assert_eq!(disabled.data["ok"], json!(false));
    }

    #[test]
    fn successful_enrollment_reports_unattributed_memories_that_may_be_theirs() {
        use crate::memory::{MemoryKind, MemoryScope, MemorySource, Provenance};
        let mem = MemoryStore::open_in_memory().unwrap();
        mem.add_in(
            MemoryKind::Fact,
            "The user's name is Brendan",
            MemorySource::Explicit,
            &MemoryScope::Unknown {
                candidate: Some("spk-b".into()),
            },
            &Provenance::default(),
        )
        .unwrap();
        let ok = enroll_result(json!({ "ok": true, "phase": "finish", "speaker_id": "spk-b" }));
        assert_eq!(
            with_pending_unknown(ok, &mem).data["pending_unknown"],
            json!(1)
        );
        let failed = enroll_result(json!({ "ok": false, "phase": "finish" }));
        assert!(with_pending_unknown(failed, &mem)
            .data
            .get("pending_unknown")
            .is_none());
    }

    #[test]
    fn enroll_finish_commits_and_cancel_clears() {
        use crate::speaker::{
            MockSpeakerEmbedder, SpeakerRegistry, SpeakerService, SpeakerThresholds,
        };
        use std::sync::Arc;

        let s = settings();
        // Disabled (no service) → an in-band failure, not a panic.
        let disabled = enroll_finish(
            &req(
                types::ENROLL_FINISH,
                json!({ "session": "s", "name": "Dana" }),
            ),
            None,
            &s,
        );
        assert_eq!(disabled.event_type, types::ENROLL_RESULT);
        assert_eq!(disabled.data["ok"], json!(false));

        let svc = SpeakerService::new(
            Arc::new(MockSpeakerEmbedder::default()),
            SpeakerRegistry::open_in_memory().unwrap(),
            SpeakerThresholds::default(),
        );
        let pcm = vec![1200i16; 20_000]; // > the 1200 ms floor

        // Too few samples → commit refused, session preserved.
        svc.enroll_sample("s", &pcm).unwrap();
        let early = enroll_finish(
            &req(
                types::ENROLL_FINISH,
                json!({ "session": "s", "name": "Dana" }),
            ),
            Some(&svc),
            &s,
        );
        assert_eq!(early.data["ok"], json!(false));
        assert!(
            s.household().member_matching("Dana").is_none(),
            "a failed commit adds no one to the household"
        );

        // Enough samples → a named, labeled profile is created.
        svc.enroll_sample("s", &pcm).unwrap();
        svc.enroll_sample("s", &pcm).unwrap();
        let done = enroll_finish(
            &req(
                types::ENROLL_FINISH,
                json!({ "session": "s", "name": "Dana" }),
            ),
            Some(&svc),
            &s,
        );
        assert_eq!(done.event_type, types::ENROLL_RESULT);
        assert_eq!(done.data["ok"], json!(true));
        assert_eq!(done.data["phase"], json!("finish"));
        assert_eq!(done.data["name"], json!("Dana"));
        assert_eq!(done.data["existing_member"], json!(false));
        assert_eq!(svc.registry().count().unwrap(), 1);
        // A new person is added to the household roster.
        assert!(s.household().member_matching("Dana").is_some());

        // Cancel discards an in-flight session.
        svc.enroll_sample("s2", &pcm).unwrap();
        let cancelled = enroll_cancel(
            &req(types::ENROLL_CANCEL, json!({ "session": "s2" })),
            Some(&svc),
        );
        assert_eq!(cancelled.data["ok"], json!(true));
        assert_eq!(svc.enroll_count("s2"), 0);
    }

    #[test]
    fn enroll_finish_attaches_to_an_existing_household_member() {
        use crate::settings::{Household, HouseholdMember};
        use crate::speaker::{
            MockSpeakerEmbedder, SpeakerRegistry, SpeakerService, SpeakerThresholds,
        };
        use std::sync::Arc;

        let s = settings();
        s.apply_household(&Household {
            members: vec![HouseholdMember {
                name: "Dana Smith".into(),
                emails: vec!["dana@example.com".into()],
                ..Default::default()
            }],
            ..Default::default()
        });

        // The People listing carries the roster's names (and no contact details).
        let list = respond(
            &WyomingEvent::new(types::LIST_SPEAKERS),
            &MemoryStore::open_in_memory().unwrap(),
            &s,
            None,
        );
        assert_eq!(list.data["members"], json!(["Dana Smith"]));
        assert!(!list.data.to_string().contains("dana@example.com"));

        let svc = SpeakerService::new(
            Arc::new(MockSpeakerEmbedder::default()),
            SpeakerRegistry::open_in_memory().unwrap(),
            SpeakerThresholds::default(),
        );
        let pcm = vec![1200i16; 20_000];
        let enroll = |session: &str, name: &str| {
            for _ in 0..3 {
                svc.enroll_sample(session, &pcm).unwrap();
            }
            enroll_finish(
                &req(
                    types::ENROLL_FINISH,
                    json!({ "session": session, "name": name }),
                ),
                Some(&svc),
                &s,
            )
        };

        // Picking the member (any case) saves under their canonical name and doesn't
        // touch the roster.
        let first = enroll("a", "dana smith");
        assert_eq!(first.data["ok"], json!(true));
        assert_eq!(first.data["name"], json!("Dana Smith"));
        assert_eq!(first.data["existing_member"], json!(true));
        assert_eq!(first.data["updated"], json!(false));
        let roster = s.household().members;
        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0].emails, vec!["dana@example.com".to_string()]);

        // Enrolling them again folds into the same voiceprint — no duplicate.
        let again = enroll("b", "Dana Smith");
        assert_eq!(again.data["updated"], json!(true));
        assert_eq!(again.data["speaker_id"], first.data["speaker_id"]);
        assert_eq!(svc.registry().count().unwrap(), 1);
        // "Add voice samples" targets the profile by id, needs no name, and leaves the
        // household roster alone.
        for _ in 0..3 {
            svc.enroll_sample("c", &pcm).unwrap();
        }
        let id = first.data["speaker_id"].as_str().unwrap().to_string();
        let more = enroll_finish(
            &req(
                types::ENROLL_FINISH,
                json!({ "session": "c", "speaker_id": id }),
            ),
            Some(&svc),
            &s,
        );
        assert_eq!(more.data["ok"], json!(true));
        assert_eq!(more.data["speaker_id"], json!(id));
        assert_eq!(more.data["name"], json!("Dana Smith"));
        assert_eq!(svc.registry().count().unwrap(), 1);
        assert_eq!(svc.registry().get(&id).unwrap().unwrap().samples, 9);
        assert_eq!(s.household().members.len(), 1);
    }

    #[tokio::test]
    async fn link_speaker_names_or_merges_into_a_household_member() {
        use crate::memory::{MemoryKind, MemorySource};
        use crate::settings::{Household, HouseholdMember};
        use crate::speaker::{MockSpeakerEmbedder, SpeakerEmbedder};
        assert!(is_control_request(types::LINK_SPEAKER));
        let mem = MemoryStore::open_in_memory().unwrap();
        let s = settings();
        s.apply_household(&Household {
            members: vec![HouseholdMember {
                name: "Christina".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        let svc = speaker_service();
        let reg = svc.registry();
        let e = MockSpeakerEmbedder::default();
        let a = reg
            .create_cluster(&e.embed(&vec![1000i16; 20_000]).unwrap())
            .unwrap();
        let b = reg
            .create_cluster(&e.embed(&vec![500i16; 20_000]).unwrap())
            .unwrap();
        mem.add_scoped(
            MemoryKind::Fact,
            "likes tea",
            MemorySource::Explicit,
            Some(&b),
        )
        .unwrap();
        let link = |id: &str, member: &str| {
            let r = req(types::LINK_SPEAKER, json!({ "id": id, "member": member }));
            let (mem, svc, s) = (&mem, &svc, &s);
            async move { link_speaker_response(&r, mem, Some(svc), None, s).await }
        };

        // Not a household member → refused, nothing changes.
        let bad = link(&a, "Zed").await;
        assert_eq!(bad.data["ok"], json!(false));
        assert!(bad.data["message"]
            .as_str()
            .unwrap()
            .contains("isn't in the household"));

        // Christina has no voiceprint yet → this voice is simply named as her
        // (canonical spelling from the roster).
        let first = link(&a, "christina").await;
        assert_eq!(first.data["ok"], json!(true));
        assert_eq!(first.data["merged"], json!(false));
        assert_eq!(first.data["name"], json!("Christina"));
        assert_eq!(
            reg.get(&a).unwrap().unwrap().name.as_deref(),
            Some("Christina")
        );

        // A second voice linked to Christina is merged into her voiceprint, carrying
        // its memories along.
        let second = link(&b, "Christina").await;
        assert_eq!(second.data["ok"], json!(true));
        assert_eq!(second.data["merged"], json!(true));
        assert_eq!(second.data["speaker_id"], json!(a));
        assert_eq!(reg.count().unwrap(), 1, "one voiceprint per person");
        let hits = mem.search_scoped("tea", Some(&a), 10).unwrap();
        assert!(hits.iter().any(|m| m.content == "likes tea"));

        // Disabled speaker ID → in-band failure.
        let off = link_speaker_response(
            &req(
                types::LINK_SPEAKER,
                json!({ "id": a, "member": "Christina" }),
            ),
            &mem,
            None,
            None,
            &s,
        )
        .await;
        assert_eq!(off.data["ok"], json!(false));
    }

    #[tokio::test]
    async fn merge_speakers_moves_memories() {
        use crate::memory::{MemoryKind, MemorySource};
        use crate::speaker::{MockSpeakerEmbedder, SpeakerEmbedder};
        let mem = MemoryStore::open_in_memory().unwrap();
        let svc = speaker_service();
        let reg = svc.registry();
        let e = MockSpeakerEmbedder::default();
        let keep = reg
            .create_cluster(&e.embed(&vec![1000i16; 20_000]).unwrap())
            .unwrap();
        let drop = reg
            .create_cluster(&e.embed(&vec![500i16; 20_000]).unwrap())
            .unwrap();
        mem.add_scoped(
            MemoryKind::Fact,
            "likes tea",
            MemorySource::Explicit,
            Some(&drop),
        )
        .unwrap();

        let resp = speaker_change_response(
            &req(types::MERGE_SPEAKERS, json!({ "keep": keep, "drop": drop })),
            &mem,
            Some(&svc),
            None,
        )
        .await;
        assert_eq!(resp.event_type, types::SPEAKER_RESULT);
        assert_eq!(resp.data["ok"], json!(true));
        assert!(resp.data["message"]
            .as_str()
            .unwrap()
            .contains("1 memories moved"));
        assert_eq!(reg.count().unwrap(), 1, "dropped profile removed");
        // The dropped speaker's memory now belongs to the kept speaker.
        let hits = mem.search_scoped("tea", Some(&keep), 10).unwrap();
        assert!(hits.iter().any(|m| m.content == "likes tea"));
    }

    #[tokio::test]
    async fn list_models_returns_the_static_fallback_without_keys() {
        // No provider keys → the catalog serves its curated fallback list.
        let catalog = ModelCatalog::new("http://unused", None, "http://unused", None);
        let resp = models_response(&catalog).await;
        assert_eq!(resp.event_type, types::MODELS);
        assert_eq!(resp.data["ok"], json!(true));
        let models = resp.data["models"].as_array().unwrap();
        assert!(models
            .iter()
            .any(|m| m["provider"] == json!("anthropic") && m["id"] == json!("claude-opus-5")));
        assert!(models
            .iter()
            .any(|m| m["provider"] == json!("openai") && m["id"] == json!("gpt-4o-mini")));
    }

    // ---- ambient-list-voices ----

    /// A connector whose `connect_tts` answers a `describe` with a fixed voice
    /// catalog (amy, lessac, and a not-installed German voice). `connect_stt` is
    /// never used by the voice path.
    struct VoiceConnector;

    #[async_trait::async_trait]
    impl crate::orchestrator::ServiceConnector for VoiceConnector {
        async fn connect_stt(&self) -> Result<crate::wyoming::DynConnection> {
            anyhow::bail!("stt not used in voice tests")
        }

        async fn connect_tts(&self) -> Result<crate::wyoming::DynConnection> {
            use crate::wyoming::protocol::write_event;
            let (client, server) = tokio::io::duplex(64 * 1024);
            tokio::spawn(async move {
                let (r, w) = tokio::io::split(server);
                let mut reader = tokio::io::BufReader::new(r);
                let mut writer = w;
                // Wait for the describe, then answer with an info catalog.
                let _ = crate::wyoming::protocol::read_event(&mut reader).await;
                let info = WyomingEvent::with_data(
                    types::INFO,
                    json!({ "tts": [{ "voices": [
                        { "name": "en_US-amy-medium", "languages": ["en_US"], "description": "amy (medium)" },
                        { "name": "en_US-lessac-medium", "languages": ["en_US"], "description": "lessac (medium)" },
                        { "name": "de_DE-thorsten-low", "languages": ["de_DE"], "description": "thorsten (low)" },
                    ]}]}),
                );
                let _ = write_event(&mut writer, &info).await;
            });
            let (r, w) = tokio::io::split(client);
            Ok(crate::wyoming::DynConnection::from_io(r, w))
        }
    }

    /// Create a unique temp dir seeded with `<name>.onnx` (+ a sibling `.onnx.json`
    /// that must be ignored) for each installed voice. Returned dir is caller-owned.
    fn temp_voices_dir(installed: &[&str]) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("ambient-voices-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in installed {
            std::fs::write(dir.join(format!("{name}.onnx")), b"").unwrap();
            std::fs::write(dir.join(format!("{name}.onnx.json")), b"{}").unwrap();
        }
        dir
    }

    #[tokio::test]
    async fn voices_response_filters_to_installed_when_dir_is_set() {
        let dir = temp_voices_dir(&["en_US-amy-medium", "en_US-lessac-medium"]);
        let resp = voices_response(&VoiceConnector, Some(&dir)).await;
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(resp.event_type, types::VOICES);
        assert_eq!(resp.data["ok"], json!(true));
        let voices = resp.data["voices"].as_array().unwrap();
        let names: Vec<&str> = voices.iter().map(|v| v["name"].as_str().unwrap()).collect();
        // Only the two installed voices survive; the German voice is filtered out.
        assert_eq!(names, vec!["en_US-amy-medium", "en_US-lessac-medium"]);
        assert_eq!(voices[0]["language"], json!("en_US"));
        assert_eq!(voices[0]["label"], json!("amy (medium)"));
    }

    #[tokio::test]
    async fn voices_response_returns_full_catalog_without_a_dir() {
        // No voices dir configured (remote Piper): the full advertised list is returned.
        let resp = voices_response(&VoiceConnector, None).await;
        assert_eq!(resp.data["ok"], json!(true));
        let voices = resp.data["voices"].as_array().unwrap();
        assert_eq!(voices.len(), 3);
    }

    #[tokio::test]
    async fn voices_response_reports_scan_error_in_band() {
        // A missing voices dir surfaces as ok:false, never a dropped connection.
        let missing = std::env::temp_dir().join("ambient-voices-does-not-exist-xyz");
        let resp = voices_response(&VoiceConnector, Some(&missing)).await;
        assert_eq!(resp.event_type, types::VOICES);
        assert_eq!(resp.data["ok"], json!(false));
        assert_eq!(resp.data["voices"].as_array().unwrap().len(), 0);
    }
}
