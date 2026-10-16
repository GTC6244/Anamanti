//! The assistant pipeline (Plan.MD Phase 4). Given a device-facing Wyoming
//! connection whose opening `audio-start` has been read ([`TurnStart`]), one
//! [`Pipeline::run_turn_after_start`] drives a full voice turn:
//!
//! 1. **STT** — forward the device's streamed PCM to the STT engine (downstream
//!    Wyoming Whisper, in-process whisper.cpp, or ElevenLabs, behind the
//!    [`crate::stt::Transcriber`] seam), detect end-of-speech with the Core's VAD
//!    gate, finalize, then relay the `transcript` back to the device
//!    ([`vad_loop`]).
//! 2. **Fast paths** — explicit memory commands ("remember…"/"forget…") and the
//!    System-1 intents ([`system1_handlers`]) answer without the LLM.
//! 3. **Memory + LLM** — auto-infer facts, recall memory context, assemble the
//!    prompt ([`prompt`]), and stream a reply from the pluggable [`LlmBackend`].
//! 4. **TTS** — synthesize the reply with Piper sentence by sentence and relay the
//!    audio back over the same socket, under the barge-in watcher ([`speech_out`]).
//!
//! The pipeline acquires its downstream connections through a [`ServiceConnector`]
//! so production dials TCP while tests wire in-memory mock servers — the turn
//! logic is identical either way.

mod prompt;
mod speech_out;
mod system1_handlers;
mod turn;
mod vad_loop;

pub use prompt::detect_personality_switch;
pub use turn::TurnStart;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use tokio::time::Instant;

use crate::audio_dump::TurnAudioDump;
use crate::config::FollowUpConfig;
use crate::llm::{DeviceAction, LlmBackend, LlmTurn, ReplyStream};
use crate::memory::chatlog::now_secs;
use crate::memory::promptlog::PromptLogRecord;
use crate::memory::{
    classify_scope, infer_memories, parse_command, ChatLog, ChatLogRecord, FactScope,
    MemoryCommand, MemoryKind, MemoryScope, MemorySource, MemoryStore, PromptLog, Recall,
    SqliteRecall, WrittenMemory,
};
use crate::settings::{RuntimeSettings, SharedSettings};
use crate::speaker::{SpeakerContext, SpeakerResolution, SpeakerService, UNKNOWN_SPEAKER};
use crate::stt::{SttEngine, Transcriber, WyomingTranscriber};
use crate::vad::SpeechGate;
use crate::wyoming::protocol::{self, types, AudioFormat, WyomingEvent};
use crate::wyoming::{DynConnection, TimedOut};

use speech_out::{
    drain_device_actions, emit_follow_up_and_stop, take_speakable, with_barge_in, BargeIn,
    Delivery, FastReply,
};
use turn::TurnCtx;
use vad_loop::Listen;

/// Inactivity budget for the LLM reply stream *between* tokens once the reply has
/// started. Rig streams every round, but a tool round can still sit silent for a tool
/// call (web search ≤15 s, places/recipe ≤12 s) plus the next round's time-to-first-
/// token, so this stays generous; a provider that has gone quiet this long has stalled.
/// On expiry the turn ends cleanly — whatever was already spoken stands, the device gets
/// its closing `audio-stop`, and music ducking is restored by the caller.
pub const LLM_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Budget for opening the LLM request *and* receiving its first token. Must exceed the
/// backends' own response-start allowance for a cold local model
/// (`crate::llm::RESPONSE_START_TIMEOUT`, 90 s) plus a few silent tool rounds, so the
/// outer guard never pre-empts a backend that is still within its own budget.
pub const LLM_FIRST_TOKEN_TIMEOUT: Duration = Duration::from_secs(150);

/// Progress events surfaced as a turn runs (logging, tests, and — via the device
/// relay — the Phase-5 UI).
#[derive(Debug, Clone, PartialEq)]
pub enum TurnEvent {
    /// PCM is now streaming to the STT service.
    Streaming,
    /// The final transcript arrived from STT.
    Transcript(String),
    /// The turn's speaker was resolved (identified, probable, new, or unknown).
    Speaker(SpeakerContext),
    /// One reply-token fragment from the LLM (or a command confirmation).
    ReplyToken(String),
    /// The complete reply text.
    Reply(String),
    /// A memory entry was stored this turn.
    MemoryStored(String),
    /// TTS audio is now streaming back to the device.
    Speaking,
    /// The turn completed.
    Finished,
}

/// The result of driving one turn: the turn ran to its end with the socket still
/// usable, or the device disconnected during it (so the connection handler stops
/// instead of reading from a closed socket).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    Completed,
    Disconnected,
}

/// Acquires downstream Wyoming connections. Abstracted so tests can substitute
/// in-process mock STT/TTS servers for real TCP dials.
#[async_trait]
pub trait ServiceConnector: Send + Sync {
    async fn connect_stt(&self) -> Result<DynConnection>;
    async fn connect_tts(&self) -> Result<DynConnection>;
}

/// Production connector: dials the configured Whisper and Piper TCP endpoints (each
/// dial bounded by [`crate::wyoming::CONNECT_TIMEOUT`]).
pub struct TcpConnector {
    pub stt_addr: std::net::SocketAddr,
    pub tts_addr: std::net::SocketAddr,
}

#[async_trait]
impl ServiceConnector for TcpConnector {
    async fn connect_stt(&self) -> Result<DynConnection> {
        DynConnection::connect_tcp(self.stt_addr)
            .await
            .context("connecting to STT (Whisper) service")
    }
    async fn connect_tts(&self) -> Result<DynConnection> {
        DynConnection::connect_tcp(self.tts_addr)
            .await
            .context("connecting to TTS (Piper) service")
    }
}

/// What the reply phase of a turn produced.
struct Reply {
    text: String,
    memories_written: Vec<WrittenMemory>,
    delivery: Delivery,
}

/// The shared, cheaply-cloneable pipeline state. One instance is reused across
/// every connection.
#[derive(Clone)]
pub struct Pipeline {
    settings: Arc<SharedSettings>,
    memory: Arc<MemoryStore>,
    /// Retrieval backend for prompt context (SQLite FTS by default; HelixDB
    /// GraphRAG when configured). Explicit/inferred writes still go to `memory`.
    recall: Arc<dyn Recall>,
    /// Optional append-only chat log; each completed turn is recorded for the
    /// background GraphRAG ingester. `None` disables logging.
    chatlog: Option<Arc<ChatLog>>,
    /// Optional append-only prompt log; the exact assembled LLM prompt per turn,
    /// for the debug GUI. `None` disables prompt logging.
    promptlog: Option<Arc<PromptLog>>,
    /// Optional per-person speaker identification (speaker_id_plan.md). `None` makes
    /// every turn's speaker [`crate::speaker::UNKNOWN_SPEAKER`]: nothing personal is
    /// recalled, and personal facts are kept unattributed.
    speaker: Option<Arc<SpeakerService>>,
    /// The last confidently identified speaker and when (unix secs), so a follow-up
    /// turn too short to identify on its own can inherit them as *tentative*
    /// (SpeakerScopedMemoryPlan.md §4.2). Shared across clones (one per Core).
    last_speaker: Arc<std::sync::Mutex<Option<(SpeakerContext, i64)>>>,
    /// Debug-only per-turn audio capture directory. `None` disables capture
    /// entirely; set from the config file's `audio_dump_dir`.
    audio_dump_dir: Option<PathBuf>,
    system_prompt: String,
    turn_timeout: Duration,
    /// Inactivity budget for the LLM reply stream ([`LLM_STREAM_IDLE_TIMEOUT`]).
    llm_idle_timeout: Duration,
    /// Budget for opening the LLM request + its first token ([`LLM_FIRST_TOKEN_TIMEOUT`]).
    llm_first_token_timeout: Duration,
    /// Auto follow-up listening: when a reply is a question, tell the device to
    /// reopen the mic (no wake word) and feed recent history into that turn's prompt.
    /// Defaults to [`FollowUpConfig::default`] (enabled) until `with_follow_up` sets it.
    follow_up: FollowUpConfig,
    /// Forecast provider for the System-1 `weather` fast path, shared with the ambient
    /// push. `None` when weather is disabled — the weather intent then defers to
    /// System-2. Set via `with_weather`.
    weather: Option<Arc<dyn crate::weather::WeatherProvider>>,
    /// Pre-loaded in-process whisper.cpp STT engine. Loaded once at boot via
    /// [`with_whisper_engine`](Self::with_whisper_engine) whenever the
    /// `stt-whisper-local` feature is compiled and the model file is present —
    /// **regardless of the boot STT selection**, so a config-page swap to `whisper-rs`
    /// takes effect live (mirrors the `silero` model-preload pattern). `None` when the
    /// feature is off or the model is missing. The live engine selection is read
    /// per-turn from the settings snapshot (`runtime.stt.engine`) in
    /// [`build_stt_transcriber`](Self::build_stt_transcriber).
    whisper_engine: Option<Arc<dyn SttEngine>>,
    /// Shared, pre-loaded Silero VAD model (feature `vad-silero`). `Some` selects the
    /// neural gate for every turn; `None` keeps the energy gate. Loaded once at boot
    /// (`with_silero`) and cloned into each per-turn [`crate::vad::SileroGate`].
    #[cfg(feature = "vad-silero")]
    silero: Option<Arc<crate::vad::SileroModel>>,
}

impl Pipeline {
    /// Build a pipeline around the runtime-swappable [`SharedSettings`] (Phase 6):
    /// the LLM backend and TTS voice are read from a per-turn snapshot, so the
    /// device settings screen can change them between turns without a restart.
    pub fn with_settings(
        settings: Arc<SharedSettings>,
        memory: Arc<MemoryStore>,
        system_prompt: impl Into<String>,
        turn_timeout: Duration,
    ) -> Self {
        let recall: Arc<dyn Recall> = Arc::new(SqliteRecall::new(memory.clone()));
        Self {
            settings,
            memory,
            recall,
            chatlog: None,
            promptlog: None,
            speaker: None,
            last_speaker: Arc::new(std::sync::Mutex::new(None)),
            audio_dump_dir: None,
            system_prompt: system_prompt.into(),
            turn_timeout,
            llm_idle_timeout: LLM_STREAM_IDLE_TIMEOUT,
            llm_first_token_timeout: LLM_FIRST_TOKEN_TIMEOUT,
            follow_up: FollowUpConfig::default(),
            weather: None,
            whisper_engine: None,
            #[cfg(feature = "vad-silero")]
            silero: None,
        }
    }

    /// Select the neural Silero VAD engine, using a model loaded once at boot. When
    /// set, every turn's end-of-speech decision runs through Silero instead of the
    /// energy gate. Only available with the `vad-silero` feature. See
    /// `plans/VadSileroPlan.md`.
    #[cfg(feature = "vad-silero")]
    pub fn with_silero(mut self, model: Arc<crate::vad::SileroModel>) -> Self {
        self.silero = Some(model);
        self
    }

    /// Set the debug-only per-turn audio capture directory. `None` leaves capture
    /// disabled. Sourced from the config file's `audio_dump_dir`.
    pub fn with_audio_dump(mut self, dir: Option<PathBuf>) -> Self {
        self.audio_dump_dir = dir;
        self
    }

    /// Swap the retrieval backend used to build prompt context (e.g. the HelixDB
    /// GraphRAG backend). Defaults to SQLite FTS.
    pub fn with_recall(mut self, recall: Arc<dyn Recall>) -> Self {
        self.recall = recall;
        self
    }

    /// Attach an append-only chat log; each completed turn is recorded for the
    /// background GraphRAG ingester.
    pub fn with_chatlog(mut self, chatlog: Arc<ChatLog>) -> Self {
        self.chatlog = Some(chatlog);
        self
    }

    /// Attach an append-only prompt log; the exact assembled LLM prompt for each
    /// turn is recorded for the debug GUI (`webconfig.rs` → `/prompts`).
    pub fn with_promptlog(mut self, promptlog: Arc<PromptLog>) -> Self {
        self.promptlog = Some(promptlog);
        self
    }

    /// Enable per-person speaker identification. Without this every turn's speaker is
    /// unknown.
    pub fn with_speaker(mut self, speaker: Arc<SpeakerService>) -> Self {
        self.speaker = Some(speaker);
        self
    }

    /// Configure auto follow-up listening (question → reopen mic + history). Without
    /// this the pipeline uses [`FollowUpConfig::default`] (enabled).
    pub fn with_follow_up(mut self, follow_up: FollowUpConfig) -> Self {
        self.follow_up = follow_up;
        self
    }

    /// Override the LLM reply-stream inactivity budget ([`LLM_STREAM_IDLE_TIMEOUT`] by
    /// default).
    pub fn with_llm_idle_timeout(mut self, timeout: Duration) -> Self {
        // One knob for tests/embedders: it bounds the whole stream, start included.
        self.llm_idle_timeout = timeout;
        self.llm_first_token_timeout = timeout;
        self
    }

    /// Install a System-1 fast-decision engine (plans/system1-fast-decisions.md) into the
    /// live settings, so the per-turn snapshot picks it up. The boot path builds the
    /// engine from config in [`SharedSettings`]; this is a convenience for tests and
    /// callers holding an already-built engine. The config page swaps it at runtime via
    /// [`SharedSettings::apply_system1`].
    pub fn with_system1(self, system1: Arc<dyn crate::system1::DecisionEngine>) -> Self {
        self.settings.set_system1(system1);
        self
    }

    /// Provide the forecast provider used by the System-1 `weather` fast path (shared
    /// with the ambient push). Without it, the weather intent defers to System-2.
    pub fn with_weather(
        mut self,
        weather: Option<Arc<dyn crate::weather::WeatherProvider>>,
    ) -> Self {
        self.weather = weather;
        self
    }

    /// Attach the pre-loaded in-process whisper.cpp engine. Loaded once at boot
    /// (whenever the feature + model are available, regardless of the boot selection),
    /// it is used for any turn whose live STT selection is `whisper-rs`. See the
    /// `whisper_engine` field doc.
    pub fn with_whisper_engine(mut self, engine: Arc<dyn SttEngine>) -> Self {
        self.whisper_engine = Some(engine);
        self
    }

    /// The speaker service, if enabled (the Phase-C control handler lists/renames
    /// its registry).
    pub fn speaker(&self) -> Option<&Arc<SpeakerService>> {
        self.speaker.as_ref()
    }

    /// Build a pipeline around a fixed LLM backend + voice (the Phase-4 behavior).
    /// The backend cannot be swapped at runtime (its settings holder has no
    /// credentials); the TTS voice can still be changed via a control frame.
    pub fn new(
        llm: Arc<dyn LlmBackend>,
        memory: Arc<MemoryStore>,
        system_prompt: impl Into<String>,
        tts_voice: Option<String>,
        turn_timeout: Duration,
    ) -> Self {
        let settings = SharedSettings::fixed(llm, "custom", tts_voice);
        Self::with_settings(settings, memory, system_prompt, turn_timeout)
    }

    /// The persistent memory store (the Phase-6 control handler lists/deletes it).
    pub fn memory(&self) -> &Arc<MemoryStore> {
        &self.memory
    }

    /// The runtime-swappable settings (the Phase-6 control handler reads/updates it).
    pub fn settings(&self) -> &Arc<SharedSettings> {
        &self.settings
    }

    /// Drive a turn whose opening `audio-start` has already been read and parsed into
    /// `start` (the server reads it to tell a turn from a control frame — see
    /// [`TurnStart::from_event`]). Returns [`TurnOutcome::Disconnected`] when the device
    /// closed the socket during the turn, else [`TurnOutcome::Completed`]. Returns
    /// `Err` only on an unrecoverable pipeline failure (STT unreachable, LLM error, …);
    /// after the transcript the device is first sent a closing `audio-stop` so it
    /// leaves SPEAKING. A stall (a [`TimedOut`]) in the reply ends the turn cleanly
    /// instead.
    pub async fn run_turn_after_start(
        &self,
        device: &mut DynConnection,
        connector: &dyn ServiceConnector,
        start: TurnStart,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<TurnOutcome> {
        let no_speech_finalize = start.no_speech_window();
        let TurnStart {
            format,
            followup_depth,
            device_ctx,
            ..
        } = start;
        // One settings snapshot for the whole turn so a concurrent control swap never
        // changes the backend/voice mid-reply.
        let mut ctx = TurnCtx {
            runtime: self.settings.snapshot(),
            device,
            connector,
            device_ctx,
            followup_depth,
            on_event,
            // Debug-only capture (no-op unless `audio_dump_dir` is configured): this
            // turn's device mic (near-end) and Piper TTS (far-end reference).
            dump: TurnAudioDump::for_turn(self.audio_dump_dir.as_deref()),
            timing: crate::memory::TurnTiming::default(),
            turn_id: turn::TURN_IDS.mint("t-"),
        };

        // 1. Open the STT stream and pump device PCM into it until the transcript. The
        //    engine is chosen live from the snapshot (`runtime.stt.engine`), and the
        //    per-chunk speech decision runs behind the `SpeechGate` seam, also built
        //    from the snapshot.
        let mut stt = self
            .build_stt_transcriber(&ctx.runtime, format, connector)
            .await?;
        (ctx.on_event)(TurnEvent::Streaming);
        let mut gate = self.build_speech_gate(&ctx.runtime);
        gate.reset();
        let listened = self
            .stream_to_transcript(
                &mut ctx,
                stt.as_mut(),
                gate.as_mut(),
                no_speech_finalize,
                format.rate,
            )
            .await?;
        let (transcript, voiced_pcm, stt_latency) = match listened {
            Listen::Heard {
                transcript,
                voiced_pcm,
                stt_latency,
            } => (transcript, voiced_pcm, stt_latency),
            Listen::DeviceClosed => {
                let _ = stt.finish().await;
                return Ok(TurnOutcome::Disconnected);
            }
            // Idle past `turn_timeout` before a transcript — abandon the turn.
            Listen::IdleTimeout => {
                let _ = stt.finish().await;
                return Ok(TurnOutcome::Completed);
            }
        };
        // Per-turn latency breakdown (persisted with the chat-log record, surfaced in
        // the `/chatlog` UI). Anchored at end-of-speech: `stt_latency` is that instant
        // → transcript; the processing timer below measures transcript → reply.
        ctx.timing.stt_ms = Some(stt_latency.as_millis() as u64);
        let processing_start = Instant::now();
        let _ = stt.finish().await; // idempotent finalize (VAD already sent audio-stop)
        if let Some(d) = ctx.dump.as_ref() {
            d.set_transcript(&transcript);
        }
        (ctx.on_event)(TurnEvent::Transcript(transcript.clone()));
        // NOTE: the transcript was already relayed to the device inside
        // `stream_to_transcript`, the instant STT returned it.

        if transcript.trim().is_empty() {
            // No speech within the listen window: sleep. Send an `audio-stop` so the
            // device leaves SPEAKING and returns to idle immediately. We do NOT send
            // `anamanti-listen`, so the follow-up chain ends here — silence is the
            // terminator.
            ctx.device.send(&WyomingEvent::audio_stop(0)).await.ok();
            (ctx.on_event)(TurnEvent::Finished);
            return Ok(TurnOutcome::Completed);
        }

        // Diagnostics screen: a transcription-only context. The words are on screen, but
        // this is a DIAGNOSTIC surface, so the audio must NOT be processed any further:
        // no speaker ID, no System-1, no LLM, no TTS, and no memory/chat-log.
        if matches!(
            ctx.device_ctx.widget,
            Some(protocol::DisplayContext::Diagnostics)
        ) {
            ctx.device.send(&WyomingEvent::audio_stop(0)).await.ok();
            (ctx.on_event)(TurnEvent::Finished);
            return Ok(TurnOutcome::Completed);
        }

        // Enrollment screen: a suppressing context like diagnostics. The captured voiced
        // PCM is a *training sample* (or a verify check) for a voiceprint, NOT a turn —
        // route it to the enrollment accumulator and answer with `anamanti-enroll-result`.
        if let Some(protocol::DisplayContext::Enroll(screen)) = &ctx.device_ctx.widget {
            self.handle_enrollment(screen, &voiced_pcm, ctx.device)
                .await;
            (ctx.on_event)(TurnEvent::Finished);
            return Ok(TurnOutcome::Completed);
        }

        // 2. Identify who is speaking (and how sure we are), so memory writes/recall and
        //    the reply are per-person. A follow-up turn too short to identify may
        //    inherit the previous turn's identified speaker as tentative.
        let speaker = self.identify_speaker(&voiced_pcm).await;
        let speaker = self.with_follow_up_continuity(speaker, ctx.followup_depth);
        log::info!(
            "speaker: {} {} ({:.2}{}{}{})",
            speaker.speaker_id,
            speaker.resolution.as_str(),
            speaker.confidence,
            speaker
                .margin
                .map(|m| format!(", lead {m:+.2}"))
                .unwrap_or_default(),
            if speaker.short_clip {
                ", short clip"
            } else {
                ""
            },
            if speaker.inherited { ", inherited" } else { "" },
        );
        (ctx.on_event)(TurnEvent::Speaker(speaker.clone()));

        // 3 + 4. Fast paths, or memory + LLM → reply, spoken sentence by sentence.
        let reply = match self
            .respond_and_speak(&mut ctx, &transcript, &speaker)
            .await
        {
            Ok(reply) => reply,
            Err(e) => {
                // The device flipped to SPEAKING on the transcript: close the turn on
                // its side before the error ends the connection.
                ctx.device.send(&WyomingEvent::audio_stop(0)).await.ok();
                return Err(e);
            }
        };
        if let Some(d) = ctx.dump.as_ref() {
            d.set_reply(&reply.text);
        }
        (ctx.on_event)(TurnEvent::Reply(reply.text.clone()));

        // Whole server-side turn: end-of-speech → reply ready (STT finalize + processing).
        ctx.timing.total_ms =
            Some(stt_latency.as_millis() as u64 + processing_start.elapsed().as_millis() as u64);

        // Record the completed turn for the background GraphRAG ingester. Never let a
        // logging failure break the turn.
        let timing = std::mem::take(&mut ctx.timing);
        self.log_turn(
            &ctx.turn_id,
            &ctx.runtime,
            &speaker,
            &transcript,
            &reply.text,
            reply.memories_written,
            timing,
        );

        (ctx.on_event)(TurnEvent::Finished);
        Ok(match reply.delivery {
            Delivery::DeviceClosed => TurnOutcome::Disconnected,
            Delivery::Completed | Delivery::Interrupted => TurnOutcome::Completed,
        })
    }

    /// Build the per-turn end-of-speech VAD gate from the live settings snapshot.
    /// The detector sits behind the [`SpeechGate`](crate::vad::SpeechGate) seam so it
    /// can be swapped without touching the pump loop's state machine. The engine is
    /// chosen live from `runtime.vad_engine` (config-page/device swap, no restart); a
    /// swap to Silero when no model is loaded (feature off or model absent) falls back
    /// to the energy gate with a warning. See `plans/VadSileroPlan.md`.
    fn build_speech_gate(&self, runtime: &RuntimeSettings) -> Box<dyn SpeechGate> {
        #[cfg(feature = "vad-silero")]
        if matches!(runtime.vad_engine, crate::config::VadEngineKind::Silero) {
            match &self.silero {
                Some(model) => {
                    return Box::new(crate::vad::SileroGate::new(
                        model.clone(),
                        runtime.silero_threshold,
                    ));
                }
                None => log::warn!(
                    "vad.engine=silero but no Silero model is loaded; using the energy VAD"
                ),
            }
        }
        #[cfg(not(feature = "vad-silero"))]
        if matches!(runtime.vad_engine, crate::config::VadEngineKind::Silero) {
            log::warn!(
                "vad.engine=silero but this binary was built without the `vad-silero` \
                 feature; using the energy VAD"
            );
        }
        Box::new(crate::vad::EnergyGate::new(runtime.voice_rms_threshold))
    }

    /// Build this turn's [`Transcriber`] from the live STT selection
    /// (`runtime.stt.engine`), so a config-page swap takes effect between turns with no
    /// restart. `wyoming` dials the downstream Whisper server via the connector (the
    /// historical default); `whisper-rs` uses the boot-loaded in-process engine;
    /// `elevenlabs` opens a fresh realtime WebSocket from the snapshot's connection
    /// config + key. An unavailable selection (whisper-rs with no engine loaded, or
    /// elevenlabs with no key) warns and falls back to the Wyoming path — mirroring the
    /// VAD gate's graceful fallback. See `plans/ElevenLabsSttPlan.md`.
    async fn build_stt_transcriber(
        &self,
        runtime: &RuntimeSettings,
        format: AudioFormat,
        connector: &dyn ServiceConnector,
    ) -> Result<Box<dyn Transcriber>> {
        use crate::config::SttEngineKind;
        match runtime.stt.engine {
            SttEngineKind::WhisperLocal => {
                if let Some(engine) = self.whisper_engine.as_ref() {
                    return engine.begin(format).await;
                }
                log::warn!(
                    "stt.engine=whisper-rs but no in-process engine is loaded (feature off \
                     or model missing); falling back to the Wyoming STT path"
                );
            }
            SttEngineKind::ElevenLabs => {
                match crate::stt::ElevenLabsEngine::from_settings(&runtime.stt) {
                    Some(engine) => return engine.begin(format).await,
                    None => log::warn!(
                        "stt.engine=elevenlabs but no ELEVENLABS_API_KEY is set; falling back \
                         to the Wyoming STT path"
                    ),
                }
            }
            SttEngineKind::Wyoming => {}
        }
        // Wyoming (the default), or a graceful fallback from an unavailable selection.
        let stt_conn = connector.connect_stt().await?;
        Ok(Box::new(WyomingTranscriber::begin(stt_conn, format).await?))
    }

    /// Answer the transcript: a memory command or a System-1 intent on the fast path,
    /// otherwise apply memory policy, stream the LLM reply, and synthesize it with
    /// Piper as complete sentences arrive so playback begins before generation
    /// finishes.
    ///
    /// Each token is relayed to the device as a `reply-token` (for token-by-token
    /// on-screen rendering) and appended to a pending buffer; whenever that buffer
    /// holds a complete sentence it is flushed to TTS immediately, so
    /// time-to-first-audio is first-sentence latency, not full-reply latency. Returns
    /// the reply text, the memory entries written this turn, and how delivery ended.
    async fn respond_and_speak(
        &self,
        ctx: &mut TurnCtx<'_>,
        transcript: &str,
        speaker: &SpeakerContext,
    ) -> Result<Reply> {
        // Memory entries written this turn, with their scopes (for the chat log /
        // ingester).
        let mut memories_written: Vec<WrittenMemory> = Vec::new();

        // Explicit command → apply, confirm, and speak the confirmation in one chunk,
        // skipping the LLM. Like every fast reply it closes on a bare `audio-stop` (no
        // follow-up listen) and is barge-in-interruptible.
        if let Some(cmd) = parse_command(transcript) {
            ctx.timing.path = Some("command".to_string());
            let text = self.apply_command(
                cmd,
                speaker,
                &ctx.runtime,
                &ctx.turn_id,
                &mut memories_written,
                ctx.on_event,
            )?;
            let reply = FastReply::say(text).ending_session();
            let delivery = self.speak_fast_reply(ctx, &reply).await;
            return Ok(Reply {
                text: reply.text,
                memories_written,
                delivery,
            });
        }

        // System-1 fast-decision fork (plans/system1-fast-decisions.md). Runs *before*
        // memory recall + the LLM; skipped entirely for the default `none` engine. On a
        // confident Resolve with a matching handler the turn is answered there.
        if let Some((text, delivery)) = self.run_system1(ctx, transcript, speaker).await {
            return Ok(Reply {
                text,
                memories_written,
                delivery,
            });
        }

        // ---- System-2 path (memory recall + full LLM completion) ----
        let sys2_start = Instant::now();
        ctx.timing.path = Some("system2".to_string());
        log::info!(
            "system2 ({}/{}) call: memory recall + LLM completion for {transcript:?}",
            ctx.runtime.llm_backend,
            ctx.runtime.llm_model.as_deref().unwrap_or("?"),
        );

        // Inferred capture from an ordinary turn — stored for whoever it is true for:
        // this person only when confidently identified (SpeakerScopedMemoryPlan.md §2).
        for (kind, content) in infer_memories(transcript) {
            let written = self.store_memory(
                kind,
                &content,
                MemorySource::Inferred,
                speaker,
                &ctx.turn_id,
            )?;
            memories_written.push(written);
            (ctx.on_event)(TurnEvent::MemoryStored(content));
        }

        // Build per-person memory context: the speaker's own memories (when recognized)
        // plus household ones — never the unknown speaker's. A recall failure (e.g. a
        // transient GraphRAG backend error) must not sink the turn.
        let recall_start = Instant::now();
        let recalled = self
            .build_context(transcript, speaker.reader())
            .await
            .unwrap_or_else(|e| {
                log::warn!("memory recall failed; answering without context: {e:#}");
                crate::memory::RecallResult::default()
            });
        let recall_ms = recall_start.elapsed().as_millis() as u64;
        let memory = prompt::RecalledMemory::split(&recalled.hits, &recalled.household);
        let context_chars = memory.chars();
        ctx.timing.recall_ms = Some(recall_ms);
        // Record which backend actually answered (the boot fallback is silent, so this
        // is the only per-turn proof that a helix-configured instance ran GraphRAG vs
        // fell back to FTS) and whether it paid the embedding round-trip, with the
        // embed-vs-search split when Helix runs.
        let recall_backend = self.recall.backend();
        ctx.timing.recall_backend = Some(recall_backend.to_string());
        ctx.timing.recall_embedded = Some(self.recall.embedder_label().is_some());
        ctx.timing.recall_embedder = self.recall.embedder_label().map(str::to_string);
        ctx.timing.recall_embed_ms = recalled.embed_ms;
        ctx.timing.recall_search_ms = recalled.search_ms;
        log::info!(
            "system2: memory recall in {recall_ms}ms via {recall_backend} \
             (embed {:?}ms, search {:?}ms, {} ctx chars)",
            recalled.embed_ms,
            recalled.search_ms,
            context_chars,
        );

        // A follow-up turn (the device auto-opened the mic after a reply) is fed the
        // recent conversation so the answer has context — every speaker's turns, each
        // labelled with who said it (decision D3); an ordinary single-shot turn carries
        // no history (locked "single-turn v1" decision). See plans/Plan.MD.
        let prompt_start = Instant::now();
        let history = if ctx.followup_depth > 0 {
            self.recent_history(speaker)
        } else {
            Vec::new()
        };

        // Prompt assembly (system prompt + grounding + recalled context), all from the
        // turn snapshot.
        let system_prompt = prompt::build_system_prompt(
            &self.system_prompt,
            speaker,
            &ctx.runtime,
            &ctx.device_ctx,
            &memory,
            !history.is_empty(),
        );
        // Record the exact prompt the model is about to see (debug GUI). Best-effort.
        self.log_prompt(
            &ctx.turn_id,
            &ctx.runtime,
            speaker,
            &system_prompt,
            transcript,
        );

        let prompt_ms = prompt_start.elapsed().as_millis() as u64;
        ctx.timing.prompt_ms = Some(prompt_ms);
        log::info!(
            "system2: prompt build in {prompt_ms}ms ({} prompt chars, {} history msgs)",
            system_prompt.len(),
            history.len(),
        );

        // Personality-switch voice intent: a deterministic fast path handled before the
        // LLM. A recognized "talk like a gangster" / "go back to normal" utterance flips
        // the live output personality in place (the same `set_active_personality` the
        // `set_personality` tool uses) and speaks a canned confirmation as a one-item
        // reply stream, skipping the model. The new style applies from the next turn.
        let personality_confirm = detect_personality_switch(transcript, &ctx.runtime.personality)
            .map(|target| {
                log::info!("personality switch intent: → {target}");
                match self.settings.set_active_personality(&target) {
                    Ok(Some(label)) => format!("Okay, I'll talk in my {label} voice from now on."),
                    Ok(None) => "Okay, back to my normal voice.".to_string(),
                    Err(_) => "Sorry, I don't have a personality by that name.".to_string(),
                }
            });

        // Per-turn device-action channel: action tools (timers, recipe, weather, …)
        // push `DeviceAction`s here and the drive loop relays them to the device on the
        // same socket. Unbounded so a tool's `invoke` never blocks.
        let (action_tx, mut action_rx) = tokio::sync::mpsc::unbounded_channel::<DeviceAction>();

        // Timing (surfaced in `/chatlog`): LLM + TTS interleave in the streaming path, so
        // we capture two anchors — `llm_ms` = time to the first reply token, `tts_ms` =
        // the first TTS chunk's synthesis.
        let gen_start = Instant::now();
        let mut first_token_ms: Option<u64> = None;
        let mut first_tts_ms: Option<u64> = None;
        let mut reply = String::new();
        let mut pending = String::new();
        // Whether the single, coalesced device-facing `audio-start` has been sent.
        // Piper emits an `audio-start`/`audio-stop` per sentence, but the device ends
        // its turn on the *first* `audio-stop` — so we forward exactly one
        // `audio-start` up front, relay only the chunks, and send one `audio-stop` at
        // the very end.
        let mut audio_started = false;

        let outcome = {
            // Split the device socket so we can *concurrently* stream reply tokens + TTS
            // audio out on the writer while watching the reader for a barge-in.
            let (reader, writer) = ctx.device.split_mut();
            let on_event = &mut *ctx.on_event;
            let runtime = &ctx.runtime;
            let connector = ctx.connector;
            let dump = ctx.dump.as_ref();
            let followup_depth = ctx.followup_depth;
            let idle = self.llm_idle_timeout;
            // The first token gets the longer start budget (cold model + tool rounds).
            let first_budget = self.llm_first_token_timeout;
            let first_deadline = tokio::time::Instant::now() + first_budget;

            // The generation + speaking driver. Dropping it (when a barge-in wins the
            // race) drops the LLM stream and any in-flight `TtsSession`, cancelling the
            // upstream request and Piper synthesis.
            let drive = async {
                let mut stream: ReplyStream = match personality_confirm {
                    Some(confirm) => {
                        Box::pin(futures_util::stream::once(async move { Ok(confirm) }))
                    }
                    None => {
                        let turn = LlmTurn::new(system_prompt, transcript)
                            .with_actions(action_tx)
                            .with_history(history);
                        tokio::time::timeout_at(first_deadline, runtime.llm.respond(turn))
                            .await
                            .map_err(|_| TimedOut::new("LLM request", first_budget))?
                            .with_context(|| {
                                format!("LLM backend `{}` failed", runtime.llm.name())
                            })?
                    }
                };
                let mut speaking = false;
                loop {
                    // Until the first token arrives, the start deadline (shared with the
                    // request open) applies; after that, the per-token idle budget.
                    let next = if first_token_ms.is_none() {
                        tokio::time::timeout_at(first_deadline, stream.next())
                            .await
                            .map_err(|_| TimedOut::new("LLM first token", first_budget))?
                    } else {
                        tokio::time::timeout(idle, stream.next())
                            .await
                            .map_err(|_| TimedOut::new("LLM token stream", idle))?
                    };
                    let Some(tok) = next else { break };
                    // Relay any device actions a tool emitted (e.g. a timer) before
                    // rendering more of the reply.
                    drain_device_actions(&mut action_rx, writer, &self.settings).await;
                    let tok = tok?;
                    if first_token_ms.is_none() {
                        first_token_ms = Some(gen_start.elapsed().as_millis() as u64);
                    }
                    reply.push_str(&tok);
                    pending.push_str(&tok);
                    protocol::write_event(writer, &WyomingEvent::reply_token(&tok))
                        .await
                        .ok();
                    on_event(TurnEvent::ReplyToken(tok));

                    // Flush every complete sentence that has formed so far.
                    while let Some(sentence) = take_speakable(&mut pending, false) {
                        if !speaking {
                            on_event(TurnEvent::Speaking);
                            speaking = true;
                        }
                        let tts_t0 = Instant::now();
                        let spoke = self
                            .speak_chunk(
                                writer,
                                runtime,
                                connector,
                                &sentence,
                                &mut audio_started,
                                dump,
                            )
                            .await?;
                        first_tts_ms.get_or_insert(tts_t0.elapsed().as_millis() as u64);
                        if !spoke {
                            // Device closed mid-relay; stop generating.
                            return Ok::<Delivery, anyhow::Error>(Delivery::DeviceClosed);
                        }
                    }
                }
                // Relay any device actions emitted late in the reply (e.g. a tool call on
                // the final round) before closing the turn.
                drain_device_actions(&mut action_rx, writer, &self.settings).await;
                // Speak any trailing clause left without terminal punctuation.
                if let Some(rest) = take_speakable(&mut pending, true) {
                    if !speaking {
                        on_event(TurnEvent::Speaking);
                    }
                    let tts_t0 = Instant::now();
                    let spoke = self
                        .speak_chunk(writer, runtime, connector, &rest, &mut audio_started, dump)
                        .await?;
                    first_tts_ms.get_or_insert(tts_t0.elapsed().as_millis() as u64);
                    if !spoke {
                        return Ok(Delivery::DeviceClosed);
                    }
                }
                // Follow-up listen (after EVERY reply) + the final `audio-stop`. Reaching
                // here means the reply completed (a barge-in would have dropped this
                // whole future), so we never reopen over an interruption.
                emit_follow_up_and_stop(
                    writer,
                    &self.follow_up,
                    audio_started,
                    &reply,
                    followup_depth,
                )
                .await;
                Ok(Delivery::Completed)
            };
            with_barge_in(reader, drive).await
        };

        let delivery = match outcome {
            Ok(Ok(delivery)) => delivery,
            Ok(Err(e)) if TimedOut::is_in(&e) => {
                // A stall: keep what was already said, close the device's turn with the
                // usual frame, and end the turn normally.
                log::warn!("system2: {e:#}; ending the turn");
                ctx.device.send(&WyomingEvent::audio_stop(0)).await.ok();
                Delivery::Completed
            }
            Ok(Err(e)) => return Err(e),
            Err(BargeIn::Interrupt) => {
                log::info!("barge-in: aborting in-flight LLM generation + TTS for this turn");
                Delivery::Interrupted
            }
            Err(BargeIn::Closed) => Delivery::DeviceClosed,
        };
        ctx.timing.llm_ms = first_token_ms;
        ctx.timing.tts_ms = first_tts_ms;

        let reply = reply.trim().to_string();
        log::info!(
            "system2 complete in {}ms ({} chars{})",
            sys2_start.elapsed().as_millis(),
            reply.len(),
            match delivery {
                Delivery::Interrupted => ", barged-in",
                Delivery::DeviceClosed => ", device closed",
                Delivery::Completed => "",
            },
        );
        Ok(Reply {
            text: reply,
            memories_written,
            delivery,
        })
    }

    /// Append the completed turn to the chat log, if one is attached. A failure is
    /// logged and swallowed — logging must never break a turn.
    #[allow(clippy::too_many_arguments)]
    fn log_turn(
        &self,
        turn_id: &str,
        runtime: &RuntimeSettings,
        speaker: &SpeakerContext,
        transcript: &str,
        reply: &str,
        memories_written: Vec<WrittenMemory>,
        timing: crate::memory::TurnTiming,
    ) {
        let Some(log) = &self.chatlog else {
            return;
        };
        let id = log.next_id();
        let record = ChatLogRecord {
            session_id: format!("s-{id}"),
            id,
            turn_id: Some(turn_id.to_string()),
            ts: now_secs(),
            transcript: transcript.to_string(),
            reply: reply.to_string(),
            memories_written: memories_written.iter().map(|m| m.content.clone()).collect(),
            memories: memories_written,
            llm_backend: runtime.llm_backend.clone(),
            model: runtime.llm_model.clone(),
            speaker_id: speaker.speaker_id.clone(),
            speaker_name: speaker.name.clone(),
            speaker_resolution: Some(speaker.resolution.as_str().to_string()),
            speaker_confidence: Some(speaker.confidence),
            speaker_margin: speaker.margin,
            speaker_inherited: speaker.inherited,
            speaker_short_clip: speaker.short_clip,
            timing: Some(timing),
        };
        if let Err(e) = log.append(&record) {
            log::warn!("failed to append chat log record: {e:#}");
        }
    }

    /// Append the assembled LLM prompt to the prompt log, if one is attached. A
    /// failure is logged and swallowed — logging must never break a turn.
    fn log_prompt(
        &self,
        turn_id: &str,
        runtime: &RuntimeSettings,
        speaker: &SpeakerContext,
        system_prompt: &str,
        user_message: &str,
    ) {
        let Some(log) = &self.promptlog else {
            return;
        };
        let record = PromptLogRecord {
            id: log.next_id(),
            turn_id: Some(turn_id.to_string()),
            ts: now_secs(),
            llm_backend: runtime.llm_backend.clone(),
            model: runtime.llm_model.clone(),
            speaker_id: speaker.speaker_id.clone(),
            speaker_name: speaker.name.clone(),
            speaker_resolution: Some(speaker.resolution.as_str().to_string()),
            system_prompt: system_prompt.to_string(),
            user_message: user_message.to_string(),
        };
        if let Err(e) = log.append(&record) {
            log::warn!("failed to append prompt log record: {e:#}");
        }
    }

    /// Store one memory said in this turn, in the scope it is true for — household
    /// facts to the household; personal ones to the speaker only when confidently
    /// identified, else unattributed with the best guess as candidate (D1) — with this
    /// turn's provenance. Returns the scoped record for the chat log.
    fn store_memory(
        &self,
        kind: MemoryKind,
        content: &str,
        source: MemorySource,
        speaker: &SpeakerContext,
        turn_id: &str,
    ) -> Result<WrittenMemory> {
        let scope = speaker.write_scope(classify_scope(content));
        self.store_in(kind, content, source, scope, speaker, turn_id)
    }

    /// [`store_memory`](Self::store_memory) into an explicit `scope`.
    fn store_in(
        &self,
        kind: MemoryKind,
        content: &str,
        source: MemorySource,
        scope: MemoryScope,
        speaker: &SpeakerContext,
        turn_id: &str,
    ) -> Result<WrittenMemory> {
        self.memory
            .add_in(kind, content, source, &scope, &speaker.provenance(turn_id))?;
        Ok(WrittenMemory {
            content: content.trim().to_string(),
            scope: scope.kind().as_str().to_string(),
            speaker_id: scope.speaker_id().or(scope.candidate()).map(str::to_string),
        })
    }

    /// Apply an explicit memory command; returns the spoken confirmation. Anything it
    /// stores is appended to `written`.
    fn apply_command(
        &self,
        cmd: MemoryCommand,
        speaker: &SpeakerContext,
        runtime: &RuntimeSettings,
        turn_id: &str,
        written: &mut Vec<WrittenMemory>,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<String> {
        Ok(match cmd {
            MemoryCommand::Remember { kind, content } => {
                written.push(self.store_memory(
                    kind,
                    &content,
                    MemorySource::Explicit,
                    speaker,
                    turn_id,
                )?);
                on_event(TurnEvent::MemoryStored(content));
                "Okay, I'll remember that.".to_string()
            }
            MemoryCommand::ForgetMatching(query) => {
                // Only the asker's own memories — plus household ones when they
                // explicitly said "we"/"our"/"everyone".
                let household = classify_scope(&query) == FactScope::Household;
                let n = self
                    .memory
                    .forget_matching_for(&query, speaker.forgetter(), household)?;
                if n > 0 {
                    "Okay, I've forgotten that.".to_string()
                } else {
                    "I didn't have anything about that.".to_string()
                }
            }
            MemoryCommand::ForgetLast => match self.memory.forget_last_for(speaker.forgetter())? {
                Some(_) => "Okay, I've forgotten that.".to_string(),
                None => "There was nothing to forget.".to_string(),
            },
            MemoryCommand::WhoAmI => who_am_i_reply(speaker, &runtime.household),
            MemoryCommand::NameSpeaker(name) => {
                let reply =
                    self.name_speaker_by_voice(&name, speaker, runtime, turn_id, written)?;
                if let Some(w) = written.last() {
                    on_event(TurnEvent::MemoryStored(w.content.clone()));
                }
                reply
            }
        })
    }

    /// "My name is X" by voice (SpeakerScopedMemoryPlan.md §4.3, decision D7). Guided
    /// enrollment is how a voiceprint gets a trusted name; speech only names a
    /// **confidently identified, still-anonymous** voiceprint, and only with the
    /// household roster's spelling. Everything else is stored as an unattributed fact
    /// and answered with a pointer to enrollment. Never renames a named voiceprint and
    /// never takes a name another voiceprint already has.
    fn name_speaker_by_voice(
        &self,
        spoken: &str,
        speaker: &SpeakerContext,
        runtime: &RuntimeSettings,
        turn_id: &str,
        written: &mut Vec<WrittenMemory>,
    ) -> Result<String> {
        let canonical = runtime
            .household
            .member_matching_spoken(spoken)
            .map(|m| m.name.trim().to_string());
        let display = canonical.clone().unwrap_or_else(|| spoken.to_string());
        let fact = format!("The user's name is {display}");
        let unattributed = MemoryScope::Unknown {
            candidate: speaker.best_guess().map(str::to_string),
        };
        let enroll_hint = "On the display, go to Settings, People, Add a voice, and pick your \
                           name so I'll recognise you.";

        let identified = speaker.resolution == SpeakerResolution::Identified;
        if !identified {
            written.push(self.store_in(
                MemoryKind::Fact,
                &fact,
                MemorySource::Explicit,
                unattributed,
                speaker,
                turn_id,
            )?);
            return Ok(format!(
                "Nice to meet you, {display}. I don't know your voice yet. {enroll_hint}"
            ));
        }

        // A confidently identified voice that already has a name keeps it.
        if let Some(known) = speaker.name.as_deref().filter(|n| !n.trim().is_empty()) {
            let same = known.trim().eq_ignore_ascii_case(display.trim())
                || crate::household::spoken_name_distance(known, &display)
                    <= crate::household::spoken_name_tolerance(known);
            if same {
                return Ok(format!("Yes, I know. Hi, {}!", known.trim()));
            }
            return Ok(format!(
                "I know your voice as {}. If that's wrong, you can rename it under Settings, \
                 People on the display.",
                known.trim()
            ));
        }

        let Some(name) = canonical else {
            // Not in the household roster: keep the fact unattributed rather than
            // labelling a voiceprint with a transcript spelling.
            written.push(self.store_in(
                MemoryKind::Fact,
                &fact,
                MemorySource::Explicit,
                unattributed,
                speaker,
                turn_id,
            )?);
            return Ok(format!(
                "Nice to meet you, {display}. I couldn't find you in the household list. \
                 {enroll_hint}"
            ));
        };

        if let Some(svc) = &self.speaker {
            match svc.registry().find_named(&name)? {
                Some(other) if other.id != speaker.speaker_id => {
                    log::warn!(
                        "speaker {} said their name is {name}, which voiceprint {} already has \
                         — possible duplicate; consider merging them on the admin console",
                        speaker.speaker_id,
                        other.id
                    );
                    written.push(self.store_in(
                        MemoryKind::Fact,
                        &fact,
                        MemorySource::Explicit,
                        unattributed,
                        speaker,
                        turn_id,
                    )?);
                    return Ok(format!(
                        "I already know a {name} with a different voice, so I won't change \
                         anything yet. {name}, you can add more voice samples under Settings, \
                         People on the display."
                    ));
                }
                _ => {
                    if let Err(e) = svc.registry().name_from_voice(&speaker.speaker_id, &name) {
                        log::warn!("failed to name speaker {}: {e:#}", speaker.speaker_id);
                    }
                }
            }
        }
        written.push(self.store_in(
            MemoryKind::Fact,
            &format!("The user's name is {name}"),
            MemorySource::Explicit,
            MemoryScope::Person(speaker.speaker_id.clone()),
            speaker,
            turn_id,
        )?);
        Ok(format!("Nice to meet you, {name}!"))
    }

    /// Recall memory entries relevant to the transcript for this speaker (their own
    /// entries plus shared), via the configured recall backend (SQLite FTS by default;
    /// HelixDB GraphRAG when set). Returns the raw [`RecallResult`] — hits plus the
    /// optional embed/search timing split — so the caller can both format the context
    /// and record the per-stage latencies.
    ///
    /// [`RecallResult`]: crate::memory::RecallResult
    async fn build_context(
        &self,
        transcript: &str,
        speaker_id: Option<&str>,
    ) -> Result<crate::memory::RecallResult> {
        self.recall.recall(transcript, speaker_id, 8).await
    }

    /// The recent conversation, as `(user, assistant)` pairs oldest-first, for a
    /// **follow-up** turn's prompt: the last `follow_up.history_turns` chat-log turns
    /// that completed within `follow_up.history_window_secs`. Empty when no chat log is
    /// attached, the feature is off, or nothing recent qualifies. Recency-scoped rather
    /// than device-scoped (the chat log has no device id), so back-to-back conversations
    /// on one display thread correctly; two displays talking at once could interleave
    /// (an accepted v1 limitation — see plans/Plan.MD). A read failure yields no history
    /// rather than breaking the turn.
    ///
    /// Every speaker's turns are kept (decision D3), but each user message is
    /// **labelled** with who said it — `[you]` for the current speaker, the person's
    /// live name otherwise (`[unknown speaker]` when unattributed) — so the model never
    /// applies one person's "I am…" to another.
    fn recent_history(&self, current: &SpeakerContext) -> Vec<(String, String)> {
        let Some(log) = &self.chatlog else {
            return Vec::new();
        };
        if self.follow_up.history_turns == 0 {
            return Vec::new();
        }
        let cutoff = now_secs() - self.follow_up.history_window_secs;
        let recent = crate::memory::chatlog::read_tail(log.path(), self.follow_up.history_turns)
            .unwrap_or_else(|e| {
                log::warn!("follow-up: reading recent chat history failed: {e:#}");
                Vec::new()
            });
        // `read_tail` is newest-first: keep only in-window turns with real content, then
        // reverse to chronological (oldest-first) order for the replayed message list.
        recent
            .into_iter()
            .filter(|r| r.ts >= cutoff)
            .filter(|r| !r.transcript.trim().is_empty() && !r.reply.trim().is_empty())
            .map(|r| {
                let label = self.history_label(&r, current);
                (format!("[{label}]: {}", r.transcript.trim()), r.reply)
            })
            .rev()
            .collect()
    }

    /// Who said a logged turn, as a short label for the follow-up history.
    fn history_label(&self, r: &ChatLogRecord, current: &SpeakerContext) -> String {
        let id = r.speaker_id.as_str();
        let unattributed = id.is_empty() || id == UNKNOWN_SPEAKER || id == "household";
        if unattributed {
            return "unknown speaker".to_string();
        }
        if current.reader() == Some(id) {
            return "you".to_string();
        }
        // The live registry name (renames apply retroactively), else the logged one.
        let name = self
            .speaker
            .as_ref()
            .and_then(|svc| svc.registry().get(id).ok().flatten())
            .and_then(|p| p.name)
            .or_else(|| r.speaker_name.clone());
        let probable = matches!(r.speaker_resolution.as_deref(), Some("tentative"));
        match (name, probable) {
            (Some(n), false) => n,
            (Some(n), true) => format!("{n} (probably)"),
            (None, _) => "another household member".to_string(),
        }
    }

    /// Follow-up continuity (SpeakerScopedMemoryPlan.md §4.2): remember a confidently
    /// identified speaker, and let a follow-up turn that was **too short to identify**
    /// (no voiceprint comparison was possible) inherit them as *tentative* — it may read
    /// their memories but not write to them. Never upgrades a voice that was compared
    /// and did not match, never chains an inherited identity, and expires with the
    /// follow-up history window.
    fn with_follow_up_continuity(
        &self,
        speaker: SpeakerContext,
        followup_depth: u32,
    ) -> SpeakerContext {
        let now = now_secs();
        if speaker.resolution == SpeakerResolution::Identified && !speaker.inherited {
            *self.last_speaker.lock().unwrap() = Some((speaker.clone(), now));
            return speaker;
        }
        let uncompared = speaker.is_unknown() && speaker.confidence == 0.0;
        if followup_depth == 0 || !uncompared {
            return speaker;
        }
        let last = self.last_speaker.lock().unwrap().clone();
        match last {
            Some((prev, at)) if now - at <= self.follow_up.history_window_secs => {
                let mut inherited = prev;
                inherited.resolution = SpeakerResolution::Tentative;
                inherited.inherited = true;
                inherited.short_clip = speaker.short_clip;
                inherited.is_new = false;
                inherited
            }
            _ => speaker,
        }
    }

    /// Identify the turn's speaker via the [`SpeakerService`], degrading gracefully
    /// to the unknown speaker on any failure (or when identification is disabled).
    /// Runs on the blocking pool (feature extraction + embedding + SQLite + clip write).
    async fn identify_speaker(&self, voiced_pcm: &[i16]) -> SpeakerContext {
        let Some(svc) = &self.speaker else {
            return SpeakerContext::unknown();
        };
        match svc
            .identify_and_attribute_blocking(voiced_pcm.to_vec())
            .await
        {
            Ok(ctx) => ctx,
            Err(e) => {
                log::warn!("speaker identification failed; treating the speaker as unknown: {e:#}");
                SpeakerContext::unknown()
            }
        }
    }

    /// Handle one guided-enrollment capture (SpeakerEnrollmentPlan.md Phase 2): either
    /// accumulate a training sample or run the read-only verify check, then answer with
    /// an `anamanti-enroll-result` frame and `audio-stop` (ending the turn without any
    /// LLM/TTS). Never fails the connection — a problem is reported in the frame's `ok`.
    async fn handle_enrollment(
        &self,
        screen: &protocol::EnrollScreen,
        voiced_pcm: &[i16],
        device: &mut DynConnection,
    ) {
        let result = match &self.speaker {
            None => serde_json::json!({
                "ok": false,
                "phase": screen.phase,
                "session": screen.session,
                "message": "speaker identification is disabled on the orchestrator",
            }),
            Some(svc) if screen.phase == "verify" => match svc.identify_only(voiced_pcm) {
                Ok(Some((id, name, score))) => serde_json::json!({
                    "ok": true,
                    "phase": "verify",
                    "session": screen.session,
                    "speaker_id": id,
                    "name": name,
                    "confidence": score,
                    "message": format!(
                        "recognized {} ({score:.2})",
                        name.as_deref().unwrap_or("this voice")
                    ),
                }),
                Ok(None) => serde_json::json!({
                    "ok": true,
                    "phase": "verify",
                    "session": screen.session,
                    "confidence": 0.0,
                    "message": "didn't confidently recognize that voice — you can add more samples",
                }),
                Err(e) => serde_json::json!({
                    "ok": false, "phase": "verify", "session": screen.session,
                    "message": format!("verify failed: {e:#}"),
                }),
            },
            Some(svc) => match svc.enroll_sample(&screen.session, voiced_pcm) {
                Ok(out) => serde_json::json!({
                    "ok": true,
                    "phase": "sample",
                    "session": screen.session,
                    "accepted": out.accepted,
                    "samples": out.samples,
                    "consistency": out.consistency,
                    "ready": out.ready,
                    "message": out.reason.unwrap_or_else(|| {
                        format!("captured sample {} of {}", out.samples, crate::speaker::MIN_ENROLL_SAMPLES.max(out.samples))
                    }),
                }),
                Err(e) => serde_json::json!({
                    "ok": false, "phase": "sample", "session": screen.session,
                    "message": format!("enroll failed: {e:#}"),
                }),
            },
        };
        device
            .send(&WyomingEvent::with_data(types::ENROLL_RESULT, result))
            .await
            .ok();
        device.send(&WyomingEvent::audio_stop(0)).await.ok();
    }

    /// Synthesize a standalone phrase with Piper and stream it to the device as one
    /// self-contained audio stream (`audio-start` → `audio-chunk`… → `audio-stop`).
    ///
    /// Unlike a voice turn this answers an out-of-band `anamanti-speak` request (see
    /// [`crate::server`]) so an on-device timer can voice its "Time's up …"
    /// announcement in the real Piper voice when the Mac is reachable. Best-effort — a
    /// device that drops the socket mid-stream is a graceful stop, not an error.
    pub async fn announce(
        &self,
        device: &mut DynConnection,
        connector: &dyn ServiceConnector,
        text: &str,
    ) -> Result<()> {
        let runtime = self.settings.snapshot();
        let (_reader, writer) = device.split_mut();
        let mut audio_started = false;
        self.speak_chunk(writer, &runtime, connector, text, &mut audio_started, None)
            .await?;
        // `speak_chunk` swallows Piper's per-request `audio-stop`; send the single
        // terminating stop so the device ends playback (only if a stream was opened).
        if audio_started {
            let _ = protocol::write_event(writer, &WyomingEvent::audio_stop(0)).await;
        }
        Ok(())
    }
}

/// The deterministic answer to "who am I?" (SpeakerScopedMemoryPlan.md §5): from the
/// voiceprint resolution, never from memory text, with the household roster's spelling.
fn who_am_i_reply(speaker: &SpeakerContext, household: &crate::settings::Household) -> String {
    let name = speaker
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(|n| {
            household
                .member_matching_spoken(n)
                .map_or_else(|| n.to_string(), |m| m.name.trim().to_string())
        });
    let enroll = "To teach me, go to Settings, People, Add a voice on the display.";
    match (speaker.resolution, name) {
        (SpeakerResolution::Identified, Some(n)) => format!("You're {n}."),
        (SpeakerResolution::Tentative, Some(n)) => {
            format!("I think you're {n}, but I'm not completely sure.")
        }
        (SpeakerResolution::Identified, None) => {
            format!("I recognise your voice, but I don't know your name yet. {enroll}")
        }
        (SpeakerResolution::Tentative, None) => {
            format!("I'm not sure who you are yet. {enroll}")
        }
        (SpeakerResolution::New | SpeakerResolution::Unknown, _) => {
            format!("I don't recognise your voice yet. {enroll}")
        }
    }
}
