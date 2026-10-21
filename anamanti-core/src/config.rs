//! Runtime configuration for the orchestrator, loaded from a **per-instance JSON
//! file** (`anamanti.json` in the working directory by default, overridable with
//! `--config <path>`). This is where the **pluggable** LLM decision is bound:
//! `llm.backend` picks local (Ollama) vs cloud (Claude) vs the offline mock, and the
//! rest of the pipeline is handed a `dyn LlmBackend` — it never knows which was chosen.
//!
//! Only **secrets** remain environment variables: the provider API keys/tokens
//! (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `TAVILY_API_KEY`, `MAPBOX_TOKEN`/
//! `MAPBOX_ACCESS_TOKEN`, `VISUALCROSSING_API_KEY`, `ANTHROPIC_OAUTH_TOKEN`) and the
//! standard `RUST_LOG`.
//! Everything else — addresses, paths, identity, feature toggles, and the OAuth
//! *client* credentials for Spotify/Drive — lives in the JSON file.

use std::collections::HashMap;
use std::env;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::calendar::CalendarSpec;
use crate::llm::anthropic_auth::{AnthropicAuth, AnthropicTokenProvider};
use crate::llm::catalog::ModelCatalog;
use crate::music::{
    GroupSelector, ManagedProc, MpvControl, MusicDucker, MusicHub, MusicSupervisor, ProcSpec,
    SnapcastClient,
};
use crate::settings::{
    load_persisted, AppSaidConfig, CadoraConfig, DriveConfig, Household, LlmEngine, LlmFactory,
    PersistedSettings, Personality, RuntimeSettings, SharedSettings, SpotifyConfig,
};

/// Default Google Drive OAuth scope for the photo slideshow (read-only).
pub const DEFAULT_DRIVE_SCOPE: &str = "https://www.googleapis.com/auth/drive.readonly";

/// The convention config-file path, looked up in the working directory when no
/// `--config <path>` is given. Absent → built-in defaults.
pub const DEFAULT_CONFIG_FILE: &str = "anamanti.json";

/// The default command that prints a fresh Anthropic subscription access token, used
/// when `llm.anthropic_token_cmd` is unset.
pub const DEFAULT_ANTHROPIC_TOKEN_CMD: &str = "ant auth print-credentials --access-token";

/// Interpret a config-file string that may carry the disable sentinel `off`/`none`/
/// (empty) as "feature disabled" (`None`), else `Some(trimmed)`.
fn disable_sentinel(v: &str) -> Option<&str> {
    match v.trim() {
        s if matches!(s.to_lowercase().as_str(), "off" | "none" | "") => None,
        s => Some(s),
    }
}

/// Resolve the boot-seed home address from the config file: prefer the structured
/// `home_address` object; otherwise fold the legacy single-line `home_location` string
/// into `city` (which preserves the old weather behavior of passing the whole string to
/// the provider). Blank in → empty `HomeAddress`.
fn resolve_home_address(
    structured: Option<crate::household::HomeAddress>,
    legacy: Option<String>,
) -> crate::household::HomeAddress {
    match structured.map(|a| a.sanitized()) {
        Some(a) if !a.is_empty() => a,
        _ => crate::household::HomeAddress {
            city: legacy
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            ..Default::default()
        },
    }
}

/// Default persona/system prompt: concise, speakable replies for an ambient
/// display. Kept short because the reply is spoken aloud via Piper. It explicitly
/// allows open-ended requests (stories, jokes, explanations): with a tool-heavy
/// preamble and no such line, the model concluded it was a tools-only assistant and
/// refused "tell me a story" with a list of its tools.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are a friendly, concise voice assistant for a home \
ambient display. Your replies are spoken aloud, so do not use markdown, lists, or emoji. Keep \
ordinary answers to one or two short sentences. You are also a general conversational \
assistant: answer general-knowledge questions, tell stories, jokes, or poems, explain things, \
and chat directly from your own knowledge — a story or explanation may run a short spoken \
paragraph. Tools are only for actions and live information; never refuse a request just \
because no tool covers it, and do not list your features unless asked what you can do. If \
you don't know a fact, say so briefly.";

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
    /// Resolution order: the config file's `instance_id` → the **git branch code** of
    /// the working directory (so a copy running from a test branch/worktree identifies
    /// itself by its branch without extra config) → the sanitized `service_name`.
    /// The "local production" install lives outside a git checkout and sets
    /// `instance_id` in its `anamanti.json`, so it never falls through to the branch.
    pub instance_id: String,
    /// Downstream Wyoming STT (Whisper) address.
    pub stt_addr: SocketAddr,
    /// Downstream Wyoming TTS (Piper) address.
    pub tts_addr: SocketAddr,
    /// Optional per-language Piper servers (`tts_addrs`, `plans/BilingualPlan.md` §2.5):
    /// wyoming-piper keeps one voice loaded, so switching languages on one server reloads
    /// the model (~1 s). A Piper per language keeps each voice resident. Unlisted
    /// languages use `tts_addr`.
    pub tts_addrs: std::collections::BTreeMap<crate::lang::Lang, SocketAddr>,
    /// Optional Piper voice name.
    pub tts_voice: Option<String>,
    /// Directory holding Piper voice models (`<name>.onnx`). When set (Piper is
    /// co-located with the orchestrator), the settings voice dropdown lists only the
    /// voices actually present here; when `None`, it lists Piper's full advertised
    /// catalog. Set via the config file's `tts_voices_dir`.
    pub tts_voices_dir: Option<PathBuf>,
    /// The household's languages + per-language Piper voices (`plans/BilingualPlan.md`),
    /// from the `language` and `tts_voices` blocks. Absent ⇒ English only, exactly the
    /// pre-bilingual behaviour. Seeds the runtime settings (page-editable).
    pub language: crate::lang::LanguageSettings,
    /// Selected LLM backend.
    pub llm: LlmChoice,
    /// SQLite memory database path.
    pub db_path: PathBuf,
    /// Base system prompt/persona.
    pub system_prompt: String,
    /// The device's physical home location as discrete fields (Address 1/2, City,
    /// State/Prov, Zip/Postal, Country). Directions + nearby-places use the full combined
    /// address; weather + the prompt's "here" grounding use only City + State/Prov. An
    /// all-blank value omits the location grounding. Seeded from the config file's
    /// `home_address` object (or the legacy single-line `home_location` string → City).
    pub home_address: crate::household::HomeAddress,
    /// Preferred measurement units for answers (e.g. "imperial" / "metric"), paired
    /// with `home_address`. Set via the config file's `weather_units`.
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
    /// STT engine selection: the downstream Wyoming Whisper server (`stt_addr`) or
    /// the in-process whisper.cpp engine (`plans/python-to-rust-whisper.md`).
    pub stt: SttConfig,
    /// End-of-speech VAD engine: the energy/RMS gate (default) or opt-in Silero
    /// (`plans/VadSileroPlan.md`).
    pub vad: VadConfig,
    /// Auto follow-up listening: reopen the mic (no wake word) when a reply is a
    /// question, and feed recent history into that turn's prompt.
    pub follow_up: FollowUpConfig,
    /// House-wide music routing (Snapcast) control-plane settings.
    pub music: MusicConfig,
    /// Weather feature settings (the `weather_lookup` tool + the ambient push).
    pub weather: WeatherSettings,
    /// Places feature settings (the `places_lookup` tool).
    pub places: PlacesSettings,
    /// System-1 fast-decision engine selection (plans/system1-fast-decisions.md).
    pub system1: System1Config,
    /// Where the runtime-swappable settings overlay is persisted (`settings_path` in
    /// the config file), or `None` to keep runtime settings in memory only. Defaults
    /// to `anamanti_settings.json`. This is a **separate** file from the boot config:
    /// the config page writes it and it overlays these seed values at the next boot.
    pub settings_path: Option<PathBuf>,
    /// Debug-only per-turn audio capture directory (AEC corpus). `None` disables it.
    pub audio_dump_dir: Option<PathBuf>,
    /// Initial Anthropic auth mode (`apikey`/`subscription`). Runtime-swappable.
    pub anthropic_auth: AnthropicAuth,
    /// Initial LLM engine (rig vs native). Runtime-swappable.
    pub engine: LlmEngine,
    /// Initial rig web-search enable. Runtime-swappable.
    pub web_search: bool,
    /// Initial web-search provider (`duckduckgo`/`tavily`). Runtime-swappable.
    pub search_provider: String,
    /// Command that prints a fresh Anthropic subscription token (`None` → the built-in
    /// `ant` default). Non-secret; the token itself stays in `ANTHROPIC_OAUTH_TOKEN`.
    pub anthropic_token_cmd: Option<String>,
    /// Ollama base URL used to (re)build the ollama backend at runtime, independent of
    /// which backend is currently selected.
    pub ollama_url: String,
    /// `max_tokens` cap for the Anthropic backend (independent of the current backend).
    pub anthropic_max_tokens: u32,
    /// `max_tokens` cap for the OpenAI backend (independent of the current backend).
    pub openai_max_tokens: u32,
    /// Web-calendar subscriptions for the `calendar_lookup` tool (empty → no tool).
    pub calendar_specs: Vec<CalendarSpec>,
    /// Explicit cache TTL for fetched `.ics` feeds (`calendar.cache_ttl_secs`; `0` =
    /// no cache). `None` when the key is absent ⇒ `tool_cache.calendar_lookup` (default
    /// 5 min) applies.
    pub calendar_cache_ttl: Option<Duration>,
    /// Routing provider label for the `directions_lookup` tool (empty → `mapbox`).
    pub directions_provider: String,
    /// Google Drive photo-slideshow seed (client creds + folders; the refresh token is
    /// minted by consent, never seeded). Overlaid by the persisted settings file.
    pub drive: DriveConfig,
    /// Spotify voice-control seed (client creds + refresh token + device name).
    /// Overlaid by the persisted settings file.
    pub spotify: SpotifyConfig,
    /// Cadora shopping-list seed (base URL + voice-link token). The token is normally
    /// minted by the config-page pairing flow. Overlaid by the persisted settings file.
    pub cadora: CadoraConfig,
    /// AppSaid phone-push seed (worker URL + recipients + default recipient). The
    /// secret app token is NOT here — it seeds from `APPSAID_APP_TOKEN`. Overlaid by
    /// the persisted settings file.
    pub appsaid: AppSaidConfig,
    /// Home Assistant (`plans/HomeAssistantPlan.md`): base URL, the default display room,
    /// exact-only device names and the dashboard path. The long-lived token is NOT here —
    /// it is a secret, read from `HOME_ASSISTANT_TOKEN`.
    pub home_assistant: crate::home_assistant::HomeAssistantConfig,
    /// Live NVR camera view (`plans/CameraPlan.md`): NVR host, RTSP port, user and the
    /// camera→channel map. The NVR password is NOT here — it is a secret, read from
    /// `REOLINK_PASSWORD`. Boot-time only (not runtime-settable yet).
    pub camera: crate::camera::CameraConfig,
    /// Per-tool response-cache TTLs (`tool_cache` in the file), keyed by tool name (e.g.
    /// `weather_lookup`, default 3600 s). Installed process-wide at boot; see
    /// [`crate::cache`]. `0` disables caching for a tool.
    pub tool_cache: crate::cache::ToolCacheConfig,
}

/// Speaker-identification configuration. Off by default (`speaker.enabled`); a
/// missing model path degrades gracefully to the deterministic mock embedder.
#[derive(Debug, Clone)]
pub struct SpeakerConfig {
    /// Whether to identify speakers per turn (`speaker.enabled`).
    pub enabled: bool,
    /// Path to the speaker-embedding ONNX model (Phase E). Absent ⇒ household.
    pub model_path: Option<PathBuf>,
    /// Cosine ≥ this ⇒ confident match (centroid updated).
    pub match_threshold: f32,
    /// Cosine below this ⇒ mint a new anonymous cluster.
    pub new_threshold: f32,
    /// Minimum voiced audio (ms) before a turn may **create or update** a voiceprint.
    pub min_speech_ms: u32,
    /// Minimum voiced audio (ms) for a **read-only** short-clip match ("who am I?" is
    /// well under `min_speech_ms`). Below it the speaker is unknown. `0` disables the
    /// short-clip path. See SpeakerScopedMemoryPlan.md §4.
    pub min_identify_ms: u32,
    /// Extra cosine a short-clip match needs above `match_threshold` to count as
    /// identified (short clips embed less reliably); otherwise it is at most tentative.
    pub short_clip_margin: f32,
    /// Household-relative scoring: cosine lead a match needs over the closest *other
    /// named person* to be confident; a closer race is only tentative and trains no
    /// voiceprint. `0` disables.
    pub min_margin: f32,
    /// Household-relative scoring: a match to a named person between `new_threshold`
    /// and `match_threshold` that leads every other named person by at least this is
    /// identified (its voiceprint is not trained). Needs ≥ 2 named people. `0` disables.
    pub clear_margin: f32,
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
            min_identify_ms: 600,
            short_clip_margin: 0.10,
            min_margin: 0.05,
            clear_margin: 0.20,
            embed_dims: 192,
        }
    }
}

/// Which STT engine transcribes a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SttEngineKind {
    /// Dial the downstream Wyoming Whisper server at `stt_addr` (historical default).
    Wyoming,
    /// In-process whisper.cpp via `whisper-rs` (requires the `stt-whisper-local`
    /// build feature).
    WhisperLocal,
    /// ElevenLabs Scribe v2 Realtime over WebSocket (always compiled; requires the
    /// `ELEVENLABS_API_KEY` secret + network). See `plans/ElevenLabsSttPlan.md`.
    ElevenLabs,
}

impl SttEngineKind {
    /// Parse the config label. `wyoming` (or `faster-whisper`) → the downstream
    /// server; `whisper-rs` / `whisper-local` / `local` → in-process;
    /// `elevenlabs` / `eleven-labs` / `11labs` → ElevenLabs realtime.
    pub fn from_label(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "wyoming" | "faster-whisper" | "whisper-wyoming" => Some(Self::Wyoming),
            "whisper-rs" | "whisper-local" | "whisper_local" | "local" => Some(Self::WhisperLocal),
            "elevenlabs" | "eleven-labs" | "eleven_labs" | "11labs" => Some(Self::ElevenLabs),
            _ => None,
        }
    }

    /// The canonical label (round-trips with [`from_label`](Self::from_label)); used to
    /// persist the selection and report it to the config page.
    pub fn as_label(self) -> &'static str {
        match self {
            Self::Wyoming => "wyoming",
            Self::WhisperLocal => "whisper-rs",
            Self::ElevenLabs => "elevenlabs",
        }
    }
}

/// STT engine selection + in-process model settings (`stt` block). See
/// `plans/python-to-rust-whisper.md`. Note the downstream address lives in the
/// top-level `stt_addr` (used when `engine = wyoming`).
#[derive(Debug, Clone)]
pub struct SttConfig {
    /// Which engine transcribes (`wyoming` default, `whisper-rs`, or `elevenlabs`).
    pub engine: SttEngineKind,
    /// Named model size for the in-process engine: `base` (default) or `small`,
    /// resolved to `<model_dir>/ggml-<model>.en.bin` unless `model_path` overrides.
    pub model: String,
    /// Use the **multilingual** ggml file (`ggml-<model>.bin`, e.g. `small` or
    /// `large-v3-turbo`) instead of the English-only `.en` one — required for bilingual
    /// mode on the in-process engine (`plans/BilingualPlan.md` §2.3). Default `false`.
    pub multilingual: bool,
    /// Directory holding the ggml model files (`ggml-base.en.bin`, …).
    pub model_dir: PathBuf,
    /// Explicit ggml model file; overrides `model`/`model_dir` when set.
    pub model_path: Option<PathBuf>,
    /// Decode language (`Some("en")`); `None` ⇒ auto-detect. Shared by the in-process
    /// Whisper engine and the ElevenLabs engine (passed as `language_code`).
    pub language: Option<String>,
    /// Decode thread cap; `0` ⇒ a sensible default from host parallelism.
    pub num_threads: u32,
    /// ElevenLabs realtime settings (used only when `engine = elevenlabs`).
    pub elevenlabs: ElevenLabsSttConfig,
}

/// ElevenLabs Scribe v2 Realtime settings (`stt.elevenlabs` block). The API key is a
/// **secret** and never lives here — it comes from `ELEVENLABS_API_KEY`.
#[derive(Debug, Clone)]
pub struct ElevenLabsSttConfig {
    /// Model id; only `scribe_v2_realtime` is accepted by this endpoint today.
    pub model_id: String,
    /// WebSocket host base, e.g. `wss://api.elevenlabs.io` (regional residency hosts
    /// like `wss://api.eu.residency.elevenlabs.io` also exist).
    pub base_url: String,
}

impl Default for ElevenLabsSttConfig {
    fn default() -> Self {
        Self {
            model_id: crate::stt::elevenlabs::DEFAULT_MODEL_ID.to_string(),
            base_url: crate::stt::elevenlabs::DEFAULT_BASE_URL.to_string(),
        }
    }
}

impl SttConfig {
    /// The ggml model file for the in-process engine: `model_path` if set, else
    /// `<model_dir>/ggml-<model>.en.bin`.
    pub fn resolved_model_path(&self) -> PathBuf {
        self.model_path.clone().unwrap_or_else(|| {
            let suffix = if self.multilingual { "" } else { ".en" };
            self.model_dir
                .join(format!("ggml-{}{suffix}.bin", self.model))
        })
    }
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            engine: SttEngineKind::Wyoming,
            model: "base".to_string(),
            multilingual: false,
            model_dir: PathBuf::from("models"),
            model_path: None,
            language: Some("en".to_string()),
            num_threads: 0,
            elevenlabs: ElevenLabsSttConfig::default(),
        }
    }
}

/// Which end-of-speech VAD engine the Anamanti Core runs (`vad.engine`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadEngineKind {
    /// The energy/RMS gate — opt-in since 2026-09-30 (`vad.engine="energy"`). No model,
    /// no extra deps; also the automatic fallback when a `vad-silero` build has no model
    /// loaded. Select it (or build `--no-default-features`) for the historical behavior.
    Energy,
    /// The neural Silero VAD — the committed default (2026-09-30). Requires the
    /// `vad-silero` build feature (on by default; onnxruntime via `ort`) and the v4 model
    /// file on disk; a `silero` boot with no model is a hard error. See
    /// `plans/VadSileroPlan.md`.
    Silero,
}

impl VadEngineKind {
    /// Parse the config label. `energy`/`rms` → energy; `silero`/`neural` → Silero.
    pub fn from_label(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "energy" | "rms" => Some(Self::Energy),
            "silero" | "neural" => Some(Self::Silero),
            _ => None,
        }
    }

    /// Canonical string label (for JSON relays + persistence).
    pub fn as_label(self) -> &'static str {
        match self {
            Self::Energy => "energy",
            Self::Silero => "silero",
        }
    }
}

/// Silero neural-VAD settings (`vad.silero` block). Only consulted when
/// `vad.engine="silero"`.
#[derive(Debug, Clone)]
pub struct SileroConfig {
    /// Path to the Silero **v4** ONNX model (`silero_vad.onnx`), resolved relative to
    /// the working directory. Provisioned by `scripts/fetch-vad-model.sh` (not
    /// committed). v4 is used deliberately: the v5 export scores a near-constant ~0
    /// under onnxruntime and never fires (see `crate::vad::silero` + VadSileroPlan.md).
    pub model_path: PathBuf,
    /// Speech-probability gate in `0.0..=1.0`: a frame is voiced when its Silero
    /// probability is `>= threshold`. 0.5 is the Silero-recommended default.
    pub threshold: f32,
}

impl Default for SileroConfig {
    fn default() -> Self {
        Self {
            model_path: PathBuf::from("models/silero_vad.onnx"),
            threshold: 0.5,
        }
    }
}

/// End-of-speech VAD engine selection + engine settings (`vad` block). The detector
/// sits behind the `SpeechGate` seam (`crate::vad`); the surrounding onset-debounce /
/// hangover state machine is engine-independent. See `plans/VadSileroPlan.md`.
#[derive(Debug, Clone)]
pub struct VadConfig {
    /// Which engine decides speech per chunk (`energy` default, or `silero`).
    pub engine: VadEngineKind,
    /// Silero engine settings (used only when `engine == Silero`).
    pub silero: SileroConfig,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            // Silero is the committed default (2026-09-30; VadSileroPlan.md §5/M3).
            // A default build (`vad-silero` on) boots on Silero and requires the v4
            // model on disk (`scripts/fetch-vad-model.sh`); a missing model is a
            // hard boot error. Set `vad.engine="energy"` to opt back into the RMS gate.
            engine: VadEngineKind::Silero,
            silero: SileroConfig::default(),
        }
    }
}

/// Auto follow-up listening (`follow_up` block). After **every** spoken reply the
/// device reopens the mic and listens for more input with no wake word; that follow-up
/// turn's prompt is seeded with recent conversation history. The listen window differs
/// by reply: a **question** (ends with `?`) waits `question_wait_secs`, any other reply
/// waits `reply_wait_secs`, then the assistant sleeps if nothing was said. The window is
/// passed to the device in the `ambient-listen` frame and echoed back so the
/// orchestrator sizes the follow-up turn's no-speech VAD window to match. See
/// `plans/Plan.MD` (Follow-up listening) and `architecture.md` §4.
#[derive(Debug, Clone)]
pub struct FollowUpConfig {
    /// Master switch (`follow_up.enabled`, default on). Off ⇒ the orchestrator never
    /// sends an `ambient-listen` frame and every turn stays single-shot.
    pub enabled: bool,
    /// Optional safety ceiling on consecutive auto follow-ups (`follow_up.max_chain`).
    /// **`0` = unlimited** (the default): the loop is instead terminated by silence (an
    /// empty follow-up transcript sends no new `ambient-listen`). A non-zero value caps
    /// the chain, requiring a wake word again after that many auto-listens.
    pub max_chain: u32,
    /// Listen window (seconds) after a **question** reply — how long the mic stays open
    /// for the answer before sleeping (`follow_up.question_wait_secs`, default 10).
    pub question_wait_secs: u32,
    /// Listen window (seconds) after any **non-question** reply, before sleeping
    /// (`follow_up.reply_wait_secs`, default 5).
    pub reply_wait_secs: u32,
    /// How many recent turns to feed into a follow-up turn's prompt as history
    /// (`follow_up.history_turns`, default 5).
    pub history_turns: usize,
    /// Only include history turns completed within this many seconds of the follow-up
    /// (`follow_up.history_window_secs`, default 600 = 10 minutes).
    pub history_window_secs: i64,
}

impl Default for FollowUpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_chain: 0,
            question_wait_secs: 10,
            reply_wait_secs: 5,
            history_turns: 5,
            history_window_secs: 600,
        }
    }
}

/// House-wide music routing (Snapcast) control-plane settings, from the config file's
/// `music` block. **Inert unless `enabled`** (`music.enabled`): the orchestrator only
/// ducks the music group while it speaks and (later) selects the active stream. It
/// never handles PCM — see `crate::music` and `plans/snapcast_routing_plan.md`.
#[derive(Debug, Clone)]
pub struct MusicConfig {
    /// Master switch (`music.enabled`). Off ⇒ the whole feature is dormant.
    pub enabled: bool,
    /// snapserver JSON-RPC control endpoint (`music.snapserver_addr`).
    pub snapserver_addr: SocketAddr,
    /// Duck the music group's volume while the assistant speaks
    /// (`music.duck_on_speech`, default on).
    pub duck_on_speech: bool,
    /// Volume percent to duck to during speech (`music.duck_percent`).
    pub duck_percent: u8,
    /// Which group to duck: `auto`, a group id (`music.group`), or the group playing a
    /// stream id (`music.stream`, most specific).
    pub group: GroupSelector,
    /// mpv JSON IPC socket for the web-URL player (`music.web_ipc`).
    pub mpv_ipc: Option<PathBuf>,
    /// Directory holding the `snapserver`/`librespot`/`mpv` binaries
    /// (`music.bin_dir`, default the Homebrew prefix bin).
    pub bin_dir: PathBuf,
    /// Directory holding the snapfifos (`music.run_dir`).
    pub run_dir: PathBuf,
    /// Directory for the supervised processes' log files (`music.log_dir`).
    pub log_dir: PathBuf,
    /// snapserver config path (`music.conf`).
    pub snapserver_conf: PathBuf,
    /// librespot Spotify Connect device name (`music.spotify_device_name`,
    /// default `Ambient`; the shared instance MusicPlan.md's tool targets).
    pub spotify_device_name: String,
    /// Auto-start the managed processes (snapserver/librespot/mpv) when the
    /// orchestrator boots, and stop them on shutdown (`music.autostart`,
    /// default on when music is enabled). The Music tab's manual buttons still work.
    pub autostart: bool,
    /// The players (mpv, librespot) are launched **by snapserver** as `process://` /
    /// `librespot://` sources reading their stdout, not by the Core writing named FIFOs
    /// (`music.players_in_snapserver`, default on — matching the committed
    /// `deploy/snapcast/snapserver.conf`). On macOS snapserver's FIFO reader stalls (its
    /// open→EOF→reopen loop trips macOS kqueue FIFO bugs: the writer fills the pipe and
    /// blocks forever), so the FIFO layout only works on Linux. When on, the Core
    /// supervises snapserver alone. Set `false` only with a `pipe://` snapserver.conf.
    pub players_in_snapserver: bool,
    /// How often (seconds) the now-playing push polls Spotify for the current track +
    /// queue and fans it out to the display's music screen (`music.nowplaying_refresh_secs`,
    /// default 5). Clamped to a 1s floor at use.
    pub nowplaying_refresh_secs: u64,
}

impl MusicConfig {
    /// The now-playing push poll interval (`music.nowplaying_refresh_secs`), floored at 1s.
    pub fn nowplaying_refresh(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.nowplaying_refresh_secs.max(1))
    }
}

impl Default for MusicConfig {
    fn default() -> Self {
        Self {
            enabled: true,
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
            players_in_snapserver: true,
            nowplaying_refresh_secs: 5,
        }
    }
}

/// Weather feature settings: the master switch, which forecast provider backs the
/// `weather_lookup` tool + ambient push, and how often the ambient indicator (the icon +
/// temperature beside the idle clock) is refreshed by the background push.
///
/// The provider is chosen by `weather.provider` (default `visualcrossing`). Visual
/// Crossing needs the `VISUALCROSSING_API_KEY` secret (read from the environment); when
/// that key is absent — or `weather.provider` is `openmeteo` — the keyless Open-Meteo
/// backend is used instead.
#[derive(Debug, Clone)]
pub struct WeatherSettings {
    /// Master switch (`weather.enabled`, default on). Off ⇒ the `weather_lookup` tool
    /// is not advertised and the ambient push does not run.
    pub enabled: bool,
    /// Forecast backend (`weather.provider`): `visualcrossing` (default; needs the
    /// `VISUALCROSSING_API_KEY` secret) or `openmeteo` (keyless). An unset/empty value
    /// means Visual Crossing; an unknown value falls back to Open-Meteo.
    pub provider: String,
    /// How often (seconds) the ambient current-conditions push refreshes
    /// (`weather.refresh_interval_secs`, default 1800 = 30 minutes). Clamped to a sane
    /// floor so a misconfiguration can't hammer the API.
    pub refresh_interval_secs: u64,
}

impl Default for WeatherSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: "visualcrossing".to_string(),
            refresh_interval_secs: 1800,
        }
    }
}

impl WeatherSettings {
    /// The refresh interval as a `Duration`, clamped to at least 5 minutes.
    pub fn refresh_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.refresh_interval_secs.max(300))
    }
}

/// Places feature settings (the `places_lookup` tool). The Google Places API key is a
/// secret (`GOOGLE_PLACES_API_KEY`), never stored here.
#[derive(Debug, Clone)]
pub struct PlacesSettings {
    /// Master switch (`places.enabled`, default on). Off ⇒ the `places_lookup` tool is
    /// not advertised.
    pub enabled: bool,
    /// Backend (`places.provider`): `google` (default/only). Unknown ⇒ tool disabled.
    pub provider: String,
}

impl Default for PlacesSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: "google".to_string(),
        }
    }
}

/// System-1 fast-decision settings (plans/system1-fast-decisions.md). Selects and
/// configures the pluggable [`crate::system1::DecisionEngine`] that runs before memory
/// recall + the LLM. Default `backend = "none"` reproduces today's behavior.
#[derive(Debug, Clone)]
pub struct System1Config {
    /// Which engine: `none` (default, disabled) | `mock` (tests) | `laya-serve` | `jev`
    /// | `laya-embedded`. The HTTP/embedded backends are wired in M1.
    pub backend: String,
    /// Base URL for the HTTP backends (`laya-serve` sidecar or OpenRouter for `jev`).
    pub base_url: String,
    /// OpenRouter model id for the `jev` backend.
    pub openrouter_model: String,
    /// Strict confidence floor below which a decision defers to System-2.
    pub min_confidence: f64,
    /// The intent labels the router is allowed to resolve (empty = the built-in set,
    /// filled in as M1+ handlers land).
    pub intents: Vec<String>,
}

impl Default for System1Config {
    fn default() -> Self {
        Self {
            backend: "none".to_string(),
            base_url: "http://127.0.0.1:8000".to_string(),
            openrouter_model: "typesafe/jev-1.13".to_string(),
            min_confidence: 0.85,
            intents: Vec::new(),
        }
    }
}

/// Which memory retrieval backend the pipeline uses for prompt context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryBackendChoice {
    /// SQLite FTS over the explicit/inferred memory store (recall fallback when
    /// Helix init fails, e.g. no `OPENAI_API_KEY`).
    Sqlite,
    /// Embedded HelixDB GraphRAG (vector KNN + graph expansion). The default.
    Helix,
}

/// Which text-embedding backend the GraphRAG memory uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EmbedBackend {
    /// Local, offline nomic-embed-text-v1.5 run in-process (feature `embed-local`).
    /// The default: no network, no API key. 768 dims.
    #[default]
    Local,
    /// OpenAI `text-embedding-3-small` over HTTP. Requires `OPENAI_API_KEY`.
    OpenAi,
}

/// Settings for the GraphRAG memory (embeddings + background entity extraction).
/// API keys are read from the environment at wiring time, not stored here.
#[derive(Debug, Clone)]
pub struct GraphRagConfig {
    /// Which embedding backend to use (default [`EmbedBackend::Local`]).
    pub embed_backend: EmbedBackend,
    /// Local nomic ONNX model file (`embed_backend = "local"`).
    pub embed_model_path: PathBuf,
    /// Directory holding nomic's four tokenizer JSON files (`embed_backend = "local"`):
    /// `tokenizer.json`, `config.json`, `special_tokens_map.json`, `tokenizer_config.json`.
    pub embed_tokenizer_dir: PathBuf,
    /// OpenAI API base (overridable for testing).
    pub openai_base_url: String,
    /// Embedding model (default `text-embedding-3-small`).
    pub embed_model: String,
    /// Embedding dimensionality. Local nomic: 768 (reducible via Matryoshka). OpenAI:
    /// native 1536 (reducible via OpenAI's `dimensions`).
    pub embed_dims: usize,
    /// Anthropic API base for entity extraction.
    pub anthropic_base_url: String,
    /// Entity-extraction chat model (default Claude Haiku 4.5).
    pub extract_model: String,
    /// Boot seed for cloud entity extraction (`graphrag.extract`: `"auto"` → on when
    /// `ANTHROPIC_API_KEY` is set, the default; `"off"` → never send turn text to
    /// Anthropic for extraction). Runtime-switchable from the config page (Memory →
    /// Recall settings); the live choice persists in `helix_path/extract_mode` and wins
    /// over `auto` at the next boot — but an explicit `"off"` here always wins.
    pub extract_enabled: bool,
    /// How often the background ingester drains the chat log.
    pub ingest_interval: Duration,
    /// KNN fan-out per vector search at recall time.
    pub recall_k: usize,
}

impl Default for GraphRagConfig {
    fn default() -> Self {
        Self {
            embed_backend: EmbedBackend::Local,
            embed_model_path: PathBuf::from("models/nomic-embed-text-v1.5.onnx"),
            embed_tokenizer_dir: PathBuf::from("models/nomic-tokenizer"),
            openai_base_url: "https://api.openai.com".to_string(),
            embed_model: "text-embedding-3-small".to_string(),
            // 768 is nomic's native width (the default backend). It is also a valid
            // reduced width for OpenAI's `dimensions`, so an `openai` user who wants the
            // full 1536 sets `embed_dims` explicitly.
            embed_dims: 768,
            anthropic_base_url: "https://api.anthropic.com".to_string(),
            extract_model: "claude-haiku-4-5".to_string(),
            extract_enabled: true,
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
            // disable with config_addr in the config file.
            config_addr: Some("127.0.0.1:8730".parse().unwrap()),
            service_name: "Anamanti Core".to_string(),
            instance_id: "anamanti-core".to_string(),
            stt_addr: "127.0.0.1:10300".parse().unwrap(), // wyoming-faster-whisper default
            tts_addr: "127.0.0.1:10200".parse().unwrap(), // wyoming-piper default
            tts_addrs: Default::default(),
            tts_voice: None,
            tts_voices_dir: None,
            language: crate::lang::LanguageSettings::default(),
            llm: LlmChoice::Ollama {
                url: "http://127.0.0.1:11434".to_string(),
                model: "llama3.2".to_string(),
            },
            db_path: PathBuf::from("anamanti_memory.sqlite"),
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
            home_address: crate::household::HomeAddress::default(),
            weather_units: None,
            turn_timeout: Duration::from_secs(30),
            memory_backend: MemoryBackendChoice::Helix,
            chatlog_path: PathBuf::from("anamanti_chatlog.jsonl"),
            promptlog_path: PathBuf::from("anamanti_promptlog.jsonl"),
            helix_path: PathBuf::from("anamanti_helix"),
            graphrag: GraphRagConfig::default(),
            speaker: SpeakerConfig::default(),
            stt: SttConfig::default(),
            vad: VadConfig::default(),
            follow_up: FollowUpConfig::default(),
            music: MusicConfig::default(),
            weather: WeatherSettings::default(),
            places: PlacesSettings::default(),
            system1: System1Config::default(),
            settings_path: Some(PathBuf::from("anamanti_settings.json")),
            audio_dump_dir: None,
            anthropic_auth: AnthropicAuth::ApiKey,
            engine: LlmEngine::Rig,
            web_search: true,
            search_provider: "tavily".to_string(),
            anthropic_token_cmd: None,
            ollama_url: "http://127.0.0.1:11434".to_string(),
            anthropic_max_tokens: 1024,
            openai_max_tokens: 1024,
            calendar_specs: Vec::new(),
            calendar_cache_ttl: None,
            directions_provider: String::new(),
            drive: DriveConfig {
                scope: Some(DEFAULT_DRIVE_SCOPE.to_string()),
                ..DriveConfig::default()
            },
            spotify: SpotifyConfig::default(),
            cadora: CadoraConfig::default(),
            appsaid: AppSaidConfig::default(),
            home_assistant: crate::home_assistant::HomeAssistantConfig::default(),
            camera: crate::camera::CameraConfig::default(),
            tool_cache: crate::cache::ToolCacheConfig::default(),
        }
    }
}

/// The git branch code of the current working directory, or `None` when this
/// process is not running inside a checkout (e.g. the "local production" install,
/// which runs a copied binary from outside a repo and sets `instance_id` in its
/// `anamanti.json`). Used as the default `instance_id` so a copy launched from a test
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
        "anamanti-core".to_string()
    } else {
        trimmed.to_lowercase()
    }
}

// ===========================================================================
// JSON file configuration
// ===========================================================================

/// The `language` block of the config file (`plans/BilingualPlan.md` §2.1).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileLanguage {
    /// Enabled language codes, in order (`en`, `pt-BR`). Empty/absent ⇒ just `default`.
    #[serde(default)]
    pub enabled: Vec<String>,
    /// The household default (defaults to the first enabled language, else `en`).
    pub default: Option<String>,
}

/// Build the household [`LanguageSettings`](crate::lang::LanguageSettings) from the
/// config file's `language` + `tts_voices` blocks. An unknown language code is a hard
/// error (like an unknown key): a typo must not silently turn bilingual mode off.
fn resolve_language(
    language: Option<FileLanguage>,
    tts_voices: Option<std::collections::BTreeMap<String, Option<String>>>,
) -> Result<crate::lang::LanguageSettings> {
    use crate::lang::{Lang, LanguageSettings};
    let parse = |code: &str, what: &str| {
        Lang::parse(code).with_context(|| {
            format!("unknown language `{code}` in {what} (supported: `en`, `pt-BR`)")
        })
    };
    let fl = language.unwrap_or_default();
    let mut enabled = Vec::new();
    for code in &fl.enabled {
        enabled.push(parse(code, "language.enabled")?);
    }
    let default = match fl.default.as_deref().filter(|d| !d.trim().is_empty()) {
        Some(code) => parse(code, "language.default")?,
        None => enabled.first().copied().unwrap_or(Lang::En),
    };
    let mut voices = std::collections::BTreeMap::new();
    for (code, voice) in tts_voices.unwrap_or_default() {
        let lang = parse(&code, "tts_voices")?;
        if let Some(v) = voice.filter(|v| !v.trim().is_empty()) {
            voices.insert(lang, v);
        }
    }
    Ok(LanguageSettings {
        enabled,
        default,
        voices,
    }
    .sanitized())
}

/// Parse the `tts_addrs` map's language keys (an unknown code is a hard error).
fn resolve_tts_addrs(
    raw: Option<std::collections::BTreeMap<String, SocketAddr>>,
) -> Result<std::collections::BTreeMap<crate::lang::Lang, SocketAddr>> {
    let mut out = std::collections::BTreeMap::new();
    for (code, addr) in raw.unwrap_or_default() {
        let lang = crate::lang::Lang::parse(&code).with_context(|| {
            format!("unknown language `{code}` in tts_addrs (supported: `en`, `pt-BR`)")
        })?;
        out.insert(lang, addr);
    }
    Ok(out)
}

/// The deserialized JSON config file. Every field is optional; an absent field
/// falls back to the built-in default (a 1:1 mirror of the historical env behavior),
/// so a partial file works. Unknown keys are rejected (`deny_unknown_fields`) so a
/// typo is a hard error rather than a silent revert-to-default.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub bind_addr: Option<SocketAddr>,
    /// Loopback config page `host:port`, or the disable sentinel `off`/`none`/(empty).
    pub config_addr: Option<String>,
    pub service_name: Option<String>,
    pub instance_id: Option<String>,
    pub stt_addr: Option<SocketAddr>,
    pub tts_addr: Option<SocketAddr>,
    /// Per-language Piper servers (`{ "pt-BR": "127.0.0.1:10201" }`).
    pub tts_addrs: Option<std::collections::BTreeMap<String, SocketAddr>>,
    pub tts_voice: Option<String>,
    pub tts_voices_dir: Option<PathBuf>,
    /// Household languages (`plans/BilingualPlan.md`): `{ "enabled": ["en", "pt-BR"],
    /// "default": "en" }`. Absent ⇒ English only.
    pub language: Option<FileLanguage>,
    /// A Piper voice per language code (`{ "pt-BR": "pt_BR-faber-medium" }`); a `null`
    /// or missing entry speaks that language with `tts_voice`.
    pub tts_voices: Option<std::collections::BTreeMap<String, Option<String>>>,
    pub db_path: Option<PathBuf>,
    pub chatlog_path: Option<PathBuf>,
    pub promptlog_path: Option<PathBuf>,
    pub helix_path: Option<PathBuf>,
    /// Runtime settings overlay path, or the disable sentinel `off`/`none`/(empty).
    pub settings_path: Option<String>,
    pub audio_dump_dir: Option<PathBuf>,
    pub system_prompt: Option<String>,
    /// Discrete home-address fields (Address 1/2, City, State/Prov, Zip/Postal, Country).
    #[serde(default)]
    pub home_address: Option<crate::household::HomeAddress>,
    /// **Legacy** single-line home location, kept for backward compatibility with config
    /// files written before the discrete-field split. When `home_address` is absent, this
    /// seeds `HomeAddress.city`.
    pub home_location: Option<String>,
    pub weather_units: Option<String>,
    pub turn_timeout_secs: Option<u64>,
    pub memory_backend: Option<String>,
    #[serde(default)]
    pub llm: FileLlm,
    #[serde(default)]
    pub graphrag: FileGraphRag,
    #[serde(default)]
    pub speaker: FileSpeaker,
    #[serde(default)]
    pub stt: FileStt,
    #[serde(default)]
    pub vad: FileVad,
    #[serde(default)]
    pub follow_up: FileFollowUp,
    #[serde(default)]
    pub music: FileMusic,
    #[serde(default)]
    pub weather: FileWeather,
    #[serde(default)]
    pub places: FilePlaces,
    #[serde(default)]
    pub system1: FileSystem1,
    #[serde(default)]
    pub calendar: FileCalendar,
    #[serde(default)]
    pub directions: FileDirections,
    #[serde(default)]
    pub drive: FileDrive,
    #[serde(default)]
    pub spotify: FileSpotify,
    #[serde(default)]
    pub cadora: FileCadora,
    #[serde(default)]
    pub appsaid: FileAppSaid,
    #[serde(default)]
    pub home_assistant: FileHomeAssistant,
    #[serde(default)]
    pub camera: FileCamera,
    /// Per-tool cache TTLs in seconds, keyed by tool name (e.g. `weather_lookup`). A flat
    /// map so it stays generic; overlaid on the built-in defaults, `0` disables a tool's
    /// cache. Absent ⇒ defaults only (see [`crate::cache::ToolCacheConfig`]).
    #[serde(default)]
    pub tool_cache: std::collections::HashMap<String, u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileLlm {
    pub backend: Option<String>,
    pub engine: Option<String>,
    pub anthropic_auth: Option<String>,
    /// Command that prints a fresh subscription token (non-secret). The token itself
    /// stays in the `ANTHROPIC_OAUTH_TOKEN` env var.
    pub anthropic_token_cmd: Option<String>,
    pub web_search: Option<bool>,
    pub search_provider: Option<String>,
    #[serde(default)]
    pub ollama: FileOllama,
    #[serde(default)]
    pub anthropic: FileAnthropic,
    #[serde(default)]
    pub openai: FileOpenAi,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileOllama {
    pub url: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileAnthropic {
    pub model: Option<String>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileOpenAi {
    pub model: Option<String>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileGraphRag {
    pub embed_backend: Option<String>,
    pub embed_model_path: Option<PathBuf>,
    pub embed_tokenizer_dir: Option<PathBuf>,
    pub openai_base_url: Option<String>,
    pub embed_model: Option<String>,
    pub embed_dims: Option<usize>,
    pub anthropic_base_url: Option<String>,
    pub extract_model: Option<String>,
    /// `"auto"` (default) or `"off"` — see [`GraphRagConfig::extract_enabled`].
    pub extract: Option<String>,
    pub ingest_interval_secs: Option<u64>,
    pub recall_k: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSpeaker {
    pub enabled: Option<bool>,
    pub model_path: Option<PathBuf>,
    pub match_threshold: Option<f32>,
    pub new_threshold: Option<f32>,
    pub min_speech_ms: Option<u32>,
    pub min_identify_ms: Option<u32>,
    pub short_clip_margin: Option<f32>,
    pub min_margin: Option<f32>,
    pub clear_margin: Option<f32>,
    pub embed_dims: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileStt {
    pub engine: Option<String>,
    pub model: Option<String>,
    pub multilingual: Option<bool>,
    pub model_dir: Option<PathBuf>,
    pub model_path: Option<PathBuf>,
    pub language: Option<String>,
    pub num_threads: Option<u32>,
    #[serde(default)]
    pub elevenlabs: FileElevenLabsStt,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileElevenLabsStt {
    pub model_id: Option<String>,
    pub base_url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileVad {
    pub engine: Option<String>,
    #[serde(default)]
    pub silero: FileSilero,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSilero {
    pub model_path: Option<PathBuf>,
    pub threshold: Option<f32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileFollowUp {
    pub enabled: Option<bool>,
    pub max_chain: Option<u32>,
    pub question_wait_secs: Option<u32>,
    pub reply_wait_secs: Option<u32>,
    pub history_turns: Option<usize>,
    pub history_window_secs: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileMusic {
    pub enabled: Option<bool>,
    pub snapserver_addr: Option<SocketAddr>,
    pub duck_on_speech: Option<bool>,
    pub duck_percent: Option<u8>,
    pub group: Option<String>,
    pub stream: Option<String>,
    /// mpv IPC socket path, or the disable sentinel `off`/`none`/(empty).
    pub web_ipc: Option<String>,
    pub bin_dir: Option<PathBuf>,
    pub run_dir: Option<PathBuf>,
    pub log_dir: Option<PathBuf>,
    pub conf: Option<PathBuf>,
    pub spotify_device_name: Option<String>,
    pub autostart: Option<bool>,
    pub players_in_snapserver: Option<bool>,
    pub nowplaying_refresh_secs: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileWeather {
    pub enabled: Option<bool>,
    /// Forecast backend: `visualcrossing` (default) or `openmeteo`.
    pub provider: Option<String>,
    pub refresh_interval_secs: Option<u64>,
}

/// The `places` block of the config file (all fields optional; absent → defaults). The
/// Google Places API key is a secret and lives in `GOOGLE_PLACES_API_KEY`, never here.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilePlaces {
    pub enabled: Option<bool>,
    /// Backend: `google` (default/only).
    pub provider: Option<String>,
}

/// The `system1` block of the config file (all fields optional; absent → defaults, and
/// an absent block leaves the engine disabled). See [`System1Config`].
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSystem1 {
    pub backend: Option<String>,
    pub base_url: Option<String>,
    pub openrouter_model: Option<String>,
    /// **Deprecated, ignored.** Was the compute device for the never-implemented
    /// `laya-embedded` backend. Still accepted so an existing file keeps loading
    /// (`deny_unknown_fields`); a value logs a deprecation warning at boot.
    pub device: Option<String>,
    /// **Deprecated, ignored.** Was the checkpoint for the never-implemented
    /// `laya-embedded` backend (the `jev` model is `openrouter_model`). Still accepted;
    /// a value logs a deprecation warning at boot.
    pub model: Option<String>,
    pub min_confidence: Option<f64>,
    #[serde(default)]
    pub intents: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileCalendar {
    #[serde(default)]
    pub subscriptions: Vec<FileCalendarSub>,
    pub cache_ttl_secs: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileCalendarSub {
    pub name: Option<String>,
    pub url: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileDirections {
    pub provider: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileDrive {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    #[serde(default)]
    pub folder_ids: Vec<String>,
    pub scope: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSpotify {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub refresh_token: Option<String>,
    pub device_name: Option<String>,
    /// OAuth redirect URL for the consent flow (default
    /// `http://127.0.0.1:8888/callback`). Must match a Redirect URI registered in the
    /// Spotify app.
    pub redirect_url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileCadora {
    /// Cadora API base URL, including the `/cadora` route prefix (default
    /// `https://api.getcadora.com/cadora`; the retired `cadora-server.fly.dev` maps to it).
    pub base_url: Option<String>,
    /// A `vl_…` voice-link token, if seeded directly (normally minted via the
    /// config-page pairing flow instead).
    pub link_token: Option<String>,
}

/// The `appsaid` block of the config file. The secret sender app token is
/// deliberately NOT a field here — it is env-only (`APPSAID_APP_TOKEN`) and
/// config-page settable, never written to `anamanti.json` (the secrets-are-env-only
/// convention).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileAppSaid {
    /// AppSaid worker base URL (e.g. `https://appsaid.you.workers.dev`).
    pub worker_url: Option<String>,
    /// Recipients: family-member name → AppSaid `user_key`.
    #[serde(default)]
    pub recipients: std::collections::BTreeMap<String, String>,
    /// Optional default recipient name used when the model omits `recipient`.
    pub default_recipient: Option<String>,
}

/// The `home_assistant` block of the config file (`plans/HomeAssistantPlan.md`). The
/// long-lived access token is deliberately NOT a field — it is env-only
/// (`HOME_ASSISTANT_TOKEN`), never written to `anamanti.json`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileHomeAssistant {
    /// HA base URL, e.g. `http://homeassistant.local:8123`.
    pub base_url: Option<String>,
    /// The room a display is in when it doesn't report one (e.g. `"Kitchen"`), so a
    /// bare "turn off the lights" means that room's lights.
    pub default_room: Option<String>,
    /// Name/id fragments of devices that must be named exactly — never swept up by a
    /// room-wide or "all" command. Default `["fireplace"]`; `[]` disables.
    pub exact_only: Option<Vec<String>>,
    /// The dashboard path the display widget opens (default `lovelace/0`).
    pub dashboard_path: Option<String>,
}

/// The `camera` block of the config file (`plans/CameraPlan.md`). The NVR password is
/// deliberately NOT a field — it is env-only (`REOLINK_PASSWORD`).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileCamera {
    /// Master switch (default `true`).
    pub enabled: Option<bool>,
    /// NVR host or IP, e.g. `192.168.1.29`.
    pub nvr_host: Option<String>,
    /// NVR RTSP port (default 554).
    pub rtsp_port: Option<u16>,
    /// The NVR user (a dedicated view-only account).
    pub username: Option<String>,
    /// The cameras the display can show: `[{ "name": "Front Door", "channel": 5 }]`.
    /// `channel` is the number the NVR's own UI shows (1-based).
    #[serde(default)]
    pub cameras: Vec<FileCameraSpec>,
}

/// One `camera.cameras[]` entry.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileCameraSpec {
    pub name: String,
    pub channel: u32,
}

/// Trim a config string and drop it when empty.
fn nonempty(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Convert the file's calendar subscriptions into [`CalendarSpec`]s, trimming and
/// dropping entries with no URL and auto-naming unnamed ones (`Calendar N`).
fn calendar_specs_from_file(subs: Vec<FileCalendarSub>) -> Vec<CalendarSpec> {
    subs.into_iter()
        .enumerate()
        .filter_map(|(i, sub)| {
            let url = sub.url.trim().to_string();
            if url.is_empty() {
                return None;
            }
            let name = nonempty(sub.name).unwrap_or_else(|| format!("Calendar {}", i + 1));
            Some(CalendarSpec { name, url })
        })
        .collect()
}

impl Config {
    /// Load configuration from a JSON file. With `Some(path)` the file is **required**
    /// (missing / unreadable / malformed → error). With `None` the convention path
    /// `anamanti.json` in the working directory is used when present; when absent, the
    /// built-in defaults are used (matching the historical "nothing configured" boot).
    pub fn load(cli_config: Option<&Path>) -> Result<Self> {
        match cli_config {
            Some(path) => {
                let data = std::fs::read_to_string(path)
                    .with_context(|| format!("reading config file {}", path.display()))?;
                let fc: FileConfig = serde_json::from_str(&data)
                    .with_context(|| format!("parsing config file {}", path.display()))?;
                Self::from_file(fc)
            }
            None => {
                let path = Path::new(DEFAULT_CONFIG_FILE);
                match std::fs::read_to_string(path) {
                    Ok(data) => {
                        let fc: FileConfig = serde_json::from_str(&data).with_context(|| {
                            format!("parsing config file {DEFAULT_CONFIG_FILE}")
                        })?;
                        Self::from_file(fc)
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        log::info!(
                            "no {DEFAULT_CONFIG_FILE} in the working directory; using built-in defaults"
                        );
                        Self::from_file(FileConfig::default())
                    }
                    Err(e) => Err(e).with_context(|| format!("reading {DEFAULT_CONFIG_FILE}")),
                }
            }
        }
    }

    /// Resolve a fully-built [`Config`] from a parsed [`FileConfig`], overlaying each
    /// present field onto the built-in defaults. Pure (no I/O), so it's unit-testable.
    pub fn from_file(fc: FileConfig) -> Result<Self> {
        let d = Config::default();

        let llm = match fc
            .llm
            .backend
            .as_deref()
            .unwrap_or("ollama")
            .to_lowercase()
            .as_str()
        {
            "mock" => LlmChoice::Mock,
            "anthropic" | "claude" => LlmChoice::Anthropic {
                model: fc
                    .llm
                    .anthropic
                    .model
                    .clone()
                    .unwrap_or_else(|| "claude-opus-5".to_string()),
                max_tokens: fc
                    .llm
                    .anthropic
                    .max_tokens
                    .unwrap_or(d.anthropic_max_tokens),
            },
            "openai" | "gpt" => LlmChoice::OpenAI {
                model: fc
                    .llm
                    .openai
                    .model
                    .clone()
                    .unwrap_or_else(|| "gpt-4o-mini".to_string()),
                max_tokens: fc.llm.openai.max_tokens.unwrap_or(d.openai_max_tokens),
            },
            _ => LlmChoice::Ollama {
                url: fc
                    .llm
                    .ollama
                    .url
                    .clone()
                    .unwrap_or_else(|| d.ollama_url.clone()),
                model: fc
                    .llm
                    .ollama
                    .model
                    .clone()
                    .unwrap_or_else(|| "llama3.2".to_string()),
            },
        };

        let memory_backend = match fc
            .memory_backend
            .as_deref()
            .unwrap_or("helix")
            .to_lowercase()
            .as_str()
        {
            "sqlite" | "fts" => MemoryBackendChoice::Sqlite,
            _ => MemoryBackendChoice::Helix,
        };

        let mut graphrag = GraphRagConfig::default();
        if let Some(v) = nonempty(fc.graphrag.embed_backend) {
            graphrag.embed_backend = match v.to_lowercase().as_str() {
                "openai" | "oai" => EmbedBackend::OpenAi,
                "local" | "nomic" => EmbedBackend::Local,
                other => {
                    log::warn!(
                        "unknown graphrag.embed_backend {other:?}; using the default (local nomic)"
                    );
                    EmbedBackend::Local
                }
            };
        }
        if let Some(v) = fc.graphrag.embed_model_path {
            graphrag.embed_model_path = v;
        }
        if let Some(v) = fc.graphrag.embed_tokenizer_dir {
            graphrag.embed_tokenizer_dir = v;
        }
        if let Some(v) = nonempty(fc.graphrag.embed_model) {
            graphrag.embed_model = v;
        }
        if let Some(v) = fc.graphrag.embed_dims {
            graphrag.embed_dims = v;
        }
        if let Some(v) = nonempty(fc.graphrag.extract_model) {
            graphrag.extract_model = v;
        }
        if let Some(v) = nonempty(fc.graphrag.extract) {
            graphrag.extract_enabled = match v.to_lowercase().as_str() {
                "off" | "false" | "none" | "disabled" => false,
                "auto" | "on" | "true" | "anthropic" => true,
                other => {
                    log::warn!("unknown graphrag.extract {other:?}; using the default (auto)");
                    true
                }
            };
        }
        if let Some(v) = nonempty(fc.graphrag.openai_base_url) {
            graphrag.openai_base_url = v;
        }
        if let Some(v) = nonempty(fc.graphrag.anthropic_base_url) {
            graphrag.anthropic_base_url = v;
        }
        if let Some(secs) = fc.graphrag.ingest_interval_secs {
            graphrag.ingest_interval = Duration::from_secs(secs);
        }
        if let Some(v) = fc.graphrag.recall_k {
            graphrag.recall_k = v;
        }

        let sd = SpeakerConfig::default();
        let speaker = SpeakerConfig {
            enabled: fc.speaker.enabled.unwrap_or(sd.enabled),
            model_path: fc.speaker.model_path,
            match_threshold: fc.speaker.match_threshold.unwrap_or(sd.match_threshold),
            new_threshold: fc.speaker.new_threshold.unwrap_or(sd.new_threshold),
            min_speech_ms: fc.speaker.min_speech_ms.unwrap_or(sd.min_speech_ms),
            min_identify_ms: fc.speaker.min_identify_ms.unwrap_or(sd.min_identify_ms),
            short_clip_margin: fc.speaker.short_clip_margin.unwrap_or(sd.short_clip_margin),
            min_margin: fc.speaker.min_margin.unwrap_or(sd.min_margin),
            clear_margin: fc.speaker.clear_margin.unwrap_or(sd.clear_margin),
            embed_dims: fc.speaker.embed_dims.unwrap_or(sd.embed_dims),
        };

        let sttd = SttConfig::default();
        let stt = SttConfig {
            engine: match fc.stt.engine.as_deref() {
                None => sttd.engine,
                Some(label) => SttEngineKind::from_label(label).with_context(|| {
                    format!(
                        "unknown stt.engine {label:?} (expected `wyoming`, `whisper-rs`, or `elevenlabs`)"
                    )
                })?,
            },
            model: nonempty(fc.stt.model).unwrap_or(sttd.model),
            multilingual: fc.stt.multilingual.unwrap_or(sttd.multilingual),
            model_dir: fc.stt.model_dir.unwrap_or(sttd.model_dir),
            model_path: fc.stt.model_path,
            // An explicit empty `language` means auto-detect (`None`); absent keeps
            // the default (`Some("en")`).
            language: match fc.stt.language {
                None => sttd.language,
                Some(s) if s.trim().is_empty() => None,
                Some(s) => Some(s),
            },
            num_threads: fc.stt.num_threads.unwrap_or(sttd.num_threads),
            elevenlabs: ElevenLabsSttConfig {
                model_id: nonempty(fc.stt.elevenlabs.model_id)
                    .unwrap_or(sttd.elevenlabs.model_id),
                base_url: nonempty(fc.stt.elevenlabs.base_url)
                    .unwrap_or(sttd.elevenlabs.base_url),
            },
        };

        let vadd = VadConfig::default();
        let vad = VadConfig {
            engine: match fc.vad.engine.as_deref() {
                None => vadd.engine,
                Some(label) => VadEngineKind::from_label(label).with_context(|| {
                    format!("unknown vad.engine {label:?} (expected `energy` or `silero`)")
                })?,
            },
            silero: SileroConfig {
                model_path: fc.vad.silero.model_path.unwrap_or(vadd.silero.model_path),
                threshold: fc.vad.silero.threshold.unwrap_or(vadd.silero.threshold),
            },
        };

        let fud = FollowUpConfig::default();
        let follow_up = FollowUpConfig {
            enabled: fc.follow_up.enabled.unwrap_or(fud.enabled),
            max_chain: fc.follow_up.max_chain.unwrap_or(fud.max_chain),
            question_wait_secs: fc
                .follow_up
                .question_wait_secs
                .unwrap_or(fud.question_wait_secs),
            reply_wait_secs: fc.follow_up.reply_wait_secs.unwrap_or(fud.reply_wait_secs),
            history_turns: fc.follow_up.history_turns.unwrap_or(fud.history_turns),
            history_window_secs: fc
                .follow_up
                .history_window_secs
                .unwrap_or(fud.history_window_secs),
        };

        let md = MusicConfig::default();
        let music = MusicConfig {
            enabled: fc.music.enabled.unwrap_or(md.enabled),
            snapserver_addr: fc.music.snapserver_addr.unwrap_or(md.snapserver_addr),
            duck_on_speech: fc.music.duck_on_speech.unwrap_or(md.duck_on_speech),
            duck_percent: fc
                .music
                .duck_percent
                .map(|p| p.min(100))
                .unwrap_or(md.duck_percent),
            group: GroupSelector::from_parts(fc.music.group.as_deref(), fc.music.stream.as_deref()),
            mpv_ipc: match fc.music.web_ipc {
                None => md.mpv_ipc.clone(),
                Some(v) => disable_sentinel(&v).map(PathBuf::from),
            },
            bin_dir: fc.music.bin_dir.unwrap_or_else(|| md.bin_dir.clone()),
            run_dir: fc.music.run_dir.unwrap_or_else(|| md.run_dir.clone()),
            log_dir: fc.music.log_dir.unwrap_or_else(|| md.log_dir.clone()),
            snapserver_conf: fc.music.conf.unwrap_or_else(|| md.snapserver_conf.clone()),
            spotify_device_name: nonempty(fc.music.spotify_device_name)
                .unwrap_or_else(|| md.spotify_device_name.clone()),
            autostart: fc.music.autostart.unwrap_or(md.autostart),
            players_in_snapserver: fc
                .music
                .players_in_snapserver
                .unwrap_or(md.players_in_snapserver),
            nowplaying_refresh_secs: fc
                .music
                .nowplaying_refresh_secs
                .unwrap_or(md.nowplaying_refresh_secs),
        };

        let wd = WeatherSettings::default();
        let weather = WeatherSettings {
            enabled: fc.weather.enabled.unwrap_or(wd.enabled),
            provider: nonempty(fc.weather.provider).unwrap_or(wd.provider),
            refresh_interval_secs: fc
                .weather
                .refresh_interval_secs
                .unwrap_or(wd.refresh_interval_secs),
        };

        let pd = PlacesSettings::default();
        let places = PlacesSettings {
            enabled: fc.places.enabled.unwrap_or(pd.enabled),
            provider: nonempty(fc.places.provider).unwrap_or(pd.provider),
        };

        let s1d = System1Config::default();
        for (key, value) in [
            ("system1.device", &fc.system1.device),
            ("system1.model", &fc.system1.model),
        ] {
            if value.is_some() {
                log::warn!(
                    "config key `{key}` is deprecated and ignored (it configured the \
                     unimplemented laya-embedded backend); remove it from the config file"
                );
            }
        }
        let system1 = System1Config {
            backend: nonempty(fc.system1.backend).unwrap_or(s1d.backend),
            base_url: nonempty(fc.system1.base_url).unwrap_or(s1d.base_url),
            openrouter_model: nonempty(fc.system1.openrouter_model).unwrap_or(s1d.openrouter_model),
            min_confidence: fc.system1.min_confidence.unwrap_or(s1d.min_confidence),
            intents: if fc.system1.intents.is_empty() {
                s1d.intents
            } else {
                fc.system1.intents
            },
        };

        // The config page: `off`/`none`/empty disables it, otherwise a host:port.
        let config_addr = match fc.config_addr {
            None => d.config_addr,
            Some(v) => match disable_sentinel(&v) {
                None => None,
                Some(s) => Some(
                    s.parse()
                        .with_context(|| format!("parsing config_addr=`{s}` as host:port"))?,
                ),
            },
        };

        let settings_path = match fc.settings_path {
            None => d.settings_path.clone(),
            Some(v) => disable_sentinel(&v).map(PathBuf::from),
        };

        let service_name = nonempty(fc.service_name).unwrap_or_else(|| d.service_name.clone());
        // Stable selection key: explicit config value, else the git branch code of the
        // working directory (a copy running from a test branch/worktree names itself by
        // its branch), else the sanitized service name. All must be stable across
        // restarts — persisted device selections depend on this not changing.
        let instance_id = nonempty(fc.instance_id)
            .or_else(git_branch_code)
            .unwrap_or_else(|| sanitize_id(&service_name));

        let calendar_specs = calendar_specs_from_file(fc.calendar.subscriptions);
        // Kept raw: an absent key must stay distinguishable from an explicit value
        // (which wins over tool_cache.calendar_lookup, even when it equals the default).
        let calendar_cache_ttl = fc.calendar.cache_ttl_secs.map(Duration::from_secs);

        let drive = DriveConfig {
            client_id: nonempty(fc.drive.client_id),
            client_secret: nonempty(fc.drive.client_secret),
            // The refresh token is minted by the consent flow, never seeded from config.
            refresh_token: None,
            folder_ids: fc
                .drive
                .folder_ids
                .into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            scope: Some(
                nonempty(fc.drive.scope).unwrap_or_else(|| DEFAULT_DRIVE_SCOPE.to_string()),
            ),
        };

        let spotify = SpotifyConfig {
            client_id: nonempty(fc.spotify.client_id),
            client_secret: nonempty(fc.spotify.client_secret),
            refresh_token: nonempty(fc.spotify.refresh_token),
            device_name: nonempty(fc.spotify.device_name),
            redirect_url: nonempty(fc.spotify.redirect_url),
            scope: None,
        };

        let cadora = CadoraConfig {
            base_url: nonempty(fc.cadora.base_url),
            link_token: nonempty(fc.cadora.link_token),
        };

        let appsaid = AppSaidConfig {
            // Trim a trailing slash so the client can join `/v1/messages` cleanly.
            worker_url: nonempty(fc.appsaid.worker_url)
                .map(|s| s.trim_end_matches('/').to_string()),
            // The sender app token is a SECRET: never seeded from the JSON config. It
            // seeds from `APPSAID_APP_TOKEN` in `shared_settings` instead.
            app_token: None,
            recipients: fc
                .appsaid
                .recipients
                .into_iter()
                .map(|(name, key)| (name.trim().to_string(), key.trim().to_string()))
                .filter(|(name, key)| !name.is_empty() && !key.is_empty())
                .collect(),
            default_recipient: nonempty(fc.appsaid.default_recipient),
        };

        let home_assistant = {
            let d = crate::home_assistant::HomeAssistantConfig::default();
            crate::home_assistant::HomeAssistantConfig {
                base_url: nonempty(fc.home_assistant.base_url)
                    .map(|s| s.trim_end_matches('/').to_string()),
                default_room: nonempty(fc.home_assistant.default_room),
                exact_only: fc
                    .home_assistant
                    .exact_only
                    .map(|v| {
                        v.into_iter()
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or(d.exact_only),
                dashboard_path: nonempty(fc.home_assistant.dashboard_path)
                    .map(|s| s.trim_start_matches('/').to_string())
                    .unwrap_or(d.dashboard_path),
            }
        };

        let camera = {
            let d = crate::camera::CameraConfig::default();
            crate::camera::CameraConfig {
                enabled: fc.camera.enabled.unwrap_or(d.enabled),
                nvr_host: nonempty(fc.camera.nvr_host),
                rtsp_port: fc.camera.rtsp_port.unwrap_or(d.rtsp_port),
                username: nonempty(fc.camera.username),
                cameras: fc
                    .camera
                    .cameras
                    .into_iter()
                    .map(|c| crate::camera::CameraSpec {
                        name: c.name.trim().to_string(),
                        channel: c.channel,
                    })
                    .collect(),
            }
        };

        let ollama_url = match &llm {
            LlmChoice::Ollama { url, .. } => url.clone(),
            _ => fc
                .llm
                .ollama
                .url
                .clone()
                .unwrap_or_else(|| d.ollama_url.clone()),
        };
        let anthropic_max_tokens = match &llm {
            LlmChoice::Anthropic { max_tokens, .. } => *max_tokens,
            _ => fc
                .llm
                .anthropic
                .max_tokens
                .unwrap_or(d.anthropic_max_tokens),
        };
        let openai_max_tokens = match &llm {
            LlmChoice::OpenAI { max_tokens, .. } => *max_tokens,
            _ => fc.llm.openai.max_tokens.unwrap_or(d.openai_max_tokens),
        };

        Ok(Self {
            bind_addr: fc.bind_addr.unwrap_or(d.bind_addr),
            config_addr,
            service_name,
            instance_id,
            stt_addr: fc.stt_addr.unwrap_or(d.stt_addr),
            tts_addr: fc.tts_addr.unwrap_or(d.tts_addr),
            tts_addrs: resolve_tts_addrs(fc.tts_addrs)?,
            language: resolve_language(fc.language, fc.tts_voices)?,
            tts_voice: nonempty(fc.tts_voice),
            tts_voices_dir: fc.tts_voices_dir.or(d.tts_voices_dir),
            llm,
            db_path: fc.db_path.unwrap_or(d.db_path),
            system_prompt: nonempty(fc.system_prompt).unwrap_or(d.system_prompt),
            home_address: resolve_home_address(fc.home_address, fc.home_location),
            weather_units: nonempty(fc.weather_units),
            turn_timeout: fc
                .turn_timeout_secs
                .map(Duration::from_secs)
                .unwrap_or(d.turn_timeout),
            memory_backend,
            chatlog_path: fc.chatlog_path.unwrap_or(d.chatlog_path),
            promptlog_path: fc.promptlog_path.unwrap_or(d.promptlog_path),
            helix_path: fc.helix_path.unwrap_or(d.helix_path),
            graphrag,
            speaker,
            stt,
            vad,
            follow_up,
            music,
            weather,
            places,
            system1,
            settings_path,
            audio_dump_dir: fc.audio_dump_dir,
            anthropic_auth: fc
                .llm
                .anthropic_auth
                .as_deref()
                .map(AnthropicAuth::from_label)
                .unwrap_or(d.anthropic_auth),
            engine: fc
                .llm
                .engine
                .as_deref()
                .map(LlmEngine::from_label)
                .unwrap_or(d.engine),
            web_search: fc.llm.web_search.unwrap_or(d.web_search),
            search_provider: nonempty(fc.llm.search_provider).unwrap_or(d.search_provider),
            anthropic_token_cmd: nonempty(fc.llm.anthropic_token_cmd),
            ollama_url,
            anthropic_max_tokens,
            openai_max_tokens,
            calendar_specs,
            calendar_cache_ttl,
            directions_provider: fc
                .directions
                .provider
                .map(|s| s.trim().to_string())
                .unwrap_or(d.directions_provider),
            drive,
            spotify,
            cadora,
            appsaid,
            home_assistant,
            camera,
            tool_cache: {
                let mut tc = crate::cache::ToolCacheConfig::default();
                tc.overlay(fc.tool_cache);
                tc
            },
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
    /// an IPC socket is configured (`music.web_ipc`).
    pub fn mpv_control(&self) -> Option<MpvControl> {
        if !self.music.enabled {
            return None;
        }
        self.music.mpv_ipc.as_ref().map(MpvControl::new)
    }

    /// Build the process supervisor for the music sibling processes
    /// (snapserver / librespot / mpv), or `None` when music is disabled. The
    /// launch commands mirror `anamanti-core/deploy/snapcast/` (the runbook +
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
                // Guarded by the "already reachable" check in start_all; no reap.
                reap_pattern: None,
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
                    // Persist the Spotify account session across restarts: the
                    // one-time Connect hand-off authenticates this device, and the
                    // cached credentials let it re-authenticate on every respawn so
                    // it keeps appearing in the Web API device list that
                    // `spotify_control` targets. `--disable-audio-cache` keeps only
                    // the credentials/volume cache (no on-disk audio files).
                    "--cache".into(),
                    m.run_dir.join("librespot-cache").display().to_string(),
                    "--disable-audio-cache".into(),
                ],
                log_path: log("ambient-librespot.log"),
                // Reap any orphaned librespot for this device name before spawning
                // (specific enough it can't match the Core or an unrelated proc).
                reap_pattern: Some(format!("librespot --name {}", m.spotify_device_name)),
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
                // Reap any orphaned mpv bound to this IPC socket before spawning.
                reap_pattern: Some(format!("input-ipc-server={}", mpv_ipc.display())),
            },
        );
        if m.players_in_snapserver {
            // snapserver launches the players itself (process:// / librespot:// sources),
            // so spawning them here would start duplicates fighting over the IPC socket
            // and the Spotify device name.
            specs.retain(|proc, _| *proc == ManagedProc::Snapserver);
        }
        Some(Arc::new(MusicSupervisor::new(specs)))
    }

    /// Build the [`MusicHub`] (supervisor + snapserver control + mpv control) the
    /// config-page Music tab drives, or `None` when music is disabled. `ducker` is the
    /// same instance the turn pipeline ducks with, so manual volume changes survive a
    /// restore.
    pub fn build_hub(&self, ducker: Option<Arc<MusicDucker>>) -> Option<MusicHub> {
        let supervisor = self.build_supervisor()?;
        Some(MusicHub {
            supervisor,
            snapcast: SnapcastClient::new(self.music.snapserver_addr),
            snapserver_addr: self.music.snapserver_addr.to_string(),
            mpv: self.mpv_control(),
            ducker,
        })
    }

    /// Load the in-process whisper.cpp engine for a **live-swappable** STT setup, or
    /// `Ok(None)` when it isn't available. Mirroring the Silero model preload, this is
    /// called at boot **regardless of the selected engine** so a later config-page swap
    /// to `whisper-rs` works without a restart: the model is loaded once and held on the
    /// pipeline (`Pipeline::with_whisper_engine`), and the per-turn
    /// `build_stt_transcriber` picks it when the live selection is `whisper-rs`. Returns
    /// `None` when the `stt-whisper-local` feature is off or the model file is absent (a
    /// later swap to `whisper-rs` then falls back to Wyoming with a warning, like VAD).
    /// The `wyoming` and `elevenlabs` engines need no boot resource (Wyoming dials the
    /// connector per turn; ElevenLabs opens its WebSocket per turn from the live
    /// snapshot). See `plans/ElevenLabsSttPlan.md`.
    pub fn build_whisper_engine(&self) -> Result<Option<Arc<dyn crate::stt::SttEngine>>> {
        #[cfg(feature = "stt-whisper-local")]
        {
            let model = self.stt.resolved_model_path();
            if !model.exists() {
                log::info!(
                    "in-process whisper model {} not present; whisper-rs STT unavailable \
                     (wyoming/elevenlabs still selectable)",
                    model.display()
                );
                return Ok(None);
            }
            let model_str = model.to_string_lossy().into_owned();
            let engine = crate::stt::WhisperEngine::open(
                &model_str,
                self.stt.language.clone(),
                self.stt.num_threads as i32,
            )
            .with_context(|| format!("loading in-process Whisper model {model_str}"))?;
            log::info!(
                "in-process whisper engine loaded (model {model_str}, {} threads); \
                 live-swappable",
                self.stt.num_threads
            );
            Ok(Some(Arc::new(crate::stt::WhisperSttEngine::new(engine))))
        }
        #[cfg(not(feature = "stt-whisper-local"))]
        {
            Ok(None)
        }
    }

    /// The inputs a runtime backend swap (Phase 6) needs, captured from the config
    /// once so a later swap never re-reads anything. The provider API keys read here
    /// (still from the environment, since they're secrets) are only the **boot seed**:
    /// they flow into the live [`RuntimeSettings`], where the config page can override
    /// them at runtime, so a cloud backend can be enabled without a restart even if its
    /// key was unset at boot. If no key is present at boot *or* runtime, selecting that
    /// backend is rejected in-band.
    pub fn llm_factory(&self) -> LlmFactory {
        let imperial = crate::directions::units_are_imperial(self.weather_units.as_deref());
        LlmFactory {
            ollama_url: self.ollama_url.clone(),
            anthropic_base_url: "https://api.anthropic.com".to_string(),
            anthropic_api_key: env::var("ANTHROPIC_API_KEY").ok().filter(|s| !s.is_empty()),
            anthropic_max_tokens: self.anthropic_max_tokens,
            openai_base_url: self.graphrag.openai_base_url.clone(),
            openai_api_key: env::var("OPENAI_API_KEY").ok().filter(|s| !s.is_empty()),
            openai_max_tokens: self.openai_max_tokens,
            anthropic_token: Some(Arc::new(AnthropicTokenProvider::new(
                self.anthropic_token_cmd.clone(),
            ))),
            // Seeded from the resolved Household in `shared_settings` (config → household
            // → this handle); starts empty here.
            home_location: crate::directions::LiveHomeLocation::default(),
            // Seeded per-build from the resolved Spotify config in `shared_settings`
            // (and refreshed by `apply`/`apply_spotify`).
            spotify: None,
            // Seeded per-build from the resolved Cadora config in `shared_settings`
            // (and refreshed by `apply`/`apply_cadora`).
            cadora: None,
            // Seeded per-build from the resolved AppSaid config in `shared_settings`
            // (and refreshed by `apply`/`apply_appsaid`).
            appsaid: None,
            // The one Home Assistant hub (connection + cached device catalog), built once
            // here and shared by every rebuilt backend's `home_control` tool and by the
            // pipeline's fast path. It starts unconnected; `LlmFactory::for_settings`
            // points it at the live URL + token (config seed / env / config page).
            home_assistant: Some(crate::home_assistant::from_config(&self.home_assistant)),
            // The NVR camera hub (plans/CameraPlan.md): boot-time config + the
            // REOLINK_PASSWORD secret; `None` (feature off) when anything is missing.
            camera: crate::camera::from_config(&self.camera, self.camera_password().as_deref()),
            music_display: Default::default(),
            // Prebuilt from the config: the calendar source (web .ics) and the
            // directions provider (Mapbox). The Mapbox token is seeded from the env
            // secret here but is runtime-settable (Tools tab) — `shared_settings`
            // rebuilds `directions` from the resolved token, and `apply_directions`
            // rebuilds it on a config-page change.
            calendar: crate::calendar::build_with(
                self.calendar_specs.clone(),
                self.calendar_cache_ttl,
            ),
            directions: crate::directions::from_token(
                &self.directions_provider,
                self.initial_mapbox_token().as_deref(),
                imperial,
            ),
            directions_provider: self.directions_provider.clone(),
            directions_imperial: imperial,
            // Weather backend (Visual Crossing by default, keyless Open-Meteo fallback);
            // present whenever enabled, and the tool/push still no-op gracefully until a
            // home location is set. Rebuilt from the live provider label + key on every
            // swap; `weather_enabled` gates it entirely.
            weather: self.weather_provider(),
            weather_imperial: imperial,
            weather_enabled: self.weather.enabled,
            // Places backend (Google Places API New); present only when enabled AND a key
            // is set (no keyless fallback). Rebuilt from the live key on every swap.
            places: self.places_provider(),
            places_enabled: self.places.enabled,
            places_provider: self.places.provider.clone(),
            // Built once; its catalog is seeded in `shared_settings` and refreshed by
            // `apply_radio`.
            radio: self.radio_player(),
        }
    }

    /// Build the internet-radio player (`plans/RadioPlan.md`) over the mpv web player +
    /// snapserver, or `None` when music routing is off or no mpv IPC is configured.
    pub fn radio_player(&self) -> Option<Arc<crate::music::RadioPlayer>> {
        let mpv = self.mpv_control()?;
        Some(Arc::new(crate::music::RadioPlayer::over(
            mpv,
            SnapcastClient::new(self.music.snapserver_addr),
        )))
    }

    /// The selectable-model catalog for the settings dropdown, wired with the same
    /// provider endpoints the factory uses. Credentials (provider keys + the Anthropic
    /// auth mode) are read **live** from `settings` on every refresh, and subscription
    /// listing shares the LLM's token provider, so keys and tokens entered on the config
    /// page reach the dropdown without a restart.
    pub fn model_catalog(&self, settings: &Arc<SharedSettings>) -> ModelCatalog {
        let live = settings.clone();
        ModelCatalog::new(
            "https://api.anthropic.com",
            None,
            self.graphrag.openai_base_url.clone(),
            None,
        )
        .with_anthropic_auth(self.anthropic_auth, settings.anthropic_token_provider())
        .with_credential_source(Arc::new(move || live.catalog_credentials()))
    }

    /// The initial engine + web-search selection from the config. These seed the live
    /// [`RuntimeSettings`] and can be changed at runtime (config page / control frame).
    pub fn initial_engine(&self) -> LlmEngine {
        self.engine
    }
    pub fn initial_web_search(&self) -> bool {
        self.web_search
    }
    /// Initial search provider (`llm.search_provider`, default `tavily`).
    pub fn initial_search_provider(&self) -> String {
        self.search_provider.clone()
    }
    /// Initial search API key — a secret, so it's still read from `TAVILY_API_KEY`.
    pub fn initial_search_api_key(&self) -> Option<String> {
        env::var("TAVILY_API_KEY").ok().filter(|s| !s.is_empty())
    }

    /// Initial Mapbox token for the directions tool — a secret, seeded from
    /// `MAPBOX_TOKEN` / `MAPBOX_ACCESS_TOKEN`. Runtime-settable from the Tools tab.
    pub fn initial_mapbox_token(&self) -> Option<String> {
        env::var("MAPBOX_TOKEN")
            .ok()
            .or_else(|| env::var("MAPBOX_ACCESS_TOKEN").ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// Initial Visual Crossing API key for the weather provider — a secret, seeded from
    /// `VISUALCROSSING_API_KEY`. Absent/empty ⇒ weather falls back to keyless Open-Meteo.
    pub fn initial_visualcrossing_key(&self) -> Option<String> {
        env::var("VISUALCROSSING_API_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// Build the weather forecast provider from the config + the environment secret, or
    /// `None` when weather is disabled. Defaults to Visual Crossing when
    /// `VISUALCROSSING_API_KEY` is set; otherwise falls back to keyless Open-Meteo. Used
    /// by both the `weather_lookup` tool ([`Self::llm_factory`]) and the ambient push
    /// (`main`).
    pub fn weather_provider(&self) -> Option<Arc<dyn crate::weather::WeatherProvider>> {
        crate::weather::from_config(
            self.weather.enabled,
            &self.weather.provider,
            self.initial_visualcrossing_key().as_deref(),
        )
    }

    /// Initial Google Places API key for the `places_lookup` tool — a secret, seeded from
    /// `GOOGLE_PLACES_API_KEY`. Runtime-settable from the Tools tab. Absent ⇒ the tool
    /// isn't advertised (Google Places has no keyless fallback).
    pub fn initial_google_places_key(&self) -> Option<String> {
        env::var("GOOGLE_PLACES_API_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// Build the places provider from the config + the environment secret, or `None` when
    /// places is disabled or no key is present. Used by [`Self::llm_factory`].
    pub fn places_provider(&self) -> Option<Arc<dyn crate::places::PlacesProvider>> {
        if !self.places.enabled {
            return None;
        }
        crate::places::from_key(
            &self.places.provider,
            self.initial_google_places_key().as_deref(),
        )
    }

    /// Initial Anthropic subscription OAuth token — a secret, seeded from
    /// `ANTHROPIC_OAUTH_TOKEN`. Runtime-settable from the Config tab (auth =
    /// subscription); overrides the token command when set.
    pub fn initial_anthropic_oauth_token(&self) -> Option<String> {
        env::var("ANTHROPIC_OAUTH_TOKEN")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// Initial Google Drive photo-slideshow config seeded from the config file's
    /// `drive` block (client id/secret + folder ids). The refresh token is never
    /// seeded — it's minted by the consent flow. A persisted file overlays these at
    /// boot (see [`Self::shared_settings`]).
    pub fn initial_drive(&self) -> DriveConfig {
        self.drive.clone()
    }

    /// The initial household + home information seeded from the config file
    /// (`home_address` / `weather_units`). The member roster has no config form — it
    /// is dashboard-only — so it starts empty. A persisted file overlays these at boot
    /// (see [`Self::shared_settings`]), so an address edited on the config page wins
    /// over the config-file seed.
    pub fn initial_household(&self) -> Household {
        Household {
            address: self.home_address.clone(),
            location: None,
            weather_units: self.weather_units.clone(),
            members: Vec::new(),
        }
    }

    /// Initial Spotify voice-control config seeded from the config file's `spotify`
    /// block. Any of these may instead be set (or overridden) at runtime by the
    /// config-page consent flow; a persisted file overlays these at boot (see
    /// [`Self::shared_settings`]). Requires Spotify Premium to actually play.
    pub fn initial_spotify(&self) -> SpotifyConfig {
        self.spotify.clone()
    }

    /// Initial Cadora shopping-list config seeded from the config file's `cadora`
    /// block. The voice-link token is normally minted at runtime by the config-page
    /// pairing flow; a persisted file overlays these at boot (see
    /// [`Self::shared_settings`]).
    pub fn initial_cadora(&self) -> CadoraConfig {
        self.cadora.clone()
    }

    /// Initial AppSaid phone-push config seeded from the config file's `appsaid` block
    /// (worker URL + recipients + default recipient). The secret app token is not here —
    /// see [`Self::initial_appsaid_app_token`]. A persisted file overlays these at boot
    /// (see [`Self::shared_settings`]).
    pub fn initial_appsaid(&self) -> AppSaidConfig {
        self.appsaid.clone()
    }

    /// The Home Assistant long-lived access token — a secret, read from
    /// `HOME_ASSISTANT_TOKEN` (never from the JSON config).
    pub fn initial_home_assistant_token(&self) -> Option<String> {
        env::var("HOME_ASSISTANT_TOKEN")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// The NVR password for the camera view — a secret, read from `REOLINK_PASSWORD`
    /// (never from the JSON config). Boot-time only.
    pub fn camera_password(&self) -> Option<String> {
        env::var("REOLINK_PASSWORD").ok().filter(|s| !s.is_empty())
    }

    /// Initial AppSaid sender app token — a secret, seeded from `APPSAID_APP_TOKEN`.
    /// Runtime-settable from the config page; persisted to the settings file (0600).
    pub fn initial_appsaid_app_token(&self) -> Option<String> {
        env::var("APPSAID_APP_TOKEN")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// Build the shared, runtime-swappable settings (Phase 6): the initial backend
    /// selected by config plus the factory that rebuilds backends when the device
    /// changes them. The initial backend must build successfully (anthropic still
    /// needs its key at startup).
    ///
    /// Resolution order per value: built-in default → config file / env secret (the
    /// **boot seed**) → the persisted settings file (page/device changes from a previous
    /// run win across restarts; see [`RuntimeSettings::overlay_persisted`]). The seed is
    /// also handed to [`SharedSettings`] so a seeded secret cleared on the page is
    /// persisted as cleared rather than reverting to the env value at the next boot.
    pub fn shared_settings(&self) -> Result<Arc<SharedSettings>> {
        let factory = self.llm_factory();
        let persist_path = self.settings_path.clone();

        // ---- the boot seed: config file + env secrets ----
        let (backend, model) = match &self.llm {
            LlmChoice::Mock => ("mock".to_string(), None),
            LlmChoice::Ollama { model, .. } => ("ollama".to_string(), Some(model.clone())),
            LlmChoice::Anthropic { model, .. } => ("anthropic".to_string(), Some(model.clone())),
            LlmChoice::OpenAI { model, .. } => ("openai".to_string(), Some(model.clone())),
        };
        // AppSaid: worker URL + recipients + default seed from the config file; the
        // secret app token seeds from `APPSAID_APP_TOKEN` (env), like the Mapbox token.
        let mut appsaid = self.initial_appsaid();
        appsaid.app_token = self.initial_appsaid_app_token();
        // Whether the in-process whisper engine can be selected live: the feature must be
        // compiled in and the ggml model present on disk (it is loaded at boot regardless
        // of the seeded engine, mirroring Silero). Reported to the config page so it can
        // disable the `whisper-rs` option when unavailable.
        let whisper_available =
            cfg!(feature = "stt-whisper-local") && self.stt.resolved_model_path().exists();
        let mut s = RuntimeSettings {
            // Placeholder until the resolved settings are built below.
            llm: Arc::new(crate::llm::mock::MockLlm::default()),
            engine: self.initial_engine(),
            web_search: self.initial_web_search(),
            search_provider: self.initial_search_provider(),
            search_api_key: self.initial_search_api_key(),
            llm_backend: backend,
            llm_model: model,
            // Live provider keys start from the environment (the factory's boot seed).
            anthropic_api_key: factory.anthropic_api_key.clone(),
            openai_api_key: factory.openai_api_key.clone(),
            anthropic_oauth_token: self.initial_anthropic_oauth_token(),
            mapbox_token: self.initial_mapbox_token(),
            anthropic_auth: self.anthropic_auth,
            tts_voice: self.tts_voice.clone(),
            end_silence_ms: crate::settings::DEFAULT_END_SILENCE_MS,
            voice_rms_threshold: crate::settings::DEFAULT_VOICE_RMS_THRESHOLD,
            silero_threshold: self.vad.silero.threshold,
            vad_engine: self.vad.engine,
            stt: crate::settings::SttRuntime {
                engine: self.stt.engine,
                elevenlabs_model_id: self.stt.elevenlabs.model_id.clone(),
                elevenlabs_base_url: self.stt.elevenlabs.base_url.clone(),
                elevenlabs_api_key: env::var("ELEVENLABS_API_KEY")
                    .ok()
                    .filter(|s| !s.is_empty()),
                language: self.stt.language.clone(),
                whisper_available,
            },
            drive: self.initial_drive(),
            household: self.initial_household(),
            spotify: self.initial_spotify(),
            cadora: self.initial_cadora(),
            appsaid,
            // Home Assistant: URL + room seed from the config file, the token from the
            // HOME_ASSISTANT_TOKEN secret; the persisted overlay (config page) wins.
            home_assistant: crate::settings::HomeAssistantSettings {
                base_url: self.home_assistant.base_url.clone(),
                token: self.initial_home_assistant_token(),
                default_room: self.home_assistant.default_room.clone(),
            },
            weather_provider: self.weather.provider.clone(),
            visualcrossing_key: self.initial_visualcrossing_key(),
            google_places_key: self.initial_google_places_key(),
            system1: crate::settings::System1Runtime {
                engine: crate::system1::none(),
                backend: self.system1.backend.clone(),
                base_url: self.system1.base_url.clone(),
                model: self.system1.openrouter_model.clone(),
                min_confidence: self.system1.min_confidence,
                intents: self.system1.intents.clone(),
                openrouter_api_key: env::var("OPENROUTER_API_KEY")
                    .ok()
                    .filter(|s| !s.is_empty()),
            },
            // Output personality + the pronunciation table are pure runtime/GUI state
            // with no config-file seed.
            personality: Personality::default(),
            pronunciations: Vec::new(),
            language: self.language.clone(),
            music_display_pins: Default::default(),
            radio: Default::default(),
        };
        let seed = PersistedSettings::from_runtime(&s, None);

        // ---- overlay the persisted file (a previous run's page/device changes) ----
        if let Some(p) = persist_path.as_deref().and_then(load_persisted) {
            log::info!("loaded persisted settings");
            s.overlay_persisted(p);
        }

        // Sanitize the resolved household once: trims fields and **migrates** any legacy
        // single-line `location` (from an older config/persisted file) into `address.city`
        // so downstream only ever sees the discrete `address`.
        s.household = s.household.sanitized();
        // Unify AppSaid recipients onto the household (the single source of truth): fold
        // any legacy `appsaid.recipients` seed (config file / older persisted settings)
        // into the roster — upsert a member by name, which auto-adds them if absent — then
        // DERIVE the messenger roster from the household's members' `appsaid` addresses.
        // A member that already carries an AppSaid address (persisted household) wins over
        // the legacy seed, so re-runs are idempotent and never clobber a page/pairing edit.
        for (name, key) in std::mem::take(&mut s.appsaid.recipients) {
            let already_set = s
                .household
                .member_matching(&name)
                .and_then(|m| m.appsaid.as_deref())
                .is_some_and(|v| !v.trim().is_empty());
            if !already_set {
                s.household.set_member_appsaid(&name, &key);
            }
        }
        s.appsaid.recipients = s.household.appsaid_roster();
        // Seed the location-aware tools' live home address from the resolved household.
        // Every rebuilt backend's tools share this cell; `apply_household` updates it.
        factory.home_location.set(s.household.address.clone());
        // Seed the radio player's live catalog from the resolved (persisted) station list
        // before the first build, so `radio_control` advertises it from boot.
        s.radio = s.radio.sanitized();
        if let Some(radio) = &factory.radio {
            radio.set_catalog(s.radio.stations.clone());
        }
        // Seed the shared token provider's override from the resolved subscription
        // token. The Arc is shared with every rebuilt backend, so a token entered on the
        // config page later takes effect in place.
        if let Some(provider) = &factory.anthropic_token {
            provider.set_override(s.anthropic_oauth_token.clone());
        }

        // Build the initial backend + its full tool set from the resolved settings via
        // the same settings → factory mapping every runtime `apply_*` uses.
        let (llm, llm_backend, llm_model) = factory
            .build_for(&s)
            .context("building the initial LLM backend")?;
        s.llm = llm;
        s.llm_backend = llm_backend;
        s.llm_model = llm_model;
        // Build the initial System-1 engine from the resolved (seed → persisted) config.
        if s.system1.backend.eq_ignore_ascii_case("jev") && s.system1.openrouter_api_key.is_none() {
            log::warn!(
                "system1.backend=jev but OPENROUTER_API_KEY is unset; Jev requests will be \
                 rejected until it is provided (env or config page)."
            );
        }
        s.system1.engine = crate::system1::build(
            &s.system1.backend,
            &s.system1.base_url,
            &s.system1.model,
            s.system1.openrouter_api_key.clone(),
            s.system1.min_confidence,
            s.system1.intents.clone(),
        )
        .context("building the initial System-1 engine")?;
        Ok(SharedSettings::new_seeded(
            factory,
            s,
            persist_path,
            Some(seed),
        ))
    }

    /// Build the per-person [`SpeakerService`], or `None` when speaker ID is
    /// disabled (`speaker.enabled = false`). The registry lives in the same SQLite
    /// file as memory. When `speaker.model_path` is set the real ECAPA-TDNN ONNX
    /// embedder is loaded; otherwise it falls back to the deterministic mock
    /// embedder (with a loud warning, as the mock is not accurate for real voices).
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
        )
        .with_short_clip(self.speaker.min_identify_ms, self.speaker.short_clip_margin)
        .with_household_margins(self.speaker.min_margin, self.speaker.clear_margin);
        let embedder: Arc<dyn SpeakerEmbedder> = match &self.speaker.model_path {
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
            None => {
                log::warn!(
                    "speaker ID enabled with no model path; using the deterministic mock embedder \
                     (dev/testing only — not accurate for real voices). Set speaker.model_path \
                     to use the ONNX model."
                );
                Arc::new(MockSpeakerEmbedder::default())
            }
        };
        // Rolling raw-clip store (the People screen's "play" button): keep the newest
        // few utterances per speaker next to the registry database so a human can
        // listen to an anonymous cluster and name it. Clip capture is best-effort — a
        // store that can't be opened just means there's nothing to play back.
        let clips_root = self
            .db_path
            .parent()
            .map(|dir| dir.join("anamanti_clips"))
            .unwrap_or_else(|| std::path::PathBuf::from("anamanti_clips"));
        let clips = match crate::speaker::ClipStore::open(&clips_root) {
            Ok(store) => {
                log::info!("voice-clip store: {}", clips_root.display());
                Some(store)
            }
            Err(e) => {
                log::warn!(
                    "voice-clip store unavailable at {} ({e:#}); clips won't be saved",
                    clips_root.display()
                );
                None
            }
        };
        Ok(Some(Arc::new(SpeakerService::new_with_clips(
            embedder, registry, thresholds, clips,
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
        assert_eq!(Config::default().instance_id, "anamanti-core");
    }

    #[test]
    fn sanitize_id_maps_service_names_to_stable_keys() {
        assert_eq!(sanitize_id("Anamanti Core"), "anamanti-core");
        assert_eq!(sanitize_id("Test Mac"), "test-mac");
        assert_eq!(sanitize_id("Mac Mini (prod)"), "mac-mini--prod");
        assert_eq!(sanitize_id("***"), "anamanti-core");
    }

    fn parse(json: &str) -> FileConfig {
        serde_json::from_str(json).expect("valid FileConfig JSON")
    }

    #[test]
    fn deprecated_system1_device_and_model_keys_still_load() {
        // A deployed file may still carry the retired laya-embedded keys; with
        // `deny_unknown_fields` they must keep parsing (and are ignored).
        let c = Config::from_file(parse(
            r#"{ "system1": { "backend": "jev", "device": "metal", "model": "x/y" } }"#,
        ))
        .unwrap();
        assert_eq!(c.system1.backend, "jev");
        assert_eq!(c.system1.openrouter_model, "typesafe/jev-1.13");
    }

    #[test]
    fn empty_file_reproduces_defaults() {
        // `{}` overlays nothing, so every resolved value matches `Config::default()`
        // (except `instance_id`, which resolves via git branch / service name).
        let c = Config::from_file(parse("{}")).unwrap();
        let d = Config::default();
        assert_eq!(c.bind_addr, d.bind_addr);
        assert_eq!(c.config_addr, d.config_addr);
        assert_eq!(c.stt_addr, d.stt_addr);
        assert_eq!(c.tts_addr, d.tts_addr);
        assert_eq!(c.db_path, d.db_path);
        assert_eq!(c.helix_path, d.helix_path);
        assert_eq!(c.system_prompt, d.system_prompt);
        assert_eq!(c.memory_backend, d.memory_backend);
        assert_eq!(c.llm_label(), "ollama");
        assert_eq!(c.ollama_url, d.ollama_url);
        assert_eq!(c.engine, d.engine);
        assert!(c.web_search);
        assert_eq!(c.search_provider, "tavily");
        assert_eq!(c.anthropic_auth, AnthropicAuth::ApiKey);
        assert_eq!(c.settings_path, d.settings_path);
        assert!(c.audio_dump_dir.is_none());
        assert!(c.calendar_specs.is_empty());
        assert_eq!(c.calendar_cache_ttl, d.calendar_cache_ttl);
        assert_eq!(c.drive.scope.as_deref(), Some(DEFAULT_DRIVE_SCOPE));
        assert!(!c.speaker.enabled);
        assert!(c.music.enabled);
        // STT defaults to the downstream Wyoming engine.
        assert_eq!(c.stt.engine, SttEngineKind::Wyoming);
        assert_eq!(c.stt.model, "base");
        assert_eq!(c.stt.language.as_deref(), Some("en"));
        // VAD defaults to the neural Silero gate (committed default since 2026-09-30).
        assert_eq!(c.vad.engine, VadEngineKind::Silero);
        assert_eq!(c.vad.silero.threshold, 0.5);
        // No tool_cache block ⇒ built-in defaults (weather cached 60 min).
        assert_eq!(
            c.tool_cache.ttl("weather_lookup"),
            std::time::Duration::from_secs(3600)
        );
    }

    #[test]
    fn tool_cache_block_overlays_per_tool_ttls() {
        let c = Config::from_file(parse(
            r#"{ "tool_cache": { "weather_lookup": 1800, "directions_lookup": 600 } }"#,
        ))
        .unwrap();
        // File entries win; an unmentioned tool stays uncached.
        assert_eq!(
            c.tool_cache.ttl("weather_lookup"),
            std::time::Duration::from_secs(1800)
        );
        assert_eq!(
            c.tool_cache.ttl("directions_lookup"),
            std::time::Duration::from_secs(600)
        );
        assert_eq!(c.tool_cache.ttl("recipe_lookup"), std::time::Duration::ZERO);
    }

    #[test]
    fn tool_cache_zero_disables_a_defaulted_tool() {
        let c = Config::from_file(parse(r#"{ "tool_cache": { "weather_lookup": 0 } }"#)).unwrap();
        assert_eq!(
            c.tool_cache.ttl("weather_lookup"),
            std::time::Duration::ZERO
        );
    }

    #[test]
    fn stt_block_selects_in_process_engine() {
        let c = Config::from_file(parse(
            r#"{ "stt": { "engine": "whisper-rs", "model": "small",
                          "model_dir": "/models", "num_threads": 4 } }"#,
        ))
        .unwrap();
        assert_eq!(c.stt.engine, SttEngineKind::WhisperLocal);
        assert_eq!(c.stt.model, "small");
        assert_eq!(c.stt.num_threads, 4);
        assert_eq!(
            c.stt.resolved_model_path(),
            std::path::PathBuf::from("/models/ggml-small.en.bin")
        );
    }

    #[test]
    fn vad_block_selects_silero_engine_and_settings() {
        let c = Config::from_file(parse(
            r#"{ "vad": { "engine": "silero",
                          "silero": { "model_path": "/m/silero_vad.onnx", "threshold": 0.35 } } }"#,
        ))
        .unwrap();
        assert_eq!(c.vad.engine, VadEngineKind::Silero);
        assert_eq!(
            c.vad.silero.model_path,
            std::path::PathBuf::from("/m/silero_vad.onnx")
        );
        assert_eq!(c.vad.silero.threshold, 0.35);
    }

    #[test]
    fn vad_unknown_engine_is_a_hard_error() {
        let err = Config::from_file(parse(r#"{ "vad": { "engine": "webrtc" } }"#)).unwrap_err();
        assert!(
            err.to_string().contains("unknown vad.engine"),
            "expected a clear engine error, got: {err}"
        );
    }

    #[test]
    fn vad_rejects_unknown_keys() {
        // `deny_unknown_fields` turns a typo into a hard parse error, not a silent default.
        assert!(
            serde_json::from_str::<FileConfig>(r#"{ "vad": { "engien": "silero" } }"#).is_err()
        );
    }

    #[test]
    fn stt_model_path_overrides_dir_and_name() {
        let c = Config::from_file(parse(
            r#"{ "stt": { "model_path": "/opt/x.bin", "language": "" } }"#,
        ))
        .unwrap();
        assert_eq!(
            c.stt.resolved_model_path(),
            std::path::PathBuf::from("/opt/x.bin")
        );
        // An explicit empty language means auto-detect.
        assert_eq!(c.stt.language, None);
    }

    #[test]
    fn stt_unknown_engine_is_rejected() {
        let err = Config::from_file(parse(r#"{ "stt": { "engine": "vosk" } }"#)).unwrap_err();
        assert!(err.to_string().contains("stt.engine"), "{err}");
    }

    #[test]
    fn calendar_cache_ttl_is_passed_through_raw() {
        let ttl = |json: &str| Config::from_file(parse(json)).unwrap().calendar_cache_ttl;
        // Absent ⇒ None, so tool_cache.calendar_lookup decides.
        assert_eq!(ttl("{}"), None);
        assert_eq!(ttl(r#"{ "calendar": {} }"#), None);
        // Set ⇒ it wins, including a value equal to the 300 s default and 0 (no cache).
        assert_eq!(
            ttl(r#"{ "calendar": { "cache_ttl_secs": 300 } }"#),
            Some(Duration::from_secs(300))
        );
        assert_eq!(
            ttl(r#"{ "calendar": { "cache_ttl_secs": 0 } }"#),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn full_file_overlays_every_layer() {
        let c = Config::from_file(parse(
            r#"{
                "bind_addr": "0.0.0.0:10701",
                "config_addr": "127.0.0.1:8731",
                "service_name": "Test Mac",
                "instance_id": "test-key",
                "system_prompt": "hi",
                "home_location": "  Austin, Texas  ",
                "weather_units": "imperial",
                "turn_timeout_secs": 45,
                "memory_backend": "sqlite",
                "audio_dump_dir": "/tmp/dump",
                "settings_path": "custom_settings.json",
                "llm": {
                    "backend": "anthropic",
                    "engine": "native",
                    "anthropic_auth": "subscription",
                    "anthropic_token_cmd": "my-token-cmd",
                    "web_search": false,
                    "search_provider": "tavily",
                    "ollama": { "url": "http://ollama:11434", "model": "qwen2.5" },
                    "anthropic": { "model": "claude-x", "max_tokens": 2048 },
                    "openai": { "model": "gpt-x", "max_tokens": 512 }
                },
                "graphrag": { "embed_dims": 256, "ingest_interval_secs": 10, "recall_k": 9 },
                "speaker": { "enabled": true, "match_threshold": 0.7, "min_margin": 0.08, "clear_margin": 0 },
                "music": { "enabled": true, "duck_percent": 250, "group": "g1" },
                "calendar": {
                    "subscriptions": [ { "url": "https://ex.com/a.ics" }, { "name": "Work", "url": "https://ex.com/w.ics" } ],
                    "cache_ttl_secs": 600
                },
                "directions": { "provider": "mapbox" },
                "drive": { "client_id": "cid", "folder_ids": ["f1", " ", "f2"] },
                "spotify": { "client_id": "sid", "refresh_token": "rt" }
            }"#,
        ))
        .unwrap();
        assert_eq!(c.bind_addr.to_string(), "0.0.0.0:10701");
        assert_eq!(
            c.config_addr.map(|a| a.to_string()).as_deref(),
            Some("127.0.0.1:8731")
        );
        assert_eq!(c.service_name, "Test Mac");
        assert_eq!(c.instance_id, "test-key");
        // Legacy single-line `home_location` folds into City (trimmed), so weather still
        // resolves and the full address is the same string.
        assert_eq!(c.home_address.city.as_deref(), Some("Austin, Texas"));
        assert_eq!(
            c.home_address.weather_location().as_deref(),
            Some("Austin, Texas")
        );
        assert_eq!(
            c.home_address.full_address().as_deref(),
            Some("Austin, Texas")
        );
        assert_eq!(c.weather_units.as_deref(), Some("imperial"));
        assert_eq!(c.turn_timeout, Duration::from_secs(45));
        assert_eq!(c.memory_backend, MemoryBackendChoice::Sqlite);
        assert_eq!(
            c.audio_dump_dir.as_deref(),
            Some(std::path::Path::new("/tmp/dump"))
        );
        assert_eq!(
            c.settings_path.as_deref(),
            Some(std::path::Path::new("custom_settings.json"))
        );
        assert_eq!(c.llm_label(), "anthropic");
        assert_eq!(c.engine, LlmEngine::Native);
        assert_eq!(c.anthropic_auth, AnthropicAuth::Subscription);
        assert_eq!(c.anthropic_token_cmd.as_deref(), Some("my-token-cmd"));
        assert!(!c.web_search);
        assert_eq!(c.search_provider, "tavily");
        // The non-selected backends' params still resolve from their sub-blocks.
        assert_eq!(c.ollama_url, "http://ollama:11434");
        assert_eq!(c.anthropic_max_tokens, 2048);
        assert_eq!(c.openai_max_tokens, 512);
        assert_eq!(c.graphrag.embed_dims, 256);
        assert_eq!(c.graphrag.ingest_interval, Duration::from_secs(10));
        assert_eq!(c.graphrag.recall_k, 9);
        assert!(c.speaker.enabled);
        assert_eq!(c.speaker.match_threshold, 0.7);
        assert_eq!(c.speaker.min_margin, 0.08);
        assert_eq!(c.speaker.clear_margin, 0.0);
        assert!(c.music.enabled);
        assert_eq!(c.music.duck_percent, 100); // capped at 100
                                               // Calendar: bare URL auto-named, explicit name kept.
        assert_eq!(c.calendar_specs.len(), 2);
        assert_eq!(c.calendar_specs[0].name, "Calendar 1");
        assert_eq!(c.calendar_specs[1].name, "Work");
        assert_eq!(c.calendar_cache_ttl, Some(Duration::from_secs(600)));
        // Drive: blank folder id dropped; refresh token never seeded.
        assert_eq!(c.drive.client_id.as_deref(), Some("cid"));
        assert_eq!(c.drive.folder_ids, vec!["f1", "f2"]);
        assert!(c.drive.refresh_token.is_none());
        assert_eq!(c.spotify.client_id.as_deref(), Some("sid"));
        assert_eq!(c.spotify.refresh_token.as_deref(), Some("rt"));
    }

    #[test]
    fn graphrag_embed_backend_defaults_to_local_and_parses() {
        // Default: local nomic, 768 dims, default on-disk model/tokenizer paths.
        let d = Config::from_file(parse("{}")).unwrap();
        assert_eq!(d.graphrag.embed_backend, EmbedBackend::Local);
        assert_eq!(d.graphrag.embed_dims, 768);
        assert_eq!(
            d.graphrag.embed_model_path,
            std::path::PathBuf::from("models/nomic-embed-text-v1.5.onnx")
        );
        assert_eq!(
            d.graphrag.embed_tokenizer_dir,
            std::path::PathBuf::from("models/nomic-tokenizer")
        );

        // Explicit OpenAI backend + custom local paths parse through.
        let c = Config::from_file(parse(
            r#"{ "graphrag": {
                "embed_backend": "openai",
                "embed_model_path": "/m/x.onnx",
                "embed_tokenizer_dir": "/m/tok"
            } }"#,
        ))
        .unwrap();
        assert_eq!(c.graphrag.embed_backend, EmbedBackend::OpenAi);
        assert_eq!(
            c.graphrag.embed_model_path,
            std::path::PathBuf::from("/m/x.onnx")
        );
        assert_eq!(
            c.graphrag.embed_tokenizer_dir,
            std::path::PathBuf::from("/m/tok")
        );

        // Unknown backend string falls back to the default (local).
        let u =
            Config::from_file(parse(r#"{ "graphrag": { "embed_backend": "weirdo" } }"#)).unwrap();
        assert_eq!(u.graphrag.embed_backend, EmbedBackend::Local);
    }

    #[test]
    fn disable_sentinels_turn_features_off() {
        let c = Config::from_file(parse(
            r#"{ "config_addr": "off", "settings_path": "none", "music": { "web_ipc": "" } }"#,
        ))
        .unwrap();
        assert!(c.config_addr.is_none());
        assert!(c.settings_path.is_none());
        assert!(c.music.mpv_ipc.is_none());
    }

    #[test]
    fn players_in_snapserver_limits_the_supervisor_to_snapserver() {
        // Default: snapserver launches mpv + librespot itself; the Core runs only it.
        let c = Config::from_file(parse(r#"{ "settings_path": "none" }"#)).unwrap();
        assert!(c.music.players_in_snapserver);
        let sup = c.build_supervisor().unwrap();
        assert!(sup.manages(ManagedProc::Snapserver));
        assert!(!sup.manages(ManagedProc::Librespot));
        assert!(!sup.manages(ManagedProc::MpvWeb));

        // Opt-out (Linux FIFO layout): the Core launches all three.
        let c = Config::from_file(parse(
            r#"{ "settings_path": "none", "music": { "players_in_snapserver": false } }"#,
        ))
        .unwrap();
        let sup = c.build_supervisor().unwrap();
        assert!(ManagedProc::all().into_iter().all(|p| sup.manages(p)));
    }

    #[test]
    fn unknown_key_is_rejected() {
        // `deny_unknown_fields` turns a typo into a hard error, not a silent default.
        let err = serde_json::from_str::<FileConfig>(r#"{ "bnid_addr": "0.0.0.0:1" }"#)
            .expect_err("a misspelled key must be rejected");
        assert!(err.to_string().contains("bnid_addr"), "{err}");
    }

    #[test]
    fn instance_id_falls_back_to_sanitized_service_name_when_no_git() {
        // With an explicit service name and no instance_id, the key is the sanitized
        // service name — unless a git branch is detected. Assert it's one of the two
        // stable forms (never empty / random).
        let c = Config::from_file(parse(r#"{ "service_name": "Kitchen Mac" }"#)).unwrap();
        assert!(!c.instance_id.is_empty());
        assert_eq!(c.instance_id, c.instance_id.to_lowercase());
    }

    #[test]
    fn load_missing_explicit_config_is_an_error() {
        let missing = std::env::temp_dir().join(format!(
            "ambient_no_such_config_{}_{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&missing);
        assert!(Config::load(Some(&missing)).is_err());
    }

    #[test]
    fn shipped_example_config_is_valid() {
        // The committed `anamanti.example.json` must always parse under
        // `deny_unknown_fields` and resolve, so it can't drift out of sync with the
        // schema.
        let example = concat!(env!("CARGO_MANIFEST_DIR"), "/anamanti.example.json");
        let c = Config::load(Some(std::path::Path::new(example)))
            .expect("anamanti.example.json must be a valid config");
        assert_eq!(c.service_name, "Anamanti Core");
        assert_eq!(c.llm_label(), "ollama");
    }

    #[test]
    fn load_explicit_config_reads_the_file() {
        let path = std::env::temp_dir().join(format!(
            "ambient_load_config_{}_{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, r#"{ "service_name": "Loaded Mac" }"#).unwrap();
        let c = Config::load(Some(&path)).unwrap();
        assert_eq!(c.service_name, "Loaded Mac");
        let _ = std::fs::remove_file(&path);
    }

    /// A settings file of exactly the shape a deployed Core has on disk (every key the
    /// production `anamanti_settings.json` carries, with the same JSON types).
    fn live_shape_settings_json() -> serde_json::Value {
        serde_json::json!({
            "engine": "rig",
            "web_search": true,
            "search_provider": "tavily",
            "search_api_key": "tvly-live",
            "llm_backend": "anthropic",
            "llm_model": "claude-opus-5",
            "anthropic_api_key": "sk-ant-live",
            "openai_api_key": "sk-openai-live",
            "anthropic_oauth_token": "oauth-live",
            "mapbox_token": "pk.live",
            "anthropic_auth": "apikey",
            "tts_voice": null,
            "end_silence_ms": 650,
            "voice_rms_threshold": 210.5,
            "silero_threshold": 0.5,
            "vad_engine": "silero",
            "stt_engine": "wyoming",
            "stt_elevenlabs_model_id": "scribe_v2_realtime",
            "stt_elevenlabs_base_url": "wss://api.elevenlabs.io",
            "stt_elevenlabs_api_key": "xi-live",
            "drive": {
                "client_id": "cid.apps.googleusercontent.com",
                "client_secret": "gocspx-live",
                "refresh_token": "1//refresh-live",
                "folder_ids": ["1AbC", "1XyZ"],
                "scope": "https://www.googleapis.com/auth/drive.readonly"
            },
            "household": {
                "address": {
                    "address1": "123 Main St",
                    "address2": null,
                    "city": "Austin",
                    "state": "TX",
                    "postal": "78701",
                    "country": "USA"
                },
                "location": null,
                "weather_units": "imperial",
                "members": [{
                    "name": "Mom",
                    "emails": ["mom@example.com"],
                    "phones": ["+1 555 0100"],
                    "appsaid": "key_mom",
                    "relationship": "parent"
                }]
            },
            "spotify": {
                "client_id": "sp-cid",
                "client_secret": "sp-secret",
                "refresh_token": "sp-refresh",
                "device_name": "Kitchen",
                "redirect_url": null,
                "scope": "user-modify-playback-state"
            },
            "cadora": { "base_url": null, "link_token": "vl_live" },
            "appsaid": {
                "worker_url": "https://appsaid.example.workers.dev",
                "app_token": "appsaid-live",
                "recipients": { "Mom": "key_mom" },
                "default_recipient": "Mom"
            },
            "weather_provider": "visualcrossing",
            "visualcrossing_key": "vc-live",
            "google_places_key": "gp-live",
            "system1_backend": "none",
            "system1_base_url": "http://127.0.0.1:8000",
            "system1_model": "typesafe/jev-1.13",
            "system1_min_confidence": 0.85,
            "system1_intents": [],
            "openrouter_api_key": "or-live",
            "personality": {
                "enabled": true,
                "active": "gangster",
                "definitions": [
                    { "key": "gangster", "label": "1940s Gangster", "description": "Talk tough." }
                ]
            },
            "pronunciations": [
                { "canonical": "Zehrs", "variants": ["zairs", "zay rz"] }
            ]
        })
    }

    fn settings_test_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("anamanti-cfg-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Boot a Core whose `settings_path` points at `path` (built-in config defaults).
    fn boot_with_settings(path: &Path) -> Arc<SharedSettings> {
        let fc = parse(&format!(
            r#"{{ "settings_path": {} }}"#,
            serde_json::to_string(&path.display().to_string()).unwrap()
        ));
        Config::from_file(fc)
            .unwrap()
            .shared_settings()
            .expect("boot from the persisted settings")
    }

    #[test]
    fn live_shape_settings_file_loads_and_round_trips_without_loss() {
        let dir = settings_test_dir("live-shape");
        let path = dir.join("anamanti_settings.json");
        let original = live_shape_settings_json();
        std::fs::write(&path, serde_json::to_string_pretty(&original).unwrap()).unwrap();

        let s = boot_with_settings(&path);
        // Spot-check the loaded values (secrets via the snapshot, never a view).
        let snap = s.snapshot();
        assert_eq!(snap.llm_backend, "anthropic");
        assert_eq!(snap.llm_model.as_deref(), Some("claude-opus-5"));
        assert_eq!(snap.anthropic_api_key.as_deref(), Some("sk-ant-live"));
        assert_eq!(snap.search_api_key.as_deref(), Some("tvly-live"));
        assert_eq!(snap.google_places_key.as_deref(), Some("gp-live"));
        assert_eq!(snap.appsaid.app_token.as_deref(), Some("appsaid-live"));
        assert_eq!(snap.cadora.link_token.as_deref(), Some("vl_live"));
        assert_eq!(
            snap.household.members[0].appsaid.as_deref(),
            Some("key_mom")
        );
        assert!(snap.personality.enabled);

        // Any save rewrites the file; every key must come back with the same value.
        s.apply_personality(&s.personality());
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for (key, value) in original.as_object().unwrap() {
            assert_eq!(
                written.get(key),
                Some(value),
                "key `{key}` changed on rewrite"
            );
        }
        // And the rewritten file boots to the same state.
        let again = boot_with_settings(&path).snapshot();
        assert_eq!(again.anthropic_api_key, snap.anthropic_api_key);
        assert_eq!(again.household, snap.household);
        assert_eq!(again.spotify, snap.spotify);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spotify_redirect_url_survives_a_restart() {
        let dir = settings_test_dir("redirect");
        let path = dir.join("anamanti_settings.json");
        let mut v = live_shape_settings_json();
        v["spotify"]["redirect_url"] = serde_json::json!("http://127.0.0.1:9999/callback");
        std::fs::write(&path, v.to_string()).unwrap();
        let s = boot_with_settings(&path);
        assert_eq!(
            s.spotify().redirect_url_or_default(),
            "http://127.0.0.1:9999/callback"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
