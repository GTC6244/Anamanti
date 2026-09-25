//! Native audio layer for the anamanti-display engine (Plan.MD §3, Phase 2).
//!
//! Everything real-time and resource-sensitive on the Echo Show lives in Rust
//! (see `architecture.md` §2.1). This module owns audio *capture*: pulling raw
//! PCM off the mic array with `cpal`, downmixing to mono, and decoupling the
//! real-time capture callback from the wake-word consumer via a pre-allocated
//! ring buffer.
//!
//! Playback (returned TTS frames) is the symmetric twin and lands in Phase 5
//! ([`playback`]); it reuses the same `cpal` layer and the [`resample`] path.

pub mod capture;
/// Kotlin `AudioRecord` capture bridge — Android only (VOICE_RECOGNITION source +
/// platform effects, reached through JNI since `cpal` can't request an input preset).
#[cfg(target_os = "android")]
pub mod mic_bridge;
pub mod playback;
pub mod resample;
pub mod ring_buffer;

/// The sample rate the wake-word pipeline (and, later, the Wyoming STT stream)
/// expects: 16 kHz. openWakeWord models are trained on 16 kHz mono audio.
pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// Wake-word inference and STT both work on a single mono channel; the mic
/// array is downmixed to mono in the capture callback.
pub const TARGET_CHANNELS: u16 = 1;
