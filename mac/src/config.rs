//! Runtime configuration for the orchestrator, read from environment variables
//! with sensible defaults. This is where the **pluggable** LLM decision is bound:
//! `AMBIENT_LLM_BACKEND` picks local (Ollama) vs cloud (Claude) vs the offline
//! mock, and the rest of the pipeline is handed a `dyn LlmBackend` — it never
//! knows which was chosen.

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::llm::{anthropic::AnthropicBackend, mock::MockLlm, ollama::OllamaBackend, LlmBackend};

/// Default persona/system prompt: concise, speakable replies for an ambient
/// display. Kept short because the reply is spoken aloud via Piper.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are a friendly, concise voice assistant for a home \
ambient display. Answer in one or two short spoken sentences. Do not use markdown, lists, or \
emoji. If you don't know, say so briefly.";

/// Which LLM backend the pipeline should use.
#[derive(Debug, Clone)]
pub enum LlmChoice {
    /// Deterministic offline echo backend (no server, no network).
    Mock,
    /// Local Ollama / llama.cpp endpoint.
    Ollama { url: String, model: String },
    /// Cloud Claude Messages API.
    Anthropic { model: String, max_tokens: u32 },
}

/// Fully-resolved orchestrator configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Address the device-facing Wyoming server binds to (advertised via mDNS).
    pub bind_addr: SocketAddr,
    /// Human-readable mDNS instance name.
    pub service_name: String,
    /// Downstream Wyoming STT (Whisper) address.
    pub stt_addr: SocketAddr,
    /// Downstream Wyoming TTS (Piper) address.
    pub tts_addr: SocketAddr,
    /// Optional Piper voice name.
    pub tts_voice: Option<String>,
    /// Selected LLM backend.
    pub llm: LlmChoice,
    /// SQLite memory database path.
    pub db_path: PathBuf,
    /// Base system prompt/persona.
    pub system_prompt: String,
    /// Idle timeout for a stalled turn.
    pub turn_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // Port 10700 is the conventional Wyoming satellite/host port.
            bind_addr: "0.0.0.0:10700".parse().unwrap(),
            service_name: "Ambient Orchestrator".to_string(),
            stt_addr: "127.0.0.1:10300".parse().unwrap(), // wyoming-faster-whisper default
            tts_addr: "127.0.0.1:10200".parse().unwrap(), // wyoming-piper default
            tts_voice: None,
            llm: LlmChoice::Ollama {
                url: "http://127.0.0.1:11434".to_string(),
                model: "llama3.2".to_string(),
            },
            db_path: PathBuf::from("ambient_memory.sqlite"),
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
            turn_timeout: Duration::from_secs(30),
        }
    }
}

fn env_addr(key: &str, default: SocketAddr) -> Result<SocketAddr> {
    match env::var(key) {
        Ok(v) => v
            .parse()
            .with_context(|| format!("parsing {key}=`{v}` as host:port")),
        Err(_) => Ok(default),
    }
}

impl Config {
    /// Build a config from the environment, falling back to defaults.
    pub fn from_env() -> Result<Self> {
        let d = Config::default();

        let llm = match env::var("AMBIENT_LLM_BACKEND")
            .unwrap_or_else(|_| "ollama".to_string())
            .to_lowercase()
            .as_str()
        {
            "mock" => LlmChoice::Mock,
            "anthropic" | "claude" => LlmChoice::Anthropic {
                model: env::var("AMBIENT_ANTHROPIC_MODEL")
                    .unwrap_or_else(|_| "claude-opus-5".to_string()),
                max_tokens: env::var("AMBIENT_ANTHROPIC_MAX_TOKENS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(1024),
            },
            _ => LlmChoice::Ollama {
                url: env::var("AMBIENT_OLLAMA_URL")
                    .unwrap_or_else(|_| "http://127.0.0.1:11434".to_string()),
                model: env::var("AMBIENT_OLLAMA_MODEL").unwrap_or_else(|_| "llama3.2".to_string()),
            },
        };

        Ok(Self {
            bind_addr: env_addr("AMBIENT_BIND_ADDR", d.bind_addr)?,
            service_name: env::var("AMBIENT_SERVICE_NAME").unwrap_or(d.service_name),
            stt_addr: env_addr("AMBIENT_STT_ADDR", d.stt_addr)?,
            tts_addr: env_addr("AMBIENT_TTS_ADDR", d.tts_addr)?,
            tts_voice: env::var("AMBIENT_TTS_VOICE").ok().filter(|s| !s.is_empty()),
            llm,
            db_path: env::var("AMBIENT_DB_PATH")
                .map(PathBuf::from)
                .unwrap_or(d.db_path),
            system_prompt: env::var("AMBIENT_SYSTEM_PROMPT").unwrap_or(d.system_prompt),
            turn_timeout: d.turn_timeout,
        })
    }

    /// Instantiate the selected LLM backend behind the trait object the pipeline
    /// consumes. The Anthropic backend requires `ANTHROPIC_API_KEY`.
    pub fn build_llm(&self) -> Result<Arc<dyn LlmBackend>> {
        Ok(match &self.llm {
            LlmChoice::Mock => Arc::new(MockLlm::default()),
            LlmChoice::Ollama { url, model } => Arc::new(OllamaBackend::new(url, model)),
            LlmChoice::Anthropic { model, max_tokens } => {
                let key = env::var("ANTHROPIC_API_KEY")
                    .context("AMBIENT_LLM_BACKEND=anthropic requires ANTHROPIC_API_KEY")?;
                Arc::new(AnthropicBackend::new(
                    "https://api.anthropic.com",
                    key,
                    model,
                    *max_tokens,
                ))
            }
        })
    }

    /// A short label for the selected backend (logging/settings).
    pub fn llm_label(&self) -> &'static str {
        match self.llm {
            LlmChoice::Mock => "mock",
            LlmChoice::Ollama { .. } => "ollama",
            LlmChoice::Anthropic { .. } => "anthropic",
        }
    }
}
