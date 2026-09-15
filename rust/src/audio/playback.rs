//! `cpal`-based speaker playback of returned TTS audio (Plan.MD §3, Phase 5;
//! architecture.md §2.1 SPEAKING).
//!
//! Playback is the symmetric twin of [`super::capture`]: the same `cpal` layer
//! that owns the mic also owns the speaker, so the returned Piper frames are
//! played through the Echo Show without a second audio stack. On the device
//! `cpal` drives the NDK's AAudio *output* backend; on the host it uses
//! coreaudio. Both are hidden behind `cpal`'s cross-platform API.
//!
//! Threading & real-time discipline mirror capture:
//! - The `cpal` output stream is `!Send`, so [`PlaybackStream`] is created on and
//!   kept alive by the engine thread (never moved across threads).
//! - The audio callback is the only real-time path: it just pops `f32` samples
//!   from a pre-allocated lock-free ring and writes them out, substituting
//!   silence on underrun. It never allocates, blocks, or locks.
//! - [`PlaybackSink`] is the `Send`/`Sync` producer handle given to the async
//!   Wyoming turn task. `submit_pcm` resamples the server's PCM (Piper is usually
//!   22.05 kHz mono) to the output device's rate/channel count and pushes it into
//!   the ring. `clear` flushes queued audio for barge-in.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, Stream, StreamConfig};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

use super::resample::Resampler;

/// Output ring capacity in `f32` samples: ~4 s at 48 kHz stereo. TTS replies are
/// short, but a generous buffer means the async turn task can hand off a whole
/// utterance without ever blocking the real-time output callback.
const PLAYBACK_RING_SAMPLES: usize = 48_000 * 2 * 4;

/// Describes the output stream that was actually opened.
#[derive(Debug, Clone)]
pub struct PlaybackInfo {
    pub device_name: String,
    pub sample_rate: u32,
    pub channels: u16,
}

/// A live output stream. Holding this value keeps playback running; dropping it
/// stops it. `cpal::Stream` is `!Send`, so this must be created and dropped on the
/// thread that owns the engine loop (the same one that owns capture).
pub struct PlaybackStream {
    _stream: Stream,
    pub info: PlaybackInfo,
}

/// The producer side of playback: resamples and enqueues PCM for the output
/// callback to drain. Cheaply shareable (`Arc<PlaybackSink>`) and `Send`/`Sync`,
/// so the async turn task can push TTS frames while the `!Send` stream stays put.
pub struct PlaybackSink {
    inner: Mutex<SinkState>,
    /// Set by [`PlaybackSink::clear`]; the output callback observes it and drains
    /// the ring to silence the current utterance (barge-in).
    flush: std::sync::Arc<AtomicBool>,
    out_rate: u32,
    out_channels: u16,
}

struct SinkState {
    prod: HeapProd<f32>,
    /// The resampler for the current source rate; rebuilt when the rate changes
    /// (each TTS `audio-start` announces its own rate).
    resampler: Option<(u32, Resampler)>,
    in_f32: Vec<f32>,
    out_f32: Vec<f32>,
}

impl PlaybackSink {
    /// Convert and enqueue a mono `i16` PCM block sampled at `src_rate`. Resamples
    /// to the output device rate and fans the mono sample out across every output
    /// channel. Never blocks: if the ring is full the excess is dropped.
    pub fn submit_pcm(&self, samples: &[i16], src_rate: u32) {
        if samples.is_empty() {
            return;
        }
        let mut st = self.inner.lock().unwrap();
        if st.resampler.as_ref().map(|(r, _)| *r) != Some(src_rate) {
            st.resampler = Some((src_rate, Resampler::new(src_rate, self.out_rate)));
        }

        let SinkState {
            prod,
            resampler,
            in_f32,
            out_f32,
        } = &mut *st;

        in_f32.clear();
        in_f32.extend(samples.iter().map(|&s| s as f32 / 32_768.0));
        out_f32.clear();
        let (_, r) = resampler.as_mut().expect("resampler set above");
        r.process(in_f32, out_f32);

        for &s in out_f32.iter() {
            for _ in 0..self.out_channels.max(1) {
                if prod.try_push(s).is_err() {
                    return; // ring full — drop the rest of this block
                }
            }
        }
    }

    /// Flush any queued audio so the next callback outputs silence. Used for
    /// barge-in: when the user speaks over the reply, the current utterance is cut.
    pub fn clear(&self) {
        self.flush.store(true, Ordering::SeqCst);
    }

    /// Test-only constructor: builds a sink backed by a standalone ring and hands
    /// back the consumer so tests can assert what would be played without opening
    /// a real audio device.
    #[cfg(test)]
    pub fn for_test(out_rate: u32, out_channels: u16, capacity: usize) -> (Self, HeapCons<f32>) {
        let (prod, cons) = HeapRb::<f32>::new(capacity).split();
        let sink = Self {
            inner: Mutex::new(SinkState {
                prod,
                resampler: None,
                in_f32: Vec::new(),
                out_f32: Vec::new(),
            }),
            flush: std::sync::Arc::new(AtomicBool::new(false)),
            out_rate,
            out_channels,
        };
        (sink, cons)
    }
}

/// Open the default output device and start a silent stream that plays whatever
/// [`PlaybackSink::submit_pcm`] enqueues. Returns the (`!Send`) stream to keep
/// alive on the engine thread plus the shareable sink for the turn task.
pub fn start_playback() -> Result<(PlaybackStream, PlaybackSink)> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default output device (are speakers available?)"))?;
    let device_name = device.to_string();

    let supported = device
        .default_output_config()
        .context("querying default output config")?;
    let sample_format = supported.sample_format();
    let config: StreamConfig = supported.config();
    let channels = config.channels;
    let sample_rate = config.sample_rate;

    let (prod, cons) = HeapRb::<f32>::new(PLAYBACK_RING_SAMPLES).split();
    let flush = std::sync::Arc::new(AtomicBool::new(false));

    let stream = match sample_format {
        SampleFormat::F32 => build_output::<f32>(&device, &config, cons, flush.clone())?,
        SampleFormat::I16 => build_output::<i16>(&device, &config, cons, flush.clone())?,
        SampleFormat::U16 => build_output::<u16>(&device, &config, cons, flush.clone())?,
        SampleFormat::I32 => build_output::<i32>(&device, &config, cons, flush.clone())?,
        SampleFormat::F64 => build_output::<f64>(&device, &config, cons, flush.clone())?,
        other => return Err(anyhow!("unsupported output sample format: {other:?}")),
    };
    stream.play().context("starting playback stream")?;

    let info = PlaybackInfo {
        device_name,
        sample_rate,
        channels,
    };
    let sink = PlaybackSink {
        inner: Mutex::new(SinkState {
            prod,
            resampler: None,
            in_f32: Vec::new(),
            out_f32: Vec::new(),
        }),
        flush,
        out_rate: sample_rate,
        out_channels: channels,
    };
    Ok((
        PlaybackStream {
            _stream: stream,
            info,
        },
        sink,
    ))
}

/// Build an output stream for sample type `T`, pulling queued `f32` samples and
/// converting to `T`. On underrun (ring empty) it writes silence, so the stream
/// never stalls or clicks between utterances.
fn build_output<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    mut cons: HeapCons<f32>,
    flush: std::sync::Arc<AtomicBool>,
) -> Result<Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let data_callback = move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
        // Barge-in: drop everything still queued before writing this buffer.
        if flush.swap(false, Ordering::SeqCst) {
            while cons.try_pop().is_some() {}
        }
        for slot in data.iter_mut() {
            let sample = cons.try_pop().unwrap_or(0.0);
            *slot = T::from_sample(sample);
        }
    };
    let error_callback = |err| log::error!("audio playback stream error: {err}");
    device
        .build_output_stream(*config, data_callback, error_callback, None)
        .context("building output stream")
        .map_err(|e| anyhow!(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_mono_enqueues_same_samples() {
        // Output at the source rate, single channel → samples pass straight
        // through (normalized to f32 and back is exact for these small values).
        let (sink, mut cons) = PlaybackSink::for_test(16_000, 1, 1024);
        sink.submit_pcm(&[0, 16_384, -16_384, 32_767], 16_000);

        let mut got = Vec::new();
        while let Some(s) = cons.try_pop() {
            got.push(s);
        }
        assert_eq!(got.len(), 4);
        assert!((got[0] - 0.0).abs() < 1e-6);
        assert!((got[1] - 0.5).abs() < 1e-6);
        assert!((got[2] + 0.5).abs() < 1e-6);
    }

    #[test]
    fn mono_is_fanned_out_to_every_output_channel() {
        // Two output channels → each mono sample is duplicated L/R.
        let (sink, mut cons) = PlaybackSink::for_test(16_000, 2, 1024);
        sink.submit_pcm(&[16_384, -16_384], 16_000);

        let mut got = Vec::new();
        while let Some(s) = cons.try_pop() {
            got.push(s);
        }
        assert_eq!(got.len(), 4);
        assert!((got[0] - 0.5).abs() < 1e-6);
        assert!((got[1] - 0.5).abs() < 1e-6); // duplicated
        assert!((got[2] + 0.5).abs() < 1e-6);
        assert!((got[3] + 0.5).abs() < 1e-6); // duplicated
    }

    #[test]
    fn upsampling_produces_more_samples_than_input() {
        // 16 kHz source into a 48 kHz mono output ≈ 3× as many samples.
        let (sink, mut cons) = PlaybackSink::for_test(48_000, 1, 8192);
        let input = vec![1000i16; 160]; // 10 ms at 16 kHz
        sink.submit_pcm(&input, 16_000);

        let mut n = 0;
        while cons.try_pop().is_some() {
            n += 1;
        }
        assert!(n > 400, "expected ~3x upsample, got {n}");
    }

    #[test]
    fn full_ring_drops_excess_without_panicking() {
        let (sink, mut cons) = PlaybackSink::for_test(16_000, 1, 8);
        sink.submit_pcm(&[7i16; 100], 16_000); // far more than 8 slots
        let mut n = 0;
        while cons.try_pop().is_some() {
            n += 1;
        }
        assert_eq!(n, 8, "ring holds at most its capacity");
    }
}
