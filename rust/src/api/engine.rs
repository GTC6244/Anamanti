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
    format!("Hello {name}, the ambient-display Rust engine is alive 👋")
}

/// Reports the native engine's version and build target so the device can show
/// exactly which cross-compiled binary it is running.
#[flutter_rust_bridge::frb(sync)]
pub fn engine_version() -> String {
    format!(
        "ambient-display engine v{} ({})",
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
    /// Seconds of server silence before a turn is defensively abandoned
    /// (0 = use the built-in default).
    pub turn_timeout_secs: u64,
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
    /// Phase 3: the turn ended / the socket dropped; `message` gives the reason.
    /// The engine returns to idle wake-word listening.
    Disconnected,
    /// The engine loop has stopped and capture has been torn down.
    Stopped,
    /// A fatal error in `message`; the engine has stopped.
    Error,
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
