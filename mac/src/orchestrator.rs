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

use crate::llm::{LlmBackend, LlmTurn};
use crate::memory::chatlog::now_secs;
use crate::memory::{
    infer_memories, parse_command, ChatLog, ChatLogRecord, MemoryCommand, MemorySource,
    MemoryStore, Recall, SqliteRecall,
};
use crate::settings::SharedSettings;
use crate::wyoming::protocol::{self, types, AudioFormat, WyomingEvent};
use crate::wyoming::stt::SttSession;
use crate::wyoming::tts::TtsSession;
use crate::wyoming::DynConnection;

/// Progress events surfaced as a turn runs (logging, tests, and — via the device
/// relay — the Phase-5 UI).
#[derive(Debug, Clone, PartialEq)]
pub enum TurnEvent {
    /// PCM is now streaming to the STT service.
    Streaming,
    /// The final transcript arrived from STT.
    Transcript(String),
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

        // 2. Open the STT stream and pump device PCM into it until the transcript.
        let stt_conn = connector.connect_stt().await?;
        let mut stt = SttSession::begin(stt_conn, format).await?;
        on_event(TurnEvent::Streaming);

        let end_silence = Duration::from_millis(runtime.end_silence_ms);
        let voice_rms_threshold = runtime.voice_rms_threshold;
        let Some(transcript) = self
            .stream_to_transcript(device, &mut stt, end_silence, voice_rms_threshold)
            .await?
        else {
            // Device closed or timed out before a transcript — abandon the turn.
            let _ = stt.finish().await;
            return Ok(TurnOutcome::Completed);
        };
        let _ = stt.finish().await; // close the STT audio stream (post server-VAD)
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

        // 3. Memory + LLM → reply text, relaying each reply token to the device
        //    so it can render the reply token-by-token (Phase 5).
        let (reply, memories_written) = self
            .generate_reply(&runtime, &transcript, device, on_event)
            .await?;
        on_event(TurnEvent::Reply(reply.clone()));

        // Record the completed turn for the background GraphRAG ingester. Never
        // let a logging failure break the turn.
        self.log_turn(&runtime, &transcript, &reply, memories_written);

        // 4. Synthesize and stream the reply audio back to the device.
        if !reply.trim().is_empty() {
            self.speak(&runtime, device, connector, &reply, on_event)
                .await?;
        }

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
    async fn stream_to_transcript(
        &self,
        device: &mut DynConnection,
        stt: &mut SttSession<crate::wyoming::DynRead, crate::wyoming::DynWrite>,
        end_silence: std::time::Duration,
        voice_rms_threshold: f64,
    ) -> Result<Option<String>> {
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
                                if !finalized {
                                    let now = Instant::now();
                                    if rms_i16_le(&pcm) > voice_rms_threshold {
                                        if !speech_started {
                                            log::debug!("VAD: speech started");
                                        }
                                        speech_started = true;
                                        last_voice = now;
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
                            return Ok(Some(ev.transcript_text().unwrap_or_default().to_string()));
                        }
                        Some(_) => {} // voice-started / voice-stopped etc.
                        None => anyhow::bail!("STT service closed before returning a transcript"),
                    }
                }
            }
        }
    }

    /// Apply memory policy and produce the reply text, emitting reply tokens as
    /// they stream and relaying each one to `device` for token-by-token rendering.
    async fn generate_reply(
        &self,
        runtime: &crate::settings::RuntimeSettings,
        transcript: &str,
        device: &mut DynConnection,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<(String, Vec<String>)> {
        // Memory entries written this turn (for the chat log / ingester).
        let mut memories_written = Vec::new();

        // Explicit command → apply and confirm, skipping the LLM.
        if let Some(cmd) = parse_command(transcript) {
            if let MemoryCommand::Remember { content, .. } = &cmd {
                memories_written.push(content.clone());
            }
            let reply = self.apply_command(cmd, on_event)?;
            on_event(TurnEvent::ReplyToken(reply.clone()));
            device.send(&WyomingEvent::reply_token(&reply)).await.ok();
            return Ok((reply, memories_written));
        }

        // Inferred capture from an ordinary turn.
        for (kind, content) in infer_memories(transcript) {
            self.memory.add(kind, &content, MemorySource::Inferred)?;
            memories_written.push(content.clone());
            on_event(TurnEvent::MemoryStored(content));
        }

        // Build memory context and stream the LLM reply. A recall failure (e.g. a
        // transient GraphRAG backend error) must not sink the turn — proceed with
        // no memory context rather than erroring.
        let context = self.build_context(transcript).await.unwrap_or_else(|e| {
            log::warn!("memory recall failed; answering without context: {e:#}");
            String::new()
        });
        // Ground the model in the real wall-clock: without this it hallucinates the
        // time/date (it has no clock). Injected every turn so "what time is it" /
        // date-relative questions are answered from fact.
        let mut system_prompt = format!("{}\n\n{}", self.system_prompt, current_datetime_line());
        if !context.is_empty() {
            system_prompt.push_str(&format!(
                "\n\nWhat you remember about this user:\n{context}"
            ));
        }

        let mut stream = runtime
            .llm
            .respond(LlmTurn::new(system_prompt, transcript))
            .await
            .with_context(|| format!("LLM backend `{}` failed", runtime.llm.name()))?;

        let mut reply = String::new();
        while let Some(tok) = stream.next().await {
            let tok = tok?;
            reply.push_str(&tok);
            device.send(&WyomingEvent::reply_token(&tok)).await.ok();
            on_event(TurnEvent::ReplyToken(tok));
        }
        Ok((reply.trim().to_string(), memories_written))
    }

    /// Append the completed turn to the chat log, if one is attached. A failure is
    /// logged and swallowed — logging must never break a turn.
    fn log_turn(
        &self,
        runtime: &crate::settings::RuntimeSettings,
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
        };
        if let Err(e) = log.append(&record) {
            log::warn!("failed to append chat log record: {e:#}");
        }
    }

    /// Apply an explicit memory command; returns the spoken confirmation.
    fn apply_command(
        &self,
        cmd: MemoryCommand,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<String> {
        Ok(match cmd {
            MemoryCommand::Remember { kind, content } => {
                self.memory.add(kind, &content, MemorySource::Explicit)?;
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
        })
    }

    /// Gather memory entries relevant to the transcript as prompt context, via the
    /// configured recall backend (SQLite FTS by default; HelixDB GraphRAG when set).
    async fn build_context(&self, transcript: &str) -> Result<String> {
        let hits = self.recall.recall(transcript, 8).await?;
        Ok(hits
            .iter()
            .map(|c| format!("- {c}"))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    /// Synthesize `reply` with Piper and relay each audio frame to the device. A
    /// device that closed early (e.g. the Phase-3 client, which ends on transcript)
    /// is treated as a graceful stop, not a turn failure.
    async fn speak(
        &self,
        runtime: &crate::settings::RuntimeSettings,
        device: &mut DynConnection,
        connector: &dyn ServiceConnector,
        reply: &str,
        on_event: &mut (dyn FnMut(TurnEvent) + Send),
    ) -> Result<()> {
        let tts_conn = connector.connect_tts().await?;
        let mut tts = TtsSession::begin(tts_conn, reply, runtime.tts_voice.as_deref()).await?;
        on_event(TurnEvent::Speaking);

        while let Some(ev) = tts.next_audio().await? {
            if device.send(&ev).await.is_err() {
                log::info!("device closed before TTS playback finished; stopping relay");
                break;
            }
        }
        Ok(())
    }
}

/// A one-line statement of the current local date + time + timezone, injected into
/// the LLM system prompt each turn so the model answers time/date questions from
/// fact instead of hallucinating (it has no clock of its own). Example:
/// `Current date and time: Monday, 16 September 2026, 6:36 PM EDT (UTC-04:00).`
fn current_datetime_line() -> String {
    let now = chrono::Local::now();
    format!(
        "Current date and time: {} ({}). Use this as the source of truth for any \
         time-, date-, day-of-week-, or \"today/tomorrow/now\"-related question.",
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
