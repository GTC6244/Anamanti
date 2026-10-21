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
//! Each frame also carries the capture's exposure time / ISO / aperture, which feed
//! the camera-as-lux-sensor ([`AmbientLightDetector`]): an `AmbientLight` event is
//! emitted when the room's lights go off or back on, plus a periodic lux reading
//! (every [`LUX_REPORT_INTERVAL`]) for the settings screen's live readout.
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
use crate::camera::ambient::{estimate_lux, AmbientLightDetector, CoverDetector};
use crate::camera::presence::PresenceDetector;
use crate::frb_generated::StreamSink;

/// The engine's event sink (a clone), so [`nativePushLuma`] can emit `Presence`
/// events. `None` once proximity is torn down (late frames become no-ops).
static SINK: Mutex<Option<StreamSink<WakeWordEvent>>> = Mutex::new(None);

/// The frame-motion presence state machine, fed by every pushed luma frame.
static DETECTOR: Mutex<Option<PresenceDetector>> = Mutex::new(None);

/// How often to report the current lux estimate even without a dark/lit flip, so
/// the settings screen can show a live reading for tuning the darkness threshold.
const LUX_REPORT_INTERVAL: Duration = Duration::from_secs(5);

/// The camera-as-lux-sensor, the covered-lens detector, and when they last
/// reported, fed by every pushed frame.
struct LightState {
    detector: AmbientLightDetector,
    cover: CoverDetector,
    last_report: Option<Instant>,
}

static LIGHT: Mutex<Option<LightState>> = Mutex::new(None);

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
    dark_lux: f32,
    target_w: i32,
    target_h: i32,
    fps: i32,
) -> Result<()> {
    *DETECTOR.lock().unwrap() = Some(PresenceDetector::new(
        motion_threshold,
        Duration::from_secs(release_secs as u64),
    ));
    *LIGHT.lock().unwrap() = Some(LightState {
        detector: AmbientLightDetector::new(dark_lux),
        cover: CoverDetector::default(),
        last_report: None,
    });
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
    *LIGHT.lock().unwrap() = None;
}

/// The lux sensor's report for one frame: room dark?, smoothed lux, lens covered?
struct LightReport {
    dark: bool,
    lux: f32,
    covered: bool,
}

/// Fold one frame into the lux sensor + covered-lens detector. Returns a report on a
/// dark/lit or covered/uncovered flip, or when [`LUX_REPORT_INTERVAL`] has passed
/// since the last one — `None` when there's nothing to emit (including when the
/// sensor isn't running).
fn observe_light(
    luma: &[u8],
    exposure_ns: i64,
    iso: i32,
    aperture: f32,
    now: Instant,
) -> Option<LightReport> {
    let lux = estimate_lux(luma, exposure_ns, iso, aperture)?;
    let mut guard = LIGHT.lock().unwrap();
    let state = guard.as_mut()?;
    let dark_flip = state.detector.observe(lux, now).is_some();
    let cover_flip = state.cover.observe(luma, now);
    if let Some(covered) = cover_flip {
        log::info!(
            "camera lens {}",
            if covered { "covered" } else { "uncovered" }
        );
    }
    let flipped = dark_flip || cover_flip.is_some();
    let due = state
        .last_report
        .is_none_or(|t| now.duration_since(t) >= LUX_REPORT_INTERVAL);
    if !(flipped || due) {
        return None;
    }
    state.last_report = Some(now);
    if dark_flip {
        log::info!(
            "camera lux: room {} (~{:.1} lux)",
            if state.detector.dark() { "dark" } else { "lit" },
            state.detector.lux()
        );
    }
    if due {
        // Raw frame stats for tuning the covered-lens thresholds on hardware.
        if let Some((mean, sd)) = crate::camera::ambient::frame_stats(luma) {
            log::debug!(
                "camera light: ~{:.1} lux, luma mean {mean:.1} sd {sd:.1}, exp {exposure_ns} ns iso {iso}",
                state.detector.lux()
            );
        }
    }
    Some(LightReport {
        dark: state.detector.dark(),
        lux: state.detector.lux(),
        covered: state.cover.covered(),
    })
}

/// JNI down-call: `CameraBridge.nativePushLuma(byte[] luma, int width, int height,
/// long exposureNs, int iso, float aperture)`. Folds one packed luma frame into the
/// presence detector (emitting `Presence` on a present/absent transition) and, with
/// the frame's capture metadata (`<= 0` = not reported by the HAL), into the lux
/// sensor (emitting `AmbientLight`). Runs on the Kotlin camera reader thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_anamanti_anamanti_1display_CameraBridge_nativePushLuma<'local>(
    mut env: jni::EnvUnowned<'local>,
    _class: JObject<'local>,
    data: JByteArray<'local>,
    width: jint,
    height: jint,
    exposure_ns: jni::sys::jlong,
    iso: jint,
    aperture: jni::sys::jfloat,
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

        let now = Instant::now();
        let transition = DETECTOR
            .lock()
            .unwrap()
            .as_mut()
            .and_then(|d| d.observe(luma, now));
        let light = observe_light(luma, exposure_ns, iso, aperture, now);

        if let Some(present) = transition {
            log::info!(
                "camera proximity: user {}",
                if present { "present" } else { "absent" }
            );
        }
        if transition.is_some() || light.is_some() {
            if let Some(sink) = SINK.lock().unwrap().as_ref() {
                if let Some(present) = transition {
                    let _ = sink.add(WakeWordEvent::presence(present));
                }
                if let Some(r) = light {
                    let _ = sink.add(WakeWordEvent::ambient_light(r.dark, r.lux, r.covered));
                }
            }
        }
        Ok(())
    })
    .resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}
