//! Android bootstrap: initialize `ndk_context` so cpal's AAudio backend can reach
//! the Java runtime (Plan.MD §3, Phase 5; architecture.md §2.1 SPEAKING).
//!
//! cpal's Android **output** path queries the Java `AudioManager` (for the mixer
//! burst size) via `ndk_context::android_context()`, which returns the process's
//! `JavaVM` + `Context`. Nothing initializes that global when the Rust library is
//! opened by Dart's `DynamicLibrary.open` (a plain `dlopen`, which does **not** run
//! `JNI_OnLoad`), so `start_playback` panics with "android context was not
//! initialized" and every spoken reply is lost (the engine catches the unwind and
//! degrades to silent playback).
//!
//! The fix has two halves:
//!   1. `MainActivity` calls `System.loadLibrary("rust_lib_ambient_display")`, which
//!      loads the library through the *Java* path and therefore invokes `JNI_OnLoad`
//!      below. (Dart's later `DynamicLibrary.open` then just reuses the same
//!      already-loaded, already-initialized library.)
//!   2. `JNI_OnLoad` captures the `JavaVM`, looks up the process `Application`
//!      (a `Context`) via `ActivityThread.currentApplication()`, and hands both to
//!      `ndk_context::initialize_android_context`.
//!
//! Getting the context through `ActivityThread` means no extra Kotlin plumbing (no
//! native method to register, no context passed across the boundary) — the Rust side
//! is self-contained and works the moment the library is loaded.

use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use jni::sys::{jint, JavaVM as RawJavaVM};
use jni::{Env, JavaVM};

/// Called by the Java runtime when the library is loaded via `System.loadLibrary`.
/// Initializes `ndk_context` exactly once; any failure degrades to the previous
/// behavior (silent playback) rather than taking down library load.
///
/// # Safety
/// Invoked by the JVM with a valid `JavaVM` pointer, per the JNI contract.
#[no_mangle]
pub unsafe extern "system" fn JNI_OnLoad(vm: *mut RawJavaVM, _reserved: *mut c_void) -> jint {
    // JNI 1.6 is the minimum this app targets; return it regardless of whether the
    // context hookup below succeeds so the library still loads.
    const JNI_VERSION_1_6: jint = 0x0001_0006;

    // Guard against a second initialization: `initialize_android_context` asserts it
    // is called once, and a redundant load would otherwise panic.
    static INITIALIZED: AtomicBool = AtomicBool::new(false);
    if INITIALIZED.swap(true, Ordering::SeqCst) {
        return JNI_VERSION_1_6;
    }

    if let Err(e) = init_android_context(vm) {
        // Can't rely on `log` being wired up this early; stderr is discarded on
        // Android but harmless. Playback simply stays silent, as before.
        eprintln!("ndk_context init failed, playback will be silent: {e}");
    }
    JNI_VERSION_1_6
}

/// Resolve the process `Application` context and register it (plus the `JavaVM`) with
/// `ndk_context`.
unsafe fn init_android_context(raw_vm: *mut RawJavaVM) -> jni::errors::Result<()> {
    let vm = JavaVM::from_raw(raw_vm);

    // `ActivityThread.currentApplication()` returns the process-wide Application,
    // which is a `Context` — exactly what cpal's `getSystemService(AUDIO_SERVICE)`
    // needs. By the time the library is loaded from `MainActivity`, the Application
    // has been created, so this is non-null.
    let context_raw = vm.attach_current_thread(|env: &mut Env<'_>| {
        let app = env
            .call_static_method(
                jni::jni_str!("android/app/ActivityThread"),
                jni::jni_str!("currentApplication"),
                jni::jni_sig!("()Landroid/app/Application;"),
                &[],
            )?
            .l()?;
        // A global ref keeps the Context alive for the process lifetime; `into_raw`
        // hands ownership to `ndk_context` (which never deletes it — that's fine, it
        // must live as long as audio can be used, i.e. the whole process).
        let global = env.new_global_ref(app)?;
        Ok::<_, jni::errors::Error>(global.into_raw())
    })?;

    ndk_context::initialize_android_context(raw_vm.cast::<c_void>(), context_raw.cast::<c_void>());
    Ok(())
}
