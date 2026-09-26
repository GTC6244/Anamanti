//! In-process STT engine abstraction (see `plans/python-to-rust-whisper.md`).
//!
//! Historically STT was hardwired to a downstream Wyoming Whisper server. This
//! module introduces a [`Transcriber`] seam so the turn pipeline
//! ([`crate::orchestrator::Pipeline`]'s `stream_to_transcript`) is engine-agnostic:
//! it forwards the device's PCM and reads a final transcript without knowing whether
//! the words come from a remote `wyoming-faster-whisper` process or an in-process
//! `whisper-rs` engine.
//!
//! **Stage 1** (this file) ships exactly one implementation, [`WyomingTranscriber`],
//! which wraps the existing [`SttSession`] with byte-for-byte the old behavior. The
//! in-process whisper-rs engine lands behind this same trait in Stage 2.
//!
//! The method set deliberately mirrors `SttSession` (`forward_pcm` / `read_event` /
//! `finish`) so the pump loop's two-arm `select!` — and its cancellation-safety
//! invariants — are preserved verbatim across engines. An in-process engine that
//! produces its transcript synchronously at `finish` implements `read_event` as
//! "pending until finished, then return the decoded transcript once", which slots
//! into the same loop.

use anyhow::Result;
use async_trait::async_trait;

use crate::wyoming::protocol::AudioFormat;
use crate::wyoming::stt::SttSession;
use crate::wyoming::{DynConnection, DynRead, DynWrite};

#[cfg(feature = "stt-whisper-local")]
mod whisper_local;
#[cfg(feature = "stt-whisper-local")]
pub use whisper_local::{WhisperEngine, WhisperLocal, WhisperSttEngine};

/// An event surfaced by a [`Transcriber`] while a turn is streaming. The pump loop
/// only cares about the final transcript; every other engine signal
/// (`voice-started`/`voice-stopped`, partials we don't yet consume) collapses to
/// [`SttEvent::Other`].
#[derive(Debug, Clone)]
pub enum SttEvent {
    /// The final transcript for the utterance.
    Transcript(String),
    /// Any other engine signal; ignored by the current pump loop.
    Other,
}

/// Produces a per-turn [`Transcriber`]. One engine instance is held by the pipeline
/// and reused across turns: a downstream Wyoming engine dials a fresh socket each
/// turn, while an in-process engine hands out a session over its already-loaded
/// model. The pipeline falls back to the historical Wyoming-via-`ServiceConnector`
/// path when no engine is attached (`Pipeline::with_stt_engine` is not called).
#[async_trait]
pub trait SttEngine: Send + Sync {
    /// Start a transcription session for one turn's PCM format.
    async fn begin(&self, format: AudioFormat) -> Result<Box<dyn Transcriber>>;
}

/// A single in-flight transcription request, created per turn. The energy VAD in
/// `orchestrator::stream_to_transcript` feeds it PCM via [`forward_pcm`] and, at
/// end-of-speech, calls [`finish`] and drains [`read_event`] until the
/// [`SttEvent::Transcript`] arrives.
///
/// [`forward_pcm`]: Transcriber::forward_pcm
/// [`finish`]: Transcriber::finish
/// [`read_event`]: Transcriber::read_event
#[async_trait]
pub trait Transcriber: Send {
    /// Forward one chunk of raw little-endian `i16` PCM (as received from the device)
    /// to the engine.
    async fn forward_pcm(&mut self, pcm: Vec<u8>) -> Result<()>;

    /// Read the next engine event (typically the final transcript). `Ok(None)` means
    /// the engine closed before returning a transcript (the caller treats this as an
    /// error at the pump loop).
    async fn read_event(&mut self) -> Result<Option<SttEvent>>;

    /// Signal end-of-speech once the VAD has fired. Implementations must be
    /// idempotent — the pump loop and its caller may both request finalization.
    async fn finish(&mut self) -> Result<()>;
}

/// The downstream **Wyoming** STT engine: a client to an external
/// `wyoming-faster-whisper` (or CoreML Whisper) server. Wraps [`SttSession`] with no
/// behavior change from the pre-seam pipeline.
pub struct WyomingTranscriber {
    session: SttSession<DynRead, DynWrite>,
}

impl WyomingTranscriber {
    /// Open a transcription stream over an already-connected downstream socket
    /// (from `ServiceConnector::connect_stt`): announce `transcribe`, then the
    /// `audio-start` header describing the PCM format that follows.
    pub async fn begin(conn: DynConnection, format: AudioFormat) -> Result<Self> {
        Ok(Self {
            session: SttSession::begin(conn, format).await?,
        })
    }
}

#[async_trait]
impl Transcriber for WyomingTranscriber {
    async fn forward_pcm(&mut self, pcm: Vec<u8>) -> Result<()> {
        self.session.forward_pcm(pcm).await
    }

    async fn read_event(&mut self) -> Result<Option<SttEvent>> {
        Ok(self.session.read_event().await?.map(|ev| {
            if ev.is_transcript() {
                SttEvent::Transcript(ev.transcript_text().unwrap_or_default().to_string())
            } else {
                SttEvent::Other
            }
        }))
    }

    async fn finish(&mut self) -> Result<()> {
        self.session.finish().await
    }
}
