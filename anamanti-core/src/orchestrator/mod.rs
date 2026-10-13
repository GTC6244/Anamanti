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
    infer_memories, parse_command, ChatLog, ChatLogRecord, MemoryCommand, MemoryKind, MemorySource,
    MemoryStore, PromptLog, Recall, SqliteRecall,
};
use crate::settings::{RuntimeSettings, SharedSettings};
use crate::speaker::{SpeakerContext, SpeakerService};
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

/// Inactivity budget for the LLM reply stream: the longest we wait for the stream to
/// be opened or for the *next* token. Generous because a rig turn with tool rounds
/// (web search, weather, directions) produces nothing until its final completion
/// returns; a provider that has gone quiet this long has stalled. On expiry the turn
/// ends cleanly — whatever was already spoken stands, the device gets its closing
/// `audio-stop`, and music ducking is restored by the caller.
pub const LLM_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Progress events surfaced as a turn runs (logging, tests, and — via the device
/// relay — the Phase-5 UI).
#[derive(Debug, Clone, PartialEq)]
pub enum TurnEvent {
    /// PCM is now streaming to the STT service.
    Streaming,
    /// The final transcript arrived from STT.
    Transcript(String),
    /// The turn's speaker was identified (or attributed to the shared household).
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
    memories_written: Vec<String>,
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
    /// Optional per-person speaker identification (speaker_id_plan.md). `None`
    /// preserves the shared-household behavior — every turn attributes to
    /// [`crate::speaker::HOUSEHOLD_SPEAKER`].
    speaker: Option<Arc<SpeakerService>>,
    /// Debug-only per-turn audio capture directory. `None` disables capture
    /// entirely; set from the config file's `audio_dump_dir`.
    audio_dump_dir: Option<PathBuf>,
    system_prompt: String,
    turn_timeout: Duration,
    /// Inactivity budget for the LLM reply stream ([`LLM_STREAM_IDLE_TIMEOUT`]).
    llm_idle_timeout: Duration,
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
            audio_dump_dir: None,
            system_prompt: system_prompt.into(),
            turn_timeout,
            llm_idle_timeout: LLM_STREAM_IDLE_TIMEOUT,
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

    /// Enable per-person speaker identification. Without this the pipeline keeps the
    /// shared-household behavior (all turns → `household`).
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
        self.llm_idle_timeout = timeout;
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

        // 2. Identify who is speaking (or attribute to the shared household), so memory
        //    writes/recall and the reply are per-person.
        let speaker = self.identify_speaker(&voiced_pcm).await;
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
        // Memory entries written this turn (for the chat log / ingester).
        let mut memories_written = Vec::new();
        // The scope memory writes/recall use: this person, or shared (household).
        let scope = speaker_scope(speaker);

        // Explicit command → apply, confirm, and speak the confirmation in one chunk,
        // skipping the LLM. Like every fast reply it closes on a bare `audio-stop` (no
        // follow-up listen) and is barge-in-interruptible.
        if let Some(cmd) = parse_command(transcript) {
            ctx.timing.path = Some("command".to_string());
            match &cmd {
                MemoryCommand::Remember { content, .. } => memories_written.push(content.clone()),
                MemoryCommand::NameSpeaker(name) => {
                    memories_written.push(format!("The user's name is {name}"))
                }
                _ => {}
            }
            let text = self.apply_command(cmd, scope, ctx.on_event)?;
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
        if let Some((text, delivery)) = self.run_system1(ctx, transcript).await {
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

        // Inferred capture from an ordinary turn — attributed to this speaker.
        for (kind, content) in infer_memories(transcript) {
            self.memory
                .add_scoped(kind, &content, MemorySource::Inferred, scope)?;
            memories_written.push(content.clone());
            (ctx.on_event)(TurnEvent::MemoryStored(content));
        }

        // Build per-person memory context. A recall failure (e.g. a transient GraphRAG
        // backend error) must not sink the turn — proceed with no memory context.
        let recall_start = Instant::now();
        let recalled = self
            .build_context(transcript, scope)
            .await
            .unwrap_or_else(|e| {
                log::warn!("memory recall failed; answering without context: {e:#}");
                crate::memory::RecallResult::default()
            });
        let recall_ms = recall_start.elapsed().as_millis() as u64;
        let context = recalled
            .hits
            .iter()
            .map(|c| format!("- {c}"))
            .collect::<Vec<_>>()
            .join("\n");
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
            context.len(),
        );

        // Prompt assembly (system prompt + grounding + recalled context), all from the
        // turn snapshot.
        let prompt_start = Instant::now();
        let system_prompt = prompt::build_system_prompt(
            &self.system_prompt,
            speaker,
            &ctx.runtime,
            &ctx.device_ctx,
            &context,
        );
        // Record the exact prompt the model is about to see (debug GUI). Best-effort.
        self.log_prompt(&ctx.runtime, speaker, &system_prompt, transcript);

        // A follow-up turn (the device auto-opened the mic after a reply) is fed the
        // recent conversation so the answer has context; an ordinary single-shot turn
        // carries no history (locked "single-turn v1" decision). See plans/Plan.MD.
        let history = if ctx.followup_depth > 0 {
            self.recent_history()
        } else {
            Vec::new()
        };
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
                        tokio::time::timeout(idle, runtime.llm.respond(turn))
                            .await
                            .map_err(|_| TimedOut::new("LLM request", idle))?
                            .with_context(|| {
                                format!("LLM backend `{}` failed", runtime.llm.name())
                            })?
                    }
                };
                let mut speaking = false;
                loop {
                    let next = tokio::time::timeout(idle, stream.next())
                        .await
                        .map_err(|_| TimedOut::new("LLM token stream", idle))?;
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
    fn log_turn(
        &self,
        runtime: &RuntimeSettings,
        speaker: &SpeakerContext,
        transcript: &str,
        reply: &str,
        memories_written: Vec<String>,
        timing: crate::memory::TurnTiming,
    ) {
        let Some(log) = &self.chatlog else {
            return;
        };
        let id = log.next_id();
        let record = ChatLogRecord {
            session_id: format!("s-{id}"),
            id,
            ts: now_secs(),
            transcript: transcript.to_string(),
            reply: reply.to_string(),
            memories_written,
            llm_backend: runtime.llm_backend.clone(),
            model: runtime.llm_model.clone(),
            speaker_id: speaker.speaker_id.clone(),
            speaker_name: speaker.name.clone(),
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
            ts: now_secs(),
            llm_backend: runtime.llm_backend.clone(),
            model: runtime.llm_model.clone(),
            speaker_id: speaker.speaker_id.clone(),
            speaker_name: speaker.name.clone(),
            system_prompt: system_prompt.to_string(),
            user_message: user_message.to_string(),
        };
        if let Err(e) = log.append(&record) {
            log::warn!("failed to append prompt log record: {e:#}");
        }
    }

    /// Apply an explicit memory command; returns the spoken confirmation.
    fn apply_command(
        &self,
        cmd: MemoryCommand,
        speaker_id: Option<&str>,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<String> {
        Ok(match cmd {
            MemoryCommand::Remember { kind, content } => {
                self.memory
                    .add_scoped(kind, &content, MemorySource::Explicit, speaker_id)?;
                on_event(TurnEvent::MemoryStored(content));
                "Okay, I'll remember that.".to_string()
            }
            MemoryCommand::ForgetMatching(query) => {
                let n = self.memory.forget_matching(&query)?;
                if n > 0 {
                    "Okay, I've forgotten that.".to_string()
                } else {
                    "I didn't have anything about that.".to_string()
                }
            }
            MemoryCommand::ForgetLast => match self.memory.forget_last()? {
                Some(_) => "Okay, I've forgotten that.".to_string(),
                None => "There was nothing to forget.".to_string(),
            },
            MemoryCommand::NameSpeaker(name) => {
                // Record the name as a per-person fact (parity with inferred capture)…
                let fact = format!("The user's name is {name}");
                self.memory.add_scoped(
                    MemoryKind::Fact,
                    &fact,
                    MemorySource::Explicit,
                    speaker_id,
                )?;
                on_event(TurnEvent::MemoryStored(fact));
                // …and attach it to the voiceprint profile, when a person was identified.
                if let (Some(svc), Some(id)) = (&self.speaker, speaker_id) {
                    if let Err(e) = svc.registry().rename(id, &name) {
                        log::warn!("failed to name speaker {id}: {e:#}");
                    }
                }
                format!("Nice to meet you, {name}!")
            }
        })
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
    fn recent_history(&self) -> Vec<(String, String)> {
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
            .map(|r| (r.transcript, r.reply))
            .rev()
            .collect()
    }

    /// Identify the turn's speaker via the [`SpeakerService`], degrading gracefully
    /// to the shared household on any failure (or when identification is disabled).
    /// Runs on the blocking pool (feature extraction + embedding + SQLite + clip write).
    async fn identify_speaker(&self, voiced_pcm: &[i16]) -> SpeakerContext {
        let Some(svc) = &self.speaker else {
            return SpeakerContext::household();
        };
        match svc
            .identify_and_attribute_blocking(voiced_pcm.to_vec())
            .await
        {
            Ok(ctx) => ctx,
            Err(e) => {
                log::warn!("speaker identification failed; attributing to household: {e:#}");
                SpeakerContext::household()
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

/// The memory scope for a speaker: `None` (shared/household) for the sentinel
/// household context, else the concrete `speaker_id`.
fn speaker_scope(speaker: &SpeakerContext) -> Option<&str> {
    if speaker.is_household() {
        None
    } else {
        Some(speaker.speaker_id.as_str())
    }
}
