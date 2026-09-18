//! Debug-only per-turn audio capture for the Phase 0 AEC corpus.
//!
//! When the env var `AMBIENT_AUDIO_DUMP_DIR` is set, each voice turn writes the
//! two streams the host-side APM will need — the device's far-field **mic** PCM
//! (near-end, as streamed up for STT) and the **Piper TTS** PCM relayed back
//! (far-end reference) — as raw little-endian `i16` files, plus a JSON-lines
//! manifest for pairing them offline. Unset by default, so production is
//! unaffected and nothing is written.
//!
//! ## Pairing (why per-turn + timestamps)
//!
//! The mic only streams while a turn is *listening*, so a normal turn yields
//! clean far-field speech (good for NS/AGC), while an **echo** sample comes from a
//! **barge-in**: the mic of turn `N+1` captures the still-playing reply of turn
//! `N`. To reconstruct that pair offline we record, per turn, the wall-clock time
//! of the first mic byte and the first TTS byte, so the mic of `N+1` can be lined
//! up against the reference (TTS of `N`) by their absolute start times. AEC3's
//! delay estimator then refines the residual offset.
//!
//! Convert a dump to the `aec_spike` format (16 kHz mono) with, e.g.:
//!   ffmpeg -f s16le -ar <rate> -ac 1 -i turn_N_tts_<rate>.raw \
//!          -f s16le -ar 16000 -ac 1 ref.pcm

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Monotonic per-process turn counter so dumped files sort in turn order and
/// adjacent (barge-in) turns are trivially `N` / `N+1`.
static TURN_SEQ: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

struct Stream {
    file: File,
    rate: u32,
    bytes: u64,
    first_ms: u128,
}

struct Inner {
    mic: Option<Stream>,
    tts: Option<Stream>,
    transcript: String,
    reply: String,
}

/// One turn's capture. Cheap no-op unless `AMBIENT_AUDIO_DUMP_DIR` is set.
pub struct TurnAudioDump {
    dir: PathBuf,
    seq: u64,
    start_ms: u128,
    inner: Mutex<Inner>,
}

impl TurnAudioDump {
    /// Begin capturing a turn if `AMBIENT_AUDIO_DUMP_DIR` is set; otherwise `None`.
    /// Files are created lazily on the first mic/TTS byte so silent turns leave no
    /// stray empties.
    pub fn for_turn() -> Option<Self> {
        let dir = std::env::var_os("AMBIENT_AUDIO_DUMP_DIR")?;
        let dir = PathBuf::from(dir);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            log::warn!("audio-dump: cannot create {}: {e}", dir.display());
            return None;
        }
        let seq = TURN_SEQ.fetch_add(1, Ordering::Relaxed);
        log::info!("audio-dump: capturing turn {seq} to {}", dir.display());
        Some(Self {
            dir,
            seq,
            start_ms: now_ms(),
            inner: Mutex::new(Inner {
                mic: None,
                tts: None,
                transcript: String::new(),
                reply: String::new(),
            }),
        })
    }

    /// Append a near-end (device mic) PCM chunk sampled at `rate`.
    pub fn push_mic(&self, pcm: &[u8], rate: u32) {
        self.push(true, pcm, rate);
    }

    /// Append a far-end (Piper TTS reference) PCM chunk sampled at `rate`.
    pub fn push_tts(&self, pcm: &[u8], rate: u32) {
        self.push(false, pcm, rate);
    }

    fn push(&self, is_mic: bool, pcm: &[u8], rate: u32) {
        if pcm.is_empty() {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        let slot = if is_mic { &mut inner.mic } else { &mut inner.tts };
        if slot.is_none() {
            let kind = if is_mic { "mic" } else { "tts" };
            let path = self
                .dir
                .join(format!("turn_{:04}_{kind}_{rate}.raw", self.seq));
            match File::create(&path) {
                Ok(file) => {
                    *slot = Some(Stream {
                        file,
                        rate,
                        bytes: 0,
                        first_ms: now_ms(),
                    })
                }
                Err(e) => {
                    log::warn!("audio-dump: cannot create {}: {e}", path.display());
                    return;
                }
            }
        }
        if let Some(stream) = slot {
            if let Err(e) = stream.file.write_all(pcm) {
                log::warn!("audio-dump: write failed: {e}");
            } else {
                stream.bytes += pcm.len() as u64;
            }
        }
    }

    /// Record the recognized transcript (for the manifest).
    pub fn set_transcript(&self, text: &str) {
        self.inner.lock().unwrap().transcript = text.to_string();
    }

    /// Record the spoken reply text (for the manifest).
    pub fn set_reply(&self, text: &str) {
        self.inner.lock().unwrap().reply = text.to_string();
    }
}

impl Drop for TurnAudioDump {
    fn drop(&mut self) {
        let inner = self.inner.lock().unwrap();
        // Skip turns that captured nothing at all.
        if inner.mic.is_none() && inner.tts.is_none() {
            return;
        }
        let describe = |s: &Option<Stream>| match s {
            Some(st) => serde_json::json!({
                "rate": st.rate,
                "bytes": st.bytes,
                "samples": st.bytes / 2,
                "first_ms": st.first_ms as u64,
            }),
            None => serde_json::Value::Null,
        };
        let record = serde_json::json!({
            "seq": self.seq,
            "turn_start_ms": self.start_ms as u64,
            "mic": describe(&inner.mic),
            "tts": describe(&inner.tts),
            "transcript": inner.transcript,
            "reply": inner.reply,
        });
        append_manifest(&self.dir, &record);
    }
}

fn append_manifest(dir: &Path, record: &serde_json::Value) {
    let path = dir.join("manifest.jsonl");
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            let _ = writeln!(f, "{record}");
        }
        Err(e) => log::warn!("audio-dump: cannot append manifest {}: {e}", path.display()),
    }
}
