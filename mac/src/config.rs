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
use crate::settings::{LlmEngine, LlmFactory, RuntimeSettings, SharedSettings};

/// Read the LLM engine selector from the environment. `AMBIENT_LLM_ENGINE=rig`
/// routes ollama/anthropic through rig-core (needs the `rig` feature); anything
/// else (or unset) keeps the native HTTP backends.
fn llm_engine_from_env() -> LlmEngine {
    match env::var("AMBIENT_LLM_ENGINE")
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "rig" | "rig-core" | "rigcore" => LlmEngine::Rig,
        _ => LlmEngine::Native,
    }
}

/// Whether to enable the rig web-search tool (`AMBIENT_WEB_SEARCH=1/true/on`).
/// Only effective with the rig engine + `rig` feature.
fn web_search_from_env() -> bool {
    matches!(
        env::var("AMBIENT_WEB_SEARCH")
            .unwrap_or_default()
            .trim()
            .to_lowercase()
            .as_str(),
        "1" | "true" | "on" | "yes"
    )
}

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
    /// Address the local HTTP **config page** binds to, or `None` to disable it.
    /// Defaults to loopback (`127.0.0.1:8730`) since the page has no auth.
    pub config_addr: Option<SocketAddr>,
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
    /// Memory retrieval backend: `sqlite` (FTS, default) or `helix` (GraphRAG).
    pub memory_backend: MemoryBackendChoice,
    /// Append-only JSONL chat log path (always written; the ingester's queue).
    pub chatlog_path: PathBuf,
    /// Embedded HelixDB on-disk store root (used when `memory_backend = helix`).
    pub helix_path: PathBuf,
    /// GraphRAG embedding + extraction settings (used when `memory_backend = helix`).
    pub graphrag: GraphRagConfig,
}

/// Which memory retrieval backend the pipeline uses for prompt context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryBackendChoice {
    /// SQLite FTS over the explicit/inferred memory store (default).
    Sqlite,
    /// Embedded HelixDB GraphRAG (vector KNN + graph expansion).
    Helix,
}

/// Settings for the GraphRAG memory (embeddings + background entity extraction).
/// API keys are read from the environment at wiring time, not stored here.
#[derive(Debug, Clone)]
pub struct GraphRagConfig {
    /// OpenAI API base (overridable for testing).
    pub openai_base_url: String,
    /// Embedding model (default `text-embedding-3-small`).
    pub embed_model: String,
    /// Embedding dimensionality (native 1536; reducible via OpenAI's `dimensions`).
    pub embed_dims: usize,
    /// Anthropic API base for entity extraction.
    pub anthropic_base_url: String,
    /// Entity-extraction chat model (default Claude Haiku 4.5).
    pub extract_model: String,
    /// How often the background ingester drains the chat log.
    pub ingest_interval: Duration,
    /// KNN fan-out per vector search at recall time.
    pub recall_k: usize,
}

impl Default for GraphRagConfig {
    fn default() -> Self {
        Self {
            openai_base_url: "https://api.openai.com".to_string(),
            embed_model: "text-embedding-3-small".to_string(),
            embed_dims: 1536,
            anthropic_base_url: "https://api.anthropic.com".to_string(),
            extract_model: "claude-haiku-4-5".to_string(),
            ingest_interval: Duration::from_secs(30),
            recall_k: 6,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // Port 10700 is the conventional Wyoming satellite/host port.
            bind_addr: "0.0.0.0:10700".parse().unwrap(),
            // Config page on loopback only by default (no auth); override or
            // disable with AMBIENT_CONFIG_ADDR.
            config_addr: Some("127.0.0.1:8730".parse().unwrap()),
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
            memory_backend: MemoryBackendChoice::Sqlite,
            chatlog_path: PathBuf::from("ambient_chatlog.jsonl"),
            helix_path: PathBuf::from("ambient_helix"),
            graphrag: GraphRagConfig::default(),
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

        let memory_backend = match env::var("AMBIENT_MEMORY_BACKEND")
            .unwrap_or_else(|_| "sqlite".to_string())
            .to_lowercase()
            .as_str()
        {
            "helix" | "graphrag" => MemoryBackendChoice::Helix,
            _ => MemoryBackendChoice::Sqlite,
        };

        let mut graphrag = GraphRagConfig::default();
        if let Ok(model) = env::var("AMBIENT_EMBED_MODEL") {
            graphrag.embed_model = model;
        }
        if let Some(dims) = env::var("AMBIENT_EMBED_DIMS")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            graphrag.embed_dims = dims;
        }
        if let Ok(model) = env::var("AMBIENT_EXTRACT_MODEL") {
            graphrag.extract_model = model;
        }
        if let Ok(url) = env::var("AMBIENT_OPENAI_BASE_URL") {
            graphrag.openai_base_url = url;
        }
        if let Ok(url) = env::var("AMBIENT_ANTHROPIC_BASE_URL") {
            graphrag.anthropic_base_url = url;
        }
        if let Some(secs) = env::var("AMBIENT_INGEST_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            graphrag.ingest_interval = Duration::from_secs(secs);
        }

        // The config page: `off`/`none`/empty disables it, otherwise a host:port.
        let config_addr = match env::var("AMBIENT_CONFIG_ADDR") {
            Ok(v) if matches!(v.trim().to_lowercase().as_str(), "off" | "none" | "") => None,
            Ok(v) => Some(
                v.trim()
                    .parse()
                    .with_context(|| format!("parsing AMBIENT_CONFIG_ADDR=`{v}` as host:port"))?,
            ),
            Err(_) => d.config_addr,
        };

        Ok(Self {
            bind_addr: env_addr("AMBIENT_BIND_ADDR", d.bind_addr)?,
            config_addr,
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
            memory_backend,
            chatlog_path: env::var("AMBIENT_CHATLOG_PATH")
                .map(PathBuf::from)
                .unwrap_or(d.chatlog_path),
            helix_path: env::var("AMBIENT_HELIX_PATH")
                .map(PathBuf::from)
                .unwrap_or(d.helix_path),
            graphrag,
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

    /// The immutable inputs a runtime backend swap (Phase 6) needs, captured from
    /// the environment once so a later swap never re-reads `env`. `ANTHROPIC_API_KEY`
    /// is read here: if absent, the cloud backend simply can't be selected at
    /// runtime (the request is rejected in-band).
    pub fn llm_factory(&self) -> LlmFactory {
        let anthropic_max_tokens = match &self.llm {
            LlmChoice::Anthropic { max_tokens, .. } => *max_tokens,
            _ => env::var("AMBIENT_ANTHROPIC_MAX_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1024),
        };
        LlmFactory {
            engine: llm_engine_from_env(),
            web_search: web_search_from_env(),
            ollama_url: env::var("AMBIENT_OLLAMA_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:11434".to_string()),
            anthropic_base_url: "https://api.anthropic.com".to_string(),
            anthropic_api_key: env::var("ANTHROPIC_API_KEY").ok().filter(|s| !s.is_empty()),
            anthropic_max_tokens,
        }
    }

    /// Build the shared, runtime-swappable settings (Phase 6): the initial backend
    /// selected by config plus the factory that rebuilds backends when the device
    /// changes them. The initial backend must build successfully (anthropic still
    /// needs its key at startup, matching [`Self::build_llm`]).
    pub fn shared_settings(&self) -> Result<Arc<SharedSettings>> {
        let factory = self.llm_factory();
        let (backend, model) = match &self.llm {
            LlmChoice::Mock => ("mock", None),
            LlmChoice::Ollama { model, .. } => ("ollama", Some(model.clone())),
            LlmChoice::Anthropic { model, .. } => ("anthropic", Some(model.clone())),
        };
        let (llm, llm_backend, llm_model) = factory
            .build(backend, model.as_deref())
            .context("building the initial LLM backend")?;
        Ok(SharedSettings::new(
            factory,
            RuntimeSettings {
                llm,
                llm_backend,
                llm_model,
                tts_voice: self.tts_voice.clone(),
            },
        ))
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
