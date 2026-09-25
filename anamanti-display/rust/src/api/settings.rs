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
        let endpoints = crate::wyoming::discovery::discover_all(timeout(discovery_timeout_secs))
            .await?;
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
