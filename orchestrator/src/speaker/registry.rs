//! Speaker profile registry (speaker_id_plan.md Phase A / §3.1, §4.1).
//!
//! A SQLite table of per-person voiceprints: each row is a `speaker_id`, an
//! optional user-given `name`, and a running-mean `centroid` embedding. The
//! registry answers [`identify`](SpeakerRegistry::identify) (nearest centroid by
//! cosine), auto-creates anonymous clusters for unrecognized voices, folds new
//! samples into a profile's centroid, and supports naming / merging / deletion
//! from the settings surface (Phase C).
//!
//! Household speaker counts are tiny (single digits), so identification is a plain
//! linear scan — no vector index needed. Synchronous (rusqlite) and guarded by a
//! `Mutex`, mirroring [`crate::memory::MemoryStore`]; in production it opens a
//! second connection to the same DB file (a different table), which SQLite
//! serializes with file locks.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

/// One stored speaker profile (the settings "People" view; §4.5).
#[derive(Debug, Clone, PartialEq)]
pub struct SpeakerProfile {
    /// Stable id (`spk-…`) referenced by memories, chat-log records, and the graph.
    pub id: String,
    /// User-given name, or `None` while the cluster is still anonymous.
    pub name: Option<String>,
    /// Whether a person has named this cluster (`true`) vs. auto-created (`false`).
    pub labeled: bool,
    /// How many embeddings back this centroid (online-mean weight).
    pub samples: u64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// The persistent voiceprint store.
pub struct SpeakerRegistry {
    conn: Mutex<Connection>,
    /// Disambiguates ids minted within the same millisecond.
    counter: AtomicU64,
}

impl SpeakerRegistry {
    /// Open (creating if needed) the registry in the SQLite database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_conn(Connection::open(path).context("opening speaker registry database")?)
    }

    /// An ephemeral in-memory registry (tests).
    pub fn open_in_memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Self> {
        // Two connections may touch the same file (registry + memory store); wait
        // rather than fail on a transient write lock.
        conn.busy_timeout(std::time::Duration::from_secs(5)).ok();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS speakers (
                id         TEXT PRIMARY KEY,
                name       TEXT,
                labeled    INTEGER NOT NULL DEFAULT 0,
                centroid   BLOB NOT NULL,
                dims       INTEGER NOT NULL,
                samples    INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            "#,
        )
        .context("initializing speaker schema")?;
        Ok(Self {
            conn: Mutex::new(conn),
            counter: AtomicU64::new(0),
        })
    }

    /// Nearest profile to `embedding` by cosine similarity. Returns `(speaker_id,
    /// score)` for the best match, or `None` when no profiles exist yet. The
    /// caller ([`super::SpeakerService`]) applies the match/new thresholds.
    pub fn identify(&self, embedding: &[f32]) -> Result<Option<(String, f32)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, centroid FROM speakers")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        let mut best: Option<(String, f32)> = None;
        for row in rows {
            let (id, blob) = row?;
            let centroid = decode_vec(&blob);
            let score = cosine(embedding, &centroid);
            if best.as_ref().is_none_or(|(_, b)| score > *b) {
                best = Some((id, score));
            }
        }
        Ok(best)
    }

    /// Create a new anonymous cluster seeded with `embedding`; returns its id.
    pub fn create_cluster(&self, embedding: &[f32]) -> Result<String> {
        let id = self.next_id();
        let now = now();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO speakers (id, name, labeled, centroid, dims, samples, created_at, updated_at) \
             VALUES (?1, NULL, 0, ?2, ?3, 1, ?4, ?4)",
            params![id, encode_vec(embedding), embedding.len() as i64, now],
        )
        .context("inserting speaker cluster")?;
        Ok(id)
    }

    /// Fold `embedding` into `id`'s centroid via an online mean over normalized
    /// vectors, then renormalize; bumps `samples`. A no-op if the id is unknown.
    pub fn update_centroid(&self, id: &str, embedding: &[f32]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let existing: Option<(Vec<u8>, i64)> = conn
            .query_row(
                "SELECT centroid, samples FROM speakers WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();
        let Some((blob, samples)) = existing else {
            return Ok(());
        };
        let centroid = decode_vec(&blob);
        let merged = fold_mean(&centroid, samples as f32, embedding, 1.0);
        conn.execute(
            "UPDATE speakers SET centroid = ?1, samples = samples + 1, updated_at = ?2 WHERE id = ?3",
            params![encode_vec(&merged), now(), id],
        )?;
        Ok(())
    }

    /// Give a cluster a name and mark it user-labeled. Returns whether a row changed.
    pub fn rename(&self, id: &str, name: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE speakers SET name = ?1, labeled = 1, updated_at = ?2 WHERE id = ?3",
            params![name.trim(), now(), id],
        )?;
        Ok(n > 0)
    }

    /// Fetch one profile by id.
    pub fn get(&self, id: &str) -> Result<Option<SpeakerProfile>> {
        let conn = self.conn.lock().unwrap();
        let p = conn
            .query_row(
                "SELECT id, name, labeled, samples, created_at, updated_at FROM speakers WHERE id = ?1",
                params![id],
                row_to_profile,
            )
            .ok();
        Ok(p)
    }

    /// All profiles, oldest first (so anonymous "Speaker N" ordinals are stable).
    pub fn list(&self) -> Result<Vec<SpeakerProfile>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, labeled, samples, created_at, updated_at FROM speakers \
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([], row_to_profile)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Delete a profile by id; returns whether a row was removed.
    pub fn delete(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("DELETE FROM speakers WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    /// Merge `drop_id` into `keep_id`: fold the voiceprints (weighted by sample
    /// counts) and delete the dropped profile. Reassigning that speaker's memory
    /// rows is the caller's job (Phase B, `MemoryStore`). Returns whether a merge
    /// happened. A user-given name on `keep` is preserved; if only `drop` was named,
    /// its name carries over.
    pub fn merge(&self, keep_id: &str, drop_id: &str) -> Result<bool> {
        if keep_id == drop_id {
            return Ok(false);
        }
        let conn = self.conn.lock().unwrap();
        let load = |id: &str| -> Option<(Vec<u8>, i64, Option<String>, i64)> {
            conn.query_row(
                "SELECT centroid, samples, name, labeled FROM speakers WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .ok()
        };
        let (Some((kb, ks, kn, kl)), Some((db, ds, dn, _))) = (load(keep_id), load(drop_id)) else {
            return Ok(false);
        };
        let merged = fold_mean(&decode_vec(&kb), ks as f32, &decode_vec(&db), ds as f32);
        let name = kn.or(dn);
        let labeled = if kl == 1 || name.is_some() { 1 } else { 0 };
        conn.execute(
            "UPDATE speakers SET centroid = ?1, samples = ?2, name = ?3, labeled = ?4, updated_at = ?5 \
             WHERE id = ?6",
            params![encode_vec(&merged), ks + ds, name, labeled, now(), keep_id],
        )?;
        conn.execute("DELETE FROM speakers WHERE id = ?1", params![drop_id])?;
        Ok(true)
    }

    /// Number of stored profiles.
    pub fn count(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row("SELECT COUNT(*) FROM speakers", [], |r| r.get::<_, i64>(0))? as usize)
    }

    fn next_id(&self) -> String {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("spk-{millis}-{n}")
    }
}

fn row_to_profile(row: &rusqlite::Row) -> rusqlite::Result<SpeakerProfile> {
    Ok(SpeakerProfile {
        id: row.get(0)?,
        name: row.get(1)?,
        labeled: row.get::<_, i64>(2)? != 0,
        samples: row.get::<_, i64>(3)? as u64,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

/// Cosine similarity. Vectors from the embedder are already L2-normalized, but we
/// divide by norms defensively so a stray unnormalized input can't exceed 1.0.
/// Mismatched lengths (e.g. a model/dims change) score 0 rather than panicking.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na <= f32::EPSILON || nb <= f32::EPSILON {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// Weighted online mean of two normalized direction vectors, renormalized:
/// `normalize(a·wa + b·wb)`. Used for centroid updates (wb=1) and merges.
fn fold_mean(a: &[f32], wa: f32, b: &[f32], wb: f32) -> Vec<f32> {
    let n = a.len().max(b.len());
    let mut out = vec![0.0f32; n];
    for (i, o) in out.iter_mut().enumerate() {
        let av = a.get(i).copied().unwrap_or(0.0);
        let bv = b.get(i).copied().unwrap_or(0.0);
        *o = av * wa + bv * wb;
    }
    super::embed::l2_normalize(out)
}

fn encode_vec(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

fn decode_vec(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speaker::embed::{l2_normalize, MockSpeakerEmbedder, SpeakerEmbedder};

    fn tone(hz: f32, ms: usize) -> Vec<i16> {
        let n = 16_000usize * ms / 1000;
        (0..n)
            .map(|t| ((std::f32::consts::TAU * hz * t as f32 / 16_000.0).sin() * 8000.0) as i16)
            .collect()
    }

    #[test]
    fn identify_is_none_when_empty() {
        let reg = SpeakerRegistry::open_in_memory().unwrap();
        let e = MockSpeakerEmbedder::default();
        assert!(reg
            .identify(&e.embed(&tone(200.0, 1500)).unwrap())
            .unwrap()
            .is_none());
    }

    #[test]
    fn create_then_identify_matches_same_voice_and_separates_others() {
        let reg = SpeakerRegistry::open_in_memory().unwrap();
        let e = MockSpeakerEmbedder::default();
        let sam = e.embed(&tone(180.0, 1500)).unwrap();
        let id = reg.create_cluster(&sam).unwrap();

        // Same voice, a later utterance: matches its own cluster well above the
        // 0.55 match threshold (exact value varies with DFT leakage across lengths).
        let sam2 = e.embed(&tone(180.0, 1800)).unwrap();
        let (mid, score) = reg.identify(&sam2).unwrap().unwrap();
        assert_eq!(mid, id);
        assert!(score > 0.6, "same voice matches above threshold (got {score})");

        // A different voice: clearly weaker — below the match threshold and the
        // self-match — so it would not be attributed to Sam.
        let dana = e.embed(&tone(600.0, 1500)).unwrap();
        let (_, other) = reg.identify(&dana).unwrap().unwrap();
        assert!(
            other < 0.55 && other < score - 0.15,
            "different voice separates (self={score}, other={other})"
        );
    }

    #[test]
    fn centroid_update_stays_unit_length_and_counts_samples() {
        let reg = SpeakerRegistry::open_in_memory().unwrap();
        let e = MockSpeakerEmbedder::default();
        let id = reg.create_cluster(&e.embed(&tone(200.0, 1500)).unwrap()).unwrap();
        reg.update_centroid(&id, &e.embed(&tone(205.0, 1500)).unwrap()).unwrap();
        let p = reg.get(&id).unwrap().unwrap();
        assert_eq!(p.samples, 2);
        // Read the centroid back and confirm it is still ~unit length.
        let conn = reg.conn.lock().unwrap();
        let blob: Vec<u8> = conn
            .query_row("SELECT centroid FROM speakers WHERE id = ?1", params![id], |r| r.get(0))
            .unwrap();
        drop(conn);
        let c = decode_vec(&blob);
        let norm: f32 = c.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "centroid stays normalized (got {norm})");
    }

    #[test]
    fn rename_marks_labeled() {
        let reg = SpeakerRegistry::open_in_memory().unwrap();
        let id = reg.create_cluster(&l2_normalize(vec![1.0, 0.0, 0.0])).unwrap();
        assert!(!reg.get(&id).unwrap().unwrap().labeled);
        assert!(reg.rename(&id, "Sam").unwrap());
        let p = reg.get(&id).unwrap().unwrap();
        assert_eq!(p.name.as_deref(), Some("Sam"));
        assert!(p.labeled);
    }

    #[test]
    fn merge_folds_and_removes_dropped() {
        let reg = SpeakerRegistry::open_in_memory().unwrap();
        let keep = reg.create_cluster(&l2_normalize(vec![1.0, 0.0])).unwrap();
        let drop = reg.create_cluster(&l2_normalize(vec![0.0, 1.0])).unwrap();
        reg.rename(&drop, "Dana").unwrap();
        assert!(reg.merge(&keep, &drop).unwrap());
        assert_eq!(reg.count().unwrap(), 1);
        let p = reg.get(&keep).unwrap().unwrap();
        assert_eq!(p.samples, 2);
        assert_eq!(p.name.as_deref(), Some("Dana"), "dropped name carries over");
        assert!(reg.get(&drop).unwrap().is_none());
    }

    #[test]
    fn delete_removes_profile() {
        let reg = SpeakerRegistry::open_in_memory().unwrap();
        let id = reg.create_cluster(&l2_normalize(vec![1.0, 0.0])).unwrap();
        assert!(reg.delete(&id).unwrap());
        assert!(!reg.delete(&id).unwrap());
        assert_eq!(reg.count().unwrap(), 0);
    }

    #[test]
    fn ids_are_unique() {
        let reg = SpeakerRegistry::open_in_memory().unwrap();
        let a = reg.create_cluster(&l2_normalize(vec![1.0, 0.0])).unwrap();
        let b = reg.create_cluster(&l2_normalize(vec![0.0, 1.0])).unwrap();
        assert_ne!(a, b);
    }
}
