//! FRB surface for the Phase-6 settings screen (Plan.MD §3, Phase 6).
//!
//! The on-device settings screen manages state that lives on the **orchestrator**:
//! the runtime LLM backend + Piper voice, and the persistent memory list. These
//! functions are the Dart→Rust control calls (architecture.md §3): each discovers
//! the Mac's Wyoming host over mDNS, opens a short-lived connection, sends one
//! project-local `anamanti-*` control frame, and returns the parsed response.
//!
//! Device-local settings (wake word, thresholds, photo source) do **not** go
//! through here — they are applied by restarting the engine with a new
//! [`crate::api::engine::WakeWordConfig`] and are persisted on the Flutter side.
//!
//! Each call is a self-contained blocking operation: it stands up a tiny
//! current-thread `tokio` runtime and drives the async round trip to completion.
//! FRB runs these off the Dart UI isolate, so the returned `Future` never blocks
//! the UI. They are deliberately *not* `#[frb(sync)]`.

use std::time::Duration;

use anyhow::Result;

use crate::wyoming::control;
use crate::wyoming::discovery::{EndpointCache, DEFAULT_DISCOVERY_TIMEOUT};

/// The orchestrator's runtime settings, as reported by a describe/set response.
#[derive(Debug, Clone)]
pub struct OrchestratorSettings {
    /// Whether the request succeeded (a rejected change reports `false` with a
    /// human-readable `message`, leaving the previous settings in effect).
    pub ok: bool,
    /// Human-readable status/error (e.g. why a backend change was rejected).
    pub message: String,
    /// The active LLM backend label (`ollama` / `anthropic` / `openai` / `mock`).
    pub llm_backend: String,
    /// The active model name, if the backend uses one.
    pub llm_model: Option<String>,
    /// Anthropic auth mode: `apikey` or `subscription` (Claude OAuth).
    pub anthropic_auth: String,
    /// The active Piper voice, or `None` for the server default.
    pub tts_voice: Option<String>,
    /// Orchestrator VAD: end-of-speech trailing silence in ms (0 if unknown).
    pub end_silence_ms: u32,
    /// Orchestrator VAD: speech-vs-noise RMS threshold (0 if unknown).
    pub voice_rms_threshold: f64,
    /// Orchestrator VAD engine label: `energy` or `silero` (empty if unknown).
    pub vad_engine: String,
    /// Orchestrator VAD: Silero speech-probability gate, `0.0..1.0` (0 if unknown).
    pub silero_threshold: f64,
}

/// One selectable LLM model for the settings model dropdown, as reported by the
/// orchestrator's catalog (scoped to the last 12 months per provider).
#[derive(Debug, Clone)]
pub struct ModelInfo {
    /// `anthropic` or `openai`.
    pub provider: String,
    /// The model id to send as the backend's model (e.g. `claude-opus-5`).
    pub id: String,
    /// A human-friendly label for the dropdown (falls back to `id`).
    pub label: String,
}

/// One selectable Piper voice for the settings TTS voice dropdown, as reported by
/// the orchestrator (its intersection of Piper's advertised catalog with the voices
/// installed on disk).
#[derive(Debug, Clone)]
pub struct VoiceInfo {
    /// Piper voice id sent as `tts_voice` (e.g. `en_US-amy-medium`).
    pub name: String,
    /// Primary locale (e.g. `en_US`), or `None` if the server didn't report one.
    pub language: Option<String>,
    /// A human-friendly label for the dropdown (falls back to `name`).
    pub label: String,
}

/// The Google Drive photo-slideshow bundle the orchestrator owns, pulled by the
/// device to drive the idle slideshow. The device mints Drive access tokens
/// on-device from `refresh_token` using `client_id`/`client_secret`, so the tablet
/// APK ships with no baked-in Google credentials. When the orchestrator isn't linked
/// the fields are empty and `linked`/`configured` are false (the device then falls
/// back to local gradient images).
#[derive(Debug, Clone)]
pub struct DriveToken {
    /// True when a refresh token is present (Drive is fully linked and usable).
    pub linked: bool,
    /// True when the OAuth client id + secret are present (access tokens can be
    /// minted). Mirrors the old build-time `kGoogleDriveConfigured`, now runtime.
    pub configured: bool,
    /// "Desktop app" OAuth client id (empty when unset).
    pub client_id: String,
    /// "Desktop app" OAuth client secret (empty when unset).
    pub client_secret: String,
    /// Long-lived refresh token (empty when not linked).
    pub refresh_token: String,
    /// Drive folder ids the slideshow reads images from.
    pub folder_ids: Vec<String>,
    /// OAuth scope granted (informational).
    pub scope: String,
}

/// The live stream for one camera, pulled from the Core each time the camera view
/// opens (`plans/CameraPlan.md`, `anamanti-get-camera-stream` → `anamanti-camera-stream`).
///
/// **`url` embeds the NVR credentials** (`rtsp://user:pass@host:554/...`): the device
/// holds it in memory only — never persisted, never logged. For that reason this type
/// deliberately has a hand-written `Debug` that redacts `url` (in
/// `wyoming/control.rs`, outside the FRB-scanned `api` module).
#[derive(Clone)]
pub struct CameraStream {
    /// Whether the Core resolved a stream. When `false`, `message` is a speakable
    /// reason and `camera`/`url` are empty.
    pub ok: bool,
    /// The resolved camera display name (e.g. `"Front Door"`).
    pub camera: String,
    /// The `rtsp://` URL **with credentials embedded**. Empty when `ok` is false.
    pub url: String,
    /// A human-readable failure reason (empty on success).
    pub message: String,
}

/// One capability row in the Core's "functions" snapshot, shown read-only on the
/// device Settings screen. Describes whether a function runs in the `cloud` or
/// `local`ly (or is `off`), which model it uses, and whether a required API key is
/// set — never the key itself. Built by the Core's `functions_response`.
#[derive(Debug, Clone)]
pub struct FunctionStatus {
    /// Grouping header: `Config` | `Tools` | `System1` | `Memory` | `Speech`.
    pub category: String,
    /// Human-friendly capability name (e.g. `LLM`, `Weather`, `Embeddings`).
    pub name: String,
    /// Where it runs: `cloud` | `local` | `off`.
    pub mode: String,
    /// Provider label (e.g. `Anthropic (Claude)`, `Mapbox`); empty when n/a.
    pub provider: String,
    /// Selected model / voice / dims; empty when n/a.
    pub model: String,
    /// Whether this function needs an API key / credential to work.
    pub key_required: bool,
    /// Whether that key/credential is currently set on the Core.
    pub key_set: bool,
    /// Optional nuance (e.g. a keyless fallback); empty when none.
    pub note: String,
}

/// One discovered orchestrator, for the settings "Orchestrator" dropdown.
#[derive(Debug, Clone)]
pub struct OrchestratorInfo {
    /// Stable selection key (TXT `instance_id`) the device persists to pin this
    /// orchestrator across restarts / IP changes.
    pub key: String,
    /// Human-friendly label (TXT `name`) shown in the dropdown.
    pub name: String,
    /// Resolved LAN address (for display / diagnostics).
    pub host: String,
    /// Resolved Wyoming port.
    pub port: u16,
}

/// One persistent memory entry, for the settings memory list.
#[derive(Debug, Clone)]
pub struct MemoryEntry {
    pub id: i64,
    /// `fact` or `preference`.
    pub kind: String,
    pub content: String,
    /// `explicit` (user asked) or `inferred` (auto-extracted).
    pub source: String,
    /// Unix seconds when the entry was stored.
    pub created_at: i64,
}

/// One identified speaker, for the settings "People" list (speaker_id_plan.md
/// Phase C).
#[derive(Debug, Clone)]
pub struct SpeakerInfo {
    /// Stable id (`spk-…`) referenced by memory + the graph.
    pub id: String,
    /// User-given name, or `None` while the cluster is still anonymous.
    pub name: Option<String>,
    /// Whether a person has named this cluster (vs. auto-created).
    pub labeled: bool,
    /// How many utterances back this voiceprint.
    pub samples: i64,
    /// Unix seconds when the cluster was first heard.
    pub created_at: i64,
}

/// One stored voice clip for a speaker, for the People screen's clip playback
/// (speaker_id_plan.md Phase C).
#[derive(Debug, Clone)]
pub struct SpeakerClipInfo {
    /// Opaque clip id (the Core's on-disk file stem); the key to fetch/play it.
    pub name: String,
    /// Capture time in Unix milliseconds (newest-first ordering).
    pub created_ms: i64,
    /// Number of `i16` samples in the clip.
    pub samples: i64,
    /// Sample rate of the clip's PCM in Hz (16 kHz house format).
    pub sample_rate: u32,
}

/// One entity ("keyword") a turn mentions, decorating a [`TurnInfo`] row.
#[derive(Debug, Clone)]
pub struct EntityRef {
    /// Canonical surface form (e.g. `jazz`, `Portland`, `Sam`).
    pub name: String,
    /// Coarse type: `person` | `place` | `thing` | `topic` (free-form).
    pub kind: String,
}

/// One stored conversation turn from the GraphRAG graph, for the device Memory
/// screen's "Conversations" tab. Built by the Core's `list_turns`.
#[derive(Debug, Clone)]
pub struct TurnInfo {
    /// Graph node id (`$id`), used to delete this turn.
    pub id: i64,
    /// The chat-log record id this turn was ingested from.
    pub ext_id: String,
    /// The conversation session id.
    pub session: String,
    /// Unix seconds when the turn happened.
    pub ts: i64,
    /// What the user said (the STT transcript).
    pub transcript: String,
    /// What the assistant replied.
    pub reply: String,
    /// Speaker id (`spk-…`, or `household` for the shared/unattributed user).
    pub speaker_id: String,
    /// Entities ("keywords") this turn mentions.
    pub entities: Vec<EntityRef>,
}

/// One stored entity ("keyword") from the GraphRAG graph, for the device Memory
/// screen's "Keywords" tab. Built by the Core's `list_entities`.
#[derive(Debug, Clone)]
pub struct EntityInfo {
    /// Graph node id (`$id`), used to delete this entity.
    pub id: i64,
    /// Canonical surface form.
    pub name: String,
    /// Coarse type: `person` | `place` | `thing` | `topic`.
    pub kind: String,
}

/// A requested settings change from the screen. Absent fields are left unchanged.
#[derive(Debug, Clone)]
pub struct SettingsUpdate {
    /// New LLM backend label, or `None` to leave it unchanged.
    pub llm_backend: Option<String>,
    /// New model name, or `None` to leave it unchanged.
    pub llm_model: Option<String>,
    /// New Anthropic auth mode (`apikey`/`subscription`), or `None` to leave it.
    pub anthropic_auth: Option<String>,
    /// When `true`, apply `tts_voice` (a `None`/empty value clears the voice); when
    /// `false`, leave the voice unchanged.
    pub set_tts_voice: bool,
    /// The voice to set when `set_tts_voice` is `true`.
    pub tts_voice: Option<String>,
    /// New orchestrator VAD end-of-speech silence (ms), or `None` to leave it.
    pub end_silence_ms: Option<u32>,
    /// New orchestrator VAD speech RMS threshold, or `None` to leave it.
    pub voice_rms_threshold: Option<f64>,
    /// New VAD engine label (`energy`/`silero`), or `None` to leave it unchanged.
    pub vad_engine: Option<String>,
    /// New Silero speech-probability gate (`0.0..1.0`), or `None` to leave it.
    pub silero_threshold: Option<f64>,
}

fn timeout(secs: u64) -> Duration {
    match secs {
        0 => DEFAULT_DISCOVERY_TIMEOUT,
        n => Duration::from_secs(n),
    }
}

/// Run one async control op to completion on a private current-thread runtime.
fn block_on<F, T>(fut: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(fut)
}

/// Normalize a persisted selection key: empty (Auto) → `None`, else the trimmed
/// key. Every control call is pinned through this so the settings/control path
/// hits the *same* orchestrator the voice-turn path does.
fn preferred(key: &str) -> Option<&str> {
    let k = key.trim();
    if k.is_empty() {
        None
    } else {
        Some(k)
    }
}

/// Discover every orchestrator on the LAN for the settings "Orchestrator"
/// dropdown. Pure mDNS — unfiltered by any current selection — so the picker can
/// always show all choices (including ones the device isn't currently pinned to).
pub fn list_orchestrators(discovery_timeout_secs: u64) -> Result<Vec<OrchestratorInfo>> {
    block_on(async move {
        let endpoints =
            crate::wyoming::discovery::discover_all(timeout(discovery_timeout_secs)).await?;
        Ok(endpoints
            .into_iter()
            .map(|e| OrchestratorInfo {
                key: e.key,
                name: e.name,
                host: e.address.to_string(),
                port: e.port,
            })
            .collect())
    })
}

/// Read the orchestrator's current runtime settings.
pub fn fetch_orchestrator_settings(
    orchestrator_key: String,
    discovery_timeout_secs: u64,
) -> Result<OrchestratorSettings> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::describe_settings(
            &cache,
            timeout(discovery_timeout_secs),
            preferred(&orchestrator_key),
        )
        .await
    })
}

/// Apply a settings change on the orchestrator and return the resulting settings.
pub fn update_orchestrator_settings(
    orchestrator_key: String,
    update: SettingsUpdate,
    discovery_timeout_secs: u64,
) -> Result<OrchestratorSettings> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::update_settings(
            &cache,
            timeout(discovery_timeout_secs),
            preferred(&orchestrator_key),
            &update,
        )
        .await
    })
}

/// List the orchestrator's selectable LLM models (Anthropic + OpenAI, scoped to the
/// last 12 months) for the settings model dropdown.
pub fn list_models(
    orchestrator_key: String,
    discovery_timeout_secs: u64,
) -> Result<Vec<ModelInfo>> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::list_models(
            &cache,
            timeout(discovery_timeout_secs),
            preferred(&orchestrator_key),
        )
        .await
    })
}

/// List the installed Piper voices for the settings TTS voice dropdown.
pub fn list_voices(
    orchestrator_key: String,
    discovery_timeout_secs: u64,
) -> Result<Vec<VoiceInfo>> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::list_voices(
            &cache,
            timeout(discovery_timeout_secs),
            preferred(&orchestrator_key),
        )
        .await
    })
}

/// Fetch the Google Drive photo bundle (client creds + refresh token + folder ids)
/// from the orchestrator, for the idle photo slideshow. Called at boot / on the
/// periodic photo refresh; the device stores the result in app settings and mints
/// Drive access tokens on-device.
pub fn get_drive_token(
    orchestrator_key: String,
    discovery_timeout_secs: u64,
) -> Result<DriveToken> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::get_drive_token(
            &cache,
            timeout(discovery_timeout_secs),
            preferred(&orchestrator_key),
        )
        .await
    })
}

/// Fetch the live stream for the camera named `camera` (its display name from the
/// `ShowCamera` event; blank = the Core's first configured camera), each time the
/// camera view opens. The returned `url` embeds the NVR credentials: the caller keeps
/// it in memory only and must never persist or log it. A Core-side failure comes back
/// as `ok: false` with a speakable `message`; a transport failure (no Core, timeout —
/// e.g. an older Core that doesn't know the frame) is an `Err`.
pub fn get_camera_stream(
    orchestrator_key: String,
    discovery_timeout_secs: u64,
    camera: String,
) -> Result<CameraStream> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::get_camera_stream(
            &cache,
            timeout(discovery_timeout_secs),
            preferred(&orchestrator_key),
            &camera,
        )
        .await
    })
}

/// Fetch the read-only snapshot of the Core's configured functions (cloud/local,
/// model, and whether each required API key is set) for the Settings screen's
/// "Functions" section. Never returns secret values. An older Core that doesn't
/// recognize the frame surfaces as the round-trip timeout error.
pub fn get_functions(
    orchestrator_key: String,
    discovery_timeout_secs: u64,
) -> Result<Vec<FunctionStatus>> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::get_functions(
            &cache,
            timeout(discovery_timeout_secs),
            preferred(&orchestrator_key),
        )
        .await
    })
}

/// List all persistent memory entries (settings memory management view).
pub fn list_memories(
    orchestrator_key: String,
    discovery_timeout_secs: u64,
) -> Result<Vec<MemoryEntry>> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::list_memories(
            &cache,
            timeout(discovery_timeout_secs),
            preferred(&orchestrator_key),
        )
        .await
    })
}

/// Delete one memory entry by id. Returns whether a row was removed.
pub fn delete_memory(
    orchestrator_key: String,
    id: i64,
    discovery_timeout_secs: u64,
) -> Result<bool> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::delete_memory(
            &cache,
            timeout(discovery_timeout_secs),
            preferred(&orchestrator_key),
            id,
        )
        .await
    })
}

/// Delete every memory entry. Returns the number removed.
pub fn clear_memories(orchestrator_key: String, discovery_timeout_secs: u64) -> Result<u32> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::clear_memories(
            &cache,
            timeout(discovery_timeout_secs),
            preferred(&orchestrator_key),
        )
        .await
    })
}

/// The Core the on-screen music controls last reached, kept for the life of the process
/// so a burst of taps (a volume drag) skips the per-call mDNS browse. See
/// [`control::music_control`] for how a stale entry is dropped.
static MUSIC_CONTROL_CACHE: EndpointCache = EndpointCache::new();

/// Issue a music transport/volume command from the display's on-screen controls
/// (`action` = `pause`/`resume`/`next`/`previous`/`volume`). Returns the orchestrator's
/// short confirmation; an in-band failure (Spotify not linked, device asleep, unknown
/// action) surfaces as an `Err`. `value` is the 0–100 percent for `"volume"` and ignored
/// otherwise.
pub fn music_control(
    orchestrator_key: String,
    action: String,
    value: Option<i64>,
    discovery_timeout_secs: u64,
) -> Result<String> {
    block_on(async move {
        control::music_control(
            &MUSIC_CONTROL_CACHE,
            timeout(discovery_timeout_secs),
            preferred(&orchestrator_key),
            &action,
            value,
            // Which display pressed it, so the widget toggle changes only this display.
            &crate::engine::device_id(),
        )
        .await
    })
}

/// Browse stored conversation turns from the GraphRAG graph (Memory screen →
/// Conversations), newest first. All filters optional: `query` (substring over the
/// turn text), `speaker_id` (that speaker only), `entity` (turns mentioning that
/// keyword). An older Core (or the SQLite recall backend) surfaces as an error.
pub fn list_turns(
    orchestrator_key: String,
    query: String,
    speaker_id: String,
    entity: String,
    limit: u32,
    discovery_timeout_secs: u64,
) -> Result<Vec<TurnInfo>> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::list_turns(
            &cache,
            timeout(discovery_timeout_secs),
            preferred(&orchestrator_key),
            &query,
            &speaker_id,
            &entity,
            limit,
        )
        .await
    })
}

/// Browse stored entities / "keywords" from the GraphRAG graph (Memory screen →
/// Keywords). `query` is an optional substring filter on the name.
pub fn list_entities(
    orchestrator_key: String,
    query: String,
    limit: u32,
    discovery_timeout_secs: u64,
) -> Result<Vec<EntityInfo>> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::list_entities(
            &cache,
            timeout(discovery_timeout_secs),
            preferred(&orchestrator_key),
            &query,
            limit,
        )
        .await
    })
}

/// Drop one graph node (a turn or entity) by its id. Helix-index-only — the durable
/// SQLite facts and the chat log are untouched. Returns whether the Core accepted it.
pub fn delete_graph_node(
    orchestrator_key: String,
    id: i64,
    discovery_timeout_secs: u64,
) -> Result<bool> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::delete_graph_node(
            &cache,
            timeout(discovery_timeout_secs),
            preferred(&orchestrator_key),
            id,
        )
        .await
    })
}

/// List the identified speakers (settings "People" view).
pub fn list_speakers(
    orchestrator_key: String,
    discovery_timeout_secs: u64,
) -> Result<Vec<SpeakerInfo>> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::list_speakers(
            &cache,
            timeout(discovery_timeout_secs),
            preferred(&orchestrator_key),
        )
        .await
    })
}

/// The household roster's member names, for the voice-enrollment "existing person"
/// picker. Empty when the roster is empty (or the Core predates this field).
pub fn list_household_members(
    orchestrator_key: String,
    discovery_timeout_secs: u64,
) -> Result<Vec<String>> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::list_household_members(
            &cache,
            timeout(discovery_timeout_secs),
            preferred(&orchestrator_key),
        )
        .await
    })
}

/// Name (or rename) a speaker. Returns whether the change was applied.
pub fn name_speaker(
    orchestrator_key: String,
    id: String,
    name: String,
    discovery_timeout_secs: u64,
) -> Result<bool> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::name_speaker(
            &cache,
            timeout(discovery_timeout_secs),
            preferred(&orchestrator_key),
            &id,
            &name,
        )
        .await
    })
}

/// Link a family member's phone to AppSaid by redeeming the 4-character pairing
/// `code` shown in their AppSaid app and storing it on the Core as a phone-message
/// recipient under `name`. Returns the Core's confirmation on success; a rejected or
/// expired code (or an unreachable Core) is an `Err` carrying the reason to show.
pub fn pair_phone(
    orchestrator_key: String,
    name: String,
    code: String,
    discovery_timeout_secs: u64,
) -> Result<String> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::pair_phone(
            &cache,
            timeout(discovery_timeout_secs),
            preferred(&orchestrator_key),
            &name,
            &code,
        )
        .await
    })
}

/// Merge the `drop` speaker into `keep` (same person, two clusters). Returns
/// whether the merge was applied.
pub fn merge_speakers(
    orchestrator_key: String,
    keep: String,
    drop: String,
    discovery_timeout_secs: u64,
) -> Result<bool> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::merge_speakers(
            &cache,
            timeout(discovery_timeout_secs),
            preferred(&orchestrator_key),
            &keep,
            &drop,
        )
        .await
    })
}

/// Link a voice to a household member on the Core (People → "Link to household
/// member"): merged into the member's existing voiceprint, or named as them. Returns
/// the Core's confirmation message; errors carry the reason.
pub fn link_speaker(
    orchestrator_key: String,
    id: String,
    member: String,
    discovery_timeout_secs: u64,
) -> Result<String> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::link_speaker(
            &cache,
            timeout(discovery_timeout_secs),
            preferred(&orchestrator_key),
            &id,
            &member,
        )
        .await
    })
}

/// Delete a speaker profile. Returns whether a profile was removed.
pub fn delete_speaker(
    orchestrator_key: String,
    id: String,
    discovery_timeout_secs: u64,
) -> Result<bool> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::delete_speaker(
            &cache,
            timeout(discovery_timeout_secs),
            preferred(&orchestrator_key),
            &id,
        )
        .await
    })
}

/// List a speaker's stored voice clips (newest first), for the People screen's
/// playback. Empty when the speaker has no clips (or clip capture is disabled on the
/// Core).
pub fn list_speaker_clips(
    orchestrator_key: String,
    id: String,
    discovery_timeout_secs: u64,
) -> Result<Vec<SpeakerClipInfo>> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::list_speaker_clips(
            &cache,
            timeout(discovery_timeout_secs),
            preferred(&orchestrator_key),
            &id,
        )
        .await
    })
}

/// Fetch and play back a person's stored voice clips through the running engine's
/// speaker, so a human can listen and put a name to an anonymous cluster. Clips play
/// back-to-back (newest first) with a short gap between them. Returns the number of
/// clips queued (0 when the person has none).
///
/// Reuses the one existing audio output path (the engine's [`PlaybackSink`]); errors
/// if the engine isn't running / has no audio output. Playback itself runs on a
/// background thread so the call returns promptly while the clips play out.
pub fn play_speaker_clips(
    orchestrator_key: String,
    id: String,
    discovery_timeout_secs: u64,
) -> Result<i32> {
    // A live output path is required — the People screen can only play clips while the
    // assistant engine is running (it owns the speaker stream).
    let sink = crate::engine::playback_sink().ok_or_else(|| {
        anyhow::anyhow!("audio output isn't available right now — is the assistant running?")
    })?;

    // Fetch the clip list and each clip's raw PCM over the control socket.
    let clips: Vec<(Vec<i16>, u32)> = block_on(async move {
        let cache = EndpointCache::new();
        let t = timeout(discovery_timeout_secs);
        let pref = preferred(&orchestrator_key);
        let infos = control::list_speaker_clips(&cache, t, pref, &id).await?;
        let mut out = Vec::with_capacity(infos.len());
        for info in &infos {
            let bytes = control::fetch_speaker_clip(&cache, t, pref, &id, &info.name).await?;
            out.push((decode_pcm_le(&bytes), info.sample_rate.max(1)));
        }
        Ok(out)
    })?;

    let count = clips.len() as i32;
    if count == 0 {
        return Ok(0);
    }

    // Play through the shared sink on a background thread: `submit_pcm` applies
    // real-time backpressure, so submitting the whole run here would block the caller
    // for the clips' duration. `clear()` first so a re-tap restarts cleanly.
    std::thread::spawn(move || {
        sink.clear();
        for (i, (pcm, rate)) in clips.iter().enumerate() {
            if i > 0 {
                // ~300 ms of silence between clips so they're distinguishable.
                let gap = vec![0i16; (*rate as usize) * 3 / 10];
                sink.submit_pcm(&gap, *rate);
            }
            sink.submit_pcm(pcm, *rate);
        }
    });

    Ok(count)
}

/// Decode raw little-endian `i16` PCM bytes (the clip wire format) into samples. A
/// trailing odd byte (shouldn't happen) is ignored.
fn decode_pcm_le(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// Commit the current guided voice-enrollment session into a named profile
/// (SpeakerEnrollmentPlan.md Phase 2). The sample captures were streamed over the
/// ordinary wake-word turns while the enroll screen was foreground; this averages
/// them into a voiceprint. Returns the orchestrator's confirmation message, or an
/// error carrying the reason (e.g. too few samples).
///
/// With `speaker_id` set (the People screen's "Add voice samples"), the samples are
/// folded into that existing person's voiceprint instead and `name` is ignored.
pub fn enroll_finish(
    orchestrator_key: String,
    session: String,
    name: String,
    speaker_id: Option<String>,
    discovery_timeout_secs: u64,
) -> Result<String> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::enroll_finish(
            &cache,
            timeout(discovery_timeout_secs),
            preferred(&orchestrator_key),
            &session,
            &name,
            speaker_id.as_deref(),
        )
        .await
    })
}

/// Discard an in-flight guided voice-enrollment session (the user backed out).
pub fn enroll_cancel(
    orchestrator_key: String,
    session: String,
    discovery_timeout_secs: u64,
) -> Result<()> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::enroll_cancel(
            &cache,
            timeout(discovery_timeout_secs),
            preferred(&orchestrator_key),
            &session,
        )
        .await
    })
}
