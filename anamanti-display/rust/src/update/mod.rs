//! In-app updater networking: fetch `latest.json` and stream-download the signed
//! APK from Cloudflare R2, verifying a SHA-256 as the bytes arrive.
//!
//! Blocking HTTPS via `ureq` (rustls + the `ring` provider, Mozilla roots bundled
//! by `webpki-roots` — see `Cargo.toml` for why this is NOT reqwest). The FRB
//! surface in [`crate::api::updater`] wraps these on a dedicated thread. JSON is
//! parsed with the already-present `serde_json` rather than ureq's `json` feature,
//! to keep the dependency surface minimal. See
//! `plans/UpdaterPlan.md`.

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Streaming read buffer. Fixed and small — the APK is never held whole in memory
/// (the device has ~1 GB RAM); it is hashed and written to disk as it arrives.
const READ_BUF: usize = 16 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Parsed `latest.json`. The JSON uses camelCase (`versionCode`, `apkUrl`); serde
/// maps it to snake_case Rust fields.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub version_code: i64,
    #[serde(default)]
    pub version_name: String,
    pub apk_url: String,
    pub sha256: String,
    #[serde(default)]
    pub notes: String,
}

/// One progress tick of a streaming download.
pub struct Progress {
    pub downloaded: i64,
    /// Total bytes from `Content-Length`, or 0 if the server didn't send it.
    pub total: i64,
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .build()
}

/// GET `{base_url}/latest.json` and parse it.
pub fn fetch_manifest(base_url: &str) -> Result<Manifest> {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        bail!("update base URL is empty");
    }
    let url = format!("{base}/latest.json");
    let body = agent()
        .get(&url)
        .call()
        .with_context(|| format!("fetching {url}"))?
        .into_string()
        .with_context(|| format!("reading {url}"))?;
    let manifest: Manifest =
        serde_json::from_str(&body).with_context(|| format!("parsing latest.json from {url}"))?;
    if manifest.apk_url.is_empty() || manifest.sha256.is_empty() {
        bail!("latest.json is missing apkUrl or sha256");
    }
    Ok(manifest)
}

/// Stream-download `apk_url` to `dest`, verifying `expected_sha256` (hex) while
/// streaming. `on_progress` is called after each chunk; `cancel` is polled each
/// chunk (if it flips true the download aborts). On any error — cancel, hash
/// mismatch, or IO — the partial file is removed. Returns the number of bytes
/// written on success.
pub fn download_and_verify(
    apk_url: &str,
    expected_sha256: &str,
    dest: &Path,
    cancel: &Arc<AtomicBool>,
    mut on_progress: impl FnMut(Progress),
) -> Result<i64> {
    let result = download_inner(apk_url, expected_sha256, dest, cancel, &mut on_progress);
    if result.is_err() {
        let _ = std::fs::remove_file(dest);
    }
    result
}

fn download_inner(
    apk_url: &str,
    expected_sha256: &str,
    dest: &Path,
    cancel: &Arc<AtomicBool>,
    on_progress: &mut impl FnMut(Progress),
) -> Result<i64> {
    let resp = agent()
        .get(apk_url)
        .call()
        .with_context(|| format!("downloading {apk_url}"))?;
    let total: i64 = resp
        .header("Content-Length")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);

    let mut reader = resp.into_reader();
    let mut file =
        File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; READ_BUF];
    let mut downloaded: i64 = 0;

    loop {
        if cancel.load(Ordering::SeqCst) {
            bail!("download cancelled");
        }
        let n = reader.read(&mut buf).context("reading response body")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).context("writing apk to disk")?;
        downloaded += n as i64;
        on_progress(Progress { downloaded, total });
    }
    file.flush().ok();
    file.sync_all().ok();

    let actual = hex_encode(&hasher.finalize());
    let expected = expected_sha256.trim().to_ascii_lowercase();
    if actual != expected {
        bail!("sha256 mismatch: expected {expected}, got {actual}");
    }
    Ok(downloaded)
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_manifest_camel_case() {
        let json = r#"{
            "versionCode": 12,
            "versionName": "1.2.0",
            "apkUrl": "https://dl.example.com/app-1.2.0.apk",
            "sha256": "abc123",
            "notes": "Bug fixes"
        }"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.version_code, 12);
        assert_eq!(m.version_name, "1.2.0");
        assert_eq!(m.apk_url, "https://dl.example.com/app-1.2.0.apk");
        assert_eq!(m.sha256, "abc123");
        assert_eq!(m.notes, "Bug fixes");
    }

    #[test]
    fn manifest_notes_and_version_name_optional() {
        let json = r#"{"versionCode":3,"apkUrl":"https://x/y.apk","sha256":"deadbeef"}"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.version_code, 3);
        assert!(m.version_name.is_empty());
        assert!(m.notes.is_empty());
    }

    #[test]
    fn hex_encode_is_lowercase_padded() {
        assert_eq!(hex_encode(&[0x00, 0x0f, 0xff, 0xa0]), "000fffa0");
    }

    #[test]
    fn download_verifies_sha256_and_removes_on_mismatch() {
        // Serve a known body from a tiny localhost HTTP server, then check the
        // on-disk file is removed when the expected hash doesn't match.
        use std::io::Write as _;
        use std::net::TcpListener;

        let body = b"hello anamanti";
        // Precomputed sha256("hello anamanti").
        let good = {
            let mut h = Sha256::new();
            h.update(body);
            hex_encode(&h.finalize())
        };

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let body_vec = body.to_vec();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(_) => return,
                };
                // Drain the request line(s) minimally; we don't parse them.
                let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                let mut scratch = [0u8; 1024];
                let _ = stream.read(&mut scratch);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body_vec.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.write_all(&body_vec);
            }
        });

        let url = format!("http://{addr}/app.apk");
        let dir = std::env::temp_dir();
        let dest = dir.join(format!("anamanti-test-{}.apk", std::process::id()));
        let cancel = Arc::new(AtomicBool::new(false));

        // Good hash → file kept, byte count correct.
        let n = download_and_verify(&url, &good, &dest, &cancel, |_| {}).unwrap();
        assert_eq!(n as usize, body.len());
        assert!(dest.exists());

        // Bad hash → error and the partial file is cleaned up.
        let bad = "0".repeat(64);
        let err = download_and_verify(&url, &bad, &dest, &cancel, |_| {});
        assert!(err.is_err());
        assert!(!dest.exists());

        let _ = server.join();
    }
}
