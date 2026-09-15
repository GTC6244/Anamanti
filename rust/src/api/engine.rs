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
//
// Flutter starts the engine and consumes a stream of events; Rust owns capture,
// the ring buffer, resampling, and `tract-onnx` wake-word scoring (Plan.MD §3,
// Phase 2; architecture.md §2.1, §3). The events below are the Phase-2 slice of
// the Rust -> Dart stream contract; the full state machine (transcript/reply
// streams, Wyoming client) arrives in later phases.
// ---------------------------------------------------------------------------

use crate::frb_generated::StreamSink;
use flutter_rust_bridge::frb;

/// Paths and tuning for the openWakeWord three-model chain. The Flutter layer
/// resolves these from bundled/assets or the settings screen (Phase 6) and hands
/// them to the engine; discovery of the Wyoming host is a separate concern
/// (Phase 3), so nothing here touches the network.
pub struct WakeWordConfig {
    /// Path to the melspectrogram ONNX model (`melspectrogram.onnx`).
    pub melspec_model_path: String,
    /// Path to the shared feature/embedding ONNX model (`embedding_model.onnx`).
    pub embedding_model_path: String,
    /// Path to the wake-word classifier ONNX model (e.g. `alexa_v0.1.onnx`).
    pub wakeword_model_path: String,
    /// Human-readable wake-word name, echoed back on detection events.
    pub model_name: String,
    /// Confidence in [0, 1] above which a detection is reported.
    pub threshold: f32,
}

/// Discriminates the kind of [`WakeWordEvent`]. A unit-only enum so FRB maps it
/// to a plain Dart `enum` (no `freezed` codegen dependency needed).
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
    /// The engine loop has stopped and capture has been torn down.
    Stopped,
    /// A fatal error in `message`; the engine has stopped.
    Error,
}

/// A single event streamed from the Rust engine to the Flutter UI during Phase
/// 2. Modeled as a flat struct with a `kind` tag (rather than a data-carrying
/// enum) so the FRB boundary stays dependency-free; fields not relevant to a
/// given `kind` carry neutral defaults.
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
