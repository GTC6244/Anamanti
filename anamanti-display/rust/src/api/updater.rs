//! FRB surface for the in-app APK auto-updater
//! (`plans/UpdaterPlan.md`).
//!
//! Networking lives in Rust behind the FRB boundary (the "Rust owns networking"
//! locked decision): [`check_for_update`] fetches `latest.json` and
//! [`download_update`] streams the signed APK to disk, verifying a SHA-256 as the
//! bytes arrive. The system install itself is done natively (the Kotlin
//! `anamanti_display/updater` MethodChannel) because `PackageInstaller` /
//! `canRequestPackageInstalls` have no Rust or pure-Dart equivalent.
//!
//! [`check_for_update`] is a one-shot blocking call — FRB runs non-`sync` functions
//! off the Dart UI isolate, so a blocking HTTPS round-trip never stalls the UI.
//! [`download_update`] mirrors the notify-channel idiom in [`crate::api::engine`]:
//! a dedicated `std::thread` streams [`DownloadProgress`] via a [`StreamSink`], with
//! a stop flag so Dart can [`cancel_download`]. Unlike the persistent notify/weather
//! channels this thread is one-shot (it exits after the terminal event).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

use crate::frb_generated::StreamSink;

/// The update manifest (`latest.json`), reported to Dart.
#[derive(Clone)]
pub struct UpdateManifest {
    /// Android `versionCode` of the published build. Dart compares this against the
    /// running app's `versionCode` (read via the Kotlin updater channel).
    pub version_code: i64,
    /// Human-readable `versionName` (e.g. "1.2.0").
    pub version_name: String,
    /// Absolute HTTPS URL of the APK to download.
    pub apk_url: String,
    /// Expected lowercase-hex SHA-256 of the APK.
    pub sha256: String,
    /// Release notes to show the user.
    pub notes: String,
}

/// One progress tick of a streaming download. Terminal states are `done == true`
/// (the verified APK is at the download path) or a non-empty `error`.
#[derive(Clone)]
pub struct DownloadProgress {
    /// Bytes written so far (or the final total on the `done` event).
    pub downloaded: i64,
    /// Total bytes from `Content-Length`, or 0 if the server didn't send it.
    pub total: i64,
    /// True only on the final event, once the download verified successfully.
    pub done: bool,
    /// Human-readable error (empty when none). A non-empty value is terminal.
    pub error: String,
}

/// Fetch `{base_url}/latest.json`. Blocking; returns the parsed manifest.
pub fn check_for_update(base_url: String) -> anyhow::Result<UpdateManifest> {
    let m = crate::update::fetch_manifest(&base_url)?;
    Ok(UpdateManifest {
        version_code: m.version_code,
        version_name: m.version_name,
        apk_url: m.apk_url,
        sha256: m.sha256,
        notes: m.notes,
    })
}

struct DownloadHandle {
    /// Cancellation flag polled by [`crate::update::download_and_verify`]: `false`
    /// while the download should proceed, flipped to `true` to abort it. This MUST
    /// match the `cancel` semantics in `update/mod.rs` (true == stop) — initializing
    /// it the other way makes every download bail instantly with "download cancelled".
    cancel: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

static DOWNLOAD: OnceLock<Mutex<Option<DownloadHandle>>> = OnceLock::new();

fn download_slot() -> &'static Mutex<Option<DownloadHandle>> {
    DOWNLOAD.get_or_init(|| Mutex::new(None))
}

/// Stream-download `apk_url` to `dest_path`, verifying `expected_sha256` as the
/// bytes arrive, emitting [`DownloadProgress`] events. Replaces any download already
/// in flight. The final event is either `done = true` or carries an `error`; on
/// error (including cancel or hash mismatch) the partial file is removed.
pub fn download_update(
    apk_url: String,
    expected_sha256: String,
    dest_path: String,
    sink: StreamSink<DownloadProgress>,
) -> anyhow::Result<()> {
    cancel_download();

    let cancel = Arc::new(AtomicBool::new(false));
    let loop_cancel = cancel.clone();
    let join = std::thread::Builder::new()
        .name("apk-download".to_string())
        .spawn(move || {
            let dest = std::path::PathBuf::from(dest_path);
            let result = crate::update::download_and_verify(
                &apk_url,
                &expected_sha256,
                &dest,
                &loop_cancel,
                |pr| {
                    let _ = sink.add(DownloadProgress {
                        downloaded: pr.downloaded,
                        total: pr.total,
                        done: false,
                        error: String::new(),
                    });
                },
            );
            match result {
                Ok(downloaded) => {
                    let _ = sink.add(DownloadProgress {
                        downloaded,
                        total: downloaded,
                        done: true,
                        error: String::new(),
                    });
                }
                Err(e) => {
                    let _ = sink.add(DownloadProgress {
                        downloaded: 0,
                        total: 0,
                        done: false,
                        error: format!("{e:#}"),
                    });
                }
            }
        })?;

    *download_slot().lock().unwrap() = Some(DownloadHandle {
        cancel,
        join: Some(join),
    });
    Ok(())
}

/// Cancel an in-flight download (if any) and join its thread. Idempotent.
pub fn cancel_download() {
    let handle = download_slot().lock().unwrap().take();
    if let Some(mut h) = handle {
        h.cancel.store(true, Ordering::SeqCst);
        if let Some(join) = h.join.take() {
            let _ = join.join();
        }
    }
}
