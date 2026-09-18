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
use crate::settings::{LlmFactory, RuntimeSettings, SharedSettings};

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
    /// Memory retrieval backend: `sqlite` (FTS, default) or `helix` (GraphRAG).
    pub memory_backend: MemoryBackendChoice,
    /// Append-only JSONL chat log path (always written; the ingester's queue).
    pub chatlog_path: PathBuf,
    /// Embedded HelixDB on-disk store root (used when `memory_backend = helix`).
    pub helix_path: PathBuf,
    /// GraphRAG embedding + extraction settings (used when `memory_backend = helix`).
    pub graphrag: GraphRagConfig,
    /// Per-person speaker identification settings (speaker_id_plan.md).
    pub speaker: SpeakerConfig,
}

/// Speaker-identification configuration. Off by default (opt-in like `helix`); a
/// missing model path degrades gracefully to the shared household.
#[derive(Debug, Clone)]
pub struct SpeakerConfig {
    /// Whether to identify speakers per turn (`AMBIENT_SPEAKER_ID=on`).
    pub enabled: bool,
    /// Path to the speaker-embedding ONNX model (Phase E). Absent ⇒ household.
    pub model_path: Option<PathBuf>,
    /// Cosine ≥ this ⇒ confident match (centroid updated).
    pub match_threshold: f32,
    /// Cosine below this ⇒ mint a new anonymous cluster.
    pub new_threshold: f32,
    /// Minimum voiced audio (ms) before attempting identification.
    pub min_speech_ms: u32,
    /// Embedding dimensionality the ONNX model produces (ECAPA-TDNN → 192).
    pub embed_dims: usize,
}

impl Default for SpeakerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model_path: None,
            match_threshold: 0.55,
            new_threshold: 0.40,
            min_speech_ms: 1200,
            embed_dims: 192,
        }
    }
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
            speaker: SpeakerConfig::default(),
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

        let sd = SpeakerConfig::default();
        let speaker = SpeakerConfig {
            enabled: matches!(
                env::var("AMBIENT_SPEAKER_ID")
                    .unwrap_or_default()
                    .to_lowercase()
                    .as_str(),
                "on" | "1" | "true" | "yes"
            ),
            model_path: env::var("AMBIENT_SPEAKER_MODEL_PATH")
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from),
            match_threshold: env::var("AMBIENT_SPEAKER_MATCH_THRESHOLD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(sd.match_threshold),
            new_threshold: env::var("AMBIENT_SPEAKER_NEW_THRESHOLD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(sd.new_threshold),
            min_speech_ms: env::var("AMBIENT_SPEAKER_MIN_SPEECH_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(sd.min_speech_ms),
            embed_dims: env::var("AMBIENT_SPEAKER_EMBED_DIMS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(sd.embed_dims),
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
            memory_backend,
            chatlog_path: env::var("AMBIENT_CHATLOG_PATH")
                .map(PathBuf::from)
                .unwrap_or(d.chatlog_path),
            helix_path: env::var("AMBIENT_HELIX_PATH")
                .map(PathBuf::from)
                .unwrap_or(d.helix_path),
            graphrag,
            speaker,
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

    /// Build the per-person [`SpeakerService`], or `None` when speaker ID is
    /// disabled. The registry lives in the same SQLite file as memory. Until the
    /// ONNX embedder lands (Phase E) this uses the deterministic mock embedder so
    /// the end-to-end per-person plumbing is exercisable — with a loud warning, as
    /// the mock is not accurate for real voices.
    pub fn build_speaker_service(&self) -> Result<Option<Arc<crate::speaker::SpeakerService>>> {
        use crate::speaker::{
            MockSpeakerEmbedder, SpeakerEmbedder, SpeakerRegistry, SpeakerService, SpeakerThresholds,
        };
        if !self.speaker.enabled {
            return Ok(None);
        }
        let registry =
            SpeakerRegistry::open(&self.db_path).context("opening speaker registry database")?;
        let thresholds = SpeakerThresholds::from_ms(
            self.speaker.match_threshold,
            self.speaker.new_threshold,
            self.speaker.min_speech_ms,
        );
        let embedder: Arc<dyn SpeakerEmbedder> = match &self.speaker.model_path {
            #[cfg(feature = "speaker")]
            Some(path) => {
                use crate::speaker::embed::OnnxSpeakerEmbedder;
                use crate::speaker::features::FbankConfig;
                match OnnxSpeakerEmbedder::open(path, self.speaker.embed_dims, FbankConfig::default())
                {
                    Ok(e) => {
                        log::info!("speaker embedder: ONNX model {}", path.display());
                        Arc::new(e)
                    }
                    Err(err) => {
                        log::error!(
                            "failed to load speaker model {} ({err:#}); using the mock embedder",
                            path.display()
                        );
                        Arc::new(MockSpeakerEmbedder::default())
                    }
                }
            }
            #[cfg(not(feature = "speaker"))]
            Some(path) => {
                log::warn!(
                    "AMBIENT_SPEAKER_MODEL_PATH is set ({}) but the binary was built without the \
                     `speaker` feature; using the deterministic mock embedder (dev only). Rebuild \
                     with --features speaker to use the ONNX model.",
                    path.display()
                );
                Arc::new(MockSpeakerEmbedder::default())
            }
            None => {
                log::warn!(
                    "speaker ID enabled with no model path; using the deterministic mock embedder \
                     (dev/testing only — not accurate for real voices). Set AMBIENT_SPEAKER_MODEL_PATH \
                     and build with --features speaker to use the ONNX model."
                );
                Arc::new(MockSpeakerEmbedder::default())
            }
        };
        Ok(Some(Arc::new(SpeakerService::new(
            embedder, registry, thresholds,
        ))))
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
