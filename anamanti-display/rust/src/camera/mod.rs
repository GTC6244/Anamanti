//! Camera-as-proximity sensor for the ambient display (Plan.MD §5).
//!
//! The Echo Show's front camera doubles as a cheap proximity sensor: when someone
//! approaches, the idle screen brightens; when the room is quiet for a while, it
//! dims. Per the repo boundaries (`agents.md`), the *sensing* lives in Rust — the
//! [`presence`] frame-motion state machine — while a thin Kotlin shim
//! (`CameraBridge.kt`, the twin of `MicBridge.kt`) only opens the camera and hands
//! packed luma frames down over JNI. Rust never touches the camera pixels beyond a
//! few thousand integer subtractions per frame, keeping the RAM/CPU budget intact.
//!
//! Actuation (the actual backlight change) is presentation, so it stays in Flutter:
//! Rust emits a `Presence` event on the engine's event stream and the Dart UI sets
//! the window brightness.

pub mod presence;

/// Kotlin `CameraBridge` ↔ Rust proximity bridge — Android only. Opens the front
/// camera, delivers luma frames over JNI, and runs the [`presence`] detector on them.
#[cfg(target_os = "android")]
pub mod bridge;
