//! FRB surface for the Phase-6 settings screen (Plan.MD §3, Phase 6).
//!
//! The on-device settings screen manages state that lives on the **orchestrator**:
//! the runtime LLM backend + Piper voice, and the persistent memory list. These
//! functions are the Dart→Rust control calls (architecture.md §3): each discovers
//! the Mac's Wyoming host over mDNS, opens a short-lived connection, sends one
//! project-local `ambient-*` control frame, and returns the parsed response.
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

/// Read the orchestrator's current runtime settings.
pub fn fetch_orchestrator_settings(discovery_timeout_secs: u64) -> Result<OrchestratorSettings> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::describe_settings(&cache, timeout(discovery_timeout_secs)).await
    })
}

/// Apply a settings change on the orchestrator and return the resulting settings.
pub fn update_orchestrator_settings(
    update: SettingsUpdate,
    discovery_timeout_secs: u64,
) -> Result<OrchestratorSettings> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::update_settings(&cache, timeout(discovery_timeout_secs), &update).await
    })
}

/// List the orchestrator's selectable LLM models (Anthropic + OpenAI, scoped to the
/// last 12 months) for the settings model dropdown.
pub fn list_models(discovery_timeout_secs: u64) -> Result<Vec<ModelInfo>> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::list_models(&cache, timeout(discovery_timeout_secs)).await
    })
}

/// List all persistent memory entries (settings memory management view).
pub fn list_memories(discovery_timeout_secs: u64) -> Result<Vec<MemoryEntry>> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::list_memories(&cache, timeout(discovery_timeout_secs)).await
    })
}

/// Delete one memory entry by id. Returns whether a row was removed.
pub fn delete_memory(id: i64, discovery_timeout_secs: u64) -> Result<bool> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::delete_memory(&cache, timeout(discovery_timeout_secs), id).await
    })
}

/// Delete every memory entry. Returns the number removed.
pub fn clear_memories(discovery_timeout_secs: u64) -> Result<u32> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::clear_memories(&cache, timeout(discovery_timeout_secs)).await
    })
}

/// List the identified speakers (settings "People" view).
pub fn list_speakers(discovery_timeout_secs: u64) -> Result<Vec<SpeakerInfo>> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::list_speakers(&cache, timeout(discovery_timeout_secs)).await
    })
}

/// Name (or rename) a speaker. Returns whether the change was applied.
pub fn name_speaker(id: String, name: String, discovery_timeout_secs: u64) -> Result<bool> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::name_speaker(&cache, timeout(discovery_timeout_secs), &id, &name).await
    })
}

/// Merge the `drop` speaker into `keep` (same person, two clusters). Returns
/// whether the merge was applied.
pub fn merge_speakers(keep: String, drop: String, discovery_timeout_secs: u64) -> Result<bool> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::merge_speakers(&cache, timeout(discovery_timeout_secs), &keep, &drop).await
    })
}

/// Delete a speaker profile. Returns whether a profile was removed.
pub fn delete_speaker(id: String, discovery_timeout_secs: u64) -> Result<bool> {
    block_on(async move {
        let cache = EndpointCache::new();
        control::delete_speaker(&cache, timeout(discovery_timeout_secs), &id).await
    })
}
