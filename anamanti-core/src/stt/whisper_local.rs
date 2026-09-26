//! In-process **Whisper** STT engine — whisper.cpp via the `whisper-rs` bindings,
//! behind the `stt-whisper-local` cargo feature (see
//! `plans/python-to-rust-whisper.md`, Stage 2).
//!
//! [`WhisperEngine`] loads a ggml model once at startup and is cheaply cloned;
//! [`WhisperEngine::begin`] hands the turn pipeline a fresh [`WhisperLocal`] session
//! that implements [`Transcriber`]. The pump loop feeds it the utterance's PCM via
//! `forward_pcm`, and at end-of-speech calls `finish`, which kicks off a **single**
//! blocking whisper.cpp decode on Tokio's blocking pool (`spawn_blocking`) — decode
//! is CPU-bound and must never run on the async reactor that is pumping device
//! audio. `read_event` stays pending until `finish` is called, then yields the
//! transcript, mirroring the remote engine's "emit only after `audio-stop`" contract
//! so it drops into the existing two-arm `select!` loop unchanged.

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::task::JoinHandle;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::wyoming::protocol::AudioFormat;

use super::{SttEngine, SttEvent, Transcriber};

/// A loaded Whisper model, shared across every turn. Construct once at startup and
/// clone freely (the heavy model context sits behind an `Arc`); call
/// [`begin`](WhisperEngine::begin) per turn for a fresh session.
#[derive(Clone)]
pub struct WhisperEngine {
    ctx: Arc<WhisperContext>,
    language: Option<String>,
    n_threads: i32,
}

impl WhisperEngine {
    /// Load a ggml Whisper model (e.g. `ggml-base.en.bin`) from disk. `language`
    /// pins the decode language (`Some("en")`); `None` lets Whisper auto-detect.
    /// `n_threads <= 0` selects a sensible default from the host's parallelism.
    pub fn open(model_path: &str, language: Option<String>, n_threads: i32) -> Result<Self> {
        let ctx = WhisperContext::new_with_params(model_path, WhisperContextParameters::default())
            .with_context(|| format!("loading Whisper model {model_path}"))?;
        let n_threads = if n_threads > 0 {
            n_threads
        } else {
            default_threads()
        };
        Ok(Self {
            ctx: Arc::new(ctx),
            language,
            n_threads,
        })
    }

    /// Start a per-turn transcription session.
    pub fn begin(&self) -> WhisperLocal {
        WhisperLocal {
            engine: self.clone(),
            buffer: Vec::new(),
            decode: None,
            finished: false,
        }
    }
}

/// [`SttEngine`] adapter for the in-process Whisper model, held by the pipeline.
pub struct WhisperSttEngine {
    engine: WhisperEngine,
}

impl WhisperSttEngine {
    pub fn new(engine: WhisperEngine) -> Self {
        Self { engine }
    }
}

#[async_trait]
impl SttEngine for WhisperSttEngine {
    async fn begin(&self, _format: AudioFormat) -> Result<Box<dyn Transcriber>> {
        // whisper.cpp fixes 16 kHz mono; the device streams exactly that, so the
        // announced format needs no negotiation here.
        Ok(Box::new(self.engine.begin()))
    }
}

/// A conservative default decode thread count: the host's parallelism, capped so a
/// burst decode doesn't monopolize the Mac while music/LLM/TTS also run.
fn default_threads() -> i32 {
    std::thread::available_parallelism()
        .map(|n| (n.get().min(8)) as i32)
        .unwrap_or(4)
}

/// One in-flight in-process transcription. Buffers the utterance's PCM as the VAD
/// forwards it; on [`finish`](Transcriber::finish) it launches a single blocking
/// whisper.cpp decode; [`read_event`](Transcriber::read_event) stays pending until
/// that decode is launched, then yields the transcript exactly once.
pub struct WhisperLocal {
    engine: WhisperEngine,
    /// Accumulated LE-i16 PCM samples for the whole utterance.
    buffer: Vec<i16>,
    /// The in-flight decode task, set by `finish`, awaited by `read_event`.
    decode: Option<JoinHandle<Result<String>>>,
    /// Latched by the first `finish` so subsequent calls are no-ops (idempotent).
    finished: bool,
}

#[async_trait]
impl Transcriber for WhisperLocal {
    async fn forward_pcm(&mut self, pcm: Vec<u8>) -> Result<()> {
        // Device PCM is little-endian i16 mono @ 16 kHz — accumulate whole samples.
        // A trailing odd byte (never expected from whole i16 frames) is dropped by
        // `chunks_exact`.
        self.buffer.extend(
            pcm.chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]])),
        );
        Ok(())
    }

    async fn read_event(&mut self) -> Result<Option<SttEvent>> {
        if let Some(handle) = self.decode.take() {
            let text = handle
                .await
                .context("whisper decode task failed to join")??;
            return Ok(Some(SttEvent::Transcript(text)));
        }
        // Not finalized yet (or the transcript was already delivered): never resolve,
        // so the pump loop's device-read arm keeps driving until the VAD calls
        // `finish`. Mirrors a remote engine that only emits after `audio-stop`.
        std::future::pending().await
    }

    async fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(()); // idempotent: the loop and its caller may both finalize
        }
        self.finished = true;
        let samples = std::mem::take(&mut self.buffer);
        let ctx = self.engine.ctx.clone();
        let language = self.engine.language.clone();
        let n_threads = self.engine.n_threads;
        // CPU-bound decode → the blocking pool, off the async reactor.
        self.decode = Some(tokio::task::spawn_blocking(move || {
            decode(&ctx, &samples, language.as_deref(), n_threads)
        }));
        Ok(())
    }
}

/// Run one blocking whisper.cpp decode over the buffered utterance and return the
/// concatenated segment text. Invoked on Tokio's blocking pool.
fn decode(
    ctx: &WhisperContext,
    samples: &[i16],
    language: Option<&str>,
    n_threads: i32,
) -> Result<String> {
    if samples.is_empty() {
        return Ok(String::new());
    }
    // whisper.cpp wants f32 mono in [-1.0, 1.0] at 16 kHz.
    let audio: Vec<f32> = samples.iter().map(|&s| s as f32 / 32768.0).collect();

    let mut state = ctx.create_state().context("creating whisper state")?;
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(n_threads);
    if let Some(l) = language {
        params.set_language(Some(l));
    }
    // Keep whisper.cpp quiet — we consume the text ourselves.
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);

    state
        .full(params, &audio)
        .context("running whisper decode")?;

    let n = state
        .full_n_segments()
        .context("counting whisper segments")?;
    let mut text = String::new();
    for i in 0..n {
        text.push_str(
            &state
                .full_get_segment_text(i)
                .with_context(|| format!("reading whisper segment {i}"))?,
        );
    }
    Ok(text.trim().to_string())
}
