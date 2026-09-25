//! Kotlin `CameraBridge` ↔ Rust proximity bridge (Android only).
//!
//! Counterpart to `CameraBridge.kt`, and a close twin of `audio/mic_bridge.rs`. The
//! engine (when `WakeWordConfig::camera_proximity` is set) installs a clone of the
//! event sink plus a fresh [`PresenceDetector`] here and up-calls [`start`], which
//! opens the front camera through Kotlin. The Kotlin `ImageReader` thread then
//! pushes packed luma (Y-plane) frames down through [`Java_..._nativePushLuma`];
//! each frame is folded into the detector, and a `Presence` event is emitted on the
//! engine's stream **only when the present/absent state flips** — so the Flutter UI
//! brightens or dims the screen on transitions, not once per frame.
//!
//! Up-calls reuse the `ndk_context` `JavaVM` + `attach_current_thread` pattern from
//! `mic_bridge.rs`; the down-call uses the same jni 0.22 `EnvUnowned::with_env`
//! native-method form. `_1` in the export symbols is JNI mangling for the `_` in the
//! `ambient_display` package segment.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use jni::objects::{Global, JByteArray, JClass, JObject, JValue};
use jni::sys::jint;
use jni::JavaVM;

use crate::api::engine::WakeWordEvent;
use crate::camera::presence::PresenceDetector;
use crate::frb_generated::StreamSink;

/// The engine's event sink (a clone), so [`nativePushLuma`] can emit `Presence`
/// events. `None` once proximity is torn down (late frames become no-ops).
static SINK: Mutex<Option<StreamSink<WakeWordEvent>>> = Mutex::new(None);

/// The frame-motion presence state machine, fed by every pushed luma frame.
static DETECTOR: Mutex<Option<PresenceDetector>> = Mutex::new(None);

/// A global ref to the `CameraBridge` **class**, cached from an app thread (see
/// `CameraBridge.nativeCacheClass`) so the engine's *native* thread can up-call it
/// without a `FindClass` (which only sees the bootstrap loader on native threads),
/// exactly as `mic_bridge` does for `MicBridge`.
static CAMERA_BRIDGE_CLASS: Mutex<Option<Global<JClass<'static>>>> = Mutex::new(None);

/// JNI: `CameraBridge.nativeCacheClass()` — a **static** native method, so its
/// second JNI argument is the `CameraBridge` class itself, resolved through the app
/// class loader. Called once from `MainActivity.onCreate` (an app thread).
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_anamanti_anamanti_1display_CameraBridge_nativeCacheClass<'local>(
    mut env: jni::EnvUnowned<'local>,
    class: JClass<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let global = env.new_global_ref(class)?;
        *CAMERA_BRIDGE_CLASS.lock().unwrap() = Some(global);
        log::info!("camera_bridge: CameraBridge class cached");
        Ok(())
    })
    .resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// Install the event sink + a fresh presence detector, then up-call
/// `CameraBridge.startCamera(w, h, fps)` to open the front camera. Returns an error
/// if the Kotlin side could not start capture (`< 0`), leaving nothing installed.
pub fn start(
    sink: StreamSink<WakeWordEvent>,
    motion_threshold: f32,
    release_secs: u32,
    target_w: i32,
    target_h: i32,
    fps: i32,
) -> Result<()> {
    *DETECTOR.lock().unwrap() = Some(PresenceDetector::new(
        motion_threshold,
        Duration::from_secs(release_secs as u64),
    ));
    *SINK.lock().unwrap() = Some(sink);

    let ctx = ndk_context::android_context();
    // SAFETY: `ndk_context` was initialized in `JNI_OnLoad` with a valid `JavaVM`.
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) };
    let guard = CAMERA_BRIDGE_CLASS.lock().unwrap();
    let class = guard
        .as_ref()
        .ok_or_else(|| anyhow!("CameraBridge class not cached (JNI_OnLoad did not run?)"))?;
    let rc = vm.attach_current_thread(|env: &mut jni::Env<'_>| -> jni::errors::Result<i32> {
        env.call_static_method(
            class,
            jni::jni_str!("startCamera"),
            jni::jni_sig!("(III)I"),
            &[
                JValue::Int(target_w),
                JValue::Int(target_h),
                JValue::Int(fps),
            ],
        )?
        .i()
    });
    match rc {
        Ok(code) if code >= 0 => Ok(()),
        Ok(code) => {
            clear();
            Err(anyhow!("CameraBridge.startCamera returned {code}"))
        }
        Err(e) => {
            clear();
            Err(anyhow!("CameraBridge.startCamera up-call failed: {e}"))
        }
    }
}

/// Up-call `CameraBridge.stopCamera()` and drop the sink + detector. Best-effort;
/// errors are logged, not fatal. Idempotent.
pub fn stop() {
    let ctx = ndk_context::android_context();
    // SAFETY: `ndk_context` was initialized in `JNI_OnLoad` with a valid `JavaVM`.
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) };
    if let Some(class) = CAMERA_BRIDGE_CLASS.lock().unwrap().as_ref() {
        let res = vm.attach_current_thread(|env: &mut jni::Env<'_>| -> jni::errors::Result<()> {
            env.call_static_method(
                class,
                jni::jni_str!("stopCamera"),
                jni::jni_sig!("()V"),
                &[],
            )
            .map(|_| ())
        });
        if let Err(e) = res {
            log::warn!("camera_bridge stop up-call failed: {e}");
        }
    } else {
        log::warn!("camera_bridge stop: CameraBridge class not cached");
    }
    clear();
}

/// Register external user activity (a voice turn or a screen touch) with the
/// presence detector, resetting the dim countdown exactly as camera motion does.
/// Emits a single `Presence(true)` event if this brought the screen back out of the
/// dimmed state. A no-op when proximity isn't running (detector/sink absent), so it
/// is safe to call unconditionally from the voice path and from Flutter.
pub fn note_activity() {
    let transition = DETECTOR
        .lock()
        .unwrap()
        .as_mut()
        .and_then(|d| d.note_activity(Instant::now()));

    if let Some(present) = transition {
        log::info!("user activity: user present (screen brighten)");
        debug_assert!(present);
        if let Some(sink) = SINK.lock().unwrap().as_ref() {
            let _ = sink.add(WakeWordEvent::presence(present));
        }
    }
}

/// Drop the sink + detector so any in-flight frame push becomes a no-op.
fn clear() {
    *SINK.lock().unwrap() = None;
    *DETECTOR.lock().unwrap() = None;
}

/// JNI down-call: `CameraBridge.nativePushLuma(byte[] luma, int width, int height)`.
/// Folds one packed luma frame into the presence detector and, on a present/absent
/// transition, emits a `Presence` event. Runs on the Kotlin camera reader thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_anamanti_anamanti_1display_CameraBridge_nativePushLuma<'local>(
    mut env: jni::EnvUnowned<'local>,
    _class: JObject<'local>,
    data: JByteArray<'local>,
    width: jint,
    height: jint,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let expected = (width.max(0) as usize) * (height.max(0) as usize);
        let n = env.get_array_length(&data).unwrap_or(0).max(0) as usize;
        if n == 0 {
            return Ok(());
        }
        let mut buf = vec![0i8; n];
        #[allow(deprecated)]
        env.get_byte_array_region(&data, 0, &mut buf)?;
        // Guard against a short/torn frame (e.g. a partial ImageReader row copy):
        // only judge motion when we got the whole packed plane.
        if expected != 0 && n < expected {
            return Ok(());
        }
        // Luma is unsigned; JNI `byte[]` is `i8`. Same bytes, reinterpret in place.
        let luma: &[u8] = unsafe { &*(buf.as_slice() as *const [i8] as *const [u8]) };

        let transition = DETECTOR
            .lock()
            .unwrap()
            .as_mut()
            .and_then(|d| d.observe(luma, Instant::now()));

        if let Some(present) = transition {
            log::info!(
                "camera proximity: user {}",
                if present { "present" } else { "absent" }
            );
            if let Some(sink) = SINK.lock().unwrap().as_ref() {
                let _ = sink.add(WakeWordEvent::presence(present));
            }
        }
        Ok(())
    })
    .resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}
