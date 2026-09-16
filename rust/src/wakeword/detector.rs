//! openWakeWord inference pipeline on `tract-onnx` (Plan.MD §3, Phase 2).
//!
//! openWakeWord runs three ONNX models in series, exactly the chain used by the
//! upstream project (github.com/dscripka/openWakeWord):
//!
//! 1. **Melspectrogram** (`melspectrogram.onnx`): raw 16 kHz audio -> mel frames
//!    with 32 mel bins. openWakeWord then applies the affine transform
//!    `mel/10 + 2` before the next stage.
//! 2. **Embedding / feature** (`embedding_model.onnx`): a sliding window of 76
//!    mel frames (32 bins) -> a 96-dim embedding. Windows advance 8 frames at a
//!    time (~80 ms hop), matching openWakeWord's streaming stride.
//! 3. **Wake word** (`<model>.onnx`, e.g. `alexa_v0.1.onnx`): the last 16
//!    embeddings -> a single confidence score in [0, 1].
//!
//! We feed the melspectrogram model in fixed 1280-sample (80 ms) chunks so every
//! model has a concrete input shape and `tract` can fully optimize the graph.
//! State (partial audio chunk, rolling mel frames, rolling embeddings) is carried
//! across calls so scoring is continuous over the live audio stream.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use tract_onnx::prelude::*;

/// Samples fed to the melspectrogram model per step (80 ms @ 16 kHz).
const MELSPEC_CHUNK_SAMPLES: usize = 1280;
/// Mel bins produced per frame by the melspectrogram model.
const MEL_BINS: usize = 32;
/// Mel frames per embedding window (feature model input).
const EMBED_WINDOW_FRAMES: usize = 76;
/// Frames advanced between successive embedding windows (~80 ms hop).
const EMBED_STEP_FRAMES: usize = 8;
/// Embedding dimensionality produced by the feature model.
const EMBED_DIM: usize = 96;
/// Embeddings fed to the wake-word classifier per score.
const WAKEWORD_WINDOW_EMBEDDINGS: usize = 16;
/// openWakeWord's mel normalization applied before the feature model.
const MEL_SCALE: f32 = 10.0;
const MEL_BIAS: f32 = 2.0;

/// Filesystem locations of the three ONNX models that make up a detector.
#[derive(Debug, Clone)]
pub struct WakeWordModelPaths {
    pub melspec: PathBuf,
    pub embedding: PathBuf,
    pub wakeword: PathBuf,
}

type Plan = std::sync::Arc<TypedRunnableModel>;

/// A loaded, stateful openWakeWord detector. Not `Send`-hostile — it lives
/// entirely on the engine's inference thread.
pub struct WakeWordDetector {
    melspec: Plan,
    embedding: Plan,
    wakeword: Plan,

    /// Raw 16 kHz samples awaiting a full melspectrogram chunk.
    audio_accum: Vec<f32>,
    /// Rolling mel frames (flattened, `MEL_BINS` per frame) from the current base.
    mel: Vec<f32>,
    /// Frame index (relative to `mel`'s base) where the next embedding starts.
    next_embed_offset: usize,
    /// Rolling embeddings (flattened, `EMBED_DIM` each); at most the last 16.
    embeddings: VecDeque<f32>,
}

impl WakeWordDetector {
    /// Load and optimize the three-model chain. Fails if any model file is
    /// missing so the engine can fall back to a capture-only status mode.
    pub fn load(paths: &WakeWordModelPaths) -> Result<Self> {
        let melspec = load_model(&paths.melspec, &[1, MELSPEC_CHUNK_SAMPLES])?;
        let embedding = load_model(&paths.embedding, &[1, EMBED_WINDOW_FRAMES, MEL_BINS, 1])?;
        let wakeword = load_model(&paths.wakeword, &[1, WAKEWORD_WINDOW_EMBEDDINGS, EMBED_DIM])?;

        Ok(Self {
            melspec,
            embedding,
            wakeword,
            audio_accum: Vec::with_capacity(MELSPEC_CHUNK_SAMPLES * 2),
            mel: Vec::new(),
            next_embed_offset: 0,
            embeddings: VecDeque::with_capacity(WAKEWORD_WINDOW_EMBEDDINGS * EMBED_DIM),
        })
    }

    /// Feed 16 kHz mono audio (samples in `i16` range as `f32`). Returns the
    /// highest wake-word confidence produced while consuming this block, if any
    /// score was computed.
    pub fn push_audio(&mut self, samples: &[f32]) -> Result<Option<f32>> {
        self.audio_accum.extend_from_slice(samples);
        let mut best: Option<f32> = None;

        while self.audio_accum.len() >= MELSPEC_CHUNK_SAMPLES {
            let chunk: Vec<f32> = self.audio_accum.drain(0..MELSPEC_CHUNK_SAMPLES).collect();
            if let Some(score) = self.process_chunk(&chunk)? {
                best = Some(best.map_or(score, |b| b.max(score)));
            }
        }
        Ok(best)
    }

    /// Run one 1280-sample chunk through the full mel -> embedding -> classifier
    /// chain, advancing all rolling state.
    fn process_chunk(&mut self, chunk: &[f32]) -> Result<Option<f32>> {
        // 1) Melspectrogram: [1, 1280] -> flatten to mel frames of 32 bins.
        let input =
            tract_ndarray::Array2::from_shape_vec((1, MELSPEC_CHUNK_SAMPLES), chunk.to_vec())
                .context("shaping melspec input")?;
        let mel_out = self.melspec.run(tvec!(input.into_tensor().into()))?;
        let mel_view = mel_out[0].to_plain_array_view::<f32>()?;
        for &v in mel_view.iter() {
            self.mel.push(v / MEL_SCALE + MEL_BIAS);
        }

        let mut best: Option<f32> = None;

        // 2) Embedding: slide a 76-frame window forward in 8-frame steps.
        while frames(self.mel.len()) >= self.next_embed_offset + EMBED_WINDOW_FRAMES {
            let start = self.next_embed_offset * MEL_BINS;
            let end = start + EMBED_WINDOW_FRAMES * MEL_BINS;
            let window = self.mel[start..end].to_vec();

            let emb_in = tract_ndarray::Array4::from_shape_vec(
                (1, EMBED_WINDOW_FRAMES, MEL_BINS, 1),
                window,
            )
            .context("shaping embedding input")?;
            let emb_out = self.embedding.run(tvec!(emb_in.into_tensor().into()))?;
            let emb_view = emb_out[0].to_plain_array_view::<f32>()?;

            let emb: Vec<f32> = emb_view.iter().copied().take(EMBED_DIM).collect();
            if emb.len() != EMBED_DIM {
                return Err(anyhow!(
                    "embedding model produced {} values, expected {EMBED_DIM}",
                    emb.len()
                ));
            }
            for v in emb {
                self.embeddings.push_back(v);
            }
            // Keep only the most recent 16 embeddings.
            while self.embeddings.len() > WAKEWORD_WINDOW_EMBEDDINGS * EMBED_DIM {
                self.embeddings.pop_front();
            }

            self.next_embed_offset += EMBED_STEP_FRAMES;

            // 3) Wake word: score once we have a full 16-embedding window.
            if self.embeddings.len() == WAKEWORD_WINDOW_EMBEDDINGS * EMBED_DIM {
                let score = self.score_embeddings()?;
                best = Some(best.map_or(score, |b| b.max(score)));
            }
        }

        // Drop consumed mel frames; the next window begins at fresh index 0.
        if self.next_embed_offset > 0 {
            self.mel.drain(0..self.next_embed_offset * MEL_BINS);
            self.next_embed_offset = 0;
        }

        Ok(best)
    }

    /// Run the wake-word classifier over the current 16-embedding window.
    fn score_embeddings(&self) -> Result<f32> {
        let flat: Vec<f32> = self.embeddings.iter().copied().collect();
        let ww_in =
            tract_ndarray::Array3::from_shape_vec((1, WAKEWORD_WINDOW_EMBEDDINGS, EMBED_DIM), flat)
                .context("shaping wake-word input")?;
        let ww_out = self.wakeword.run(tvec!(ww_in.into_tensor().into()))?;
        let ww_view = ww_out[0].to_plain_array_view::<f32>()?;
        // The classifier emits a single sigmoid confidence; be tolerant of shape.
        let score = ww_view.iter().copied().fold(f32::MIN, f32::max);
        if score == f32::MIN {
            return Err(anyhow!("wake-word model produced no output"));
        }
        Ok(score)
    }
}

/// Number of complete mel frames represented by `flat_len` flattened values.
fn frames(flat_len: usize) -> usize {
    flat_len / MEL_BINS
}

/// Load an ONNX model, pin its input shape so the graph is fully typed, and
/// optimize it into a runnable plan.
fn load_model(path: &Path, input_shape: &[usize]) -> Result<Plan> {
    if !path.exists() {
        return Err(anyhow!("model file not found: {}", path.display()));
    }
    let model = tract_onnx::onnx()
        .model_for_path(path)
        .with_context(|| format!("loading ONNX model {}", path.display()))?
        .with_input_fact(
            0,
            InferenceFact::dt_shape(f32::datum_type(), input_shape.to_vec()),
        )
        .with_context(|| format!("setting input shape for {}", path.display()))?
        .into_optimized()
        .with_context(|| format!("optimizing {}", path.display()))?
        .into_runnable()
        .with_context(|| format!("making {} runnable", path.display()))?;
    Ok(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bundled openWakeWord models shipped in `assets/models/` (see
    /// `pubspec.yaml` + `lib/src/engine/model_assets.dart`). Resolved relative to
    /// this crate so the test runs from `cargo test` in `/rust`.
    fn bundled_models() -> WakeWordModelPaths {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../assets/models");
        WakeWordModelPaths {
            melspec: root.join("melspectrogram.onnx"),
            embedding: root.join("embedding_model.onnx"),
            wakeword: root.join("hey_jarvis.onnx"),
        }
    }

    /// Isolation harness for on-hardware debugging: feed a real mono PCM16 clip of
    /// the wake word through the detector and assert it scores a clear detection.
    /// This is how the Echo Show capture bug was pinned down — a clip pulled off the
    /// device scored ~0 until it was resampled to correct the HAL's 2x rate error,
    /// isolating the fault to sample-rate handling rather than the model or mic.
    ///
    /// Runs only when `HEY_JARVIS_WAV` points at a file; ignored in normal CI.
    /// Optional env knobs:
    ///  - `HEADER`   bytes to skip (44 for a canonical WAV, 0 for a raw PCM dump).
    ///  - `SRC_RATE` treat the input as this rate and run it through the engine
    ///               `Resampler` down to 16 kHz first, reproducing the device path.
    #[test]
    fn scores_real_wake_word_clip() {
        let Ok(wav) = std::env::var("HEY_JARVIS_WAV") else {
            eprintln!("skipping: set HEY_JARVIS_WAV=/path/to/mono_pcm16.wav to run");
            return;
        };
        let paths = bundled_models();
        let mut detector = WakeWordDetector::load(&paths).expect("bundled models load");

        let bytes = std::fs::read(&wav).expect("read wav");
        let header: usize = std::env::var("HEADER").ok().and_then(|s| s.parse().ok()).unwrap_or(44);
        let mut samples: Vec<f32> = bytes[header..]
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32)
            .collect();

        if let Some(src) = std::env::var("SRC_RATE").ok().and_then(|s| s.parse::<u32>().ok()) {
            let mut r = crate::audio::resample::Resampler::new(src, 16_000);
            let mut out = Vec::new();
            r.process(&samples, &mut out);
            eprintln!("resampled {src}Hz->16kHz: {} -> {} samples", samples.len(), out.len());
            samples = out;
        }
        let maxabs = samples.iter().fold(0f32, |m, &s| m.max(s.abs()));
        eprintln!("loaded {} samples (max |amp| = {maxabs:.0})", samples.len());

        let mut peak = 0.0f32;
        for chunk in samples.chunks(1280) {
            if let Some(score) = detector.push_audio(chunk).expect("inference runs") {
                peak = peak.max(score);
            }
        }
        eprintln!("peak wake-word score for clip = {peak:.4}");
        assert!(peak > 0.3, "expected a clear detection, got peak {peak:.4}");
    }

    /// The real bundled model chain loads on `tract` and scores a live stream.
    /// This is the guardrail that the shipped `.onnx` files parse and their shapes
    /// line up with the pipeline (melspec → embedding → classifier). Skipped if the
    /// assets aren't present (e.g. a checkout without the model download).
    #[test]
    fn bundled_openwakeword_models_load_and_score() {
        let paths = bundled_models();
        if !paths.melspec.exists() || !paths.embedding.exists() || !paths.wakeword.exists() {
            eprintln!("skipping: bundled models not present at {:?}", paths);
            return;
        }

        let mut detector = WakeWordDetector::load(&paths).expect("bundled models load");

        // Feed ~6 s of 16 kHz audio (a quiet 220 Hz tone) — comfortably more than
        // the ~200 mel frames the chain needs before its first 16-embedding score.
        // We don't assert a detection, only that inference runs end-to-end and
        // yields an in-range confidence once enough context has accumulated.
        let mut produced_score = false;
        for block in 0..120 {
            let mut samples = [0f32; 800]; // 50 ms blocks
            for (i, s) in samples.iter_mut().enumerate() {
                let t = (block * 800 + i) as f32 / 16_000.0;
                *s = (2.0 * std::f32::consts::PI * 220.0 * t).sin() * 1000.0;
            }
            if let Some(score) = detector.push_audio(&samples).expect("inference runs") {
                assert!(
                    (0.0..=1.0).contains(&score),
                    "confidence {score} out of range"
                );
                produced_score = true;
            }
        }
        assert!(
            produced_score,
            "detector should produce at least one score over ~2 s of audio"
        );
    }
}
