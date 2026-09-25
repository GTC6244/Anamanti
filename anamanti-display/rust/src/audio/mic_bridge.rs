//! Kotlin `AudioRecord` ↔ Rust capture bridge (Android only).
//!
//! Counterpart to `MicBridge.kt`. The engine, when `WakeWordConfig::use_audiorecord`
//! is set, installs the capture ring's producer here and up-calls [`start`] to open
//! the Kotlin `AudioRecord` (VOICE_RECOGNITION source + optional platform
//! AEC/AGC/NS). The Kotlin reader thread then pushes 10 ms mono `i16` chunks down
//! through [`Java_..._nativePush`], which drops them into the same lock-free ring the
//! `cpal` capture callback would have filled — so the rest of the engine
//! (`run_loop`) is unchanged.
//!
//! Up-calls reuse the `ndk_context` `JavaVM` + the `attach_current_thread` /
//! `call_static_method` pattern already established in `android_init.rs`. The
//! down-call uses the jni 0.22 `EnvUnowned::with_env(..).resolve()` native-method
//! form. The `_1` in the export symbol is JNI mangling for the `_` in the
//! `ambient_display` package segment.

use std::sync::Mutex;

use anyhow::{anyhow, Result};
use jni::objects::{Global, JClass, JObject, JShortArray, JValue};
use jni::sys::jint;
use jni::JavaVM;
use ringbuf::traits::Producer;

use crate::audio::ring_buffer::AudioProducer;

/// The engine's capture-ring producer, installed by `run_loop` before the Kotlin
/// reader thread starts. `None` once capture is torn down (late pushes are dropped).
static PRODUCER: Mutex<Option<AudioProducer>> = Mutex::new(None);

/// A global ref to the `MicBridge` **class**, cached from `JNI_OnLoad` (which runs
/// on the app's main thread with the app class loader). The engine runs on a
/// *native* thread whose `FindClass` only sees the bootstrap class loader and can't
/// resolve app classes — so `start`/`stop` must call through this cached class
/// rather than by name. `Global<T>` is `Send + Sync`, so it lives in a `static`.
static MIC_BRIDGE_CLASS: Mutex<Option<Global<JClass<'static>>>> = Mutex::new(None);

/// JNI: `MicBridge.nativeCacheClass()` — a **static** native method, so its second
/// JNI argument is the `MicBridge` class itself, resolved through the *app* class
/// loader. Kotlin calls this once from `MainActivity.onCreate` (an app thread) so
/// the engine's native thread can later up-call `MicBridge` by the cached class,
/// sidestepping the native-thread `FindClass` bootstrap-loader limitation.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_anamanti_anamanti_1display_MicBridge_nativeCacheClass<'local>(
    mut env: jni::EnvUnowned<'local>,
    class: JClass<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let global = env.new_global_ref(class)?;
        *MIC_BRIDGE_CLASS.lock().unwrap() = Some(global);
        log::info!("mic_bridge: MicBridge class cached");
        Ok(())
    })
    .resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// Hand the capture ring's producer to the bridge so [`nativePush`] can fill it.
pub fn install_producer(producer: AudioProducer) {
    *PRODUCER.lock().unwrap() = Some(producer);
}

/// Drop the producer at engine teardown; any in-flight `nativePush` becomes a no-op.
pub fn clear_producer() {
    *PRODUCER.lock().unwrap() = None;
}

/// Up-call `MicBridge.startRecording(...)`. Returns the actual sample rate, or an
/// error if the Kotlin side failed to open the mic (`<= 0`).
pub fn start(sample_rate: i32, source: i32, aec: bool, agc: bool, ns: bool) -> Result<u32> {
    let ctx = ndk_context::android_context();
    // SAFETY: `ndk_context` was initialized in `JNI_OnLoad` with a valid `JavaVM`.
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) };
    let guard = MIC_BRIDGE_CLASS.lock().unwrap();
    let class = guard
        .as_ref()
        .ok_or_else(|| anyhow!("MicBridge class not cached (JNI_OnLoad did not run?)"))?;
    let rate = vm.attach_current_thread(|env: &mut jni::Env<'_>| -> jni::errors::Result<i32> {
        env.call_static_method(
            class,
            jni::jni_str!("startRecording"),
            jni::jni_sig!("(IIZZZ)I"),
            &[
                JValue::Int(sample_rate),
                JValue::Int(source),
                JValue::Bool(aec),
                JValue::Bool(agc),
                JValue::Bool(ns),
            ],
        )?
        .i()
    })?;
    if rate <= 0 {
        return Err(anyhow!("MicBridge.startRecording returned {rate}"));
    }
    Ok(rate as u32)
}

/// Up-call `MicBridge.stopRecording()`. Best-effort; errors are logged, not fatal.
pub fn stop() {
    let ctx = ndk_context::android_context();
    // SAFETY: `ndk_context` was initialized in `JNI_OnLoad` with a valid `JavaVM`.
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) };
    let guard = MIC_BRIDGE_CLASS.lock().unwrap();
    let Some(class) = guard.as_ref() else {
        log::warn!("mic_bridge stop: MicBridge class not cached");
        return;
    };
    let res = vm.attach_current_thread(|env: &mut jni::Env<'_>| -> jni::errors::Result<()> {
        env.call_static_method(class, jni::jni_str!("stopRecording"), jni::jni_sig!("()V"), &[])
            .map(|_| ())
    });
    if let Err(e) = res {
        log::warn!("mic_bridge stop up-call failed: {e}");
    }
}

/// JNI down-call: `MicBridge.nativePush(short[] data, int len)`. Copies the first
/// `len` samples into the capture ring (dropping on overrun, exactly like the cpal
/// callback's `try_push`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_anamanti_anamanti_1display_MicBridge_nativePush<'local>(
    mut env: jni::EnvUnowned<'local>,
    _class: JObject<'local>,
    data: JShortArray<'local>,
    len: jint,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let n = len.max(0) as usize;
        if n == 0 {
            return Ok(());
        }
        let mut buf = vec![0i16; n];
        #[allow(deprecated)]
        env.get_short_array_region(&data, 0, &mut buf)?;
        if let Some(prod) = PRODUCER.lock().unwrap().as_mut() {
            for &s in &buf {
                let _ = prod.try_push(s);
            }
        }
        Ok(())
    })
    .resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}
