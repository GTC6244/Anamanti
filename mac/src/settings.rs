//! Runtime-swappable orchestrator settings (Plan.MD Phase 6).
//!
//! Phase 4 bound the LLM backend and Piper voice once, from the environment. Phase
//! 6 makes them **changeable at runtime** from the on-device settings screen: the
//! device sends a project-local Wyoming control frame (see `wyoming::protocol`
//! `ambient-set-settings`) and the orchestrator rebuilds the selected backend and
//! swaps it in place, without a restart.
//!
//! [`SharedSettings`] holds the live [`RuntimeSettings`] behind an `RwLock` so the
//! (cheaply cloneable) [`crate::orchestrator::Pipeline`] takes a fresh snapshot at
//! the start of each turn while a concurrent control request can swap the backend.
//! An [`LlmFactory`] carries the immutable credentials/endpoints needed to
//! (re)build any backend from a `(backend, model)` pair, so a swap never has to
//! re-read the environment.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::llm::anthropic_auth::{AnthropicAuth, AnthropicTokenProvider};
use crate::llm::{
    anthropic::AnthropicBackend, mock::MockLlm, ollama::OllamaBackend, openai::OpenAiBackend,
    LlmBackend,
};

/// Which implementation drives the local/cloud LLM backends: the hand-rolled HTTP
/// clients, or the rig-core agent framework (feature `rig`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LlmEngine {
    /// Hand-rolled `ollama.rs` / `anthropic.rs` HTTP clients (always available).
    #[default]
    Native,
    /// rig-core agents (`AMBIENT_LLM_ENGINE=rig`; needs the `rig` feature to take
    /// effect — otherwise it transparently falls back to `Native`).
    Rig,
}

impl LlmEngine {
    /// Canonical lowercase label.
    pub fn as_str(self) -> &'static str {
        match self {
            LlmEngine::Native => "native",
            LlmEngine::Rig => "rig",
        }
    }

    /// Parse a label; anything other than `rig` is `Native`.
    pub fn from_label(label: &str) -> Self {
        match label.to_lowercase().as_str() {
            "rig" | "rig-core" | "rigcore" => LlmEngine::Rig,
            _ => LlmEngine::Native,
        }
    }
}

/// Default trailing-silence (ms) the energy VAD waits for after speech before
/// finalizing the STT transcript. A mild reduction from the historical 900 ms to
/// cut end-of-turn latency; A/B-tunable from the device settings screen.
pub const DEFAULT_END_SILENCE_MS: u64 = 700;

/// Default RMS (i16 units) above which an incoming chunk counts as speech rather
/// than room noise. Set above the Echo Show's measured far-field idle noise floor
/// (~100–200 i16), which the original 120 sat *inside* — so every frame read as
/// speech and end-of-speech never fired, stalling the turn. Speech runs ~1000+, so
/// 450 cleanly separates the two. Lower it for a very quiet mic, raise it for a
/// noisy room (A/B-tunable from the device settings screen / config page).
pub const DEFAULT_VOICE_RMS_THRESHOLD: f64 = 450.0;

fn default_end_silence_ms() -> u64 {
    DEFAULT_END_SILENCE_MS
}

fn default_voice_rms_threshold() -> f64 {
    DEFAULT_VOICE_RMS_THRESHOLD
}

fn default_anthropic_auth() -> String {
    AnthropicAuth::ApiKey.as_str().to_string()
}

/// The mutable settings persisted to disk so page/device changes survive a
/// restart. Contains the Tavily key in plaintext, so the file is written with
/// `0600` permissions on unix and should stay on a trusted machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSettings {
    pub engine: String,
    pub web_search: bool,
    pub search_provider: String,
    pub search_api_key: Option<String>,
    pub llm_backend: String,
    pub llm_model: Option<String>,
    /// Anthropic auth mode (`apikey`/`subscription`). Defaulted for older files.
    #[serde(default = "default_anthropic_auth")]
    pub anthropic_auth: String,
    pub tts_voice: Option<String>,
    /// End-of-speech trailing silence in ms (VAD). Defaulted for older files.
    #[serde(default = "default_end_silence_ms")]
    pub end_silence_ms: u64,
    /// Speech-vs-noise RMS threshold (VAD). Defaulted for older files.
    #[serde(default = "default_voice_rms_threshold")]
    pub voice_rms_threshold: f64,
}

/// Load persisted settings, or `None` if the file is absent/unreadable.
pub fn load_persisted(path: &Path) -> Option<PersistedSettings> {
    let data = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&data) {
        Ok(p) => Some(p),
        Err(e) => {
            log::warn!("ignoring unreadable settings file {}: {e}", path.display());
            None
        }
    }
}

/// Best-effort write of the persisted settings (0600 on unix). Failures are
/// logged, never fatal — a settings change still takes effect in memory.
fn persist(path: &Path, s: &PersistedSettings) {
    let json = match serde_json::to_string_pretty(s) {
        Ok(j) => j,
        Err(e) => {
            log::warn!("could not serialize settings: {e}");
            return;
        }
    };
    let result = (|| -> std::io::Result<()> {
        use std::io::Write;
        #[cfg(unix)]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(path)?
        };
        #[cfg(not(unix))]
        let mut file = std::fs::File::create(path)?;
        file.write_all(json.as_bytes())
    })();
    if let Err(e) = result {
        log::warn!("could not persist settings to {}: {e}", path.display());
    }
}

/// Immutable inputs needed to (re)build any LLM backend on demand. Captured once
/// from the environment/config so a runtime swap never re-reads `env`.
#[derive(Clone)]
pub struct LlmFactory {
    /// Local Ollama / llama.cpp base URL.
    pub ollama_url: String,
    /// Cloud Anthropic API base URL.
    pub anthropic_base_url: String,
    /// Anthropic API key, if one is available in the environment. Absent means the
    /// cloud backend cannot be selected and a request for it is rejected.
    pub anthropic_api_key: Option<String>,
    /// `max_tokens` for Anthropic replies (kept small for spoken output).
    pub anthropic_max_tokens: u32,
    /// Cloud OpenAI API base URL.
    pub openai_base_url: String,
    /// OpenAI API key, if available. Absent means the OpenAI backend cannot be
    /// selected and a request for it is rejected in-band.
    pub openai_api_key: Option<String>,
    /// `max_completion_tokens` for OpenAI replies (kept small for spoken output).
    pub openai_max_tokens: u32,
    /// Subscription (OAuth) token source for Anthropic, used when the auth mode is
    /// `Subscription`. Shared with the model catalog so both authenticate the same.
    pub anthropic_token: Option<Arc<AnthropicTokenProvider>>,
}

impl LlmFactory {
    /// Default model for a backend label when the caller pins none.
    fn default_model(backend: &str) -> Option<&'static str> {
        match backend {
            "ollama" => Some("llama3.2"),
            "anthropic" | "claude" => Some("claude-opus-5"),
            "openai" | "gpt" => Some("gpt-4o-mini"),
            _ => None,
        }
    }

    /// Build a backend from a label (`ollama` / `anthropic` / `openai` / `mock`) and
    /// an optional model. Returns the trait object plus the canonical label and the
    /// resolved model so callers can report exactly what took effect.
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        &self,
        engine: LlmEngine,
        web_search: bool,
        search_provider: &str,
        search_api_key: Option<&str>,
        backend: &str,
        model: Option<&str>,
        anthropic_auth: AnthropicAuth,
    ) -> Result<(Arc<dyn LlmBackend>, String, Option<String>)> {
        // These only affect the ollama/anthropic arms under the `rig` feature;
        // silence unused warnings on native-only builds.
        let _ = (engine, web_search, search_provider, search_api_key);
        match backend.to_lowercase().as_str() {
            "mock" => Ok((Arc::new(MockLlm::default()), "mock".to_string(), None)),
            "anthropic" | "claude" => {
                let model = model
                    .or(Self::default_model("anthropic"))
                    .unwrap_or("claude-opus-5")
                    .to_string();
                let backend: Arc<dyn LlmBackend> = match anthropic_auth {
                    // Subscription (OAuth) uses the token provider; the rig engine has
                    // no subscription path, so subscription always uses native HTTP.
                    AnthropicAuth::Subscription => {
                        let token = self.anthropic_token.clone().context(
                            "the anthropic subscription backend needs a token — set \
                             ANTHROPIC_OAUTH_TOKEN (run `claude setup-token`) on the orchestrator",
                        )?;
                        Arc::new(AnthropicBackend::with_subscription(
                            &self.anthropic_base_url,
                            token,
                            &model,
                            self.anthropic_max_tokens,
                        ))
                    }
                    AnthropicAuth::ApiKey => {
                        let key = self.anthropic_api_key.clone().context(
                            "the anthropic backend requires ANTHROPIC_API_KEY on the orchestrator \
                             (or switch to subscription auth)",
                        )?;
                        match engine {
                            #[cfg(feature = "rig")]
                            LlmEngine::Rig => Arc::new(crate::llm::rig::RigBackend::anthropic(
                                &self.anthropic_base_url,
                                &key,
                                &model,
                                self.anthropic_max_tokens,
                                crate::llm::rig::tools_from_config(
                                    web_search,
                                    search_provider,
                                    search_api_key,
                                ),
                            )?),
                            _ => Arc::new(AnthropicBackend::new(
                                &self.anthropic_base_url,
                                key,
                                &model,
                                self.anthropic_max_tokens,
                            )),
                        }
                    }
                };
                Ok((backend, "anthropic".to_string(), Some(model)))
            }
            "openai" | "gpt" => {
                let key = self
                    .openai_api_key
                    .clone()
                    .context("the openai backend requires OPENAI_API_KEY on the orchestrator")?;
                let model = model
                    .or(Self::default_model("openai"))
                    .unwrap_or("gpt-4o-mini")
                    .to_string();
                // rig-openai is out of scope for v1: the OpenAI backend always uses
                // the native HTTP client, even under the rig engine.
                let backend: Arc<dyn LlmBackend> = Arc::new(OpenAiBackend::new(
                    &self.openai_base_url,
                    key,
                    &model,
                    self.openai_max_tokens,
                ));
                Ok((backend, "openai".to_string(), Some(model)))
            }
            "ollama" => {
                let model = model.unwrap_or("llama3.2").to_string();
                let backend: Arc<dyn LlmBackend> = match engine {
                    #[cfg(feature = "rig")]
                    LlmEngine::Rig => Arc::new(crate::llm::rig::RigBackend::ollama(
                        &self.ollama_url,
                        &model,
                        crate::llm::rig::tools_from_config(
                            web_search,
                            search_provider,
                            search_api_key,
                        ),
                    )?),
                    _ => Arc::new(OllamaBackend::new(&self.ollama_url, &model)),
                };
                Ok((backend, "ollama".to_string(), Some(model)))
            }
            other => {
                anyhow::bail!(
                    "unknown LLM backend `{other}` (expected ollama/anthropic/openai/mock)"
                )
            }
        }
    }
}

/// The live, swappable settings the pipeline reads each turn.
#[derive(Clone)]
pub struct RuntimeSettings {
    /// The currently selected LLM backend.
    pub llm: Arc<dyn LlmBackend>,
    /// Which engine backs ollama/anthropic (native HTTP vs rig-core).
    pub engine: LlmEngine,
    /// Whether the rig web-search tool is enabled (rig engine only).
    pub web_search: bool,
    /// Web-search backend: `duckduckgo` (keyless) or `tavily` (needs a key).
    pub search_provider: String,
    /// API key for the search provider (Tavily). `None` = unset.
    pub search_api_key: Option<String>,
    /// Canonical label of the selected backend (`ollama` / `anthropic` / `mock`).
    pub llm_backend: String,
    /// The resolved model name, if the backend uses one.
    pub llm_model: Option<String>,
    /// How the Anthropic backend authenticates (API key vs subscription OAuth).
    pub anthropic_auth: AnthropicAuth,
    /// The Piper voice to synthesize with, or `None` for the server default.
    pub tts_voice: Option<String>,
    /// End-of-speech trailing silence (ms) the energy VAD waits for before
    /// finalizing the STT transcript. A/B-tunable from the device.
    pub end_silence_ms: u64,
    /// RMS (i16 units) above which a chunk counts as speech for the VAD.
    pub voice_rms_threshold: f64,
}

/// A description of the settings currently in effect, for reporting back to the
/// device settings screen or the config page.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsView {
    pub llm_backend: String,
    pub llm_model: Option<String>,
    /// Anthropic auth mode (API key vs subscription OAuth).
    pub anthropic_auth: AnthropicAuth,
    pub tts_voice: Option<String>,
    pub engine: LlmEngine,
    pub web_search: bool,
    pub search_provider: String,
    /// Whether a search API key is configured. The key itself is never exposed.
    pub search_key_set: bool,
    /// End-of-speech trailing silence (ms) the VAD waits for.
    pub end_silence_ms: u64,
    /// Speech-vs-noise RMS threshold for the VAD.
    pub voice_rms_threshold: f64,
}

/// A requested settings change. Absent fields are left unchanged; a `tts_voice` of
/// `Some(None)` clears the voice.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SettingsUpdate {
    pub llm_backend: Option<String>,
    pub llm_model: Option<String>,
    /// New Anthropic auth mode, or `None` to leave it unchanged.
    pub anthropic_auth: Option<AnthropicAuth>,
    /// `None` = leave unchanged; `Some(None)` = clear; `Some(Some(v))` = set to `v`.
    pub tts_voice: Option<Option<String>>,
    /// Switch the LLM engine (native vs rig).
    pub engine: Option<LlmEngine>,
    /// Toggle the rig web-search tool.
    pub web_search: Option<bool>,
    /// Switch the search backend (`duckduckgo` / `tavily`).
    pub search_provider: Option<String>,
    /// `None` = leave unchanged; `Some(None)` = clear; `Some(Some(v))` = set.
    pub search_api_key: Option<Option<String>>,
    /// New end-of-speech trailing silence (ms) for the VAD, or `None` to leave it.
    pub end_silence_ms: Option<u64>,
    /// New speech-vs-noise RMS threshold for the VAD, or `None` to leave it.
    pub voice_rms_threshold: Option<f64>,
}

/// Thread-safe holder for the runtime settings plus the factory that rebuilds
/// backends on a swap. Shared (via `Arc`) by every connection.
pub struct SharedSettings {
    inner: RwLock<RuntimeSettings>,
    factory: LlmFactory,
    /// Where to persist changes, or `None` to keep settings in-memory only.
    persist_path: Option<PathBuf>,
}

impl SharedSettings {
    /// Create an in-memory-only shared holder (no persistence).
    pub fn new(factory: LlmFactory, initial: RuntimeSettings) -> Arc<Self> {
        Self::new_persistent(factory, initial, None)
    }

    /// Create a shared holder that persists every applied change to `persist_path`
    /// (when `Some`), reloaded at boot by the caller.
    pub fn new_persistent(
        factory: LlmFactory,
        initial: RuntimeSettings,
        persist_path: Option<PathBuf>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(initial),
            factory,
            persist_path,
        })
    }

    /// Snapshot the mutable settings for persistence.
    fn persisted_snapshot(s: &RuntimeSettings) -> PersistedSettings {
        PersistedSettings {
            engine: s.engine.as_str().to_string(),
            web_search: s.web_search,
            search_provider: s.search_provider.clone(),
            search_api_key: s.search_api_key.clone(),
            llm_backend: s.llm_backend.clone(),
            llm_model: s.llm_model.clone(),
            anthropic_auth: s.anthropic_auth.as_str().to_string(),
            tts_voice: s.tts_voice.clone(),
            end_silence_ms: s.end_silence_ms,
            voice_rms_threshold: s.voice_rms_threshold,
        }
    }

    /// A fixed holder around an already-built backend whose LLM cannot be swapped
    /// (its factory has no credentials). Used by tests and any caller that only
    /// needs the immutable Phase-4 behavior. The TTS voice is still settable.
    pub fn fixed(
        llm: Arc<dyn LlmBackend>,
        llm_backend: impl Into<String>,
        tts_voice: Option<String>,
    ) -> Arc<Self> {
        let factory = LlmFactory {
            ollama_url: "http://127.0.0.1:11434".to_string(),
            anthropic_base_url: "https://api.anthropic.com".to_string(),
            anthropic_api_key: None,
            anthropic_max_tokens: 1024,
            openai_base_url: "https://api.openai.com".to_string(),
            openai_api_key: None,
            openai_max_tokens: 1024,
            anthropic_token: None,
        };
        Self::new(
            factory,
            RuntimeSettings {
                llm,
                engine: LlmEngine::Native,
                web_search: false,
                search_provider: "duckduckgo".to_string(),
                search_api_key: None,
                llm_backend: llm_backend.into(),
                llm_model: None,
                anthropic_auth: AnthropicAuth::ApiKey,
                tts_voice,
                end_silence_ms: DEFAULT_END_SILENCE_MS,
                voice_rms_threshold: DEFAULT_VOICE_RMS_THRESHOLD,
            },
        )
    }

    /// A snapshot of the live settings (cheap: `Arc`/`String` clones). Taken once
    /// per turn so a mid-turn swap never changes the backend under a running reply.
    pub fn snapshot(&self) -> RuntimeSettings {
        self.inner.read().unwrap().clone()
    }

    /// The current settings, for reporting to the device.
    pub fn view(&self) -> SettingsView {
        let s = self.inner.read().unwrap();
        SettingsView {
            llm_backend: s.llm_backend.clone(),
            llm_model: s.llm_model.clone(),
            anthropic_auth: s.anthropic_auth,
            tts_voice: s.tts_voice.clone(),
            engine: s.engine,
            web_search: s.web_search,
            search_provider: s.search_provider.clone(),
            search_key_set: s.search_api_key.as_deref().is_some_and(|k| !k.is_empty()),
            end_silence_ms: s.end_silence_ms,
            voice_rms_threshold: s.voice_rms_threshold,
        }
    }

    /// Apply a settings change, rebuilding the LLM backend if the backend or model
    /// changed. On success the swap is atomic and the new [`SettingsView`] is
    /// returned; on failure (e.g. anthropic requested with no key) the current
    /// settings are left untouched and the error is returned.
    pub fn apply(&self, update: &SettingsUpdate) -> Result<SettingsView> {
        // Any of these change which backend object we need, so rebuild the LLM.
        let needs_rebuild = update.llm_backend.is_some()
            || update.llm_model.is_some()
            || update.anthropic_auth.is_some()
            || update.engine.is_some()
            || update.web_search.is_some()
            || update.search_provider.is_some()
            || update.search_api_key.is_some();

        // Build the new LLM *before* taking the write lock so a failed build never
        // leaves the settings half-changed.
        let (rebuilt, targets) = if needs_rebuild {
            let current = self.inner.read().unwrap().clone();
            let target_backend = update
                .llm_backend
                .clone()
                .unwrap_or(current.llm_backend.clone());
            // A backend change with no explicit model resets to that backend's
            // default; a model-only change keeps the current backend.
            let target_model = match (&update.llm_backend, &update.llm_model) {
                (_, Some(m)) => Some(m.clone()),
                (Some(_), None) => None,
                (None, None) => current.llm_model.clone(),
            };
            let target_engine = update.engine.unwrap_or(current.engine);
            let target_auth = update.anthropic_auth.unwrap_or(current.anthropic_auth);
            let target_web_search = update.web_search.unwrap_or(current.web_search);
            let target_provider = update
                .search_provider
                .clone()
                .unwrap_or(current.search_provider.clone());
            let target_key = match &update.search_api_key {
                None => current.search_api_key.clone(),
                Some(k) => k.clone().filter(|s| !s.is_empty()),
            };
            (
                Some(self.factory.build(
                    target_engine,
                    target_web_search,
                    &target_provider,
                    target_key.as_deref(),
                    &target_backend,
                    target_model.as_deref(),
                    target_auth,
                )?),
                Some((
                    target_engine,
                    target_auth,
                    target_web_search,
                    target_provider,
                    target_key,
                )),
            )
        } else {
            (None, None)
        };

        let mut w = self.inner.write().unwrap();
        if let Some((llm, label, model)) = rebuilt {
            let (engine, auth, web_search, provider, key) = targets.unwrap();
            w.llm = llm;
            w.llm_backend = label;
            w.llm_model = model;
            w.anthropic_auth = auth;
            w.engine = engine;
            w.web_search = web_search;
            w.search_provider = provider;
            w.search_api_key = key;
        }
        if let Some(voice) = &update.tts_voice {
            w.tts_voice = voice.clone().filter(|s| !s.is_empty());
        }
        // VAD tuning needs no backend rebuild — it's read from the per-turn snapshot
        // by `stream_to_transcript`. Clamp to sane ranges so a bad request can't wedge
        // end-of-speech detection.
        if let Some(ms) = update.end_silence_ms {
            w.end_silence_ms = ms.clamp(150, 5000);
        }
        if let Some(thr) = update.voice_rms_threshold {
            w.voice_rms_threshold = thr.clamp(0.0, 5000.0);
        }
        let view = SettingsView {
            llm_backend: w.llm_backend.clone(),
            llm_model: w.llm_model.clone(),
            anthropic_auth: w.anthropic_auth,
            tts_voice: w.tts_voice.clone(),
            engine: w.engine,
            web_search: w.web_search,
            search_provider: w.search_provider.clone(),
            search_key_set: w.search_api_key.as_deref().is_some_and(|k| !k.is_empty()),
            end_silence_ms: w.end_silence_ms,
            voice_rms_threshold: w.voice_rms_threshold,
        };
        // Persist the new state (best-effort) after dropping the write lock so IO
        // never blocks a concurrent turn's snapshot.
        let snapshot = self
            .persist_path
            .is_some()
            .then(|| Self::persisted_snapshot(&w));
        drop(w);
        if let (Some(path), Some(snap)) = (&self.persist_path, snapshot) {
            persist(path, &snap);
        }
        Ok(view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn factory_with_key(key: Option<&str>) -> LlmFactory {
        LlmFactory {
            ollama_url: "http://127.0.0.1:11434".to_string(),
            anthropic_base_url: "https://api.anthropic.com".to_string(),
            anthropic_api_key: key.map(String::from),
            anthropic_max_tokens: 256,
            openai_base_url: "https://api.openai.com".to_string(),
            openai_api_key: None,
            openai_max_tokens: 256,
            anthropic_token: None,
        }
    }

    fn shared(factory: LlmFactory) -> Arc<SharedSettings> {
        let (llm, label, model) = factory
            .build(
                LlmEngine::Native,
                false,
                "duckduckgo",
                None,
                "ollama",
                Some("llama3.2"),
                AnthropicAuth::ApiKey,
            )
            .unwrap();
        SharedSettings::new(
            factory,
            RuntimeSettings {
                llm,
                engine: LlmEngine::Native,
                web_search: false,
                search_provider: "duckduckgo".to_string(),
                search_api_key: None,
                llm_backend: label,
                llm_model: model,
                anthropic_auth: AnthropicAuth::ApiKey,
                tts_voice: None,
                end_silence_ms: DEFAULT_END_SILENCE_MS,
                voice_rms_threshold: DEFAULT_VOICE_RMS_THRESHOLD,
            },
        )
    }

    #[test]
    fn subscription_backend_builds_with_a_token_provider() {
        use crate::llm::anthropic_auth::AnthropicTokenProvider;
        let mut factory = factory_with_key(None); // no API key…
        factory.anthropic_token = Some(Arc::new(AnthropicTokenProvider::with_fetcher(Arc::new(
            || Ok("tok".into()),
        ))));
        // …but subscription auth builds anyway, because a token provider is wired.
        let (_, label, model) = factory
            .build(
                LlmEngine::Native,
                false,
                "duckduckgo",
                None,
                "anthropic",
                None,
                AnthropicAuth::Subscription,
            )
            .unwrap();
        assert_eq!(label, "anthropic");
        assert_eq!(model.as_deref(), Some("claude-opus-5"));
    }

    #[test]
    fn subscription_without_token_provider_is_rejected() {
        let factory = factory_with_key(None); // anthropic_token: None
                                              // `Arc<dyn LlmBackend>` isn't Debug, so take the error via `.err()`.
        let err = factory
            .build(
                LlmEngine::Native,
                false,
                "duckduckgo",
                None,
                "anthropic",
                None,
                AnthropicAuth::Subscription,
            )
            .err()
            .expect("subscription without a token provider must be rejected");
        assert!(format!("{err:#}").contains("ANTHROPIC_OAUTH_TOKEN"));
    }

    #[test]
    fn apply_persists_settings_and_they_reload() {
        let path = std::env::temp_dir().join(format!(
            "ambient_settings_test_{}_{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);

        let factory = factory_with_key(None);
        let (llm, label, model) = factory
            .build(
                LlmEngine::Native,
                false,
                "duckduckgo",
                None,
                "ollama",
                Some("llama3.2"),
                AnthropicAuth::ApiKey,
            )
            .unwrap();
        let s = SharedSettings::new_persistent(
            factory,
            RuntimeSettings {
                llm,
                engine: LlmEngine::Native,
                web_search: false,
                search_provider: "duckduckgo".into(),
                search_api_key: None,
                llm_backend: label,
                llm_model: model,
                anthropic_auth: AnthropicAuth::ApiKey,
                tts_voice: None,
                end_silence_ms: DEFAULT_END_SILENCE_MS,
                voice_rms_threshold: DEFAULT_VOICE_RMS_THRESHOLD,
            },
            Some(path.clone()),
        );

        s.apply(&SettingsUpdate {
            web_search: Some(true),
            search_provider: Some("tavily".into()),
            search_api_key: Some(Some("tvly-secret".into())),
            ..Default::default()
        })
        .unwrap();

        let p = load_persisted(&path).expect("settings file should exist");
        assert!(p.web_search);
        assert_eq!(p.search_provider, "tavily");
        assert_eq!(p.search_api_key.as_deref(), Some("tvly-secret"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn model_only_change_keeps_backend() {
        let s = shared(factory_with_key(None));
        let view = s
            .apply(&SettingsUpdate {
                llm_model: Some("qwen2.5".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(view.llm_backend, "ollama");
        assert_eq!(view.llm_model.as_deref(), Some("qwen2.5"));
    }

    #[test]
    fn backend_change_without_model_uses_default_model() {
        let s = shared(factory_with_key(Some("sk-test")));
        let view = s
            .apply(&SettingsUpdate {
                llm_backend: Some("anthropic".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(view.llm_backend, "anthropic");
        assert_eq!(view.llm_model.as_deref(), Some("claude-opus-5"));
    }

    #[test]
    fn anthropic_without_key_is_rejected_and_leaves_settings_unchanged() {
        let s = shared(factory_with_key(None));
        let before = s.view();
        let err = s
            .apply(&SettingsUpdate {
                llm_backend: Some("anthropic".to_string()),
                ..Default::default()
            })
            .unwrap_err();
        assert!(err.to_string().contains("ANTHROPIC_API_KEY"));
        assert_eq!(s.view(), before, "a failed swap is atomic");
    }

    #[test]
    fn tts_voice_can_be_set_and_cleared() {
        let s = shared(factory_with_key(None));
        let set = s
            .apply(&SettingsUpdate {
                tts_voice: Some(Some("en_US-amy-medium".to_string())),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(set.tts_voice.as_deref(), Some("en_US-amy-medium"));

        let cleared = s
            .apply(&SettingsUpdate {
                tts_voice: Some(None),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(cleared.tts_voice, None);
    }

    #[test]
    fn snapshot_is_stable_across_a_later_swap() {
        let s = shared(factory_with_key(None));
        let snap = s.snapshot();
        s.apply(&SettingsUpdate {
            llm_model: Some("other".to_string()),
            ..Default::default()
        })
        .unwrap();
        // The earlier snapshot still points at the original backend name.
        assert_eq!(snap.llm_model.as_deref(), Some("llama3.2"));
        assert_eq!(s.view().llm_model.as_deref(), Some("other"));
    }
}
