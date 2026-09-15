//! Engine orchestration: wires capture -> ring buffer -> resample -> wake-word
//! inference on a single low-overhead background thread, and (Phase 3) drives a
//! Wyoming turn when the wake word fires (Plan.MD §3, Phases 2–3;
//! architecture.md §2.1, §4).
//!
//! One dedicated thread owns the `cpal` stream (which is `!Send` on some
//! backends) and runs the inference loop. The real-time audio callback only
//! feeds the ring buffer; all model work happens here, off that hot path.
//!
//! Phase 3 adds the network side without touching that thread's `!Send`
//! constraint: [`net::Network`] owns a small tokio runtime, and the inference
//! loop hands it wake-word triggers and (while a turn is active) resampled PCM
//! over channels. Wake-word scoring keeps running during a turn (full-duplex);
//! the engine just raises the confidence bar while a turn is active to suppress
//! self-triggers (the AEC-interim mitigation, Plan.MD §4).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Result;
use ringbuf::traits::Consumer;

use crate::frb_generated::StreamSink;

use crate::api::engine::{WakeWordConfig, WakeWordEvent};
use crate::audio::capture::{self, start_capture};
use crate::audio::playback::start_playback;
use crate::audio::resample::Resampler;
use crate::audio::ring_buffer::new_audio_ring;
use crate::audio::TARGET_SAMPLE_RATE;
use crate::wakeword::{WakeWordDetector, WakeWordModelPaths};

mod net;
use net::Network;

/// Ring capacity in samples (~2 s at 48 kHz; ≈192 KB of `i16`). Absorbs
/// inference-thread scheduling jitter while staying tiny in the RAM budget.
const RING_CAPACITY_SAMPLES: usize = 96_000;

/// How many samples to drain from the ring per loop iteration.
const DRAIN_CHUNK: usize = 4096;

/// Idle poll interval when the ring is momentarily empty.
const IDLE_POLL: Duration = Duration::from_millis(10);

/// Emit an audio-level event roughly this often (in drained blocks) when running
/// in capture-only mode (no wake-word model loaded).
const LEVEL_EVERY_N_BLOCKS: u32 = 10;

struct EngineHandle {
    running: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

static ENGINE: OnceLock<Mutex<Option<EngineHandle>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<EngineHandle>> {
    ENGINE.get_or_init(|| Mutex::new(None))
}

/// True while a wake-word engine thread is active.
pub fn is_running() -> bool {
    slot()
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|h| h.running.load(Ordering::SeqCst))
}

/// Stop the running engine (if any) and join its thread. Idempotent.
pub fn stop() {
    let handle = slot().lock().unwrap().take();
    if let Some(mut h) = handle {
        h.running.store(false, Ordering::SeqCst);
        if let Some(join) = h.join.take() {
            let _ = join.join();
        }
    }
}

/// Start capture + wake-word scoring, streaming events to Dart via `sink`.
/// Replaces any engine already running.
pub fn start(config: WakeWordConfig, sink: StreamSink<WakeWordEvent>) -> Result<()> {
    stop();

    let running = Arc::new(AtomicBool::new(true));
    let loop_running = running.clone();
    let join = thread::Builder::new()
        .name("wakeword-engine".to_string())
        .spawn(move || run_loop(config, sink, loop_running))?;

    *slot().lock().unwrap() = Some(EngineHandle {
        running,
        join: Some(join),
    });
    Ok(())
}

/// The engine thread body: owns the capture stream and inference state.
fn run_loop(config: WakeWordConfig, sink: StreamSink<WakeWordEvent>, running: Arc<AtomicBool>) {
    let (producer, mut consumer) = new_audio_ring(RING_CAPACITY_SAMPLES);

    let capture = match start_capture(producer) {
        Ok(c) => c,
        Err(e) => {
            let _ = sink.add(WakeWordEvent::error(format!(
                "failed to start audio capture: {e}"
            )));
            running.store(false, Ordering::SeqCst);
            return;
        }
    };
    let info = capture.info.clone();
    let _ = sink.add(WakeWordEvent::started(
        info.device_name.clone(),
        info.sample_rate,
        info.channels,
    ));

    // Load the wake-word model chain; on failure, degrade to capture-only so the
    // pipeline (capture -> ring -> resample) is still observable via levels.
    let paths = WakeWordModelPaths {
        melspec: config.melspec_model_path.clone().into(),
        embedding: config.embedding_model_path.clone().into(),
        wakeword: config.wakeword_model_path.clone().into(),
    };
    let mut detector = match WakeWordDetector::load(&paths) {
        Ok(d) => {
            let _ = sink.add(WakeWordEvent::status(format!(
                "wake-word model '{}' loaded",
                config.model_name
            )));
            Some(d)
        }
        Err(e) => {
            let _ = sink.add(WakeWordEvent::status(format!(
                "no wake-word model ({e}); running capture-only"
            )));
            None
        }
    };

    // Phase 5: speaker playback of returned TTS frames. The `!Send` cpal output
    // stream must live on this thread (like capture); the `Send` sink is handed to
    // the network layer. If no output device is available the turn still runs — it
    // just can't play audio — so degrade to `None`.
    // `start_playback()` can *panic* on Android: cpal's output-device query hits
    // the Java `AudioManager` via `ndk_context`, which isn't initialized when the
    // library is `dlopen`'d by Dart. Catch the unwind so a playback failure never
    // takes down the whole engine — wake-word detection and the Wyoming turn still
    // run; only the spoken reply is lost.
    let playback_init = std::panic::catch_unwind(std::panic::AssertUnwindSafe(start_playback));
    let (playback_stream, playback_sink) = match playback_init {
        Ok(Ok((stream, sink_handle))) => {
            let _ = sink.add(WakeWordEvent::status(format!(
                "playback ready on '{}' ({} Hz, {} ch)",
                stream.info.device_name, stream.info.sample_rate, stream.info.channels
            )));
            (Some(stream), Some(Arc::new(sink_handle)))
        }
        Ok(Err(e)) => {
            let _ = sink.add(WakeWordEvent::status(format!(
                "no audio output ({e}); replies won't be spoken"
            )));
            (None, None)
        }
        Err(_) => {
            let _ = sink.add(WakeWordEvent::status(
                "audio output unavailable (init panicked); replies won't be spoken".to_string(),
            ));
            (None, None)
        }
    };

    // Phase 3: the Wyoming turn bridge. If the runtime can't be built the engine
    // still runs wake-word detection (it just can't open a turn), so degrade to
    // `None` rather than failing the whole engine.
    let network = match Network::new(&config, sink.clone(), playback_sink) {
        Ok(n) => Some(n),
        Err(e) => {
            let _ = sink.add(WakeWordEvent::status(format!(
                "Wyoming networking unavailable ({e}); wake-word only"
            )));
            None
        }
    };

    // Idle detections use `threshold`; once a turn is active, require the higher
    // `active_threshold` (never below `threshold`) to curb self-triggering while
    // the device streams/speaks (AEC-interim mitigation, Plan.MD §4).
    let idle_threshold = config.threshold;
    let active_threshold = config.active_threshold.max(config.threshold);

    let mut resampler = Resampler::new(info.sample_rate, TARGET_SAMPLE_RATE);
    let mut scratch = vec![0i16; DRAIN_CHUNK];
    let mut in_f32: Vec<f32> = Vec::with_capacity(DRAIN_CHUNK);
    let mut resampled: Vec<f32> = Vec::with_capacity(DRAIN_CHUNK);
    let mut pcm_i16: Vec<i16> = Vec::with_capacity(DRAIN_CHUNK);
    let mut block_counter: u32 = 0;

    while running.load(Ordering::SeqCst) {
        let n = consumer.pop_slice(&mut scratch);
        if n == 0 {
            thread::sleep(IDLE_POLL);
            continue;
        }

        in_f32.clear();
        in_f32.extend(scratch[..n].iter().map(|&s| s as f32));

        resampled.clear();
        resampler.process(&in_f32, &mut resampled);
        if resampled.is_empty() {
            continue;
        }

        // While a turn is active, forward this 16 kHz block to the Wyoming client
        // so it streams up as `audio-chunk`s. Convert once, clamped into range.
        let turn_active = network.as_ref().is_some_and(Network::is_active);
        if turn_active {
            if let Some(net) = network.as_ref() {
                pcm_i16.clear();
                pcm_i16.extend(
                    resampled
                        .iter()
                        .map(|&s| s.clamp(i16::MIN as f32, i16::MAX as f32) as i16),
                );
                net.push_pcm(&pcm_i16);
            }
        }

        match detector.as_mut() {
            Some(d) => match d.push_audio(&resampled) {
                Ok(Some(score))
                    if score
                        >= if turn_active {
                            active_threshold
                        } else {
                            idle_threshold
                        } =>
                {
                    if sink
                        .add(WakeWordEvent::detected(config.model_name.clone(), score))
                        .is_err()
                    {
                        break;
                    }
                    // Fire a Wyoming turn. When one is already active this is a
                    // barge-in: the network layer flushes playback, interrupts the
                    // running turn, and restarts a fresh one (Plan.MD §3, Phase 5).
                    if let Some(net) = network.as_ref() {
                        net.on_wake_word();
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    let _ = sink.add(WakeWordEvent::error(format!(
                        "wake-word inference error: {e}"
                    )));
                    break;
                }
            },
            None => {
                block_counter = block_counter.wrapping_add(1);
                if block_counter.is_multiple_of(LEVEL_EVERY_N_BLOCKS) {
                    let rms = capture::rms_level(&resampled);
                    if sink.add(WakeWordEvent::level(rms)).is_err() {
                        break;
                    }
                }
            }
        }
    }

    // Drop the network first so its runtime stops forwarding (and stops handing
    // audio to the playback sink) before the streams are torn down; then drop the
    // playback and capture streams.
    drop(network);
    drop(playback_stream);
    drop(capture);
    running.store(false, Ordering::SeqCst);
    let _ = sink.add(WakeWordEvent::stopped());
}
