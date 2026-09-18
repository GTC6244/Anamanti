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
use std::time::{Duration, Instant};

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

mod gate;
mod net;
use gate::DetectionGate;
use net::Network;

/// Ring capacity in samples (~2 s at 48 kHz; ≈192 KB of `i16`). Absorbs
/// inference-thread scheduling jitter while staying tiny in the RAM budget.
const RING_CAPACITY_SAMPLES: usize = 96_000;

/// How many samples to drain from the ring per loop iteration.
const DRAIN_CHUNK: usize = 4096;

/// Idle poll interval when the ring is momentarily empty.
const IDLE_POLL: Duration = Duration::from_millis(10);

/// Emit an audio-level event this often (in drained blocks). Kept small so the
/// device's mic-level stream is fine-grained enough to drive the UI meter *and* the
/// Dart-side local end-of-speech cue (which flips to "processing" the instant the
/// user stops talking, without waiting on the Mac's VAD + transcript round trip).
const LEVEL_EVERY_N_BLOCKS: u32 = 2;

/// Emit the `wake-word diag: rms=… peak_score=…` logcat line this often. Coarser
/// than the level-event cadence so the finer level stream doesn't flood logs.
const DIAG_LOG_EVERY_N_BLOCKS: u32 = 10;

/// Default number of consecutive per-block scores smoothed before a detection can
/// fire when the config leaves `smoothing_window` at 0 (VACA-style smoothing,
/// WakeWordDetection.md §4.1). Trades a little latency for fewer single-frame false
/// triggers; the settings screen can lower it (to 1) or raise it for on-device A/B
/// tuning.
const DEFAULT_SMOOTH_WINDOW: usize = 2;

/// Minimum gap between two detections so one utterance fires exactly once
/// (WakeWordDetection.md §4.1).
const DETECTION_COOLDOWN: Duration = Duration::from_millis(1500);

/// Scores at or above this are logged for live tuning even when they don't fire.
/// Kept well below any usable threshold so the logcat trace shows the confidence
/// climbing toward the wake word without flooding (one line per drained block).
const SCORE_LOG_FLOOR: f32 = 0.05;

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

    // Capture source. The Kotlin `AudioRecord` bridge (Android, opt-in) reaches the
    // HAL's VOICE_RECOGNITION source + platform effects and reports its true rate;
    // `cpal` is the default and the only path off-Android. `capture_stream` holds the
    // `!Send` cpal stream alive on this thread (None in AudioRecord mode).
    #[cfg(target_os = "android")]
    let use_audiorecord = config.use_audiorecord;
    #[cfg(not(target_os = "android"))]
    let use_audiorecord = false;

    #[allow(unused_mut)]
    let mut capture_stream: Option<capture::CaptureStream> = None;
    // Set in AudioRecord mode (device reports the true rate → skip cpal calibration).
    let mut precalibrated_rate: Option<u32> = None;
    let info: capture::CaptureInfo;

    if use_audiorecord {
        #[cfg(target_os = "android")]
        {
            crate::audio::mic_bridge::install_producer(producer);
            match crate::audio::mic_bridge::start(
                TARGET_SAMPLE_RATE as i32,
                config.mic_source as i32,
                config.platform_aec,
                config.platform_agc,
                config.platform_ns,
            ) {
                Ok(rate) => {
                    precalibrated_rate = Some(rate);
                    info = capture::CaptureInfo {
                        device_name: format!("AudioRecord(source={})", config.mic_source),
                        sample_rate: rate,
                        channels: 1,
                    };
                }
                Err(e) => {
                    crate::audio::mic_bridge::clear_producer();
                    log::error!("AudioRecord capture failed to start: {e:#}");
                    let _ = sink.add(WakeWordEvent::error(format!(
                        "failed to start AudioRecord capture: {e}"
                    )));
                    running.store(false, Ordering::SeqCst);
                    return;
                }
            }
        }
        #[cfg(not(target_os = "android"))]
        {
            // Unreachable: `use_audiorecord` is a compile-time `false` off-Android.
            let _ = producer;
            unreachable!("AudioRecord capture is Android-only");
        }
    } else {
        match start_capture(producer) {
            Ok(c) => {
                info = c.info.clone();
                capture_stream = Some(c);
            }
            Err(e) => {
                let _ = sink.add(WakeWordEvent::error(format!(
                    "failed to start audio capture: {e}"
                )));
                running.store(false, Ordering::SeqCst);
                return;
            }
        }
    }
    log::info!(
        "wake-word capture started on '{}' ({} Hz, {} ch)",
        info.device_name,
        info.sample_rate,
        info.channels
    );
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
            log::info!(
                "wake-word model '{}' loaded from {}",
                config.model_name,
                paths.wakeword.display()
            );
            let _ = sink.add(WakeWordEvent::status(format!(
                "wake-word model '{}' loaded",
                config.model_name
            )));
            Some(d)
        }
        Err(e) => {
            log::warn!(
                "no wake-word model ({e}); running capture-only. paths: mel={} emb={} ww={}",
                paths.melspec.display(),
                paths.embedding.display(),
                paths.wakeword.display()
            );
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
    let playback_buffer_secs = config.playback_buffer_secs;
    let playback_init = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        start_playback(playback_buffer_secs)
    }));
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

    // Calibrate the *true* input sample rate before resampling. cpal's
    // `default_input_config()` reports the rate it asked AAudio for (48 kHz on the
    // Echo Show), but some Android HALs deliver the mono stream at a different
    // effective rate — here it arrives at half, so trusting cpal's number makes the
    // linear resampler emit audio at 2x speed / an octave high, which is
    // pitch/time-distorted enough that wake-word confidence collapses to ~0 even
    // though the mic is clearly capturing speech. Measuring the real throughput into
    // the ring and resampling from that is self-correcting across HAL quirks.
    // AudioRecord reports its true rate, so skip the cpal rate-calibration workaround.
    let input_rate = match precalibrated_rate {
        Some(rate) => rate,
        None => calibrate_input_rate(&mut consumer, info.sample_rate, &running),
    };
    if input_rate != info.sample_rate {
        let _ = sink.add(WakeWordEvent::status(format!(
            "input rate calibrated to {input_rate} Hz (device reported {})",
            info.sample_rate
        )));
    }
    let mut resampler = Resampler::new(input_rate, TARGET_SAMPLE_RATE);
    let mut scratch = vec![0i16; DRAIN_CHUNK];
    let mut in_f32: Vec<f32> = Vec::with_capacity(DRAIN_CHUNK);
    let mut resampled: Vec<f32> = Vec::with_capacity(DRAIN_CHUNK);
    let mut pcm_i16: Vec<i16> = Vec::with_capacity(DRAIN_CHUNK);
    let mut block_counter: u32 = 0;

    // VACA-style detection smoothing + debounce (WakeWordDetection.md §4.1): smooth
    // the last `smoothing_window` block scores rather than trusting a single frame,
    // and enforce a cooldown so one utterance fires exactly once. Both the window
    // size and the fire criterion (average vs peak) come from the config so they can
    // be A/B-tuned on-device for far-field responsiveness. The gating logic is a
    // pure, unit-tested state machine (see `gate`).
    let smoothing_window = if config.smoothing_window == 0 {
        DEFAULT_SMOOTH_WINDOW
    } else {
        config.smoothing_window as usize
    };
    log::info!(
        "wake-word gate: window={smoothing_window} criterion={} idle_thr={idle_threshold:.2} \
         active_thr={active_threshold:.2}",
        if config.fire_on_peak {
            "peak"
        } else {
            "average"
        }
    );
    let mut gate = DetectionGate::new(smoothing_window, DETECTION_COOLDOWN, config.fire_on_peak);
    // Rolling peak score between diagnostic log emissions, so the logcat trace shows
    // both that audio is flowing (rms) and how high the model scored (peak) even
    // when nothing crosses the detection threshold.
    let mut diag_peak: f32 = 0.0;

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
                Ok(Some(score)) => {
                    let threshold = if turn_active {
                        active_threshold
                    } else {
                        idle_threshold
                    };

                    // Periodic diagnostic + live mic level. The RMS is emitted as a
                    // `Level` event even while a model is loaded (not just in
                    // capture-only mode) so the UI meter and on-hardware tuning stay
                    // live during detection (WakeWordDetection.md §4.4), and logged
                    // alongside the rolling peak score for logcat debugging.
                    diag_peak = diag_peak.max(score);
                    block_counter = block_counter.wrapping_add(1);
                    if block_counter.is_multiple_of(LEVEL_EVERY_N_BLOCKS) {
                        let rms = capture::rms_level(&resampled);
                        if sink.add(WakeWordEvent::level(rms)).is_err() {
                            break;
                        }
                        // Log the rolling peak on a coarser cadence than the level
                        // stream so the finer mic-level events don't flood logcat.
                        if block_counter.is_multiple_of(DIAG_LOG_EVERY_N_BLOCKS) {
                            log::info!("wake-word diag: rms={rms:.4} peak_score={diag_peak:.4}");
                            diag_peak = 0.0;
                        }
                    }

                    let fired = gate.observe(score, threshold, Instant::now());

                    // Live tuning trace: show the confidence climbing toward the
                    // wake word without flooding logcat (one line per drained block,
                    // and only once a score is meaningfully above the noise floor).
                    // `gate.avg()` now includes this block's score.
                    if score >= SCORE_LOG_FLOOR {
                        log::info!(
                            "wake-word score={score:.3} avg={:.3} thr={threshold:.2}",
                            gate.avg()
                        );
                    }

                    if let Some(avg) = fired {
                        log::info!(
                            "WAKE WORD DETECTED '{}' (avg={avg:.3}, peak={score:.3})",
                            config.model_name
                        );
                        if sink
                            .add(WakeWordEvent::detected(config.model_name.clone(), avg))
                            .is_err()
                        {
                            break;
                        }
                        // Fire a Wyoming turn. When one is already active this is a
                        // barge-in: the network layer flushes playback, interrupts
                        // the running turn, and restarts a fresh one (Plan.MD §3,
                        // Phase 5).
                        if let Some(net) = network.as_ref() {
                            net.on_wake_word();
                        }
                    }
                }
                Ok(None) => {}
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
    #[cfg(target_os = "android")]
    if use_audiorecord {
        crate::audio::mic_bridge::stop();
        crate::audio::mic_bridge::clear_producer();
    }
    drop(capture_stream);
    running.store(false, Ordering::SeqCst);
    let _ = sink.add(WakeWordEvent::stopped());
}

/// Measure the real rate at which mono samples arrive in the ring and snap it to
/// the nearest standard rate. Guards against Android HALs that deliver audio at a
/// different effective rate than cpal's `default_input_config()` reports (observed
/// on the Echo Show, where the reported 48 kHz is actually delivered at half),
/// which would otherwise time/pitch-distort every block and break detection.
///
/// Drains a short warm-up (to shed the burst that queued during model load) then
/// counts samples over a fixed wall-clock window. Falls back to `reported` if the
/// measurement is too short to trust (e.g. the engine is stopping).
fn calibrate_input_rate(
    consumer: &mut crate::audio::ring_buffer::AudioConsumer,
    reported: u32,
    running: &Arc<AtomicBool>,
) -> u32 {
    const WARMUP: Duration = Duration::from_millis(300);
    const WINDOW: Duration = Duration::from_millis(1500);
    const STD_RATES: [u32; 11] = [
        8000, 11025, 16000, 22050, 24000, 32000, 44100, 48000, 64000, 88200, 96000,
    ];

    let mut scratch = vec![0i16; DRAIN_CHUNK];

    // Warm-up: discard whatever is already buffered so the measurement reflects the
    // steady-state callback cadence, not the model-load backlog.
    let warm_end = Instant::now() + WARMUP;
    while Instant::now() < warm_end && running.load(Ordering::SeqCst) {
        if consumer.pop_slice(&mut scratch) == 0 {
            thread::sleep(IDLE_POLL);
        }
    }

    // Measure sustained throughput over the window.
    let start = Instant::now();
    let mut count: u64 = 0;
    while start.elapsed() < WINDOW && running.load(Ordering::SeqCst) {
        let n = consumer.pop_slice(&mut scratch);
        if n == 0 {
            thread::sleep(IDLE_POLL);
        } else {
            count += n as u64;
        }
    }

    let secs = start.elapsed().as_secs_f64();
    if secs < 0.5 || count < 4000 {
        log::warn!("input-rate calibration inconclusive; using reported {reported} Hz");
        return reported;
    }

    let measured = (count as f64 / secs).round() as u32;
    let snapped = *STD_RATES
        .iter()
        .min_by_key(|&&r| (r as i64 - measured as i64).unsigned_abs())
        .unwrap_or(&reported);
    log::info!(
        "input-rate calibration: measured {measured} Hz over {secs:.2}s -> {snapped} Hz \
         (cpal reported {reported} Hz)"
    );
    snapped
}
