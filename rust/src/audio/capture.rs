//! `cpal`-based microphone capture (Plan.MD §3, Phase 2; architecture.md §2.1).
//!
//! On the Echo Show (LineageOS, Android 11) `cpal` drives the NDK's AAudio input
//! backend; on the macOS host it uses coreaudio. Both are hidden behind `cpal`'s
//! cross-platform API, so this code is identical on device and host — only the
//! backend differs. (Note: `RECORD_AUDIO` must be granted in the Android
//! manifest for capture to start on the device.)
//!
//! The capture callback is the one truly real-time path in the engine: it runs
//! on the backend's audio thread and must not allocate or block. It therefore
//! only (1) downmixes interleaved frames to a single mono `i16` and (2) pushes
//! into the pre-allocated ring buffer with a lock-free `try_push`. Sample-rate
//! conversion and inference happen downstream on the consumer thread.

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig};
use ringbuf::traits::Producer;

use super::ring_buffer::AudioProducer;

/// Describes the capture stream that was actually opened, so the consumer can
/// configure its resampler and the UI can show what the mic is delivering.
#[derive(Debug, Clone)]
pub struct CaptureInfo {
    pub device_name: String,
    pub sample_rate: u32,
    pub channels: u16,
}

/// A live capture stream. Holding this value keeps the stream running; dropping
/// it stops capture. `cpal::Stream` is `!Send` on some backends, so this must be
/// created and dropped on the same thread that owns the engine loop.
pub struct CaptureStream {
    _stream: Stream,
    pub info: CaptureInfo,
}

/// Open the default input device and begin streaming mono `i16` samples into
/// `producer` at the device's native rate.
pub fn start_capture(producer: AudioProducer) -> Result<CaptureStream> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow!("no default input device (is a microphone available?)"))?;
    // cpal 0.18 exposes the device name via `Display` (structured metadata is in
    // `description()`); `to_string()` is the documented way to get the name.
    let device_name = device.to_string();

    let supported = device
        .default_input_config()
        .context("querying default input config")?;
    let sample_format = supported.sample_format();
    let config: StreamConfig = supported.config();
    let channels = config.channels;
    let sample_rate = config.sample_rate;

    let info = CaptureInfo {
        device_name,
        sample_rate,
        channels,
    };

    // Monomorphize the callback for the device's native sample format, then
    // funnel everything through the same mono-`i16` downmix.
    let stream = match sample_format {
        SampleFormat::I16 => build_stream::<i16>(&device, &config, producer)?,
        SampleFormat::U16 => build_stream::<u16>(&device, &config, producer)?,
        SampleFormat::I8 => build_stream::<i8>(&device, &config, producer)?,
        SampleFormat::U8 => build_stream::<u8>(&device, &config, producer)?,
        SampleFormat::I32 => build_stream::<i32>(&device, &config, producer)?,
        SampleFormat::U32 => build_stream::<u32>(&device, &config, producer)?,
        SampleFormat::F32 => build_stream::<f32>(&device, &config, producer)?,
        SampleFormat::F64 => build_stream::<f64>(&device, &config, producer)?,
        other => return Err(anyhow!("unsupported sample format: {other:?}")),
    };

    stream.play().context("starting capture stream")?;
    Ok(CaptureStream {
        _stream: stream,
        info,
    })
}

/// Build an input stream for sample type `T`, downmixing to mono `i16` in the
/// real-time callback and pushing into the ring buffer without allocating.
fn build_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    mut producer: AudioProducer,
) -> Result<Stream>
where
    T: SizedSample,
    i16: FromSample<T>,
{
    let channels = config.channels.max(1) as usize;

    let data_callback = move |data: &[T], _: &cpal::InputCallbackInfo| {
        // Interleaved frames -> one mono sample each, averaged in i32 to avoid
        // overflow, then pushed. `try_push` is lock-free; on overrun the sample
        // is dropped (back-pressure) rather than blocking the audio thread.
        for frame in data.chunks(channels) {
            let mut acc: i32 = 0;
            for &sample in frame {
                acc += i16::from_sample(sample) as i32;
            }
            let mono = (acc / channels as i32) as i16;
            let _ = producer.try_push(mono);
        }
    };

    let error_callback = |err| {
        log::error!("audio capture stream error: {err}");
    };

    device
        .build_input_stream(*config, data_callback, error_callback, None)
        .context("building input stream")
        .map_err(|e| anyhow!(e))
}

/// Simple RMS level (0.0..~1.0) over a block of `f32` samples in `i16` range,
/// used to prove capture is live even before a wake-word model is loaded.
pub fn rms_level(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    let rms = (sum_sq / samples.len() as f64).sqrt();
    (rms / i16::MAX as f64) as f32
}
