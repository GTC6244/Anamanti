//! Speaker-embedding extraction (speaker_id_plan.md Phase A / §4.1).
//!
//! A [`SpeakerEmbedder`] turns an utterance's raw 16 kHz mono PCM into a fixed-
//! length, L2-normalized voiceprint vector. Nearest-neighbor (cosine) against
//! enrolled centroids in the [`super::registry::SpeakerRegistry`] then answers
//! "who is speaking?".
//!
//! Two implementations:
//! - [`MockSpeakerEmbedder`] — a dependency-free, deterministic embedder used by
//!   every Phase A–D test: a naive band-energy "spectral fingerprint" so two
//!   synthetic voices at different pitches are separable and *stable* across
//!   turns, with no model file or network. This is what makes the whole
//!   identification pipeline verifiable offline.
//! - `OnnxSpeakerEmbedder` (Phase E, feature `speaker`) — the real ECAPA-TDNN
//!   ONNX model run in-process. Deliberately deferred so Phases A–D carry no heavy
//!   dependency; the trait below is the seam it slots into.

use anyhow::{ensure, Result};
#[cfg(feature = "speaker")]
use anyhow::Context;

/// Sample rate the orchestrator streams at (architecture.md §2.1). The embedder
/// assumes mono `i16` PCM at this rate.
pub const SAMPLE_RATE: u32 = 16_000;

/// Produces a fixed-length, L2-normalized voiceprint from utterance PCM.
/// `Send + Sync` so it can live behind an `Arc` shared across concurrent turns.
pub trait SpeakerEmbedder: Send + Sync {
    /// Embed one utterance (mono `i16` PCM at [`SAMPLE_RATE`]). The returned vector
    /// is L2-normalized and has length [`Self::dims`].
    fn embed(&self, pcm_16k_mono: &[i16]) -> Result<Vec<f32>>;

    /// The dimensionality of vectors this embedder produces. Stored on each profile
    /// so a later model/dims change is detected rather than silently mismatched.
    fn dims(&self) -> usize;
}

/// Deterministic, dependency-free embedder for tests: a band-energy spectral
/// fingerprint over `dims` log-spaced frequency bins in the speech range. A pure
/// tone (the test voices) concentrates energy in one bin, so two pitches yield
/// near-orthogonal vectors while the *same* pitch reproduces the same vector —
/// exactly the separable-yet-stable behavior the registry's matching needs.
#[derive(Clone)]
pub struct MockSpeakerEmbedder {
    dims: usize,
    /// Center frequency of each bin (Hz), precomputed.
    freqs: Vec<f32>,
}

impl MockSpeakerEmbedder {
    /// Build a mock embedder producing `dims`-length vectors. Bins span ~100–3900
    /// Hz (the voiced speech band), log-spaced so low-pitch differences separate.
    pub fn new(dims: usize) -> Self {
        assert!(dims >= 2, "mock embedder needs at least 2 bins");
        let (lo, hi) = (100.0_f32, 3900.0_f32);
        let ratio = (hi / lo).powf(1.0 / (dims as f32 - 1.0));
        let freqs = (0..dims).map(|k| lo * ratio.powi(k as i32)).collect();
        Self { dims, freqs }
    }
}

impl Default for MockSpeakerEmbedder {
    /// 32 bins — enough resolution to separate the test voices cheaply.
    fn default() -> Self {
        Self::new(32)
    }
}

impl SpeakerEmbedder for MockSpeakerEmbedder {
    fn embed(&self, pcm: &[i16]) -> Result<Vec<f32>> {
        ensure!(!pcm.is_empty(), "cannot embed empty PCM");
        let n = pcm.len() as f32;
        let two_pi_over_sr = std::f32::consts::TAU / SAMPLE_RATE as f32;

        // Goertzel-style magnitude at each bin center: |Σ x[t]·e^{-j w t}|, averaged.
        let mut mags = vec![0.0f32; self.dims];
        for (b, &f) in self.freqs.iter().enumerate() {
            let w = two_pi_over_sr * f;
            let (mut re, mut im) = (0.0f32, 0.0f32);
            for (t, &s) in pcm.iter().enumerate() {
                let x = s as f32;
                let phase = w * t as f32;
                re += x * phase.cos();
                im -= x * phase.sin();
            }
            mags[b] = (re * re + im * im).sqrt() / n;
        }
        Ok(l2_normalize(mags))
    }

    fn dims(&self) -> usize {
        self.dims
    }
}

/// The real ECAPA-TDNN ONNX voiceprint embedder (speaker_id_plan.md Phase E),
/// behind the `speaker` feature so the default build carries no ONNX dependency.
///
/// Runs a local ONNX model with `tract` (pure Rust, no external runtime) over the
/// [`features`](crate::speaker::features) log-mel front-end and L2-normalizes the
/// output embedding. The model file (`AMBIENT_SPEAKER_MODEL_PATH`), its input
/// tensor layout, and the fbank parameters must match the exported model — the
/// values here follow the common 80-mel ECAPA contract and are the knobs to tune
/// during on-hardware calibration.
#[cfg(feature = "speaker")]
pub struct OnnxSpeakerEmbedder {
    model: tract_onnx::prelude::TypedRunnableModel<tract_onnx::prelude::TypedModel>,
    fbank: crate::speaker::features::FbankConfig,
    dims: usize,
}

#[cfg(feature = "speaker")]
impl OnnxSpeakerEmbedder {
    /// Load an ONNX speaker-embedding model from `path`, producing `dims`-length
    /// vectors. `fbank` describes the log-mel front-end the model expects.
    pub fn open(
        path: impl AsRef<std::path::Path>,
        dims: usize,
        fbank: crate::speaker::features::FbankConfig,
    ) -> Result<Self> {
        use tract_onnx::prelude::*;
        let model = tract_onnx::onnx()
            .model_for_path(path.as_ref())
            .with_context(|| format!("loading ONNX model {}", path.as_ref().display()))?
            .into_optimized()
            .context("optimizing ONNX model")?
            .into_runnable()
            .context("making ONNX model runnable")?;
        Ok(Self { model, fbank, dims })
    }
}

#[cfg(feature = "speaker")]
impl SpeakerEmbedder for OnnxSpeakerEmbedder {
    fn embed(&self, pcm: &[i16]) -> Result<Vec<f32>> {
        use tract_onnx::prelude::*;
        anyhow::ensure!(!pcm.is_empty(), "cannot embed empty PCM");
        let fb = crate::speaker::features::log_mel_fbank(pcm, &self.fbank);
        anyhow::ensure!(fb.frames > 0, "utterance too short for the fbank front-end");
        // [batch=1, frames, mels] — the common ECAPA input layout.
        let input = tract_ndarray::Array3::from_shape_vec((1, fb.frames, fb.n_mels), fb.data)
            .context("shaping fbank tensor")?
            .into_tensor();
        let out = self
            .model
            .run(tvec!(input.into()))
            .context("running ONNX speaker model")?;
        let emb: Vec<f32> = out[0]
            .to_array_view::<f32>()
            .context("reading ONNX embedding output")?
            .iter()
            .copied()
            .collect();
        Ok(l2_normalize(emb))
    }

    fn dims(&self) -> usize {
        self.dims
    }
}

/// L2-normalize a vector in place-ish (returns the normalized copy). A zero vector
/// (silence) is returned unchanged so cosine against it is a well-defined 0.
pub fn l2_normalize(mut v: Vec<f32>) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthesize `ms` of a pure tone at `hz` (mock "voice"): a deterministic
    /// 16 kHz sine so a given pitch always embeds to the same fingerprint.
    fn tone(hz: f32, ms: usize) -> Vec<i16> {
        let n = SAMPLE_RATE as usize * ms / 1000;
        (0..n)
            .map(|t| {
                let x = (std::f32::consts::TAU * hz * t as f32 / SAMPLE_RATE as f32).sin();
                (x * 8000.0) as i16
            })
            .collect()
    }

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn embedding_is_unit_length() {
        let e = MockSpeakerEmbedder::default();
        let v = e.embed(&tone(200.0, 1500)).unwrap();
        assert_eq!(v.len(), e.dims());
        assert!((cosine(&v, &v) - 1.0).abs() < 1e-4, "should be unit length");
    }

    #[test]
    fn same_voice_is_stable_different_voice_is_separable() {
        let e = MockSpeakerEmbedder::default();
        let sam_a = e.embed(&tone(180.0, 1500)).unwrap();
        let sam_b = e.embed(&tone(180.0, 2000)).unwrap(); // same pitch, different length
        let dana = e.embed(&tone(320.0, 1500)).unwrap();

        let same = cosine(&sam_a, &sam_b);
        let diff = cosine(&sam_a, &dana);
        assert!(same > 0.95, "same voice should match strongly (got {same})");
        assert!(diff < same - 0.3, "different voices should separate (got {diff})");
    }

    #[test]
    fn empty_pcm_errors() {
        assert!(MockSpeakerEmbedder::default().embed(&[]).is_err());
    }
}
