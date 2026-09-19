//! Host-side WebRTC Audio Processing (APM) stage — AEC3 + noise suppression
//! (Phase 0 spike / Phase 2 live path; see `snazzy-hopping-gadget.md` plan).
//!
//! ## Why this runs on the Mac, not the Echo Show
//!
//! The Echo Show 8 is 32-bit `armeabi-v7a` (Android 11, ~1 GB RAM). The
//! `webrtc-audio-processing` `bundled` build compiles vendored libwebrtc via
//! meson/ninja and does **not** cross-compile to `armv7-linux-androideabi` out of
//! the box (its build script emits no meson Android cross-file, so meson builds
//! abseil for the host machine and the link fails). Rather than patch that, the
//! plan streams the device mic (near-end) **and** the render reference (far-end,
//! tapped at the speaker DAC callback) to the orchestrator, which runs the APM
//! here where it builds and runs trivially. Validated in the Phase 0 spike:
//! ~23.7 dB ERLE on a synthetic echo at 16 kHz mono.
//!
//! ## Contract
//!
//! The APM works on fixed **10 ms** frames of **non-interleaved f32** in `[-1, 1]`.
//! At [`APM_SAMPLE_RATE`] that is [`AecProcessor::frame_len`] = 160 samples.
//! Callers feed the far-end frame with [`AecProcessor::process_render`] and then
//! the matching near-end (mic) frame with [`AecProcessor::process_capture`], which
//! cleans the mic in place. AEC3 estimates the render↔capture delay on its own
//! (see the plan's "render reference must be real DAC output, time-aligned"), so
//! the caller only needs to keep the two streams roughly aligned and correctly
//! ordered (render before the capture it should cancel from).
//!
//! Samples cross the wire as little-endian `i16` (the Wyoming `audio-chunk`
//! format), so the public API is `i16`-in / `i16`-out; the `[-1, 1]` f32
//! conversion is internal and matches the device's own normalization
//! (`rust/src/audio/playback.rs`, `s as f32 / 32_768.0`).

use anyhow::{anyhow, Result};
use webrtc_audio_processing::config::{
    AdaptiveDigital, Config, EchoCanceller, FixedDigital, GainController, GainController2,
    NoiseSuppression, NoiseSuppressionLevel,
};
use webrtc_audio_processing::Processor;

/// The rate the APM runs at, matching the device's wake-word/STT path
/// (`rust/src/audio/mod.rs` `TARGET_SAMPLE_RATE`). WebRTC fixes the frame to
/// 10 ms, so one frame is `APM_SAMPLE_RATE / 100` samples.
pub const APM_SAMPLE_RATE: u32 = 16_000;

/// Scale between `i16` PCM and the APM's `[-1, 1]` f32 samples.
const I16_SCALE: f32 = 1.0 / 32_768.0;

/// A configured WebRTC Audio Processing stage for one mono near/far stream pair.
///
/// Holds pre-allocated single-channel scratch buffers so steady-state processing
/// does not allocate. Not `Sync`; drive it from one task/thread.
pub struct AecProcessor {
    inner: Processor,
    frame: usize,
    /// One mono channel of far-end (render) scratch.
    render: Vec<Vec<f32>>,
    /// One mono channel of near-end (capture) scratch.
    capture: Vec<Vec<f32>>,
}

impl AecProcessor {
    /// Build a mono 16 kHz APM stage with full AEC3 + high noise suppression and
    /// automatic delay estimation — the configuration validated in the Phase 0
    /// spike. AGC is intentionally left off here: WebRTC's analog AGC requires
    /// feeding back an analog level every frame, which is a live-path concern for
    /// Phase 2, not the echo/noise core.
    pub fn new_16k_mono() -> Result<Self> {
        Self::with_noise_suppression(NoiseSuppressionLevel::High)
    }

    /// Like [`Self::new_16k_mono`] but additionally enables the digital automatic
    /// gain controller (GainController2 adaptive-digital), which brings the Echo
    /// Show's quiet far-field speech up to a target level *after* echo/noise
    /// removal. Uses the digital-only path (no analog HAL level feedback), so it
    /// needs no per-frame plumbing and no device changes — the no-root fix for the
    /// low mic level (real captures sit at ~−30..−37 dB RMS).
    pub fn new_16k_mono_with_agc() -> Result<Self> {
        Self::build(NoiseSuppressionLevel::High, true)
    }

    /// Like [`Self::new_16k_mono`] but with a caller-chosen suppression level, so
    /// Phase 2 can expose it through the existing settings frames for on-device
    /// A/B tuning.
    pub fn with_noise_suppression(level: NoiseSuppressionLevel) -> Result<Self> {
        Self::build(level, false)
    }

    fn build(ns_level: NoiseSuppressionLevel, agc: bool) -> Result<Self> {
        let inner = Processor::new(APM_SAMPLE_RATE)
            .map_err(|e| anyhow!("failed to create WebRTC APM: {e:?}"))?;
        inner.set_config(Config {
            // Full AEC3; `None` delay lets the estimator find the render↔capture
            // offset, which is what we want given network + playback buffering.
            echo_canceller: Some(EchoCanceller::Full {
                stream_delay_ms: None,
            }),
            noise_suppression: Some(NoiseSuppression {
                level: ns_level,
                analyze_linear_aec_output: false,
            }),
            gain_controller: agc.then(|| {
                GainController::GainController2(GainController2 {
                    input_volume_controller_enabled: false,
                    adaptive_digital: Some(AdaptiveDigital::default()),
                    fixed_digital: FixedDigital::default(),
                })
            }),
            ..Default::default()
        });
        let frame = inner.num_samples_per_frame();
        Ok(Self {
            inner,
            frame,
            render: vec![vec![0.0f32; frame]],
            capture: vec![vec![0.0f32; frame]],
        })
    }

    /// Samples per 10 ms frame at [`APM_SAMPLE_RATE`] (160 at 16 kHz). Every
    /// `process_*` call requires exactly this many samples.
    pub fn frame_len(&self) -> usize {
        self.frame
    }

    /// Feed one 10 ms far-end (render / what's played out the speaker) frame so the
    /// canceller has a reference for the echo it will later remove from the mic.
    pub fn process_render(&mut self, pcm: &[i16]) -> Result<()> {
        self.check_len(pcm.len())?;
        for (dst, &s) in self.render[0].iter_mut().zip(pcm) {
            *dst = s as f32 * I16_SCALE;
        }
        self.inner
            .process_render_frame(&mut self.render)
            .map_err(|e| anyhow!("APM render frame failed: {e:?}"))
    }

    /// Process one 10 ms near-end (mic) frame **in place**: echo and noise are
    /// removed and the cleaned samples are written back into `pcm`.
    pub fn process_capture(&mut self, pcm: &mut [i16]) -> Result<()> {
        self.check_len(pcm.len())?;
        for (dst, &s) in self.capture[0].iter_mut().zip(pcm.iter()) {
            *dst = s as f32 * I16_SCALE;
        }
        self.inner
            .process_capture_frame(&mut self.capture)
            .map_err(|e| anyhow!("APM capture frame failed: {e:?}"))?;
        for (out, &s) in pcm.iter_mut().zip(self.capture[0].iter()) {
            // Back to i16 with clamping (the APM can slightly overshoot ±1.0).
            *out = (s.clamp(-1.0, 1.0) * 32_767.0).round() as i16;
        }
        Ok(())
    }

    fn check_len(&self, got: usize) -> Result<()> {
        if got != self.frame {
            return Err(anyhow!(
                "APM needs exactly one 10ms frame ({} samples), got {got}",
                self.frame
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeding the APM a strong synthetic echo (a scaled copy of the render
    /// reference as the mic frame) must attenuate it by a wide margin. This is the
    /// in-repo, dependency-free version of the Phase 0 spike (which hit ~23.7 dB).
    #[test]
    fn cancels_synthetic_echo() {
        let mut apm = AecProcessor::new_16k_mono().expect("build APM");
        let n = apm.frame_len();
        assert_eq!(n, 160, "16 kHz => 160 samples per 10 ms frame");

        let mut energy_in = 0.0f64;
        let mut energy_out = 0.0f64;
        // Run long enough for AEC3's adaptive filter + delay estimator to converge.
        for blk in 0..200 {
            // Render = a 300 Hz tone (the far-end / speaker signal).
            let mut render = vec![0i16; n];
            for (i, s) in render.iter_mut().enumerate() {
                let t = (blk * n + i) as f32 / APM_SAMPLE_RATE as f32;
                *s = ((2.0 * std::f32::consts::PI * 300.0 * t).sin() * 8000.0) as i16;
            }
            apm.process_render(&render).expect("render");

            // Capture = an attenuated echo of exactly that render frame.
            let mut capture: Vec<i16> = render.iter().map(|&s| (s as f32 * 0.6) as i16).collect();
            // Measure only after the filter has had time to adapt.
            if blk >= 150 {
                for &s in &capture {
                    energy_in += (s as f64) * (s as f64);
                }
            }
            apm.process_capture(&mut capture).expect("capture");
            if blk >= 150 {
                for &s in &capture {
                    energy_out += (s as f64) * (s as f64);
                }
            }
        }

        let erle_db = 10.0 * (energy_in / energy_out.max(1e-9)).log10();
        assert!(
            erle_db > 10.0,
            "expected the echo to be attenuated by >10 dB after convergence, got {erle_db:.1} dB \
             (in={energy_in:.1} out={energy_out:.1})"
        );
    }

    #[test]
    fn rejects_wrong_frame_length() {
        let mut apm = AecProcessor::new_16k_mono().expect("build APM");
        let bad = vec![0i16; apm.frame_len() + 1];
        assert!(apm.process_render(&bad).is_err());
    }
}
