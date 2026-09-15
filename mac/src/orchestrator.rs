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
use crate::memory::{infer_memories, parse_command, MemoryCommand, MemorySource, MemoryStore};
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
        Self {
            settings,
            memory,
            system_prompt: system_prompt.into(),
            turn_timeout,
        }
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

        let Some(transcript) = self.stream_to_transcript(device, &mut stt).await? else {
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
        let reply = self
            .generate_reply(&runtime, &transcript, device, on_event)
            .await?;
        on_event(TurnEvent::Reply(reply.clone()));

        // 4. Synthesize and stream the reply audio back to the device.
        if !reply.trim().is_empty() {
            self.speak(&runtime, device, connector, &reply, on_event)
                .await?;
        }

        on_event(TurnEvent::Finished);
        Ok(TurnOutcome::Completed)
    }

    /// Pump loop: forward device `audio-chunk`s to STT while watching for STT's
    /// `transcript`. Returns the transcript, or `None` if the device hung up or
    /// the turn idled past `turn_timeout`.
    async fn stream_to_transcript(
        &self,
        device: &mut DynConnection,
        stt: &mut SttSession<crate::wyoming::DynRead, crate::wyoming::DynWrite>,
    ) -> Result<Option<String>> {
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
                                stt.forward_pcm(pcm).await?;
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
    ) -> Result<String> {
        // Explicit command → apply and confirm, skipping the LLM.
        if let Some(cmd) = parse_command(transcript) {
            let reply = self.apply_command(cmd, on_event)?;
            on_event(TurnEvent::ReplyToken(reply.clone()));
            device.send(&WyomingEvent::reply_token(&reply)).await.ok();
            return Ok(reply);
        }

        // Inferred capture from an ordinary turn.
        for (kind, content) in infer_memories(transcript) {
            self.memory.add(kind, &content, MemorySource::Inferred)?;
            on_event(TurnEvent::MemoryStored(content));
        }

        // Build memory context and stream the LLM reply.
        let context = self.build_context(transcript)?;
        let system_prompt = if context.is_empty() {
            self.system_prompt.clone()
        } else {
            format!(
                "{}\n\nWhat you remember about this user:\n{}",
                self.system_prompt, context
            )
        };

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
        Ok(reply.trim().to_string())
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

    /// Gather memory entries relevant to the transcript as prompt context.
    fn build_context(&self, transcript: &str) -> Result<String> {
        let hits = self.memory.search(transcript, 8)?;
        Ok(hits
            .iter()
            .map(|m| format!("- {}", m.content))
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
