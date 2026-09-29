//! The energy / RMS speech gate — the committed default VAD engine.

use super::{rms_i16_le, SpeechGate};

/// Amplitude-threshold speech gate: a chunk is speech when its RMS exceeds
/// `threshold` (`voice_rms_threshold`, i16 units). This preserves the historical
/// Anamanti Core VAD byte-for-byte behind the [`SpeechGate`] seam, so it stays the
/// default engine. Stateless apart from caching the last decision's confidence for
/// [`prob`](SpeechGate::prob).
pub struct EnergyGate {
    threshold: f64,
    last_prob: f32,
}

impl EnergyGate {
    /// Build a gate that trips above `threshold` RMS (i16 units, i.e.
    /// `voice_rms_threshold`).
    pub fn new(threshold: f64) -> Self {
        Self {
            threshold,
            last_prob: 0.0,
        }
    }
}

impl SpeechGate for EnergyGate {
    fn push(&mut self, pcm: &[u8], _sample_rate: u32) -> bool {
        // Byte-for-byte the legacy inline decision:
        // `rms_i16_le(&pcm) > voice_rms_threshold`.
        let voiced = rms_i16_le(pcm) > self.threshold;
        self.last_prob = if voiced { 1.0 } else { 0.0 };
        voiced
    }

    fn prob(&self) -> f32 {
        self.last_prob
    }

    fn reset(&mut self) {
        self.last_prob = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn trips_above_threshold_only() {
        let mut gate = EnergyGate::new(180.0);
        // A loud speech-level signal is voiced; a quiet noise floor is not.
        assert!(gate.push(&pcm(&[800, -600, 700, -900]), 16_000));
        assert_eq!(gate.prob(), 1.0);
        assert!(!gate.push(&pcm(&[30, -30, 25, -20]), 16_000));
        assert_eq!(gate.prob(), 0.0);
    }

    #[test]
    fn reset_clears_last_prob() {
        let mut gate = EnergyGate::new(180.0);
        assert!(gate.push(&pcm(&[1000, -1000, 1000, -1000]), 16_000));
        assert_eq!(gate.prob(), 1.0);
        gate.reset();
        assert_eq!(gate.prob(), 0.0);
    }

    #[test]
    fn matches_legacy_rms_gate_semantics() {
        // Parity guard: the gate must equal the original inline
        // `rms_i16_le(&pcm) > voice_rms_threshold` for every case.
        let threshold = 180.0;
        let mut gate = EnergyGate::new(threshold);
        for samples in [
            vec![0i16, 0, 0, 0],
            vec![200, -200, 190, -210],
            vec![50, -40, 60, -55],
            vec![800, -600, 700, -900],
        ] {
            let bytes = pcm(&samples);
            assert_eq!(
                gate.push(&bytes, 16_000),
                rms_i16_le(&bytes) > threshold,
                "gate disagreed with legacy rms decision for {samples:?}"
            );
        }
    }
}
