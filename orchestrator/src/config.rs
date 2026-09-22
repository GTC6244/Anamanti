//! Runtime configuration for the orchestrator, read from environment variables
//! with sensible defaults. This is where the **pluggable** LLM decision is bound:
//! `AMBIENT_LLM_BACKEND` picks local (Ollama) vs cloud (Claude) vs the offline
//! mock, and the rest of the pipeline is handed a `dyn LlmBackend` — it never
//! knows which was chosen.

use std::collections::HashMap;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::llm::anthropic_auth::{AnthropicAuth, AnthropicTokenProvider};
use crate::llm::catalog::ModelCatalog;
use crate::llm::{
    anthropic::AnthropicBackend, mock::MockLlm, ollama::OllamaBackend, openai::OpenAiBackend,
    LlmBackend,
};
use crate::music::{
    GroupSelector, ManagedProc, MpvControl, MusicDucker, MusicHub, MusicSupervisor, ProcSpec,
    SnapcastClient,
};
use crate::settings::{
    load_persisted, DriveConfig, LlmEngine, LlmFactory, RuntimeSettings, SharedSettings,
};

/// Default Google Drive OAuth scope for the photo slideshow (read-only).
pub const DEFAULT_DRIVE_SCOPE: &str = "https://www.googleapis.com/auth/drive.readonly";

/// Where runtime settings are persisted, or `None` to disable persistence
/// (`AMBIENT_SETTINGS_PATH=off`). Defaults to `ambient_settings.json`.
fn settings_path_from_env() -> Option<std::path::PathBuf> {
    match env::var("AMBIENT_SETTINGS_PATH") {
        Ok(v) if matches!(v.trim().to_lowercase().as_str(), "off" | "none" | "") => None,
        Ok(v) => Some(std::path::PathBuf::from(v)),
        Err(_) => Some(std::path::PathBuf::from("ambient_settings.json")),
    }
}

/// Read the LLM engine selector from the environment. The rig-core engine (with the
/// `internet_search` tool) is now the **default**; set `AMBIENT_LLM_ENGINE=native`
/// to fall back to the hand-rolled HTTP backends (no tools). rig needs the `rig`
/// feature compiled in (on by default) — without it, `Rig` transparently degrades to
/// native and `main` logs a loud warning.
fn llm_engine_from_env() -> LlmEngine {
    match env::var("AMBIENT_LLM_ENGINE")
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "native" | "http" | "legacy" => LlmEngine::Native,
        _ => LlmEngine::Rig,
    }
}

/// Initial Anthropic auth mode from the environment
/// (`AMBIENT_ANTHROPIC_AUTH=apikey|subscription`, default `apikey`). Runtime-swappable.
fn anthropic_auth_from_env() -> AnthropicAuth {
    AnthropicAuth::from_label(&env::var("AMBIENT_ANTHROPIC_AUTH").unwrap_or_default())
}

/// Whether to enable the rig web-search tool. **On by default** now that rig is the
/// default engine (so weather/news/real-time questions work out of the box); set
/// `AMBIENT_WEB_SEARCH=off` (or `0`/`false`/`no`) to disable it. Only effective with
/// the rig engine + `rig` feature.
fn web_search_from_env() -> bool {
    !matches!(
        env::var("AMBIENT_WEB_SEARCH")
            .unwrap_or_default()
            .trim()
            .to_lowercase()
            .as_str(),
        "0" | "false" | "off" | "no"
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
    /// Cloud OpenAI Chat Completions API.
    OpenAI { model: String, max_tokens: u32 },
}

/// Fully-resolved orchestrator configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Address the device-facing Wyoming server binds to (advertised via mDNS).
    pub bind_addr: SocketAddr,
    /// Address the local HTTP **config page** binds to, or `None` to disable it.
    /// Defaults to loopback (`127.0.0.1:8730`) since the page has no auth.
    pub config_addr: Option<SocketAddr>,
    /// Human-readable mDNS instance name (the friendly label shown in the
    /// device's orchestrator dropdown).
    pub service_name: String,
    /// Stable mDNS selection key advertised in the `instance_id` TXT record. The
    /// device persists this to pin a specific orchestrator across restarts / IP
    /// changes, so it MUST be stable across restarts (never random per boot).
    /// Resolution order: `AMBIENT_INSTANCE_ID` env → the **git branch code** of the
    /// working directory (so a copy running from a test branch/worktree identifies
    /// itself by its branch without extra config) → the sanitized `service_name`.
    /// The "local production" install lives outside a git checkout and sets
    /// `AMBIENT_INSTANCE_ID` in `~/.zshenv`, so it never falls through to the branch.
    pub instance_id: String,
    /// Downstream Wyoming STT (Whisper) address.
    pub stt_addr: SocketAddr,
    /// Downstream Wyoming TTS (Piper) address.
    pub tts_addr: SocketAddr,
    /// Optional Piper voice name.
    pub tts_voice: Option<String>,
    /// Directory holding Piper voice models (`<name>.onnx`). When set (Piper is
    /// co-located with the orchestrator), the settings voice dropdown lists only the
    /// voices actually present here; when `None`, it lists Piper's full advertised
    /// catalog. Set via `AMBIENT_TTS_VOICES_DIR`.
    pub tts_voices_dir: Option<PathBuf>,
    /// Selected LLM backend.
    pub llm: LlmChoice,
    /// SQLite memory database path.
    pub db_path: PathBuf,
    /// Base system prompt/persona.
    pub system_prompt: String,
    /// The device's physical home location (e.g. "Austin, Texas"), injected into the
    /// prompt so location-relative questions (weather, sunset, nearby places) resolve
    /// an unqualified "here". `None` omits the location grounding. Set via
    /// `AMBIENT_HOME_LOCATION`.
    pub home_location: Option<String>,
    /// Preferred measurement units for answers (e.g. "imperial" / "metric"), paired
    /// with `home_location`. Set via `AMBIENT_WEATHER_UNITS`.
    pub weather_units: Option<String>,
    /// Idle timeout for a stalled turn.
    pub turn_timeout: Duration,
    /// Memory retrieval backend: `helix` (GraphRAG, default) or `sqlite` (FTS).
    pub memory_backend: MemoryBackendChoice,
    /// Append-only JSONL chat log path (always written; the ingester's queue).
    pub chatlog_path: PathBuf,
    /// Append-only JSONL prompt log path (debug/audit of the exact LLM prompt).
    pub promptlog_path: PathBuf,
    /// Embedded HelixDB on-disk store root (used when `memory_backend = helix`).
    pub helix_path: PathBuf,
    /// GraphRAG embedding + extraction settings (used when `memory_backend = helix`).
    pub graphrag: GraphRagConfig,
    /// Per-person speaker identification settings (speaker_id_plan.md).
    pub speaker: SpeakerConfig,
    /// House-wide music routing (Snapcast) control-plane settings.
    pub music: MusicConfig,
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

/// House-wide music routing (Snapcast) control-plane settings. **Inert unless
/// `enabled`** (`AMBIENT_MUSIC=on`): the orchestrator only ducks the music group
/// while it speaks and (later) selects the active stream. It never handles PCM —
/// see `crate::music` and `plans/snapcast_routing_plan.md`.
#[derive(Debug, Clone)]
pub struct MusicConfig {
    /// Master switch (`AMBIENT_MUSIC`). Off ⇒ the whole feature is dormant.
    pub enabled: bool,
    /// snapserver JSON-RPC control endpoint (`AMBIENT_MUSIC_SNAPSERVER`).
    pub snapserver_addr: SocketAddr,
    /// Duck the music group's volume while the assistant speaks
    /// (`AMBIENT_MUSIC_DUCK_ON_SPEECH`, default on).
    pub duck_on_speech: bool,
    /// Volume percent to duck to during speech (`AMBIENT_MUSIC_DUCK_PERCENT`).
    pub duck_percent: u8,
    /// Which group to duck: `auto`, a group id (`AMBIENT_MUSIC_GROUP`), or the
    /// group playing a stream id (`AMBIENT_MUSIC_STREAM`, most specific).
    pub group: GroupSelector,
    /// mpv JSON IPC socket for the web-URL player (`AMBIENT_MUSIC_WEB_IPC`).
    pub mpv_ipc: Option<PathBuf>,
    /// Directory holding the `snapserver`/`librespot`/`mpv` binaries
    /// (`AMBIENT_MUSIC_BIN_DIR`, default the Homebrew prefix bin).
    pub bin_dir: PathBuf,
    /// Directory holding the snapfifos (`AMBIENT_MUSIC_RUN_DIR`).
    pub run_dir: PathBuf,
    /// Directory for the supervised processes' log files (`AMBIENT_MUSIC_LOG_DIR`).
    pub log_dir: PathBuf,
    /// snapserver config path (`AMBIENT_MUSIC_CONF`).
    pub snapserver_conf: PathBuf,
    /// librespot Spotify Connect device name (`AMBIENT_SPOTIFY_DEVICE_NAME`,
    /// default `Ambient`; the shared instance MusicPlan.md's tool targets).
    pub spotify_device_name: String,
    /// Auto-start the managed processes (snapserver/librespot/mpv) when the
    /// orchestrator boots, and stop them on shutdown (`AMBIENT_MUSIC_AUTOSTART`,
    /// default on when music is enabled). The Music tab's manual buttons still work.
    pub autostart: bool,
}

impl Default for MusicConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            snapserver_addr: "127.0.0.1:1705".parse().unwrap(),
            duck_on_speech: true,
            duck_percent: 30,
            group: GroupSelector::Auto,
            mpv_ipc: Some(PathBuf::from("/tmp/ambient-mpv.sock")),
            bin_dir: PathBuf::from("/opt/homebrew/bin"),
            run_dir: PathBuf::from("/opt/homebrew/var/run/ambient"),
            log_dir: PathBuf::from("/opt/homebrew/var/log"),
            snapserver_conf: PathBuf::from("/opt/homebrew/etc/snapserver.conf"),
            spotify_device_name: "Ambient".to_string(),
            autostart: true,
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
            // Config page on loopback only by default (no auth); override or
            // disable with AMBIENT_CONFIG_ADDR.
            config_addr: Some("127.0.0.1:8730".parse().unwrap()),
            service_name: "Ambient Orchestrator".to_string(),
            instance_id: "ambient-orchestrator".to_string(),
            stt_addr: "127.0.0.1:10300".parse().unwrap(), // wyoming-faster-whisper default
            tts_addr: "127.0.0.1:10200".parse().unwrap(), // wyoming-piper default
            tts_voice: None,
            tts_voices_dir: None,
            llm: LlmChoice::Ollama {
                url: "http://127.0.0.1:11434".to_string(),
                model: "llama3.2".to_string(),
            },
            db_path: PathBuf::from("ambient_memory.sqlite"),
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
            home_location: None,
            weather_units: None,
            turn_timeout: Duration::from_secs(30),
            memory_backend: MemoryBackendChoice::Helix,
            chatlog_path: PathBuf::from("ambient_chatlog.jsonl"),
            promptlog_path: PathBuf::from("ambient_promptlog.jsonl"),
            helix_path: PathBuf::from("ambient_helix"),
            graphrag: GraphRagConfig::default(),
            speaker: SpeakerConfig::default(),
            music: MusicConfig::default(),
        }
    }
}

fn env_pathbuf(key: &str, default: PathBuf) -> PathBuf {
    env::var(key)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or(default)
}

fn env_addr(key: &str, default: SocketAddr) -> Result<SocketAddr> {
    match env::var(key) {
        Ok(v) => v
            .parse()
            .with_context(|| format!("parsing {key}=`{v}` as host:port")),
        Err(_) => Ok(default),
    }
}

/// The git branch code of the current working directory, or `None` when this
/// process is not running inside a checkout (e.g. the "local production" install,
/// which runs a copied binary from outside a repo and sets `AMBIENT_INSTANCE_ID`
/// explicitly). Used as the default `instance_id` so a copy launched from a test
/// branch/worktree advertises itself by its branch with no extra configuration.
/// Detached HEAD (`branch == "HEAD"`) is treated as "no branch".
fn git_branch_code() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let branch = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch)
    }
}

/// Reduce a service name to a stable selection key: lowercase, alphanumerics and
/// hyphens only, collapsed/trimmed. Kept in sync with the device's expectation
/// that the `instance_id` TXT record is a stable, human-ish identifier.
fn sanitize_id(name: &str) -> String {
    let mapped: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let trimmed = mapped.trim_matches('-');
    if trimmed.is_empty() {
        "ambient-orchestrator".to_string()
    } else {
        trimmed.to_lowercase()
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
            "openai" | "gpt" => LlmChoice::OpenAI {
                model: env::var("AMBIENT_OPENAI_MODEL")
                    .unwrap_or_else(|_| "gpt-4o-mini".to_string()),
                max_tokens: env::var("AMBIENT_OPENAI_MAX_TOKENS")
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
            .unwrap_or_else(|_| "helix".to_string())
            .to_lowercase()
            .as_str()
        {
            "sqlite" | "fts" => MemoryBackendChoice::Sqlite,
            _ => MemoryBackendChoice::Helix,
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

        let md = MusicConfig::default();
        let music = MusicConfig {
            enabled: matches!(
                env::var("AMBIENT_MUSIC")
                    .unwrap_or_default()
                    .trim()
                    .to_lowercase()
                    .as_str(),
                "on" | "1" | "true" | "yes"
            ),
            snapserver_addr: env_addr("AMBIENT_MUSIC_SNAPSERVER", md.snapserver_addr)?,
            duck_on_speech: !matches!(
                env::var("AMBIENT_MUSIC_DUCK_ON_SPEECH")
                    .unwrap_or_default()
                    .trim()
                    .to_lowercase()
                    .as_str(),
                "0" | "false" | "off" | "no"
            ),
            duck_percent: env::var("AMBIENT_MUSIC_DUCK_PERCENT")
                .ok()
                .and_then(|v| v.parse::<u8>().ok())
                .map(|p| p.min(100))
                .unwrap_or(md.duck_percent),
            group: GroupSelector::from_parts(
                env::var("AMBIENT_MUSIC_GROUP").ok().as_deref(),
                env::var("AMBIENT_MUSIC_STREAM").ok().as_deref(),
            ),
            mpv_ipc: env::var("AMBIENT_MUSIC_WEB_IPC")
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .or_else(|| md.mpv_ipc.clone()),
            bin_dir: env_pathbuf("AMBIENT_MUSIC_BIN_DIR", md.bin_dir.clone()),
            run_dir: env_pathbuf("AMBIENT_MUSIC_RUN_DIR", md.run_dir.clone()),
            log_dir: env_pathbuf("AMBIENT_MUSIC_LOG_DIR", md.log_dir.clone()),
            snapserver_conf: env_pathbuf("AMBIENT_MUSIC_CONF", md.snapserver_conf.clone()),
            spotify_device_name: env::var("AMBIENT_SPOTIFY_DEVICE_NAME")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| md.spotify_device_name.clone()),
            autostart: !matches!(
                env::var("AMBIENT_MUSIC_AUTOSTART")
                    .unwrap_or_default()
                    .trim()
                    .to_lowercase()
                    .as_str(),
                "0" | "false" | "off" | "no"
            ),
        };

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

        let service_name = env::var("AMBIENT_SERVICE_NAME").unwrap_or(d.service_name);
        // Stable selection key: explicit env override, else the git branch code of
        // the working directory (a copy running from a test branch/worktree names
        // itself by its branch), else the sanitized service name. All must be stable
        // across restarts — persisted device selections depend on this not changing.
        let instance_id = env::var("AMBIENT_INSTANCE_ID")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(git_branch_code)
            .unwrap_or_else(|| sanitize_id(&service_name));

        Ok(Self {
            bind_addr: env_addr("AMBIENT_BIND_ADDR", d.bind_addr)?,
            config_addr,
            service_name,
            instance_id,
            stt_addr: env_addr("AMBIENT_STT_ADDR", d.stt_addr)?,
            tts_addr: env_addr("AMBIENT_TTS_ADDR", d.tts_addr)?,
            tts_voice: env::var("AMBIENT_TTS_VOICE").ok().filter(|s| !s.is_empty()),
            tts_voices_dir: env::var("AMBIENT_TTS_VOICES_DIR")
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .or(d.tts_voices_dir),
            llm,
            db_path: env::var("AMBIENT_DB_PATH")
                .map(PathBuf::from)
                .unwrap_or(d.db_path),
            system_prompt: env::var("AMBIENT_SYSTEM_PROMPT").unwrap_or(d.system_prompt),
            home_location: env::var("AMBIENT_HOME_LOCATION")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            weather_units: env::var("AMBIENT_WEATHER_UNITS")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            turn_timeout: d.turn_timeout,
            memory_backend,
            chatlog_path: env::var("AMBIENT_CHATLOG_PATH")
                .map(PathBuf::from)
                .unwrap_or(d.chatlog_path),
            promptlog_path: env::var("AMBIENT_PROMPTLOG_PATH")
                .map(PathBuf::from)
                .unwrap_or(d.promptlog_path),
            helix_path: env::var("AMBIENT_HELIX_PATH")
                .map(PathBuf::from)
                .unwrap_or(d.helix_path),
            graphrag,
            speaker,
            music,
        })
    }

    /// Build the music ducker when music routing **and** duck-on-speech are both
    /// enabled; otherwise `None` (the whole path stays dormant). Best-effort at
    /// runtime — a missing/unreachable snapserver never breaks a turn.
    pub fn build_ducker(&self) -> Option<Arc<MusicDucker>> {
        if self.music.enabled && self.music.duck_on_speech {
            Some(Arc::new(MusicDucker::new(
                self.music.snapserver_addr,
                self.music.group.clone(),
                self.music.duck_percent,
            )))
        } else {
            None
        }
    }

    /// Build the mpv IPC control for the web-URL player, when music is enabled and
    /// an IPC socket is configured (`AMBIENT_MUSIC_WEB_IPC`).
    pub fn mpv_control(&self) -> Option<MpvControl> {
        if !self.music.enabled {
            return None;
        }
        self.music.mpv_ipc.as_ref().map(MpvControl::new)
    }

    /// Build the process supervisor for the music sibling processes
    /// (snapserver / librespot / mpv), or `None` when music is disabled. The
    /// launch commands mirror `orchestrator/deploy/snapcast/` (the runbook +
    /// launchd agents).
    pub fn build_supervisor(&self) -> Option<Arc<MusicSupervisor>> {
        if !self.music.enabled {
            return None;
        }
        let m = &self.music;
        let bin = |name: &str| m.bin_dir.join(name);
        let fifo = |name: &str| m.run_dir.join(name).display().to_string();
        let log = |name: &str| m.log_dir.join(name);
        let mpv_ipc = m
            .mpv_ipc
            .clone()
            .unwrap_or_else(|| PathBuf::from("/tmp/ambient-mpv.sock"));

        let mut specs = HashMap::new();
        specs.insert(
            ManagedProc::Snapserver,
            ProcSpec {
                program: bin("snapserver"),
                args: vec!["-c".into(), m.snapserver_conf.display().to_string()],
                log_path: log("ambient-snapserver.log"),
            },
        );
        specs.insert(
            ManagedProc::Librespot,
            ProcSpec {
                program: bin("librespot"),
                args: vec![
                    "--name".into(),
                    m.spotify_device_name.clone(),
                    "--backend".into(),
                    "pipe".into(),
                    "--device".into(),
                    fifo("snap-spotify"),
                    "--bitrate".into(),
                    "320".into(),
                    "--initial-volume".into(),
                    "100".into(),
                ],
                log_path: log("ambient-librespot.log"),
            },
        );
        specs.insert(
            ManagedProc::MpvWeb,
            ProcSpec {
                program: bin("mpv"),
                args: vec![
                    "--idle=yes".into(),
                    "--no-video".into(),
                    format!("--input-ipc-server={}", mpv_ipc.display()),
                    "--ao=pcm".into(),
                    "--ao-pcm-waveheader=no".into(),
                    format!("--ao-pcm-file={}", fifo("snap-web")),
                    "--audio-samplerate=48000".into(),
                    "--audio-channels=stereo".into(),
                    "--audio-format=s16".into(),
                ],
                log_path: log("ambient-mpv-web.log"),
            },
        );
        Some(Arc::new(MusicSupervisor::new(specs)))
    }

    /// Build the [`MusicHub`] (supervisor + snapserver control + mpv control) the
    /// config-page Music tab drives, or `None` when music is disabled.
    pub fn build_hub(&self) -> Option<MusicHub> {
        let supervisor = self.build_supervisor()?;
        Some(MusicHub {
            supervisor,
            snapcast: SnapcastClient::new(self.music.snapserver_addr),
            snapserver_addr: self.music.snapserver_addr.to_string(),
            mpv: self.mpv_control(),
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
            LlmChoice::OpenAI { model, max_tokens } => {
                let key = env::var("OPENAI_API_KEY")
                    .context("AMBIENT_LLM_BACKEND=openai requires OPENAI_API_KEY")?;
                Arc::new(OpenAiBackend::new(
                    self.graphrag.openai_base_url.clone(),
                    key,
                    model,
                    *max_tokens,
                ))
            }
        })
    }

    /// The inputs a runtime backend swap (Phase 6) needs, captured from the
    /// environment once so a later swap never re-reads `env`. The provider API keys
    /// read here are only the **boot seed**: they flow into the live
    /// [`RuntimeSettings`], where the config page can override them at runtime, so a
    /// cloud backend can be enabled without a restart even if its key was unset at
    /// boot. If no key is present at boot *or* runtime, selecting that backend is
    /// rejected in-band.
    pub fn llm_factory(&self) -> LlmFactory {
        let anthropic_max_tokens = match &self.llm {
            LlmChoice::Anthropic { max_tokens, .. } => *max_tokens,
            _ => env::var("AMBIENT_ANTHROPIC_MAX_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1024),
        };
        let openai_max_tokens = match &self.llm {
            LlmChoice::OpenAI { max_tokens, .. } => *max_tokens,
            _ => env::var("AMBIENT_OPENAI_MAX_TOKENS")
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
            openai_base_url: self.graphrag.openai_base_url.clone(),
            openai_api_key: env::var("OPENAI_API_KEY").ok().filter(|s| !s.is_empty()),
            openai_max_tokens,
            anthropic_token: Some(Arc::new(AnthropicTokenProvider::new())),
        }
    }

    /// The selectable-model catalog for the settings dropdown, wired with the same
    /// provider endpoints/keys the factory uses (keys read from the environment) and
    /// the same Anthropic auth mode so subscription hosts list live models.
    pub fn model_catalog(&self) -> ModelCatalog {
        ModelCatalog::new(
            "https://api.anthropic.com",
            env::var("ANTHROPIC_API_KEY").ok().filter(|s| !s.is_empty()),
            self.graphrag.openai_base_url.clone(),
            env::var("OPENAI_API_KEY").ok().filter(|s| !s.is_empty()),
        )
        .with_anthropic_auth(
            anthropic_auth_from_env(),
            Some(Arc::new(AnthropicTokenProvider::new())),
        )
    }

    /// The initial engine + web-search selection from the environment. These seed
    /// the live [`RuntimeSettings`] and can be changed at runtime (config page /
    /// control frame).
    pub fn initial_engine(&self) -> LlmEngine {
        llm_engine_from_env()
    }
    pub fn initial_web_search(&self) -> bool {
        web_search_from_env()
    }
    /// Initial search provider (`AMBIENT_SEARCH_PROVIDER`, default `duckduckgo`).
    pub fn initial_search_provider(&self) -> String {
        env::var("AMBIENT_SEARCH_PROVIDER")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "duckduckgo".to_string())
    }
    /// Initial search API key (`TAVILY_API_KEY`), if set.
    pub fn initial_search_api_key(&self) -> Option<String> {
        env::var("TAVILY_API_KEY").ok().filter(|s| !s.is_empty())
    }

    /// Initial Google Drive photo-slideshow config seeded from the environment. The
    /// "Desktop app" OAuth client id/secret come from
    /// `AMBIENT_GOOGLE_DRIVE_CLIENT_ID` / `_SECRET` (the outside-git production
    /// install sets these in `~/.zshenv`); optional folder ids from
    /// `AMBIENT_GOOGLE_DRIVE_FOLDER_IDS` (comma-separated). The refresh token is
    /// never seeded from env — it's minted by the consent flow. A persisted file
    /// overlays these at boot (see [`Self::shared_settings`]).
    pub fn initial_drive(&self) -> DriveConfig {
        let env_opt = |k: &str| {
            env::var(k)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        DriveConfig {
            client_id: env_opt("AMBIENT_GOOGLE_DRIVE_CLIENT_ID"),
            client_secret: env_opt("AMBIENT_GOOGLE_DRIVE_CLIENT_SECRET"),
            refresh_token: None,
            folder_ids: env_opt("AMBIENT_GOOGLE_DRIVE_FOLDER_IDS")
                .map(|v| {
                    v.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            scope: Some(
                env_opt("AMBIENT_GOOGLE_DRIVE_SCOPE")
                    .unwrap_or_else(|| DEFAULT_DRIVE_SCOPE.to_string()),
            ),
        }
    }

    /// Build the shared, runtime-swappable settings (Phase 6): the initial backend
    /// selected by config plus the factory that rebuilds backends when the device
    /// changes them. The initial backend must build successfully (anthropic still
    /// needs its key at startup, matching [`Self::build_llm`]).
    pub fn shared_settings(&self) -> Result<Arc<SharedSettings>> {
        let factory = self.llm_factory();
        let persist_path = settings_path_from_env();

        // Environment/config defaults, then overlay a persisted file when present
        // (page/device changes from a previous run win across restarts).
        let (backend_default, model_default) = match &self.llm {
            LlmChoice::Mock => ("mock".to_string(), None),
            LlmChoice::Ollama { model, .. } => ("ollama".to_string(), Some(model.clone())),
            LlmChoice::Anthropic { model, .. } => ("anthropic".to_string(), Some(model.clone())),
            LlmChoice::OpenAI { model, .. } => ("openai".to_string(), Some(model.clone())),
        };
        let mut engine = self.initial_engine();
        let mut web_search = self.initial_web_search();
        let mut search_provider = self.initial_search_provider();
        let mut search_api_key = self.initial_search_api_key();
        let mut backend = backend_default;
        let mut model = model_default;
        // Live provider keys start from the environment (the factory's boot seed) and
        // become runtime-settable; a persisted key overlays them so a key entered on
        // the config page enables the cloud backend without a restart.
        let mut anthropic_api_key = factory.anthropic_api_key.clone();
        let mut openai_api_key = factory.openai_api_key.clone();
        let mut anthropic_auth = anthropic_auth_from_env();
        let mut tts_voice = self.tts_voice.clone();

        let mut end_silence_ms = crate::settings::DEFAULT_END_SILENCE_MS;
        let mut voice_rms_threshold = crate::settings::DEFAULT_VOICE_RMS_THRESHOLD;
        // Google Drive photo config: env seed (client creds/folders), overlaid by
        // any persisted values below (the refresh token + page-set fields win).
        let mut drive = self.initial_drive();

        if let Some(p) = persist_path.as_deref().and_then(load_persisted) {
            log::info!("loaded persisted settings");
            engine = LlmEngine::from_label(&p.engine);
            web_search = p.web_search;
            search_provider = p.search_provider;
            search_api_key = p.search_api_key;
            backend = p.llm_backend;
            model = p.llm_model;
            // Only override the env key when the persisted file actually carries one,
            // so a pre-feature settings file (key absent → serde default `None`) can't
            // wipe a working `ANTHROPIC_API_KEY`/`OPENAI_API_KEY` from the environment.
            if p.anthropic_api_key.is_some() {
                anthropic_api_key = p.anthropic_api_key;
            }
            if p.openai_api_key.is_some() {
                openai_api_key = p.openai_api_key;
            }
            anthropic_auth = AnthropicAuth::from_label(&p.anthropic_auth);
            tts_voice = p.tts_voice;
            end_silence_ms = p.end_silence_ms;
            voice_rms_threshold = p.voice_rms_threshold;
            // Overlay persisted Drive fields onto the env seed: a persisted value
            // wins (refresh token, page-set creds/folders), but keep the env seed
            // for any field the persisted file leaves empty so setting a client id
            // in `~/.zshenv` still takes effect after an older file is loaded.
            if p.drive.client_id.is_some() {
                drive.client_id = p.drive.client_id;
            }
            if p.drive.client_secret.is_some() {
                drive.client_secret = p.drive.client_secret;
            }
            if p.drive.refresh_token.is_some() {
                drive.refresh_token = p.drive.refresh_token;
            }
            if !p.drive.folder_ids.is_empty() {
                drive.folder_ids = p.drive.folder_ids;
            }
            if p.drive.scope.is_some() {
                drive.scope = p.drive.scope;
            }
        }

        // Build the initial backend with the resolved live keys (env overlaid by any
        // persisted key), never re-reading the environment during a later swap.
        let mut build_factory = factory.clone();
        build_factory.anthropic_api_key = anthropic_api_key.clone();
        build_factory.openai_api_key = openai_api_key.clone();
        let (llm, llm_backend, llm_model) = build_factory
            .build(
                engine,
                web_search,
                &search_provider,
                search_api_key.as_deref(),
                &backend,
                model.as_deref(),
                anthropic_auth,
            )
            .context("building the initial LLM backend")?;
        Ok(SharedSettings::new_persistent(
            factory,
            RuntimeSettings {
                llm,
                engine,
                web_search,
                search_provider,
                search_api_key,
                llm_backend,
                llm_model,
                anthropic_api_key,
                openai_api_key,
                anthropic_auth,
                tts_voice,
                end_silence_ms,
                voice_rms_threshold,
                drive,
            },
            persist_path,
        ))
    }

    /// Build the per-person [`SpeakerService`], or `None` when speaker ID is
    /// disabled. The registry lives in the same SQLite file as memory. Until the
    /// ONNX embedder lands (Phase E) this uses the deterministic mock embedder so
    /// the end-to-end per-person plumbing is exercisable — with a loud warning, as
    /// the mock is not accurate for real voices.
    pub fn build_speaker_service(&self) -> Result<Option<Arc<crate::speaker::SpeakerService>>> {
        use crate::speaker::{
            MockSpeakerEmbedder, SpeakerEmbedder, SpeakerRegistry, SpeakerService,
            SpeakerThresholds,
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
                match OnnxSpeakerEmbedder::open(
                    path,
                    self.speaker.embed_dims,
                    FbankConfig::default(),
                ) {
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
            LlmChoice::OpenAI { .. } => "openai",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_id_defaults_match_service_name() {
        assert_eq!(Config::default().instance_id, "ambient-orchestrator");
    }

    #[test]
    fn sanitize_id_maps_service_names_to_stable_keys() {
        assert_eq!(sanitize_id("Ambient Orchestrator"), "ambient-orchestrator");
        assert_eq!(sanitize_id("Test Mac"), "test-mac");
        assert_eq!(sanitize_id("Mac Mini (prod)"), "mac-mini--prod");
        assert_eq!(sanitize_id("***"), "ambient-orchestrator");
    }
}
