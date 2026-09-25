//! Phase 1 hello-world surface for the Rust system engine.
//!
//! These functions exist only to prove the `flutter_rust_bridge` v2 boundary and
//! the `aarch64-linux-android` cross-compile end-to-end (Plan.MD §3, Phase 1).
//! The real capture / wake-word / Wyoming client work lands in later phases; this
//! module is the smallest thing that lets the Flutter UI call into the native
//! `.so` and get an answer back.

/// A friendly greeting from the native Rust engine.
///
/// The Flutter UI calls this on startup to confirm the cross-compiled shared
/// library loaded and the FRB bridge is live on the device.
#[flutter_rust_bridge::frb(sync)]
pub fn engine_greeting(name: String) -> String {
    format!("Hello {name}, the anamanti-display Rust engine is alive 👋")
}

/// Reports the native engine's version and build target so the device can show
/// exactly which cross-compiled binary it is running.
#[flutter_rust_bridge::frb(sync)]
pub fn engine_version() -> String {
    format!(
        "anamanti-display engine v{} ({})",
        env!("CARGO_PKG_VERSION"),
        engine_target(),
    )
}

/// The build target of the running engine, used to distinguish an on-device
/// build from a host build (and which ABI) during development. Reports the real
/// OS + arch, e.g. `android/arm` on the 32-bit Echo Show, `android/aarch64` on a
/// 64-bit device, or `macos/aarch64` on the host.
fn engine_target() -> String {
    format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH)
}

#[flutter_rust_bridge::frb(init)]
pub fn init_app() {
    // Default utilities - feel free to customize
    flutter_rust_bridge::setup_default_user_utils();
    // `setup_default_user_utils()` installs a logcat logger at the *Trace* max
    // level, so every `log` record is formatted and written synchronously. On the
    // 32-bit device that firehose — especially `tract`'s per-node graph tracing at
    // model-load time and per-frame inference logging — starves the real-time
    // audio thread, stalling wake-word detection and tripping the engine's
    // error/reconnect loop. Cap it at Info so lifecycle logs survive but the
    // Debug/Trace flood is dropped before any string formatting happens.
    log::set_max_level(log::LevelFilter::Info);

    // Surface panics through the `log` crate. The engine runs its capture/inference
    // loop on a background thread; without this a panic there dies silently (the
    // default hook writes to stderr, which Android discards), so the only symptom
    // is the FRB event stream ending and the UI looping on "reconnecting". Logging
    // the panic makes such crashes visible in logcat.
    std::panic::set_hook(Box::new(|info| {
        log::error!("rust panic: {info}");
    }));
}

// ---------------------------------------------------------------------------
// Phase 2 — Native audio capture + on-device wake-word detection.
// Phase 3 — Wyoming client: on detection, the engine discovers the Mac's
//           Wyoming host over mDNS, opens a TCP turn, streams PCM up, and reads
//           `transcript` events back (Plan.MD §3, Phase 3; architecture.md §4).
//
// Flutter starts the engine and consumes a single stream of events; Rust owns
// capture, the ring buffer, resampling, `tract-onnx` wake-word scoring, and the
// Wyoming turn state machine. The events below are the device -> Dart contract;
// the dedicated transcript/reply UI streams are built on top in Phase 5.
// ---------------------------------------------------------------------------

use crate::frb_generated::StreamSink;
use flutter_rust_bridge::frb;

/// Paths and tuning for the openWakeWord three-model chain plus the Phase-3
/// Wyoming turn. The Flutter layer resolves the model paths from bundled assets
/// or the settings screen (Phase 6) and hands them to the engine. The Wyoming
/// host itself is discovered over mDNS at turn time, so no address is configured
/// here (architecture.md §5).
pub struct WakeWordConfig {
    /// Path to the melspectrogram ONNX model (`melspectrogram.onnx`).
    pub melspec_model_path: String,
    /// Path to the shared feature/embedding ONNX model (`embedding_model.onnx`).
    pub embedding_model_path: String,
    /// Path to the wake-word classifier ONNX model (e.g. `alexa_v0.1.onnx`).
    pub wakeword_model_path: String,
    /// Human-readable wake-word name, echoed back on detection events.
    pub model_name: String,
    /// Confidence in [0, 1] above which a detection is reported while idle.
    pub threshold: f32,
    /// Higher confidence required to fire *while a turn is already active*
    /// (streaming/speaking). This is the AEC-interim mitigation from the locked
    /// decisions: raise the bar during playback so the device's own speaker is
    /// less likely to self-trigger (Plan.MD §4). Set equal to `threshold` to
    /// disable. Clamped to at least `threshold` at runtime.
    pub active_threshold: f32,
    /// Seconds to browse `_wyoming._tcp` before falling back to the cached host
    /// (0 = use the built-in default).
    pub discovery_timeout_secs: u64,
    /// Stable selection key (`instance_id` TXT) of the orchestrator this display
    /// is pinned to. Empty = "Auto" (connect to the first available orchestrator).
    /// When set, discovery resolves *only* this orchestrator and stays offline if
    /// it is unreachable, rather than silently connecting to a different one.
    pub orchestrator_key: String,
    /// Seconds of server silence before a turn is defensively abandoned
    /// (0 = use the built-in default).
    pub turn_timeout_secs: u64,
    /// Number of consecutive per-block scores smoothed before a detection can fire
    /// (0 = engine default). Lower = snappier / more sensitive to brief or faint
    /// wake words; higher = fewer single-frame false triggers. A/B-tunable from the
    /// settings screen to dial in far-field responsiveness on hardware.
    pub smoothing_window: u32,
    /// Detection-gate criterion. `false` (default) fires on the *average* of the
    /// smoothing window clearing `threshold`; `true` fires as soon as the *peak*
    /// score in the window clears it — far more responsive to short/quiet "hey
    /// jarvis" utterances (whose confidence peaks for a single block and is
    /// otherwise diluted by the surrounding low blocks) at the cost of a slightly
    /// higher false-trigger rate.
    pub fire_on_peak: bool,
    /// Speaker playback buffer depth in seconds (0 = engine default). Sized to hold
    /// a whole spoken reply so long TTS answers are not truncated when the network
    /// delivers audio faster than real-time playback drains it. A/B-tunable.
    pub playback_buffer_secs: u32,
    /// **Android only.** Use the Kotlin `AudioRecord` capture layer instead of
    /// `cpal`, to reach the HAL's far-field `VOICE_RECOGNITION` source (array
    /// beamforming) + platform audio effects. `false` (default) keeps the `cpal`
    /// path; ignored entirely off-Android. A/B-tunable from the settings screen.
    pub use_audiorecord: bool,
    /// `android.media.MediaRecorder.AudioSource` for the AudioRecord path
    /// (6 = `VOICE_RECOGNITION`, 7 = `VOICE_COMMUNICATION`, 1 = `MIC`). Only used
    /// when `use_audiorecord` is set.
    pub mic_source: u32,
    /// Attach the platform `AcousticEchoCanceler` to the AudioRecord session (if the
    /// device offers it). Default off — the host-side WebRTC APM does AEC, and prior
    /// on-hardware testing found this device's platform AEC did not actually cancel.
    pub platform_aec: bool,
    /// Attach the platform `AutomaticGainControl` to the AudioRecord session (if
    /// available). Helps the Echo Show's quiet far-field pickup.
    pub platform_agc: bool,
    /// Attach the platform `NoiseSuppressor` to the AudioRecord session (if available).
    pub platform_ns: bool,
    /// **Android only.** Use the front camera as a proximity sensor: a cheap
    /// frame-motion detector runs on low-res luma frames and, when someone
    /// approaches, the UI brightens the idle screen (dimming again after a quiet
    /// period). `false` (default off-Android) disables the camera entirely — no
    /// frames are ever captured. See `camera/presence.rs` and Plan.MD §5.
    pub camera_proximity: bool,
    /// Mean absolute per-pixel luma delta (0..255) above which a frame counts as
    /// motion. `0` uses the engine default (`presence::DEFAULT_MOTION_THRESHOLD`).
    /// Lower = more sensitive (brightens on fainter/farther movement) at the cost of
    /// more false wakes from noise/lighting; A/B-tunable.
    pub proximity_motion_threshold: f32,
    /// Seconds of no motion before the screen is allowed to dim again. `0` uses the
    /// engine default (`presence::DEFAULT_RELEASE`).
    pub proximity_release_secs: u32,
}

/// Discriminates the kind of [`WakeWordEvent`]. A unit-only enum so FRB maps it
/// to a plain Dart `enum` (no `freezed` codegen dependency needed).
#[derive(Clone)]
pub enum WakeWordEventKind {
    /// Capture started; `device`/`device_sample_rate`/`channels` are populated.
    Started,
    /// Informational status in `message` (e.g. model loaded, capture-only).
    Status,
    /// Periodic input level in `rms` (~0.0..1.0) — proves capture is live even
    /// before a wake-word model is present.
    Level,
    /// The wake word fired; `model` and `score` are populated.
    Detected,
    /// Phase 3: a turn began — discovering/dialing the Wyoming host. `message`
    /// describes the resolved endpoint when known.
    Connecting,
    /// Phase 3: connected to the Wyoming host; PCM is now streaming up.
    Streaming,
    /// Phase 3: a transcript arrived from the STT server; `transcript` carries
    /// the recognized text.
    Transcript,
    /// Phase 5: one streamed LLM reply-token fragment; `reply` carries the text.
    /// The UI appends these to render the reply token-by-token.
    ReplyToken,
    /// Phase 5: the reply's TTS audio has started and is now playing back through
    /// the speakers.
    Speaking,
    /// Phase 5: the reply's TTS audio has finished playing out of the speaker — the
    /// playback ring drained naturally, or was flushed by a barge-in. UI-only: it
    /// signals that the on-screen reply text may be removed now that the audio has
    /// stopped. Fires *after* the turn has already returned to idle, because the
    /// device relays audio faster than real-time (see `engine/net.rs`).
    SpeakingDone,
    /// Follow-up listening: the assistant's reply was a question, so the device
    /// reopened the mic and is listening for the answer **without a wake word** (a
    /// fresh turn is starting). UI-only cue so the screen can show a "listening for
    /// your reply" affordance. Followed by the usual `Streaming`/`Transcript`/… of
    /// the follow-up turn. See `plans/Plan.MD` (Follow-up listening).
    ListeningFollowup,
    /// Phase 3: the turn ended / the socket dropped; `message` gives the reason.
    /// The engine returns to idle wake-word listening.
    Disconnected,
    /// The engine loop has stopped and capture has been torn down.
    Stopped,
    /// A fatal error in `message`; the engine has stopped.
    Error,
    /// Phase 2: a countdown timer started on the device. `timer_id`,
    /// `timer_label`, and `timer_remaining_secs` (the full duration) are populated;
    /// the UI shows a countdown from that deadline.
    TimerStarted,
    /// Phase 2: a countdown timer reached zero (the alarm is sounding). `timer_id`
    /// and `timer_label` identify it.
    TimerFinished,
    /// Phase 2: a running timer was cancelled. `timer_id` identifies it.
    TimerCancelled,
    /// Recipe mode: the orchestrator pushed a parsed recipe to show on the display's
    /// 3-tab recipe screen. `recipe_json` carries the recipe as a JSON string (title,
    /// summary, source_url, image_url, servings, total_time, ingredients[], steps[])
    /// which the UI parses into the Overview / Ingredients / Steps tabs.
    ShowRecipe,
    /// Recipe mode: dismiss the recipe screen and return to the idle/ambient display.
    DismissRecipe,
    /// Phase 5: the camera proximity sensor's present/absent state changed. `present`
    /// is `true` when someone has approached the display (brighten) and `false` when
    /// the room has been quiet long enough to dim again (Plan.MD §5). Emitted only on
    /// transitions, never per frame.
    Presence,
}

/// A single event streamed from the Rust engine to the Flutter UI. Modeled as a
/// flat struct with a `kind` tag (rather than a data-carrying enum) so the FRB
/// boundary stays dependency-free; fields not relevant to a given `kind` carry
/// neutral defaults. `Clone` lets the engine fan the sink out to the Phase-3
/// Wyoming turn task (which emits on the same stream).
#[derive(Clone)]
pub struct WakeWordEvent {
    pub kind: WakeWordEventKind,
    /// Status / error text (`Status`, `Error`).
    pub message: String,
    /// Capture device name (`Started`).
    pub device: String,
    /// Device-native capture rate in Hz (`Started`).
    pub device_sample_rate: u32,
    /// Device channel count before mono downmix (`Started`).
    pub channels: u16,
    /// Input RMS level (`Level`).
    pub rms: f32,
    /// Wake-word confidence in [0, 1] (`Detected`).
    pub score: f32,
    /// Wake-word name that fired (`Detected`).
    pub model: String,
    /// Recognized speech (`Transcript`).
    pub transcript: String,
    /// One streamed reply-token fragment (`ReplyToken`).
    pub reply: String,
    /// Timer id (`TimerStarted` / `TimerFinished` / `TimerCancelled`). Stable for
    /// the life of one timer so the UI can add/remove the right chip.
    pub timer_id: u32,
    /// Timer's spoken label, empty when unlabeled (`TimerStarted`/`TimerFinished`).
    pub timer_label: String,
    /// Full timer duration in seconds at start (`TimerStarted`); the UI counts down
    /// from `now + timer_remaining_secs`. Zero for finished/cancelled.
    pub timer_remaining_secs: u32,
    /// Camera proximity state (`Presence`): `true` = someone approached (brighten),
    /// `false` = quiet long enough to dim. Neutral `false` for every other kind.
    pub present: bool,
    /// The parsed recipe as a JSON string (`ShowRecipe`); empty for every other kind.
    /// The UI decodes it into the recipe-mode tabs.
    pub recipe_json: String,
}

impl WakeWordEvent {
    fn base(kind: WakeWordEventKind) -> Self {
        Self {
            kind,
            message: String::new(),
            device: String::new(),
            device_sample_rate: 0,
            channels: 0,
            rms: 0.0,
            score: 0.0,
            model: String::new(),
            transcript: String::new(),
            reply: String::new(),
            timer_id: 0,
            timer_label: String::new(),
            timer_remaining_secs: 0,
            present: false,
            recipe_json: String::new(),
        }
    }

    pub(crate) fn started(device: String, device_sample_rate: u32, channels: u16) -> Self {
        Self {
            device,
            device_sample_rate,
            channels,
            ..Self::base(WakeWordEventKind::Started)
        }
    }

    pub(crate) fn status(message: String) -> Self {
        Self {
            message,
            ..Self::base(WakeWordEventKind::Status)
        }
    }

    pub(crate) fn level(rms: f32) -> Self {
        Self {
            rms,
            ..Self::base(WakeWordEventKind::Level)
        }
    }

    pub(crate) fn detected(model: String, score: f32) -> Self {
        Self {
            model,
            score,
            ..Self::base(WakeWordEventKind::Detected)
        }
    }

    pub(crate) fn connecting(message: String) -> Self {
        Self {
            message,
            ..Self::base(WakeWordEventKind::Connecting)
        }
    }

    pub(crate) fn streaming() -> Self {
        Self::base(WakeWordEventKind::Streaming)
    }

    pub(crate) fn transcript(text: String) -> Self {
        Self {
            transcript: text,
            ..Self::base(WakeWordEventKind::Transcript)
        }
    }

    pub(crate) fn reply_token(text: String) -> Self {
        Self {
            reply: text,
            ..Self::base(WakeWordEventKind::ReplyToken)
        }
    }

    pub(crate) fn speaking() -> Self {
        Self::base(WakeWordEventKind::Speaking)
    }

    pub(crate) fn speaking_done() -> Self {
        Self::base(WakeWordEventKind::SpeakingDone)
    }

    pub(crate) fn listening_followup() -> Self {
        Self::base(WakeWordEventKind::ListeningFollowup)
    }

    pub(crate) fn disconnected(message: String) -> Self {
        Self {
            message,
            ..Self::base(WakeWordEventKind::Disconnected)
        }
    }

    pub(crate) fn stopped() -> Self {
        Self::base(WakeWordEventKind::Stopped)
    }

    pub(crate) fn error(message: String) -> Self {
        Self {
            message,
            ..Self::base(WakeWordEventKind::Error)
        }
    }

    pub(crate) fn timer_started(id: u32, label: &str, remaining_secs: u32) -> Self {
        Self {
            timer_id: id,
            timer_label: label.to_string(),
            timer_remaining_secs: remaining_secs,
            ..Self::base(WakeWordEventKind::TimerStarted)
        }
    }

    pub(crate) fn timer_finished(id: u32, label: &str) -> Self {
        Self {
            timer_id: id,
            timer_label: label.to_string(),
            ..Self::base(WakeWordEventKind::TimerFinished)
        }
    }

    pub(crate) fn timer_cancelled(id: u32) -> Self {
        Self {
            timer_id: id,
            ..Self::base(WakeWordEventKind::TimerCancelled)
        }
    }

    pub(crate) fn show_recipe(recipe_json: String) -> Self {
        Self {
            recipe_json,
            ..Self::base(WakeWordEventKind::ShowRecipe)
        }
    }

    pub(crate) fn dismiss_recipe() -> Self {
        Self::base(WakeWordEventKind::DismissRecipe)
    }

    // Constructed only by the Android camera bridge; on host builds it's unused.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub(crate) fn presence(present: bool) -> Self {
        Self {
            present,
            ..Self::base(WakeWordEventKind::Presence)
        }
    }
}

/// Start microphone capture and continuous on-device wake-word scoring, pushing
/// [`WakeWordEvent`]s to Dart. Replaces any engine already running.
pub fn start_wake_word_engine(
    config: WakeWordConfig,
    sink: StreamSink<WakeWordEvent>,
) -> anyhow::Result<()> {
    crate::engine::start(config, sink)
}

/// Stop the wake-word engine and release the microphone. Idempotent.
pub fn stop_wake_word_engine() {
    crate::engine::stop();
}

/// Whether the wake-word engine is currently running.
#[frb(sync)]
pub fn is_wake_word_engine_running() -> bool {
    crate::engine::is_running()
}

/// Register user activity that isn't camera motion — a voice turn or a screen touch
/// — so it resets the screen-dim countdown (and brightens the screen if it had
/// already dimmed). Flutter calls this on each voice-turn transition and on screen
/// touches. Safe to call any time: it's a no-op when camera proximity isn't running,
/// and a harmless no-op off Android (host tests, no camera).
#[frb(sync)]
pub fn note_user_activity() {
    #[cfg(target_os = "android")]
    crate::camera::bridge::note_activity();
}

// ---------------------------------------------------------------------------
// Proactive notifications (Approach A, visual-only). A persistent channel the
// device dials to the orchestrator and holds open, receiving pushed
// `anamanti-notify` frames *without* a voice turn (architecture.md §4). Rust owns
// the socket + reconnect/backoff; Flutter consumes `NotifyEvent`s and renders a
// banner on the idle screen. This runs alongside — and independently of — the
// wake-word engine, on its own thread + runtime.
// ---------------------------------------------------------------------------

/// Config for the persistent notify channel. The orchestrator is discovered over
/// mDNS at connect time (same as the voice path), so only the pin, the browse
/// timeout, and this display's id are configured here.
pub struct NotifyConfig {
    /// Stable selection key (`instance_id` TXT) of the pinned orchestrator; empty =
    /// "Auto" (first available). Mirrors [`WakeWordConfig::orchestrator_key`] so the
    /// notify channel targets the same Mac the voice path does.
    pub orchestrator_key: String,
    /// Seconds to browse `_wyoming._tcp` before falling back to the cached host
    /// (0 = built-in default).
    pub discovery_timeout_secs: u64,
    /// A stable identifier for this display, sent in the `anamanti-hello` frame so the
    /// orchestrator can key notifications per device (may be empty).
    pub device_id: String,
}

/// One proactive notification pushed from the orchestrator, streamed to Flutter.
/// Modeled as a flat struct (like [`WakeWordEvent`]) so the FRB boundary stays
/// dependency-free.
#[derive(Clone)]
pub struct NotifyEvent {
    /// Stable notification id (for dedup / dismiss on the UI side).
    pub id: String,
    /// `"info"` | `"reminder"` | `"alert"` — drives the banner styling.
    pub priority: String,
    /// Short headline.
    pub title: String,
    /// Body text.
    pub body: String,
}

struct NotifyHandle {
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

static NOTIFY: std::sync::OnceLock<std::sync::Mutex<Option<NotifyHandle>>> =
    std::sync::OnceLock::new();

fn notify_slot() -> &'static std::sync::Mutex<Option<NotifyHandle>> {
    NOTIFY.get_or_init(|| std::sync::Mutex::new(None))
}

/// Open the persistent proactive-notification channel and stream pushed
/// notifications to Dart. Replaces any channel already running (so it can be
/// restarted when the pinned orchestrator changes). The channel dials the pinned
/// orchestrator and reconnects with backoff for the life of the subscription.
pub fn start_notify_channel(
    config: NotifyConfig,
    sink: StreamSink<NotifyEvent>,
) -> anyhow::Result<()> {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    stop_notify_channel();

    let running = Arc::new(AtomicBool::new(true));
    let loop_running = running.clone();
    let join = std::thread::Builder::new()
        .name("notify-channel".to_string())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    log::error!("notify channel: could not build runtime: {e:#}");
                    return;
                }
            };
            let timeout = if config.discovery_timeout_secs == 0 {
                crate::wyoming::DEFAULT_DISCOVERY_TIMEOUT
            } else {
                std::time::Duration::from_secs(config.discovery_timeout_secs)
            };
            let key = {
                let k = config.orchestrator_key.trim();
                if k.is_empty() {
                    None
                } else {
                    Some(k.to_string())
                }
            };
            let cache = crate::wyoming::EndpointCache::new();
            rt.block_on(crate::wyoming::notify::run(
                &cache,
                timeout,
                key,
                config.device_id,
                loop_running,
                move |note| {
                    sink.add(NotifyEvent {
                        id: note.id,
                        priority: note.priority,
                        title: note.title,
                        body: note.body,
                    })
                    .is_ok()
                },
            ));
        })?;

    *notify_slot().lock().unwrap() = Some(NotifyHandle {
        running,
        join: Some(join),
    });
    Ok(())
}

/// Stop the proactive-notification channel (if any) and join its thread. Idempotent.
pub fn stop_notify_channel() {
    use std::sync::atomic::Ordering;
    let handle = notify_slot().lock().unwrap().take();
    if let Some(mut h) = handle {
        h.running.store(false, Ordering::SeqCst);
        if let Some(join) = h.join.take() {
            let _ = join.join();
        }
    }
}
