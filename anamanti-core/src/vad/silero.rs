//! The neural Silero VAD gate (feature `vad-silero`).
//!
//! Runs the stock Silero VAD **v4** ONNX graph on **onnxruntime** via the `ort`
//! crate. Pure-Rust `tract` cannot load Silero (its `If` control-flow op fails typed
//! translation), so the neural engine uses onnxruntime. This is Core/Mac-side only;
//! the device never runs a VAD. See `plans/VadSileroPlan.md`.
//!
//! **Why v4, not v5:** the v5 export (`silero_vad.onnx` monolithic, `state` tensor,
//! 512-sample window) produces a near-constant ~0 probability under `ort` regardless
//! of input — it never fires on real speech (verified on-device + offline against clean
//! `say` TTS and a captured far-field dump). The **v4** model
//! (`input`/`sr`/`h`/`c` → `output`/`hn`/`cn`, 1536-sample window) scores clean speech
//! ~0.999 and far-field device speech correctly, at natural gain. So the gate uses v4.
//!
//! Silero v4 is functional: the recurrent LSTM state is explicit tensor I/O (`h`/`c`
//! in → `hn`/`cn` out), so a single [`Session`] is stateless between runs and can be
//! **shared** across concurrently-running turns ([`SileroModel`], loaded once) while
//! each per-turn [`SileroGate`] carries its **own** state + framing buffer.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use ort::session::Session;
use ort::value::Tensor;

use super::SpeechGate;

/// Silero v4 @ 16 kHz frame size (samples). 1536 samples = 96 ms.
const WINDOW: usize = 1536;
/// Each recurrent state tensor (`h`, `c`) is `[2, 1, 64]` → 128 f32.
const STATE_LEN: usize = 2 * 64;
/// i16 → f32 normalization.
const I16_TO_F32: f32 = 1.0 / 32768.0;

/// The shared, loaded Silero model. Load once at boot; clone the `Arc` into each
/// per-turn [`SileroGate`]. The [`Session`] runs behind a `Mutex` because
/// `ort::Session::run` takes `&mut self`; inference is ~sub-ms so the brief
/// serialization across concurrent turns is negligible.
pub struct SileroModel {
    session: Mutex<Session>,
}

impl SileroModel {
    /// Load the Silero v4 ONNX model from `model_path`. Errors here are intended to be
    /// fatal at boot (a misconfigured `vad.engine="silero"` should fail loud, not
    /// silently fall back to the energy gate). The speech-probability *threshold* is
    /// not baked in here — it's supplied per-turn to [`SileroGate::new`] from the live
    /// settings snapshot, so it is A/B-tunable without reloading the model.
    pub fn load(model_path: impl AsRef<std::path::Path>) -> Result<Arc<Self>> {
        let path = model_path.as_ref();
        let session = Session::builder()
            .context("creating ort session builder")?
            .commit_from_file(path)
            .with_context(|| format!("loading Silero VAD model {}", path.display()))?;
        Ok(Arc::new(Self {
            session: Mutex::new(session),
        }))
    }

    /// Run one 1536-sample frame through the model, threading the `h`/`c` LSTM state in
    /// and out. Returns the speech probability; `h`/`c` are updated in place.
    fn infer(&self, frame: &[f32], h: &mut [f32], c: &mut [f32]) -> Result<f32> {
        debug_assert_eq!(frame.len(), WINDOW);
        let input = Tensor::from_array(([1_usize, WINDOW], frame.to_vec()))?;
        // `sr` is a rank-0 scalar in the Silero graph.
        let sr = Tensor::from_array(([0usize; 0], vec![16_000_i64]))?;
        let h_in = Tensor::from_array(([2_usize, 1, 64], h.to_vec()))?;
        let c_in = Tensor::from_array(([2_usize, 1, 64], c.to_vec()))?;

        let mut session = self.session.lock().expect("silero session mutex poisoned");
        let outputs = session
            .run(ort::inputs!["input" => input, "sr" => sr, "h" => h_in, "c" => c_in])
            .context("running Silero VAD inference")?;

        let (_, prob) = outputs["output"].try_extract_tensor::<f32>()?;
        let prob = prob[0];
        let (_, hn) = outputs["hn"].try_extract_tensor::<f32>()?;
        h.copy_from_slice(hn);
        let (_, cn) = outputs["cn"].try_extract_tensor::<f32>()?;
        c.copy_from_slice(cn);
        Ok(prob)
    }
}

/// Per-turn Silero gate. Re-frames the device's variable-length PCM chunks into fixed
/// 1536-sample windows, runs each through the shared [`SileroModel`], and reports the
/// most recent frame's speech decision. Holds its own recurrent state + framing carry,
/// so concurrent turns don't interfere.
pub struct SileroGate {
    model: Arc<SileroModel>,
    /// Speech-probability gate (`0.0..1.0`), from the live settings snapshot at the
    /// turn's start. A frame is voiced when `prob >= threshold`.
    threshold: f32,
    /// Carry buffer of not-yet-framed f32 samples.
    buf: Vec<f32>,
    /// Recurrent LSTM state (`h`/`c`, each `[2,1,64]` flattened), threaded across frames.
    h: Vec<f32>,
    c: Vec<f32>,
    last_prob: f32,
    /// Sticky decision returned for chunks that don't complete a new frame.
    last_voiced: bool,
}

impl SileroGate {
    /// Build a per-turn gate sharing `model`, gating at `threshold` (from the live
    /// settings snapshot — `vad.silero.threshold`, live-tunable).
    pub fn new(model: Arc<SileroModel>, threshold: f32) -> Self {
        Self {
            model,
            threshold,
            buf: Vec::with_capacity(WINDOW * 2),
            h: vec![0.0; STATE_LEN],
            c: vec![0.0; STATE_LEN],
            last_prob: 0.0,
            last_voiced: false,
        }
    }
}

impl SpeechGate for SileroGate {
    fn push(&mut self, pcm: &[u8], _sample_rate: u32) -> bool {
        // The device always streams 16 kHz mono PCM16 (`PCM_16K_MONO`), which is what
        // Silero v4's 1536-sample window assumes; `sample_rate` is accepted for the
        // trait contract but not resampled here.
        self.buf.extend(
            pcm.chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 * I16_TO_F32),
        );

        let mut frames = 0;
        while (frames + 1) * WINDOW <= self.buf.len() {
            let start = frames * WINDOW;
            let frame: [f32; WINDOW] = self.buf[start..start + WINDOW]
                .try_into()
                .expect("exact window slice");
            match self.model.infer(&frame, &mut self.h, &mut self.c) {
                Ok(prob) => {
                    self.last_prob = prob;
                    self.last_voiced = prob >= self.threshold;
                }
                Err(e) => {
                    // Model validated at boot; a runtime failure is unexpected. Keep the
                    // last decision rather than spuriously flipping the state machine.
                    log::error!("Silero inference failed: {e:#}");
                }
            }
            frames += 1;
        }
        if frames > 0 {
            self.buf.drain(0..frames * WINDOW);
        }
        self.last_voiced
    }

    fn prob(&self) -> f32 {
        self.last_prob
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.h.iter_mut().for_each(|s| *s = 0.0);
        self.c.iter_mut().for_each(|s| *s = 0.0);
        self.last_prob = 0.0;
        self.last_voiced = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODEL: &str = "models/silero_vad.onnx";

    fn model_present() -> bool {
        std::path::Path::new(MODEL).exists()
    }

    fn pcm_from_f32(samples: &[f32]) -> Vec<u8> {
        samples
            .iter()
            .flat_map(|&s| ((s * 32767.0) as i16).to_le_bytes())
            .collect()
    }

    #[test]
    fn loads_and_scores_frames_in_range() {
        if !model_present() {
            eprintln!("skipping: {MODEL} not present");
            return;
        }
        let model = SileroModel::load(MODEL).expect("load silero");
        let mut gate = SileroGate::new(model, 0.5);

        // Two windows of silence: probabilities must be valid and low.
        let silence = pcm_from_f32(&vec![0.0_f32; WINDOW * 2]);
        gate.push(&silence, 16_000);
        assert!(
            (0.0..=1.0).contains(&gate.prob()),
            "prob out of range: {}",
            gate.prob()
        );
        assert!(gate.prob() < 0.5, "silence must score low, got {}", gate.prob());
    }

    #[test]
    fn buffers_partial_frames_without_running() {
        if !model_present() {
            eprintln!("skipping: {MODEL} not present");
            return;
        }
        let model = SileroModel::load(MODEL).expect("load silero");
        let mut gate = SileroGate::new(model, 0.5);
        // Fewer than WINDOW samples: no frame completes, decision stays the initial false.
        let partial = pcm_from_f32(&vec![0.1_f32; 100]);
        assert!(!gate.push(&partial, 16_000));
        assert_eq!(gate.prob(), 0.0, "no frame ran yet");
        // Feeding the rest completes exactly one frame.
        let rest = pcm_from_f32(&vec![0.0_f32; WINDOW - 100]);
        gate.push(&rest, 16_000);
        assert!((0.0..=1.0).contains(&gate.prob()));
    }

    #[test]
    fn reset_zeroes_state_and_buffer() {
        if !model_present() {
            eprintln!("skipping: {MODEL} not present");
            return;
        }
        let model = SileroModel::load(MODEL).expect("load silero");
        let mut gate = SileroGate::new(model, 0.5);
        gate.push(&pcm_from_f32(&vec![0.2_f32; 300]), 16_000);
        gate.reset();
        assert!(gate.buf.is_empty());
        assert!(gate.h.iter().all(|&s| s == 0.0));
        assert!(gate.c.iter().all(|&s| s == 0.0));
        assert_eq!(gate.prob(), 0.0);
        assert!(!gate.last_voiced);
    }
}
