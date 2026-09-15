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
