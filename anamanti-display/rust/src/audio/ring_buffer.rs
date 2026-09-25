//! Pre-allocated, lock-light ring buffer between the audio capture callback and
//! the wake-word consumer (Plan.MD §3, Phase 2; architecture.md §2.1, §6).
//!
//! The capture callback runs on a real-time audio thread owned by the OS audio
//! backend — it must never allocate or block. We use a single-producer /
//! single-consumer heap ring buffer (`ringbuf::HeapRb`) that is allocated *once*
//! up front; from then on the producer (capture callback) and consumer
//! (inference loop) exchange samples with only atomic index updates — no locks,
//! no per-frame heap churn. This respects the Echo Show's ~1 GB RAM budget and
//! its lack of headroom for GC-style pauses.

use ringbuf::traits::Split;
use ringbuf::{HeapCons, HeapProd, HeapRb};

/// PCM sample type flowing through the engine: signed 16-bit, matching the
/// Wyoming wire format and the openWakeWord model input domain.
pub type Sample = i16;

/// Producer half handed to the real-time capture callback. `push_slice` /
/// `try_push` never allocate and never block.
pub type AudioProducer = HeapProd<Sample>;

/// Consumer half drained by the wake-word inference loop.
pub type AudioConsumer = HeapCons<Sample>;

/// Allocate the shared audio ring buffer once and split it into its producer and
/// consumer halves.
///
/// `capacity_samples` is the maximum number of mono samples buffered before the
/// producer starts dropping (back-pressure). Sizing for ~1–2 s of device-rate
/// audio comfortably absorbs scheduling jitter on the inference thread while
/// staying tiny in absolute terms (e.g. 48 kHz × 2 s × 2 bytes ≈ 192 KB).
pub fn new_audio_ring(capacity_samples: usize) -> (AudioProducer, AudioConsumer) {
    HeapRb::<Sample>::new(capacity_samples).split()
}
