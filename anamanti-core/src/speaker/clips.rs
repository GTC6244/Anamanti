//! Per-person raw voice-clip store — training-data captures for human review.
//!
//! The speaker pipeline embeds each utterance into a centroid voiceprint and
//! otherwise discards the raw PCM (see [`super::registry`]). That is enough for
//! identification, but not for a human to *recognize* an anonymous cluster: to put
//! a name to "Speaker 3" you want to **listen** to what they actually said. So,
//! alongside the centroid, this store keeps a small **rolling window** of the most
//! recent raw utterances per speaker — the audio behind the People screen's "play"
//! button (speaker_id_plan.md Phase C).
//!
//! Clips are the house capture format: raw **little-endian `i16` mono PCM at
//! [`super::embed::SAMPLE_RATE`]** (16 kHz), one file per clip under
//! `<root>/<speaker_id>/<millis>-<seq>.pcm`. Only the newest
//! [`MAX_CLIPS_PER_SPEAKER`] are retained; older clips are evicted on each save, so
//! the on-disk footprint stays bounded regardless of how much a person talks.
//!
//! Writes are serialized by a mutex (saves are infrequent — one per attributed
//! turn); the store is cheap to share behind the [`super::SpeakerService`].

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};

use crate::ids::IdMinter;

/// How many of the most recent clips to keep per speaker. Older clips are evicted on
/// each [`ClipStore::save`]. A handful of varied utterances is plenty for a human to
/// recognize a voice while keeping the footprint tiny (≈ a few hundred KB/person).
pub const MAX_CLIPS_PER_SPEAKER: usize = 10;

/// Longest stored voice clip: 60 s of 16 kHz mono `i16` (≈1.9 MB). Clips are replayed on
/// the device as a single Wyoming payload, which the shared protocol caps at 8 MiB.
pub const MAX_CLIP_SAMPLES: usize = 16_000 * 60;

/// Metadata for one stored clip (the People screen's clip list). The raw samples are
/// fetched separately by [`ClipStore::load`] using [`name`](Self::name).
#[derive(Debug, Clone, PartialEq)]
pub struct ClipInfo {
    /// Opaque clip id = the on-disk file stem (e.g. `1700000000000-7`). Stable, and
    /// the key used to fetch the clip's bytes.
    pub name: String,
    /// Capture time in Unix milliseconds (parsed from the name); the newest-first
    /// ordering key.
    pub created_ms: u64,
    /// Number of `i16` samples in the clip (file length / 2).
    pub samples: u64,
}

/// A rolling, per-speaker raw-PCM clip store rooted at a directory on disk. Thread
/// safe: a single mutex serializes the (infrequent) mutating operations.
pub struct ClipStore {
    root: PathBuf,
    /// Mints `<millis>-<seq>` clip names (see [`crate::ids`]).
    ids: IdMinter,
    /// Serializes save/prune/delete/move so concurrent turns can't race the rolling
    /// eviction. Reads (`list`/`load`) don't take it — the filesystem is the source
    /// of truth and a stale listing is harmless.
    write_lock: Mutex<()>,
}

impl ClipStore {
    /// Open (creating if needed) a clip store rooted at `root`.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)
            .with_context(|| format!("creating voice-clip dir {}", root.display()))?;
        Ok(Self {
            root,
            ids: IdMinter::new(),
            write_lock: Mutex::new(()),
        })
    }

    /// Persist one raw utterance for `speaker_id`, then prune that speaker to the
    /// newest [`MAX_CLIPS_PER_SPEAKER`]. A no-op (`Ok`) on empty PCM.
    pub fn save(&self, speaker_id: &str, pcm: &[i16]) -> Result<()> {
        if pcm.is_empty() {
            return Ok(());
        }
        // Keep the first MAX_CLIP_SAMPLES only: a clip is sent to the device in ONE
        // Wyoming payload, so it must stay well under the protocol's payload cap.
        let pcm = &pcm[..pcm.len().min(MAX_CLIP_SAMPLES)];
        let id = sanitize(speaker_id)?;
        let _guard = self.write_lock.lock().unwrap();
        let dir = self.root.join(&id);
        fs::create_dir_all(&dir)
            .with_context(|| format!("creating speaker clip dir {}", dir.display()))?;
        let name = self.next_name();
        let path = dir.join(format!("{name}.pcm"));
        fs::write(&path, encode_pcm(pcm))
            .with_context(|| format!("writing voice clip {}", path.display()))?;
        self.prune(&dir)?;
        Ok(())
    }

    /// The speaker's stored clips, newest first. Empty (not an error) when the
    /// speaker has no clips yet.
    pub fn list(&self, speaker_id: &str) -> Result<Vec<ClipInfo>> {
        let id = sanitize(speaker_id)?;
        Ok(read_clips(&self.root.join(id)))
    }

    /// The raw little-endian `i16` PCM bytes of one clip. Errors if the speaker/clip
    /// is unknown.
    pub fn load(&self, speaker_id: &str, clip: &str) -> Result<Vec<u8>> {
        let id = sanitize(speaker_id)?;
        let name = sanitize(clip)?;
        let path = self.root.join(id).join(format!("{name}.pcm"));
        let mut bytes =
            fs::read(&path).with_context(|| format!("reading voice clip {}", path.display()))?;
        // Clips saved before the length cap existed may be longer; never hand the device
        // a payload its framing reader would reject.
        bytes.truncate(MAX_CLIP_SAMPLES * 2);
        Ok(bytes)
    }

    /// Delete every clip for `speaker_id` (called when a profile is forgotten). A
    /// missing directory is success.
    pub fn delete_speaker(&self, speaker_id: &str) -> Result<()> {
        let id = sanitize(speaker_id)?;
        let _guard = self.write_lock.lock().unwrap();
        let dir = self.root.join(id);
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing clip dir {}", dir.display())),
        }
    }

    /// Move `from`'s clips into `to` (called when two clusters are merged), keeping
    /// only the newest [`MAX_CLIPS_PER_SPEAKER`] across both. Clips are renamed on
    /// move so they can't collide with `to`'s existing names.
    pub fn merge_speakers(&self, keep: &str, drop: &str) -> Result<()> {
        let keep_id = sanitize(keep)?;
        let drop_id = sanitize(drop)?;
        if keep_id == drop_id {
            return Ok(());
        }
        let _guard = self.write_lock.lock().unwrap();
        let from = self.root.join(&drop_id);
        if !from.is_dir() {
            return Ok(());
        }
        let to = self.root.join(&keep_id);
        fs::create_dir_all(&to)
            .with_context(|| format!("creating merged clip dir {}", to.display()))?;
        for clip in read_clips(&from) {
            let src = from.join(format!("{}.pcm", clip.name));
            // Preserve the original capture-millis prefix so newest-first ordering
            // survives the move; only the dedup suffix is refreshed.
            let seq = self.ids.next_seq();
            let dest = to.join(format!("{}-{seq}.pcm", clip.created_ms));
            if let Err(e) = fs::rename(&src, &dest) {
                log::warn!("merging clip {} failed: {e:#}", src.display());
            }
        }
        fs::remove_dir_all(&from).ok();
        self.prune(&to)?;
        Ok(())
    }

    /// Mint a unique `<millis>-<seq>` clip stem.
    fn next_name(&self) -> String {
        self.ids.mint("")
    }

    /// Keep only the newest [`MAX_CLIPS_PER_SPEAKER`] clips in `dir`, deleting the
    /// rest. `dir`'s clips are already parsed newest-first by [`read_clips`].
    fn prune(&self, dir: &Path) -> Result<()> {
        let clips = read_clips(dir);
        for clip in clips.into_iter().skip(MAX_CLIPS_PER_SPEAKER) {
            let path = dir.join(format!("{}.pcm", clip.name));
            if let Err(e) = fs::remove_file(&path) {
                log::warn!("evicting old voice clip {} failed: {e:#}", path.display());
            }
        }
        Ok(())
    }
}

/// Read and parse every `*.pcm` clip in `dir`, newest first. A missing/unreadable
/// directory yields an empty list (a speaker simply has no clips yet).
fn read_clips(dir: &Path) -> Vec<ClipInfo> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut clips: Vec<ClipInfo> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("pcm") {
                return None;
            }
            let name = path.file_stem().and_then(|s| s.to_str())?.to_string();
            let created_ms = name
                .split_once('-')
                .and_then(|(ms, _)| ms.parse::<u64>().ok())
                .unwrap_or(0);
            let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
            Some(ClipInfo {
                name,
                created_ms,
                samples: bytes / 2,
            })
        })
        .collect();
    // Newest first; the name tiebreaks so ordering is deterministic within a ms.
    clips.sort_by(|a, b| {
        b.created_ms
            .cmp(&a.created_ms)
            .then_with(|| b.name.cmp(&a.name))
    });
    clips
}

/// Encode mono `i16` samples as little-endian bytes (the on-disk/wire clip format).
fn encode_pcm(pcm: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pcm.len() * 2);
    for &s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Validate an untrusted path component (a speaker id or clip name arriving over the
/// control socket). Allows only `[A-Za-z0-9._-]` and forbids `.`/`..`, so a crafted
/// id can never escape the clip root via path traversal.
fn sanitize(component: &str) -> Result<String> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || !component
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(anyhow!("invalid clip path component {component:?}"));
    }
    Ok(component.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A self-cleaning temporary directory (avoids a `tempfile` dev-dependency).
    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new() -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let seq = N.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "anamanti-clips-test-{}-{nanos}-{seq}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn store() -> (ClipStore, TmpDir) {
        let dir = TmpDir::new();
        (ClipStore::open(&dir.0).unwrap(), dir)
    }

    #[test]
    fn save_caps_a_clip_to_fit_one_wyoming_payload() {
        let (s, _d) = store();
        let pcm = vec![7i16; MAX_CLIP_SAMPLES + 16_000];
        s.save("spk-1", &pcm).unwrap();
        let clips = s.list("spk-1").unwrap();
        assert_eq!(clips[0].samples, MAX_CLIP_SAMPLES as u64);
        let bytes = s.load("spk-1", &clips[0].name).unwrap();
        assert_eq!(bytes.len(), MAX_CLIP_SAMPLES * 2);
        assert!(bytes.len() < anamanti_protocol::MAX_PAYLOAD_BYTES);
    }

    #[test]
    fn load_trims_an_oversize_legacy_clip() {
        let (s, d) = store();
        let dir = d.0.join("spk-1");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("1-0.pcm"), vec![0u8; MAX_CLIP_SAMPLES * 2 + 4096]).unwrap();
        let bytes = s.load("spk-1", "1-0").unwrap();
        assert_eq!(bytes.len(), MAX_CLIP_SAMPLES * 2);
    }

    #[test]
    fn save_then_list_and_load_round_trips_pcm() {
        let (s, _d) = store();
        let pcm = vec![1i16, -2, 3, -4];
        s.save("spk-1", &pcm).unwrap();
        let clips = s.list("spk-1").unwrap();
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].samples, 4);
        let bytes = s.load("spk-1", &clips[0].name).unwrap();
        assert_eq!(bytes, encode_pcm(&pcm));
    }

    #[test]
    fn rolling_window_keeps_only_the_newest() {
        let (s, _d) = store();
        for _ in 0..(MAX_CLIPS_PER_SPEAKER + 5) {
            s.save("spk-1", &[7i16; 8]).unwrap();
        }
        assert_eq!(s.list("spk-1").unwrap().len(), MAX_CLIPS_PER_SPEAKER);
    }

    #[test]
    fn empty_pcm_is_a_noop() {
        let (s, _d) = store();
        s.save("spk-1", &[]).unwrap();
        assert!(s.list("spk-1").unwrap().is_empty());
    }

    #[test]
    fn unknown_speaker_lists_empty() {
        let (s, _d) = store();
        assert!(s.list("nobody").unwrap().is_empty());
    }

    #[test]
    fn delete_removes_all_clips() {
        let (s, _d) = store();
        s.save("spk-1", &[1i16, 2]).unwrap();
        s.delete_speaker("spk-1").unwrap();
        assert!(s.list("spk-1").unwrap().is_empty());
        // Deleting an absent speaker is success.
        s.delete_speaker("spk-1").unwrap();
    }

    #[test]
    fn merge_moves_clips_and_caps_the_total() {
        let (s, _d) = store();
        for _ in 0..7 {
            s.save("keep", &[1i16; 4]).unwrap();
        }
        for _ in 0..7 {
            s.save("drop", &[2i16; 4]).unwrap();
        }
        s.merge_speakers("keep", "drop").unwrap();
        assert!(s.list("drop").unwrap().is_empty(), "drop dir removed");
        assert_eq!(
            s.list("keep").unwrap().len(),
            MAX_CLIPS_PER_SPEAKER,
            "merged total capped at the rolling window"
        );
    }

    #[test]
    fn path_traversal_is_rejected() {
        let (s, _d) = store();
        assert!(s.save("../evil", &[1i16]).is_err());
        assert!(s.load("spk-1", "../../etc/passwd").is_err());
        assert!(s.list("..").is_err());
    }
}
