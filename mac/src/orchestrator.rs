//! The assistant pipeline (Plan.MD Phase 4). Given a device-facing Wyoming
//! connection, one [`Pipeline::run_turn`] drives a full voice turn:
//!
//! 1. **STT** — forward the device's streamed PCM to the downstream Whisper
//!    service and wait for its `transcript` (server-side VAD end-of-speech), then
//!    relay that transcript back to the device.
//! 2. **Memory + LLM** — apply explicit memory commands ("remember…"/"forget…")
//!    or auto-infer facts, build memory context, and stream a reply from the
//!    pluggable [`LlmBackend`].
//! 3. **TTS** — synthesize the reply with Piper and relay the audio frames back to
//!    the device over the same socket (architecture.md §4 SPEAKING).
//!
//! The pipeline acquires its downstream connections through a [`ServiceConnector`]
//! so production dials TCP while tests wire in-memory mock servers — the turn
//! logic is identical either way.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use tokio::time::{sleep_until, Instant};

use crate::audio_dump::TurnAudioDump;
use crate::llm::{LlmBackend, LlmTurn};
use crate::memory::chatlog::now_secs;
use crate::memory::{
    infer_memories, parse_command, ChatLog, ChatLogRecord, MemoryCommand, MemoryKind, MemorySource,
    MemoryStore, Recall, SqliteRecall,
};
use crate::settings::SharedSettings;
use crate::speaker::{SpeakerContext, SpeakerService};
use crate::wyoming::protocol::{self, types, AudioFormat, WyomingEvent};
use crate::wyoming::stt::SttSession;
use crate::wyoming::tts::TtsSession;
use crate::wyoming::{DynConnection, DynRead, DynWrite};

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

/// The result of driving one turn: either a turn ran to completion, or the device
/// had already disconnected (nothing to do). Lets the connection handler tell a
/// finished turn from a closed socket without busy-looping.
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

/// Production connector: dials the configured Whisper and Piper TCP endpoints.
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
    /// Optional per-person speaker identification (speaker_id_plan.md). `None`
    /// preserves the shared-household behavior — every turn attributes to
    /// [`crate::speaker::HOUSEHOLD_SPEAKER`].
    speaker: Option<Arc<SpeakerService>>,
    system_prompt: String,
    turn_timeout: Duration,
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
            speaker: None,
            system_prompt: system_prompt.into(),
            turn_timeout,
        }
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

    /// Enable per-person speaker identification. Without this the pipeline keeps the
    /// shared-household behavior (all turns → `household`).
    pub fn with_speaker(mut self, speaker: Arc<SpeakerService>) -> Self {
        self.speaker = Some(speaker);
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

    /// Drive one voice turn over `device`. Returns `Ok(())` on a completed turn or
    /// a clean device disconnect; returns `Err` only on an unrecoverable pipeline
    /// failure (STT unreachable, LLM error, …).
    pub async fn run_turn(
        &self,
        device: &mut DynConnection,
        connector: &dyn ServiceConnector,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<TurnOutcome> {
        // 1. Wait for the device's `audio-start`; a clean close before that just
        //    ends the connection.
        let format = loop {
            match device.read().await? {
                Some(ev) if ev.event_type == types::AUDIO_START => {
                    break protocol::audio_format(&ev.data).unwrap_or(AudioFormat::PCM_16K_MONO);
                }
                Some(_) => continue, // ignore stray pre-turn frames
                None => return Ok(TurnOutcome::Disconnected),
            }
        };
        self.run_turn_after_start(device, connector, format, on_event)
            .await
    }

    /// Drive a turn whose opening `audio-start` has already been read (the server
    /// consumes it to distinguish a turn from a Phase-6 control frame). Splitting
    /// this out lets one accept loop serve both turns and control on the same
    /// socket.
    pub async fn run_turn_after_start(
        &self,
        device: &mut DynConnection,
        connector: &dyn ServiceConnector,
        format: AudioFormat,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<TurnOutcome> {
        // Take one settings snapshot for the whole turn so a concurrent control
        // swap never changes the backend/voice mid-reply.
        let runtime = self.settings.snapshot();

        // Debug-only AEC corpus capture (no-op unless AMBIENT_AUDIO_DUMP_DIR is set):
        // records this turn's device mic (near-end) and Piper TTS (far-end reference).
        let dump = TurnAudioDump::for_turn();

        // 2. Open the STT stream and pump device PCM into it until the transcript.
        let stt_conn = connector.connect_stt().await?;
        let mut stt = SttSession::begin(stt_conn, format).await?;
        on_event(TurnEvent::Streaming);

        let end_silence = Duration::from_millis(runtime.end_silence_ms);
        let voice_rms_threshold = runtime.voice_rms_threshold;
        let Some((transcript, voiced_pcm)) = self
            .stream_to_transcript(
                device,
                &mut stt,
                end_silence,
                voice_rms_threshold,
                format.rate,
                dump.as_ref(),
            )
            .await?
        else {
            // Device closed or timed out before a transcript — abandon the turn.
            let _ = stt.finish().await;
            return Ok(TurnOutcome::Completed);
        };
        let _ = stt.finish().await; // close the STT audio stream (post server-VAD)
        if let Some(d) = dump.as_ref() {
            d.set_transcript(&transcript);
        }
        on_event(TurnEvent::Transcript(transcript.clone()));

        // Relay the transcript to the device (renders on screen; ends its input).
        device
            .send(&WyomingEvent::transcript(&transcript))
            .await
            .ok();

        if transcript.trim().is_empty() {
            on_event(TurnEvent::Finished);
            return Ok(TurnOutcome::Completed);
        }

        // 2b. Identify who is speaking (or attribute to the shared household), so
        //     memory writes/recall and the reply are per-person.
        let speaker = self.identify_speaker(&voiced_pcm);
        on_event(TurnEvent::Speaker(speaker.clone()));

        // 3 + 4. Memory + LLM → reply, relaying each token to the device for
        //    token-by-token rendering (Phase 5) AND synthesizing complete sentences
        //    with Piper as soon as they form, so playback begins before the full
        //    reply is generated (streaming TTS; architecture.md §4 SPEAKING).
        let (reply, memories_written) = self
            .respond_and_speak(
                &runtime,
                &transcript,
                &speaker,
                device,
                connector,
                on_event,
                dump.as_ref(),
            )
            .await?;
        if let Some(d) = dump.as_ref() {
            d.set_reply(&reply);
        }
        on_event(TurnEvent::Reply(reply.clone()));

        // Record the completed turn for the background GraphRAG ingester. Never
        // let a logging failure break the turn.
        self.log_turn(&runtime, &speaker, &transcript, &reply, memories_written);

        on_event(TurnEvent::Finished);
        Ok(TurnOutcome::Completed)
    }

    /// Pump loop: forward device `audio-chunk`s to STT, detect end-of-speech, and
    /// return STT's `transcript`. `None` if the device hung up or the turn idled
    /// past `turn_timeout`.
    ///
    /// wyoming-faster-whisper does **not** do streaming VAD — it transcribes the
    /// buffered utterance only once it receives `audio-stop`. The device, meanwhile,
    /// streams continuously and waits for the transcript before it stops. So the
    /// orchestrator is the only party that can close the loop: it runs a simple
    /// energy VAD over the incoming PCM and, once speech has been followed by a
    /// short trailing silence, sends `audio-stop` to STT to finalize the transcript.
    /// Returns the transcript together with the utterance's **voiced** PCM (the
    /// chunks that passed the energy gate), so the caller can compute a speaker
    /// embedding without re-reading the socket. `None` on disconnect/timeout.
    async fn stream_to_transcript(
        &self,
        device: &mut DynConnection,
        stt: &mut SttSession<crate::wyoming::DynRead, crate::wyoming::DynWrite>,
        end_silence: std::time::Duration,
        voice_rms_threshold: f64,
        mic_rate: u32,
        dump: Option<&TurnAudioDump>,
    ) -> Result<Option<(String, Vec<i16>)>> {
        // `voice_rms_threshold`: RMS (i16 units) above which a chunk counts as speech
        // rather than room noise. The Echo's far-field pickup is quiet (~50 idle,
        // several hundred+ while speaking). `end_silence`: trailing silence after
        // speech that marks end-of-utterance. Both come from the per-turn settings
        // snapshot so they are A/B-tunable from the device without a restart.
        //
        // If no speech is ever detected, still finalize after this long so a silent
        // or too-quiet utterance ends the turn instead of hanging to `turn_timeout`.
        const NO_SPEECH_FINALIZE: std::time::Duration = std::time::Duration::from_secs(6);

        let turn_start = Instant::now();
        let mut last_voice = turn_start;
        let mut speech_started = false;
        // True once we've sent `audio-stop` to STT and are just awaiting the result.
        let mut finalized = false;
        // Accumulated voiced PCM (samples from chunks above the energy gate), used
        // for the speaker embedding once the transcript arrives.
        let mut voiced_pcm: Vec<i16> = Vec::new();

        let mut deadline = Instant::now() + self.turn_timeout;
        loop {
            tokio::select! {
                biased;

                _ = sleep_until(deadline) => {
                    log::warn!("turn idle-timed-out waiting for STT transcript");
                    return Ok(None);
                }

                dev = device.read() => {
                    deadline = Instant::now() + self.turn_timeout;
                    match dev? {
                        Some(ev) if ev.event_type == types::AUDIO_CHUNK => {
                            if let Some(pcm) = ev.payload {
                                // Capture the near-end mic for the AEC corpus (the
                                // whole listening window, incl. pre/post-speech).
                                if let Some(d) = dump {
                                    d.push_mic(&pcm, mic_rate);
                                }
                                if !finalized {
                                    let now = Instant::now();
                                    if rms_i16_le(&pcm) > voice_rms_threshold {
                                        if !speech_started {
                                            log::debug!("VAD: speech started");
                                        }
                                        speech_started = true;
                                        last_voice = now;
                                        // Keep the voiced samples for speaker ID.
                                        append_pcm_i16_le(&mut voiced_pcm, &pcm);
                                    }
                                    stt.forward_pcm(pcm).await?;

                                    let ended = if speech_started {
                                        now.duration_since(last_voice) >= end_silence
                                    } else {
                                        now.duration_since(turn_start) >= NO_SPEECH_FINALIZE
                                    };
                                    if ended {
                                        log::info!(
                                            "VAD: end-of-speech (speech_started={speech_started}); \
                                             finalizing STT"
                                        );
                                        stt.finish().await?;
                                        finalized = true;
                                    }
                                }
                                // After finalizing, drop further mic chunks: STT has
                                // its `audio-stop` and is transcribing.
                            }
                        }
                        // The device sends `audio-stop` only after it sees the
                        // transcript; before then, ignore other frames.
                        Some(_) => {}
                        None => return Ok(None), // device disconnected
                    }
                }

                sev = stt.read_event() => {
                    deadline = Instant::now() + self.turn_timeout;
                    match sev? {
                        Some(ev) if ev.is_transcript() => {
                            let text = ev.transcript_text().unwrap_or_default().to_string();
                            return Ok(Some((text, std::mem::take(&mut voiced_pcm))));
                        }
                        Some(_) => {} // voice-started / voice-stopped etc.
                        None => anyhow::bail!("STT service closed before returning a transcript"),
                    }
                }
            }
        }
    }

    /// Apply memory policy, stream the LLM reply, and synthesize it with Piper as
    /// complete sentences arrive so playback begins before generation finishes.
    ///
    /// Each token is relayed to the device as a `reply-token` (for token-by-token
    /// on-screen rendering) and appended to a pending buffer; whenever that buffer
    /// holds a complete sentence it is flushed to TTS immediately. This removes the
    /// old "buffer the whole reply, then speak" barrier: time-to-first-audio drops
    /// from full-reply latency to first-sentence latency. Returns the full reply
    /// text and the memory entries written this turn.
    // These are all distinct per-turn inputs (runtime/transcript/speaker/device/
    // connector/event-sink/dump); bundling them into a struct would only relocate
    // the list, so the arg-count lint isn't worth appeasing here.
    #[allow(clippy::too_many_arguments)]
    async fn respond_and_speak(
        &self,
        runtime: &crate::settings::RuntimeSettings,
        transcript: &str,
        speaker: &SpeakerContext,
        device: &mut DynConnection,
        connector: &dyn ServiceConnector,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
        dump: Option<&TurnAudioDump>,
    ) -> Result<(String, Vec<String>)> {
        // Memory entries written this turn (for the chat log / ingester).
        let mut memories_written = Vec::new();
        // The scope memory writes/recall use: this person, or shared (household).
        let scope = speaker_scope(speaker);

        // Explicit command → apply, confirm, and speak the confirmation in one
        // chunk, skipping the LLM. (Instant, so it is not barge-in-interruptible.)
        if let Some(cmd) = parse_command(transcript) {
            match &cmd {
                MemoryCommand::Remember { content, .. } => memories_written.push(content.clone()),
                MemoryCommand::NameSpeaker(name) => {
                    memories_written.push(format!("The user's name is {name}"))
                }
                _ => {}
            }
            let reply = self.apply_command(cmd, scope, on_event)?;
            on_event(TurnEvent::ReplyToken(reply.clone()));
            let (_reader, writer) = device.split_mut();
            protocol::write_event(writer, &WyomingEvent::reply_token(&reply))
                .await
                .ok();
            on_event(TurnEvent::Speaking);
            let mut audio_started = false;
            self.speak_chunk(writer, runtime, connector, &reply, &mut audio_started, dump)
                .await?;
            if audio_started {
                protocol::write_event(writer, &WyomingEvent::audio_stop(0))
                    .await
                    .ok();
            }
            return Ok((reply, memories_written));
        }

        // Inferred capture from an ordinary turn — attributed to this speaker.
        for (kind, content) in infer_memories(transcript) {
            self.memory
                .add_scoped(kind, &content, MemorySource::Inferred, scope)?;
            memories_written.push(content.clone());
            on_event(TurnEvent::MemoryStored(content));
        }

        // Build per-person memory context and stream the LLM reply. A recall
        // failure (e.g. a transient GraphRAG backend error) must not sink the turn —
        // proceed with no memory context rather than erroring.
        let context = self.build_context(transcript, scope).await.unwrap_or_else(|e| {
            log::warn!("memory recall failed; answering without context: {e:#}");
            String::new()
        });
        // Ground the model in the real wall-clock (it has no clock) and tell it who
        // it is speaking with, so time/date questions are answered from fact and it
        // can address the person by name / apply the right person's memory.
        let identity = speaker_identity_line(speaker);
        let mut system_prompt = format!(
            "{}\n\n{}\n\n{}",
            self.system_prompt,
            current_datetime_line(),
            identity
        );
        if !context.is_empty() {
            system_prompt.push_str("\n\nWhat you remember about this person:\n");
            system_prompt.push_str(&context);
        }

        let mut stream = runtime
            .llm
            .respond(LlmTurn::new(system_prompt, transcript))
            .await
            .with_context(|| format!("LLM backend `{}` failed", runtime.llm.name()))?;

        let mut reply = String::new();
        let mut pending = String::new();
        let mut speaking = false;
        // Whether the single, coalesced device-facing `audio-start` has been sent.
        // Piper emits an `audio-start`/`audio-stop` per sentence, but the device
        // ends its turn on the *first* `audio-stop` — so we forward exactly one
        // `audio-start` up front, relay only the chunks, and send one `audio-stop`
        // at the very end. The reply still streams sentence-by-sentence (low
        // time-to-first-audio) but reaches the device as one continuous stream.
        let mut audio_started = false;

        // Race the reply against a barge-in in an inner scope so both futures (and
        // their borrows of `reply`/`pending`/the split socket halves) are dropped
        // before we read `reply` back out below.
        let interrupted = {
            // Split the device socket so we can *concurrently* stream reply tokens +
            // TTS audio out on the writer while watching the reader for a barge-in.
            // The user talking over the assistant (a new wake word or on-device VAD)
            // sends an `ambient-interrupt` frame (and/or drops the socket); either
            // wins the `select!` below, cancels the driver, and aborts the LLM + TTS
            // at once instead of finishing a reply nobody is listening to.
            let (reader, writer) = device.split_mut();

            // The generation + speaking driver. Dropping this future (when a barge-in
            // wins the race) drops the LLM `stream` and any in-flight `TtsSession`,
            // cancelling the upstream Ollama/Anthropic request and Piper synthesis.
            let drive = async {
                while let Some(tok) = stream.next().await {
                    let tok = tok?;
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
                        if !self
                            .speak_chunk(
                                writer,
                                runtime,
                                connector,
                                &sentence,
                                &mut audio_started,
                                dump,
                            )
                            .await?
                        {
                            // Device closed mid-relay; stop generating.
                            return Ok::<(), anyhow::Error>(());
                        }
                    }
                }
                // Speak any trailing clause left without terminal punctuation.
                if let Some(rest) = take_speakable(&mut pending, true) {
                    if !speaking {
                        on_event(TurnEvent::Speaking);
                    }
                    self.speak_chunk(writer, runtime, connector, &rest, &mut audio_started, dump)
                        .await?;
                }
                // Close the single coalesced device-facing audio stream.
                if audio_started {
                    protocol::write_event(writer, &WyomingEvent::audio_stop(0))
                        .await
                        .ok();
                }
                Ok(())
            };
            tokio::pin!(drive);

            let watch = watch_for_barge_in(reader);
            tokio::pin!(watch);

            let interrupted;
            tokio::select! {
                biased;
                // Barge-in (or device close) → cancel the reply immediately.
                _ = &mut watch => {
                    interrupted = true;
                }
                res = &mut drive => {
                    res?;
                    interrupted = false;
                }
            }
            interrupted
        };
        if interrupted {
            log::info!("barge-in: aborting in-flight LLM generation + TTS for this turn");
        }

        Ok((reply.trim().to_string(), memories_written))
    }

    /// Append the completed turn to the chat log, if one is attached. A failure is
    /// logged and swallowed — logging must never break a turn.
    fn log_turn(
        &self,
        runtime: &crate::settings::RuntimeSettings,
        speaker: &SpeakerContext,
        transcript: &str,
        reply: &str,
        memories_written: Vec<String>,
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
        };
        if let Err(e) = log.append(&record) {
            log::warn!("failed to append chat log record: {e:#}");
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
                self.memory
                    .add_scoped(MemoryKind::Fact, &fact, MemorySource::Explicit, speaker_id)?;
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

    /// Gather memory entries relevant to the transcript as prompt context for this
    /// speaker (their own entries plus shared), via the configured recall backend
    /// (SQLite FTS by default; HelixDB GraphRAG when set).
    async fn build_context(&self, transcript: &str, speaker_id: Option<&str>) -> Result<String> {
        let hits = self.recall.recall(transcript, speaker_id, 8).await?;
        Ok(hits
            .iter()
            .map(|c| format!("- {c}"))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    /// Identify the turn's speaker via the [`SpeakerService`], degrading gracefully
    /// to the shared household on any failure (or when identification is disabled).
    fn identify_speaker(&self, voiced_pcm: &[i16]) -> SpeakerContext {
        let Some(svc) = &self.speaker else {
            return SpeakerContext::household();
        };
        match svc.identify_and_attribute(voiced_pcm) {
            Ok(ctx) => ctx,
            Err(e) => {
                log::warn!("speaker identification failed; attributing to household: {e:#}");
                SpeakerContext::household()
            }
        }
    }

    /// Synthesize one chunk of reply text with Piper and relay its audio frames to
    /// the device over the same socket. A fresh downstream Piper connection is used
    /// per chunk so this works with any Wyoming TTS server (whether or not it keeps
    /// a connection open across `synthesize` requests).
    ///
    /// Returns `Ok(false)` if the device closed mid-relay — treated as a graceful
    /// stop (e.g. the Phase-3 client that ends on transcript, or a barge-in that
    /// dropped the socket), not a turn failure — and `Ok(true)` otherwise. An
    /// empty/whitespace chunk is a no-op that returns `Ok(true)`.
    async fn speak_chunk(
        &self,
        writer: &mut DynWrite,
        runtime: &crate::settings::RuntimeSettings,
        connector: &dyn ServiceConnector,
        text: &str,
        audio_started: &mut bool,
        dump: Option<&TurnAudioDump>,
    ) -> Result<bool> {
        let text = sanitize_for_tts(text);
        if text.trim().is_empty() {
            return Ok(true);
        }
        let tts_conn = connector.connect_tts().await?;
        let mut tts = TtsSession::begin(tts_conn, &text, runtime.tts_voice.as_deref()).await?;

        // Piper announces its rate in each `audio-start`; track it so dumped TTS
        // (the far-end reference) is tagged with the right sample rate.
        let mut tts_rate = AudioFormat::default().rate;
        while let Some(ev) = tts.next_audio().await? {
            if ev.event_type == types::AUDIO_START {
                if let Some(fmt) = protocol::audio_format(&ev.data) {
                    tts_rate = fmt.rate;
                }
            }
            if ev.event_type == types::AUDIO_CHUNK {
                if let (Some(d), Some(pcm)) = (dump, ev.payload.as_deref()) {
                    d.push_tts(pcm, tts_rate);
                }
            }
            // Coalesce this sentence's Piper stream into the turn's single
            // device-facing stream: forward exactly one `audio-start`, relay every
            // `audio-chunk`, and swallow the per-sentence `audio-stop` (the turn
            // sends one final `audio-stop`). The device ends its turn on the first
            // `audio-stop` it sees, so a per-sentence stop would truncate the reply.
            let forward = match ev.event_type.as_str() {
                types::AUDIO_START => {
                    if *audio_started {
                        false
                    } else {
                        *audio_started = true;
                        true
                    }
                }
                types::AUDIO_STOP => false,
                _ => true,
            };
            if forward && protocol::write_event(writer, &ev).await.is_err() {
                log::info!("device closed before TTS playback finished; stopping relay");
                return Ok(false);
            }
        }
        Ok(true)
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

/// The system-prompt line telling the model who it is speaking with.
fn speaker_identity_line(speaker: &SpeakerContext) -> String {
    match &speaker.name {
        Some(name) => format!("You are speaking with {name}."),
        None if speaker.is_household() => "You are speaking with a member of the household.".to_string(),
        None => "You are speaking with a household member you haven't been introduced to yet. \
                 If they tell you their name, greet them by it."
            .to_string(),
    }
}

/// Append little-endian PCM16 bytes to an `i16` sample buffer (a trailing odd byte,
/// never expected from a well-formed frame, is ignored).
fn append_pcm_i16_le(out: &mut Vec<i16>, pcm: &[u8]) {
    out.reserve(pcm.len() / 2);
    for c in pcm.chunks_exact(2) {
        out.push(i16::from_le_bytes([c[0], c[1]]));
    }
}

/// Watch the device→orchestrator half of the socket for a **barge-in** while the
/// assistant is replying. Resolves (ending the race in [`Pipeline::respond_and_speak`])
/// when the device sends an `ambient-interrupt` frame or closes the socket; any
/// other stray frame during playback (e.g. the device's own `audio-stop` from the
/// STREAMING→SPEAKING edge) is consumed and ignored so it can't be mistaken for a
/// barge-in.
///
/// This is a single long-lived future (never re-created inside a `select!` arm), so
/// it is cancellation-safe: whenever it is dropped it is parked on a fresh
/// `read_event` with no partially-consumed frame.
async fn watch_for_barge_in(reader: &mut DynRead) {
    loop {
        match protocol::read_event(reader).await {
            Ok(Some(ev)) if ev.is_interrupt() => return,
            Ok(Some(_)) => continue, // stray frame during playback — ignore
            Ok(None) => return,      // device closed the socket
            Err(e) => {
                log::debug!("barge-in watcher read error (treating as disconnect): {e:#}");
                return;
            }
        }
    }
}

/// The longest run of text (in characters) we let accumulate without terminal
/// punctuation before flushing it to TTS anyway. Bounds time-to-first-audio on a
/// long unpunctuated clause.
const TTS_MAX_CHUNK_CHARS: usize = 240;

/// Pull the next speakable chunk out of `pending`, draining it from the buffer.
///
/// A chunk is a complete sentence — text up to and including a `.`/`!`/`?` or a
/// newline — or, to bound latency on a long clause that never terminates, the
/// leading [`TTS_MAX_CHUNK_CHARS`] broken at the last whitespace. When `flush` is
/// set (end of the token stream) the entire remaining buffer is returned. Returns
/// `None` when there is nothing complete to speak yet.
fn take_speakable(pending: &mut String, flush: bool) -> Option<String> {
    if flush {
        let rest = pending.trim().to_string();
        pending.clear();
        return if rest.is_empty() { None } else { Some(rest) };
    }

    // End of the first sentence, if one has arrived.
    let boundary = pending
        .char_indices()
        .find(|(_, ch)| matches!(ch, '.' | '!' | '?' | '\n'))
        .map(|(i, ch)| i + ch.len_utf8());

    let cut = match boundary {
        Some(b) => b,
        None => {
            if pending.chars().count() < TTS_MAX_CHUNK_CHARS {
                return None;
            }
            // Overlong unterminated clause: break at the last space/newline within
            // the cap (both are single-byte, so `+ 1` stays on a char boundary).
            let cap = pending
                .char_indices()
                .nth(TTS_MAX_CHUNK_CHARS)
                .map(|(i, _)| i)
                .unwrap_or(pending.len());
            pending[..cap]
                .rfind([' ', '\n'])
                .map(|w| w + 1)
                .unwrap_or(cap)
        }
    };

    let chunk: String = pending.drain(..cut).collect();
    let chunk = chunk.trim().to_string();
    if chunk.is_empty() {
        // The drained span was only whitespace/punctuation — try the next boundary.
        return take_speakable(pending, false);
    }
    Some(chunk)
}

/// Strip Markdown decoration characters an LLM may emit (`* _ ` # ~`) so Piper does
/// not read them aloud (e.g. "asterisk asterisk"). Kept deliberately light: it only
/// removes standalone decoration glyphs, leaving words and punctuation intact.
fn sanitize_for_tts(text: &str) -> String {
    text.chars()
        .filter(|c| !matches!(c, '*' | '`' | '#' | '_' | '~'))
        .collect()
}

/// A one-line statement of the current local date + time + timezone, injected into
/// the LLM system prompt each turn so the model answers time/date questions from
/// fact instead of hallucinating (it has no clock of its own). Example:
/// `Current date and time: Monday, 16 September 2026, 6:36 PM EDT (UTC-04:00).`
///
/// Phrased *non-leadingly*: the model must only use it when the user actually asks
/// about the time/date, and must not volunteer it otherwise. Without that guard the
/// prominently-stated clock became the most salient fact in context, so on a vague or
/// mis-transcribed request the model would default to reciting the time.
fn current_datetime_line() -> String {
    let now = chrono::Local::now();
    format!(
        "For reference, the current date and time is {} ({}). Use this only to answer \
         questions that are explicitly about the time, date, or day of week; do not \
         mention the time or date otherwise, and never bring it up on your own.",
        now.format("%A, %-d %B %Y, %-I:%M %p %Z"),
        now.format("UTC%:z"),
    )
}

/// Root-mean-square amplitude (in `i16` units) of a little-endian PCM16 buffer,
/// used by the turn's energy VAD to tell speech from room noise. A trailing odd
/// byte (never expected from a well-formed frame) is ignored.
fn rms_i16_le(pcm: &[u8]) -> f64 {
    let mut sum_sq = 0f64;
    let mut n = 0u64;
    for c in pcm.chunks_exact(2) {
        let s = i16::from_le_bytes([c[0], c[1]]) as f64;
        sum_sq += s * s;
        n += 1;
    }
    if n == 0 {
        0.0
    } else {
        (sum_sq / n as f64).sqrt()
    }
}

#[cfg(test)]
mod vad_tests {
    use super::rms_i16_le;

    fn pcm(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn rms_of_silence_is_zero() {
        assert_eq!(rms_i16_le(&pcm(&[0, 0, 0, 0])), 0.0);
        assert_eq!(rms_i16_le(&[]), 0.0);
    }

    #[test]
    fn rms_tracks_amplitude() {
        // A constant ±1000 signal has RMS 1000; loud speech reads far above the
        // 120-unit voice threshold while a quiet ±30 noise floor stays below it.
        assert!((rms_i16_le(&pcm(&[1000, -1000, 1000, -1000])) - 1000.0).abs() < 1e-6);
        assert!(rms_i16_le(&pcm(&[30, -30, 25, -20])) < 120.0);
        assert!(rms_i16_le(&pcm(&[800, -600, 700, -900])) > 120.0);
    }
}

#[cfg(test)]
mod segmenter_tests {
    use super::{sanitize_for_tts, take_speakable, TTS_MAX_CHUNK_CHARS};

    #[test]
    fn waits_for_a_complete_sentence() {
        let mut p = String::from("Hello there");
        assert_eq!(take_speakable(&mut p, false), None, "no terminator yet");
        assert_eq!(p, "Hello there", "buffer untouched");
    }

    #[test]
    fn flushes_each_complete_sentence_in_order() {
        let mut p = String::from("First one. Second one! Third?");
        assert_eq!(take_speakable(&mut p, false).as_deref(), Some("First one."));
        assert_eq!(
            take_speakable(&mut p, false).as_deref(),
            Some("Second one!")
        );
        assert_eq!(take_speakable(&mut p, false).as_deref(), Some("Third?"));
        assert_eq!(take_speakable(&mut p, false), None);
        assert!(p.is_empty());
    }

    #[test]
    fn splits_on_newlines_too() {
        let mut p = String::from("Line one\nleftover");
        assert_eq!(take_speakable(&mut p, false).as_deref(), Some("Line one"));
        assert_eq!(take_speakable(&mut p, false), None);
        assert_eq!(p, "leftover");
    }

    #[test]
    fn flush_returns_trailing_clause() {
        let mut p = String::from("no terminator here");
        assert_eq!(take_speakable(&mut p, false), None);
        assert_eq!(
            take_speakable(&mut p, true).as_deref(),
            Some("no terminator here")
        );
        assert_eq!(take_speakable(&mut p, true), None, "empty flush is None");
    }

    #[test]
    fn overlong_unterminated_clause_is_broken_at_whitespace() {
        // A long run with no sentence terminator flushes near the cap at a space
        // boundary, never mid-word, so time-to-first-audio stays bounded.
        let word = "word ";
        let mut p = word.repeat(TTS_MAX_CHUNK_CHARS); // far longer than the cap
        let chunk = take_speakable(&mut p, false).expect("caps long clause");
        assert!(chunk.chars().count() <= TTS_MAX_CHUNK_CHARS);
        assert!(chunk.starts_with("word"));
        assert!(!chunk.ends_with("wor"), "must not split mid-word");
    }

    #[test]
    fn sanitize_strips_markdown_decoration() {
        assert_eq!(
            sanitize_for_tts("**bold** and _em_ and `code`"),
            "bold and em and code"
        );
        assert_eq!(sanitize_for_tts("# Heading"), " Heading");
        assert_eq!(sanitize_for_tts("plain text, ok."), "plain text, ok.");
    }
}
