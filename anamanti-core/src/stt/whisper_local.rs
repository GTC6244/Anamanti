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
//!
//! **Bilingual mode** (`plans/BilingualPlan.md` §2.3): with more than one language
//! enabled and a **multilingual** model (`ggml-small.bin`, `ggml-large-v3-turbo.bin`, …
//! — an `.en` model cannot transcribe anything else), each decode first runs
//! whisper.cpp's language detection on the utterance, picks the most probable language
//! **among the enabled ones only** (so a short Portuguese command can't drift to
//! Spanish or Galician), then decodes pinned to it and reports it on the [`Transcript`].
//! Single-language mode decodes exactly as before.

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::task::JoinHandle;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::wyoming::protocol::AudioFormat;

use super::{SttEngine, SttEvent, Transcriber, Transcript};

/// A decode's text and, in bilingual mode, the language it detected.
type Decoded = (String, Option<String>);

/// Logged once when bilingual mode meets an English-only (`.en`) model.
static WARNED_NOT_MULTILINGUAL: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// A loaded Whisper model, shared across every turn. Construct once at startup and
/// clone freely (the heavy model context sits behind an `Arc`); call
/// [`begin`](WhisperEngine::begin) per turn for a fresh session.
#[derive(Clone)]
pub struct WhisperEngine {
    ctx: Arc<WhisperContext>,
    language: Option<String>,
    n_threads: i32,
    /// Whether the loaded model is multilingual (not an `.en` model).
    multilingual: bool,
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
        let multilingual = ctx.is_multilingual();
        let engine = Self {
            ctx: Arc::new(ctx),
            language,
            n_threads,
            multilingual,
        };
        engine.warm_up();
        Ok(engine)
    }

    /// Run one throwaway decode over a short buffer of silence at startup. This
    /// forces whisper.cpp to build its compute pipeline now — notably compiling the
    /// Metal shaders, a one-time cost of several seconds on a fresh machine — so the
    /// user's first real turn decodes at the warm ~sub-200ms latency instead of
    /// eating that cold start. Non-fatal: a failure here is logged, not propagated.
    fn warm_up(&self) {
        let started = std::time::Instant::now();
        // 1s of silence is enough to exercise the full graph; it decodes to empty text.
        let silence = vec![0i16; 16_000];
        match decode(
            &self.ctx,
            &silence,
            self.language.as_deref(),
            &[],
            self.n_threads,
        ) {
            Ok(_) => log::info!(
                "whisper warm-up decode complete in {:.0}ms (Metal pipeline compiled)",
                started.elapsed().as_secs_f64() * 1000.0
            ),
            Err(e) => log::warn!("whisper warm-up decode failed (non-fatal): {e:#}"),
        }
    }

    /// Whether the loaded model can transcribe languages other than English.
    pub fn is_multilingual(&self) -> bool {
        self.multilingual
    }

    /// Start a per-turn transcription session (single-language: the configured decode
    /// language, exactly as before).
    pub fn begin(&self) -> WhisperLocal {
        self.begin_detecting(Vec::new())
    }

    /// Start a session that detects among `candidates` (ISO 639-1 codes) before
    /// decoding. Empty ⇒ [`Self::begin`].
    pub fn begin_detecting(&self, candidates: Vec<String>) -> WhisperLocal {
        WhisperLocal {
            engine: self.clone(),
            candidates,
            buffer: Vec::new(),
            decode: None,
            finished: false,
        }
    }

    /// The detection candidates for a turn: the enabled languages in bilingual mode with
    /// a multilingual model; none otherwise (an `.en` model warns once and stays
    /// English-only).
    fn candidates_for(&self, languages: &crate::lang::LanguageSettings) -> Vec<String> {
        if !languages.is_bilingual() {
            return Vec::new();
        }
        if !self.multilingual {
            if !WARNED_NOT_MULTILINGUAL.swap(true, std::sync::atomic::Ordering::Relaxed) {
                log::warn!(
                    "bilingual mode is on but the in-process Whisper model is English-only \
                     (an `.en` model); it cannot transcribe Portuguese. Load a multilingual \
                     model (stt.multilingual=true, see scripts/fetch-whisper-models.sh)."
                );
            }
            return Vec::new();
        }
        // The household default first: it is decoded first (see `decode`).
        std::iter::once(languages.default)
            .chain(languages.secondary())
            .map(|l| l.iso639_1().to_string())
            .collect()
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

    async fn begin_with(
        &self,
        _format: AudioFormat,
        languages: &crate::lang::LanguageSettings,
    ) -> Result<Box<dyn Transcriber>> {
        let candidates = self.engine.candidates_for(languages);
        Ok(Box::new(self.engine.begin_detecting(candidates)))
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
    /// Bilingual mode: detect among these (ISO 639-1) before decoding; empty ⇒ decode in
    /// the configured language.
    candidates: Vec<String>,
    /// Accumulated LE-i16 PCM samples for the whole utterance.
    buffer: Vec<i16>,
    /// The in-flight decode task, set by `finish`, awaited by `read_event`.
    decode: Option<JoinHandle<Result<Decoded>>>,
    /// Latched by the first `finish` so subsequent calls are no-ops (idempotent).
    finished: bool,
}

#[async_trait]
impl Transcriber for WhisperLocal {
    async fn forward_pcm(&mut self, pcm: Vec<u8>) -> Result<()> {
        // Device PCM is little-endian i16 mono @ 16 kHz — accumulate whole samples.
        // A trailing odd byte (never expected from whole i16 frames) is dropped by
        // `chunks_exact`.
        self.buffer.extend(crate::audio::pcm::iter_i16_le(&pcm));
        Ok(())
    }

    async fn read_event(&mut self) -> Result<Option<SttEvent>> {
        if let Some(handle) = self.decode.take() {
            let (text, lang) = handle
                .await
                .context("whisper decode task failed to join")??;
            return Ok(Some(SttEvent::Transcript(
                Transcript::text(text).with_lang(lang.as_deref()),
            )));
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
        let candidates = std::mem::take(&mut self.candidates);
        let n_threads = self.engine.n_threads;
        // CPU-bound decode → the blocking pool, off the async reactor.
        self.decode = Some(tokio::task::spawn_blocking(move || {
            decode(&ctx, &samples, language.as_deref(), &candidates, n_threads)
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
    candidates: &[String],
    n_threads: i32,
) -> Result<Decoded> {
    if samples.is_empty() {
        return Ok((String::new(), None));
    }
    // whisper.cpp wants f32 mono in [-1.0, 1.0] at 16 kHz.
    let audio: Vec<f32> = samples
        .iter()
        .copied()
        .map(crate::audio::pcm::i16_to_f32)
        .collect();
    // Decode-latency instrumentation (plans/python-to-rust-whisper.md Stage 5, M4
    // validation): time the blocking whisper.cpp decode and report it against the
    // utterance length as a real-time factor (RTF = decode_time / audio_time).
    let audio_secs = audio.len() as f64 / 16_000.0;
    let decode_started = std::time::Instant::now();

    let mut state = ctx.create_state().context("creating whisper state")?;
    // Bilingual mode (plans/BilingualPlan.md §2.3). The encoder output doesn't depend on
    // the language, so: decode pinned to the household default (`candidates[0]`), then
    // read the language distribution from ONE decoder step over the encoder state that
    // decode left behind — no second encoder pass. Only when another enabled language
    // wins is the utterance decoded again, pinned to it. A default-language (English)
    // turn therefore costs one decoder step, not a full re-encode.
    let first = candidates.first().map(String::as_str).or(language);
    let mut text = full_decode(&mut state, &audio, first, n_threads)?;
    let mut detected = None;
    if let Some(primary) = candidates.first() {
        let best = lang_from_encoder_state(ctx, &mut state, candidates, n_threads)?;
        if best != *primary {
            text = full_decode(&mut state, &audio, Some(&best), n_threads)?;
        }
        detected = Some(best);
    }
    let n = state.full_n_segments().unwrap_or(0);
    let decode_ms = decode_started.elapsed().as_secs_f64() * 1000.0;
    let rtf = if audio_secs > 0.0 {
        decode_ms / (audio_secs * 1000.0)
    } else {
        0.0
    };
    log::info!(
        "whisper decode: {audio_secs:.2}s audio in {decode_ms:.0}ms (RTF {rtf:.3}, {n} \
         segments{})",
        detected
            .as_deref()
            .map(|l| format!(", detected {l}"))
            .unwrap_or_default()
    );
    Ok((text.trim().to_string(), detected))
}

/// One `whisper_full` pass pinned to `language` (or auto when `None`), returning the
/// trimmed text.
fn full_decode(
    state: &mut whisper_rs::WhisperState,
    audio: &[f32],
    language: Option<&str>,
    n_threads: i32,
) -> Result<String> {
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
        .full(params, audio)
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

/// The most probable **enabled** language for the utterance whose encoder output is in
/// `state` (from the preceding [`full_decode`]): one decoder step on the
/// start-of-transcript token, then compare the logits of each candidate's language
/// token — the same quantity `whisper_lang_auto_detect` ranks, minus its encoder pass.
fn lang_from_encoder_state(
    ctx: &WhisperContext,
    state: &mut whisper_rs::WhisperState,
    candidates: &[String],
    n_threads: i32,
) -> Result<String> {
    state
        .decode(&[ctx.token_sot()], 0, n_threads.max(1) as usize)
        .context("whisper language detection step")?;
    let logits = state
        .get_logits()
        .context("whisper language detection logits")?;
    let best = candidates
        .iter()
        .filter_map(|c| {
            let token = ctx.token_lang(whisper_rs::get_lang_id(c)?);
            Some((c, logits.get(token as usize).copied()?))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(c, _)| c.clone())
        .unwrap_or_else(|| candidates[0].clone());
    Ok(best)
}
