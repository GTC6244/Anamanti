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

use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};

use crate::llm::{anthropic::AnthropicBackend, mock::MockLlm, ollama::OllamaBackend, LlmBackend};

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

/// Immutable inputs needed to (re)build any LLM backend on demand. Captured once
/// from the environment/config so a runtime swap never re-reads `env`.
#[derive(Clone)]
pub struct LlmFactory {
    /// Which engine backs the ollama/anthropic backends (native HTTP vs rig-core).
    pub engine: LlmEngine,
    /// Enable the rig web-search tool (`internet_search`). Only takes effect with
    /// the `rig` engine; ignored by the native backends.
    #[cfg_attr(not(feature = "rig"), allow(dead_code))]
    pub web_search: bool,
    /// Local Ollama / llama.cpp base URL.
    pub ollama_url: String,
    /// Cloud Anthropic API base URL.
    pub anthropic_base_url: String,
    /// Anthropic API key, if one is available in the environment. Absent means the
    /// cloud backend cannot be selected and a request for it is rejected.
    pub anthropic_api_key: Option<String>,
    /// `max_tokens` for Anthropic replies (kept small for spoken output).
    pub anthropic_max_tokens: u32,
}

impl LlmFactory {
    /// Default model for a backend label when the caller pins none.
    fn default_model(backend: &str) -> Option<&'static str> {
        match backend {
            "ollama" => Some("llama3.2"),
            "anthropic" | "claude" => Some("claude-opus-5"),
            _ => None,
        }
    }

    /// Build a backend from a label (`ollama` / `anthropic` / `mock`) and an
    /// optional model. Returns the trait object plus the canonical label and the
    /// resolved model so callers can report exactly what took effect.
    pub fn build(
        &self,
        backend: &str,
        model: Option<&str>,
    ) -> Result<(Arc<dyn LlmBackend>, String, Option<String>)> {
        match backend.to_lowercase().as_str() {
            "mock" => Ok((Arc::new(MockLlm::default()), "mock".to_string(), None)),
            "anthropic" | "claude" => {
                let key = self.anthropic_api_key.clone().context(
                    "the anthropic backend requires ANTHROPIC_API_KEY on the orchestrator",
                )?;
                let model = model
                    .or(Self::default_model("anthropic"))
                    .unwrap_or("claude-opus-5")
                    .to_string();
                let backend: Arc<dyn LlmBackend> = match self.engine {
                    #[cfg(feature = "rig")]
                    LlmEngine::Rig => Arc::new(crate::llm::rig::RigBackend::anthropic(
                        &self.anthropic_base_url,
                        &key,
                        &model,
                        self.anthropic_max_tokens,
                        crate::llm::rig::tools_from_flag(self.web_search),
                    )?),
                    _ => Arc::new(AnthropicBackend::new(
                        &self.anthropic_base_url,
                        key,
                        &model,
                        self.anthropic_max_tokens,
                    )),
                };
                Ok((backend, "anthropic".to_string(), Some(model)))
            }
            "ollama" => {
                let model = model.unwrap_or("llama3.2").to_string();
                let backend: Arc<dyn LlmBackend> = match self.engine {
                    #[cfg(feature = "rig")]
                    LlmEngine::Rig => Arc::new(crate::llm::rig::RigBackend::ollama(
                        &self.ollama_url,
                        &model,
                        crate::llm::rig::tools_from_flag(self.web_search),
                    )?),
                    _ => Arc::new(OllamaBackend::new(&self.ollama_url, &model)),
                };
                Ok((backend, "ollama".to_string(), Some(model)))
            }
            other => {
                anyhow::bail!("unknown LLM backend `{other}` (expected ollama/anthropic/mock)")
            }
        }
    }
}

/// The live, swappable settings the pipeline reads each turn.
#[derive(Clone)]
pub struct RuntimeSettings {
    /// The currently selected LLM backend.
    pub llm: Arc<dyn LlmBackend>,
    /// Canonical label of the selected backend (`ollama` / `anthropic` / `mock`).
    pub llm_backend: String,
    /// The resolved model name, if the backend uses one.
    pub llm_model: Option<String>,
    /// The Piper voice to synthesize with, or `None` for the server default.
    pub tts_voice: Option<String>,
}

/// A description of the settings currently in effect, for reporting back to the
/// device settings screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsView {
    pub llm_backend: String,
    pub llm_model: Option<String>,
    pub tts_voice: Option<String>,
}

/// A requested settings change. Absent fields are left unchanged; a `tts_voice` of
/// `Some(None)` clears the voice.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsUpdate {
    pub llm_backend: Option<String>,
    pub llm_model: Option<String>,
    /// `None` = leave unchanged; `Some(None)` = clear; `Some(Some(v))` = set to `v`.
    pub tts_voice: Option<Option<String>>,
}

/// Thread-safe holder for the runtime settings plus the factory that rebuilds
/// backends on a swap. Shared (via `Arc`) by every connection.
pub struct SharedSettings {
    inner: RwLock<RuntimeSettings>,
    factory: LlmFactory,
}

impl SharedSettings {
    /// Create a shared holder around an initial [`RuntimeSettings`] and the factory
    /// used to rebuild backends when the device changes them.
    pub fn new(factory: LlmFactory, initial: RuntimeSettings) -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(initial),
            factory,
        })
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
            engine: LlmEngine::Native,
            web_search: false,
            ollama_url: "http://127.0.0.1:11434".to_string(),
            anthropic_base_url: "https://api.anthropic.com".to_string(),
            anthropic_api_key: None,
            anthropic_max_tokens: 1024,
        };
        Self::new(
            factory,
            RuntimeSettings {
                llm,
                llm_backend: llm_backend.into(),
                llm_model: None,
                tts_voice,
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
            tts_voice: s.tts_voice.clone(),
        }
    }

    /// Apply a settings change, rebuilding the LLM backend if the backend or model
    /// changed. On success the swap is atomic and the new [`SettingsView`] is
    /// returned; on failure (e.g. anthropic requested with no key) the current
    /// settings are left untouched and the error is returned.
    pub fn apply(&self, update: &SettingsUpdate) -> Result<SettingsView> {
        // Decide the target backend/model, building the new LLM *before* taking the
        // write lock so a failed build never leaves the settings half-changed.
        let rebuilt = if update.llm_backend.is_some() || update.llm_model.is_some() {
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
            Some(
                self.factory
                    .build(&target_backend, target_model.as_deref())?,
            )
        } else {
            None
        };

        let mut w = self.inner.write().unwrap();
        if let Some((llm, label, model)) = rebuilt {
            w.llm = llm;
            w.llm_backend = label;
            w.llm_model = model;
        }
        if let Some(voice) = &update.tts_voice {
            w.tts_voice = voice.clone().filter(|s| !s.is_empty());
        }
        Ok(SettingsView {
            llm_backend: w.llm_backend.clone(),
            llm_model: w.llm_model.clone(),
            tts_voice: w.tts_voice.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn factory_with_key(key: Option<&str>) -> LlmFactory {
        LlmFactory {
            engine: LlmEngine::Native,
            web_search: false,
            ollama_url: "http://127.0.0.1:11434".to_string(),
            anthropic_base_url: "https://api.anthropic.com".to_string(),
            anthropic_api_key: key.map(String::from),
            anthropic_max_tokens: 256,
        }
    }

    fn shared(factory: LlmFactory) -> Arc<SharedSettings> {
        let (llm, label, model) = factory.build("ollama", Some("llama3.2")).unwrap();
        SharedSettings::new(
            factory,
            RuntimeSettings {
                llm,
                llm_backend: label,
                llm_model: model,
                tts_voice: None,
            },
        )
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
