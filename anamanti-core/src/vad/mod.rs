//! End-of-speech voice-activity detection for the turn pipeline.
//!
//! The turn's end-of-speech *state machine* lives in
//! [`crate::orchestrator::Pipeline::stream_to_transcript`] (the onset debounce,
//! the `speech_started` latch, the `end_silence` hangover, and the
//! `no_speech_finalize` fallback). This module owns only the **per-chunk speech
//! decision** behind the [`SpeechGate`] trait, so the detector can be swapped
//! without touching that machinery. The committed default is [`EnergyGate`] (an
//! amplitude/RMS threshold); an opt-in Silero neural engine is planned — see
//! `plans/VadSileroPlan.md`.

pub mod energy;
#[cfg(feature = "vad-silero")]
pub mod silero;

pub use energy::EnergyGate;
#[cfg(feature = "vad-silero")]
pub use silero::{SileroGate, SileroModel};

/// Per-chunk speech decision for the turn's end-of-speech state machine.
///
/// The caller feeds each incoming device audio chunk to [`push`](SpeechGate::push)
/// and drives the onset debounce / hangover from the returned voiced flag. A gate
/// may buffer samples or carry model state across chunks (Silero is stateful), so
/// [`reset`](SpeechGate::reset) is called once at the start of every turn — a gate
/// must not assume it is freshly constructed.
pub trait SpeechGate: Send {
    /// Feed the next chunk of little-endian PCM16 **mono** audio at `sample_rate` Hz
    /// and report whether it counts as speech (vs room noise / silence). The energy
    /// gate ignores `sample_rate`; a frame-based neural gate (Silero) uses it to
    /// re-frame the (variable-length) device chunks to its fixed input window.
    fn push(&mut self, pcm: &[u8], sample_rate: u32) -> bool;

    /// Confidence of the most recent [`push`](SpeechGate::push) decision, in
    /// `0.0..=1.0` (the energy gate reports a hard `1.0`/`0.0`; a neural gate reports
    /// its probability). For logging / diagnostics only — the state machine keys off
    /// the boolean.
    fn prob(&self) -> f32;

    /// Reset any per-turn state (recurrent model state, framing carry, last prob) so
    /// a gate instance can be reused across turns. Called once at turn start.
    fn reset(&mut self);
}

/// Root-mean-square amplitude (in `i16` units) of a little-endian PCM16 buffer, the
/// metric behind [`EnergyGate`]. A trailing odd byte (never expected from a
/// well-formed frame) is ignored.
pub(crate) fn rms_i16_le(pcm: &[u8]) -> f64 {
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
mod tests {
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
