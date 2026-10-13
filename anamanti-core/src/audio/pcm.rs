//! Little-endian PCM16 helpers. The device streams (and Piper returns) raw
//! little-endian `i16` mono PCM; this is the single place where bytes become
//! samples, samples become `f32`, and a byte count becomes a duration.

use std::time::Duration;

/// Bytes per PCM16 sample.
pub const BYTES_PER_SAMPLE: usize = 2;

/// Scale factor from an `i16` sample to `f32` in `[-1.0, 1.0)` (the range Silero and
/// whisper.cpp expect).
pub const I16_TO_F32: f32 = 1.0 / 32768.0;

/// Decode little-endian PCM16 bytes into `i16` samples. A trailing odd byte (never
/// expected from a well-formed frame) is ignored.
pub fn iter_i16_le(bytes: &[u8]) -> impl Iterator<Item = i16> + '_ {
    bytes
        .chunks_exact(BYTES_PER_SAMPLE)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
}

/// Convert one `i16` sample to `f32` in `[-1.0, 1.0)`.
pub fn i16_to_f32(sample: i16) -> f32 {
    sample as f32 * I16_TO_F32
}

/// Wall-clock duration of a mono PCM16 buffer of `byte_len` bytes at `sample_rate`
/// Hz. Zero when the rate is unknown (`0`), so callers never divide by zero.
pub fn chunk_duration(byte_len: usize, sample_rate: u32) -> Duration {
    if sample_rate == 0 {
        return Duration::ZERO;
    }
    Duration::from_secs_f64((byte_len / BYTES_PER_SAMPLE) as f64 / sample_rate as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_little_endian_and_drops_a_trailing_odd_byte() {
        let bytes = [0x01, 0x00, 0xff, 0xff, 0x00, 0x80, 0x7f];
        let samples: Vec<i16> = iter_i16_le(&bytes).collect();
        assert_eq!(samples, vec![1, -1, i16::MIN]);
    }

    #[test]
    fn scales_to_unit_range() {
        assert_eq!(i16_to_f32(0), 0.0);
        assert_eq!(i16_to_f32(i16::MIN), -1.0);
        assert!(i16_to_f32(i16::MAX) < 1.0);
    }

    #[test]
    fn chunk_duration_is_bytes_over_rate() {
        // 640 bytes = 320 samples; at 16 kHz that is 20 ms.
        assert_eq!(chunk_duration(640, 16_000), Duration::from_millis(20));
        // Unknown rate contributes nothing (avoids div-by-zero).
        assert_eq!(chunk_duration(640, 0), Duration::ZERO);
    }
}
