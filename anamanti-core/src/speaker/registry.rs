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
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::clock::now_secs as now;
use crate::ids::IdMinter;
use crate::vecmath::{cosine, l2_normalize};

/// Where a profile's **name** came from (SpeakerScopedMemoryPlan.md §4a). Guided
/// enrollment folds new samples into an existing profile *by name*, so a name taken
/// from an STT transcript ("my name is …") must never be a fold target — otherwise a
/// mis-heard spoken label could capture another person's enrollment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameSource {
    /// Named by guided enrollment (the device "Add a voice" flow).
    Enrolled,
    /// Named or renamed by a person on the People screen / admin console.
    Admin,
    /// Named by voice ("my name is …") — the weakest provenance.
    Spoken,
}

impl NameSource {
    /// Stable storage/wire label.
    pub fn as_str(self) -> &'static str {
        match self {
            NameSource::Enrolled => "enrolled",
            NameSource::Admin => "admin",
            NameSource::Spoken => "spoken",
        }
    }

    /// Parse a stored label (`None` for an unknown/absent value).
    pub fn parse(s: &str) -> Option<NameSource> {
        match s {
            "enrolled" => Some(NameSource::Enrolled),
            "admin" => Some(NameSource::Admin),
            "spoken" => Some(NameSource::Spoken),
            _ => None,
        }
    }

    /// Whether a profile named this way may receive a guided enrollment by name.
    pub fn is_trusted(self) -> bool {
        matches!(self, NameSource::Enrolled | NameSource::Admin)
    }
}

/// One stored speaker profile (the settings "People" view; §4.5).
#[derive(Debug, Clone, PartialEq)]
pub struct SpeakerProfile {
    /// Stable id (`spk-…`) referenced by memories, chat-log records, and the graph.
    pub id: String,
    /// User-given name, or `None` while the cluster is still anonymous.
    pub name: Option<String>,
    /// Where `name` came from (`None` while anonymous).
    pub name_source: Option<NameSource>,
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
    /// Mints `spk-{millis}-{n}` ids.
    ids: IdMinter,
}

impl SpeakerRegistry {
    /// Open (creating if needed) the registry in the SQLite database at `path`,
    /// through [`crate::db::open`] (busy timeout + WAL) — the same connection policy
    /// as the memory store that shares this file.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_conn(crate::db::open(path).context("opening speaker registry database")?)
    }

    /// An ephemeral in-memory registry (tests).
    pub fn open_in_memory() -> Result<Self> {
        Self::from_conn(crate::db::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Self> {
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
        migrate_name_source(&conn)?;
        super::aliases::ensure_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            ids: IdMinter::new(),
        })
    }

    /// Nearest profile to `embedding` by cosine similarity. Returns `(speaker_id,
    /// score)` for the best match, or `None` when no profiles exist yet. The
    /// caller ([`super::SpeakerService`]) applies the match/new thresholds.
    pub fn identify(&self, embedding: &[f32]) -> Result<Option<(String, f32)>> {
        let conn = self.conn.lock().unwrap();
        best_match(&conn, embedding)
    }

    /// Identify `embedding` **and act on the decision atomically**: one lock and
    /// one SQLite write transaction cover the nearest-centroid scan and its outcome,
    /// so two concurrent turns from the same new voice can't both see "no match" and
    /// mint two clusters.
    ///
    /// The nearest profile plus its closest **named** rival (household-relative
    /// scoring input), or `None` when no profiles exist yet. Read-only.
    pub fn rank(&self, embedding: &[f32]) -> Result<Option<Ranked>> {
        let conn = self.conn.lock().unwrap();
        rank(&conn, embedding)
    }

    /// Identify `embedding` **and act on the decision atomically**: one lock and
    /// one SQLite write transaction cover the nearest-centroid scan and its outcome,
    /// so two concurrent turns from the same new voice can't both see "no match" and
    /// mint two clusters. The decision is [`MatchPolicy::decide`]:
    ///
    /// - [`Attribution::Matched`] (confident and unambiguous): the profile's centroid
    ///   absorbs the sample;
    /// - [`Attribution::Tentative`] / [`Attribution::Distinct`]: attributed, centroid
    ///   untouched;
    /// - otherwise (or no profiles) ⇒ [`Attribution::Created`]: a new anonymous
    ///   cluster seeded with `embedding`.
    pub fn identify_or_create(
        &self,
        embedding: &[f32],
        policy: &MatchPolicy,
    ) -> Result<Identified> {
        let mut conn = self.conn.lock().unwrap();
        // IMMEDIATE takes the write lock up front, so another process sharing the
        // file can't interleave its own identify-or-create either.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ranked = rank(&tx, embedding)?;
        let attribution = ranked
            .as_ref()
            .map_or(Attribution::Created, |r| policy.decide(r));
        let out = match (ranked, attribution) {
            (Some(r), attribution) if attribution != Attribution::Created => {
                if attribution == Attribution::Matched {
                    fold_into(&tx, &r.id, embedding)?;
                }
                let (name, name_source) = name_of(&tx, &r.id)?;
                Identified {
                    margin: r.margin(),
                    id: r.id,
                    name,
                    name_source,
                    score: r.score,
                    attribution,
                }
            }
            _ => {
                let id = self.next_id();
                insert_cluster(&tx, &id, embedding)?;
                Identified {
                    id,
                    name: None,
                    name_source: None,
                    score: 0.0,
                    margin: None,
                    attribution: Attribution::Created,
                }
            }
        };
        tx.commit().context("committing speaker identification")?;
        Ok(out)
    }

    /// Create a new anonymous cluster seeded with `embedding`; returns its id.
    pub fn create_cluster(&self, embedding: &[f32]) -> Result<String> {
        let id = self.next_id();
        let conn = self.conn.lock().unwrap();
        insert_cluster(&conn, &id, embedding)?;
        Ok(id)
    }

    /// Create a **named, user-labeled** profile from a pre-built centroid and its
    /// supporting `samples` count (active enrollment; SpeakerEnrollmentPlan.md
    /// Phase 2). Unlike [`create_cluster`](Self::create_cluster) (anonymous, one
    /// sample) this is the result of a guided multi-sample enrollment, so it starts
    /// named and `labeled = 1`. Returns the new `speaker_id`.
    pub fn create_labeled(&self, name: &str, centroid: &[f32], samples: u64) -> Result<String> {
        let id = self.next_id();
        let now = now();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO speakers (id, name, name_source, labeled, centroid, dims, samples, created_at, updated_at) \
             VALUES (?1, ?2, 'enrolled', 1, ?3, ?4, ?5, ?6, ?6)",
            params![
                id,
                name.trim(),
                encode_vec(centroid),
                centroid.len() as i64,
                samples.max(1) as i64,
                now
            ],
        )
        .context("inserting enrolled speaker")?;
        Ok(id)
    }

    /// The oldest profile whose name matches `name` (case-insensitive, trimmed) **and
    /// was named by enrollment or by a person** ([`NameSource::is_trusted`]), if any.
    /// Used by guided enrollment to attach a new voiceprint to a person who already has
    /// one instead of minting a same-named duplicate. A **spoken** name (from an STT
    /// transcript) is never a fold target: a mis-heard "my name is …" must not let one
    /// person's enrollment train another person's voiceprint.
    pub fn find_by_name(&self, name: &str) -> Result<Option<String>> {
        Ok(self.find_named(name)?.and_then(|p| {
            p.name_source
                .is_some_and(NameSource::is_trusted)
                .then_some(p.id)
        }))
    }

    /// The oldest profile carrying `name` (case-insensitive, trimmed), whatever its
    /// [`NameSource`] — e.g. to tell a speaker that a name is already taken.
    pub fn find_named(&self, name: &str) -> Result<Option<SpeakerProfile>> {
        let needle = name.trim();
        if needle.is_empty() {
            return Ok(None);
        }
        Ok(self.list()?.into_iter().find(|p| {
            p.name
                .as_deref()
                .is_some_and(|n| n.trim().eq_ignore_ascii_case(needle))
        }))
    }

    /// Fold a guided-enrollment `centroid` (backed by `samples` utterances) into the
    /// existing profile `id` — a sample-weighted mean, renormalized. `Some(name)`
    /// (re)labels it; `None` keeps its current name and labeled flag (e.g. adding
    /// samples to a still-anonymous cluster). Returns whether the profile existed.
    pub fn absorb(
        &self,
        id: &str,
        name: Option<&str>,
        centroid: &[f32],
        samples: u64,
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let existing: Option<(Vec<u8>, i64)> = conn
            .query_row(
                "SELECT centroid, samples FROM speakers WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((blob, have)) = existing else {
            return Ok(false);
        };
        let samples = samples.max(1) as i64;
        let merged = fold_mean(&decode_vec(&blob), have as f32, centroid, samples as f32);
        match name {
            Some(name) => conn.execute(
                "UPDATE speakers SET centroid = ?1, samples = ?2, name = ?3, labeled = 1, \
                 name_source = 'enrolled', updated_at = ?4 WHERE id = ?5",
                params![encode_vec(&merged), have + samples, name.trim(), now(), id],
            ),
            None => conn.execute(
                "UPDATE speakers SET centroid = ?1, samples = ?2, updated_at = ?3 WHERE id = ?4",
                params![encode_vec(&merged), have + samples, now(), id],
            ),
        }
        .context("folding enrollment into speaker")?;
        Ok(true)
    }

    /// Fold `embedding` into `id`'s centroid via an online mean over normalized
    /// vectors, then renormalize; bumps `samples`. A no-op if the id is unknown.
    pub fn update_centroid(&self, id: &str, embedding: &[f32]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        fold_into(&conn, id, embedding)
    }

    /// Give a cluster a name **as a person's deliberate edit** (People screen / admin
    /// console) and mark it user-labeled ([`NameSource::Admin`]). Returns whether a row
    /// changed.
    pub fn rename(&self, id: &str, name: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE speakers SET name = ?1, labeled = 1, name_source = 'admin', updated_at = ?2 \
             WHERE id = ?3",
            params![name.trim(), now(), id],
        )?;
        Ok(n > 0)
    }

    /// Name a still-**anonymous** cluster from speech ("my name is …";
    /// [`NameSource::Spoken`]). Never overwrites an existing name. Returns whether the
    /// profile was named (false when it is unknown or already has a name).
    pub fn name_from_voice(&self, id: &str, name: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE speakers SET name = ?1, labeled = 1, name_source = 'spoken', updated_at = ?2 \
             WHERE id = ?3 AND (name IS NULL OR TRIM(name) = '')",
            params![name.trim(), now(), id],
        )?;
        Ok(n > 0)
    }

    /// Fetch one profile by id.
    pub fn get(&self, id: &str) -> Result<Option<SpeakerProfile>> {
        let conn = self.conn.lock().unwrap();
        let p = conn
            .query_row(
                "SELECT id, name, labeled, samples, created_at, updated_at, name_source \
                 FROM speakers WHERE id = ?1",
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
            "SELECT id, name, labeled, samples, created_at, updated_at, name_source \
             FROM speakers ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([], row_to_profile)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Delete a profile by id; returns whether a row was removed. In the same
    /// transaction, records the alias `id → household` (see [`super::aliases`]) so the
    /// chat log and inactive GraphRAG stores stop attributing anything to `id`.
    pub fn delete(&self, id: &str) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let n = tx.execute("DELETE FROM speakers WHERE id = ?1", params![id])?;
        if n > 0 {
            super::aliases::record(&tx, id, None)?;
        }
        tx.commit().context("committing speaker delete")?;
        Ok(n > 0)
    }

    /// The persisted speaker-id aliases left by earlier merges/deletes.
    pub fn aliases(&self) -> Result<super::AliasMap> {
        super::aliases::load(&self.conn.lock().unwrap())
    }

    /// Merge `drop_id` into `keep_id`: fold the voiceprints (weighted by sample
    /// counts), delete the dropped profile and record the alias `drop_id → keep_id`
    /// (see [`super::aliases`]), in **one transaction** — a crash leaves either both
    /// profiles untouched or the merge complete. Moving the
    /// speaker's memories, clips and graph nodes is [`super::SpeakerService::merge`]'s
    /// job. Returns whether a merge happened. A user-given name on `keep` is
    /// preserved; if only `drop` was named, its name carries over.
    pub fn merge(&self, keep_id: &str, drop_id: &str) -> Result<bool> {
        if keep_id == drop_id {
            return Ok(false);
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // (centroid blob, samples, name, labeled, name_source)
        type MergeRow = (Vec<u8>, i64, Option<String>, i64, Option<String>);
        let load = |id: &str| -> Result<Option<MergeRow>> {
            Ok(tx
                .query_row(
                    "SELECT centroid, samples, name, labeled, name_source FROM speakers WHERE id = ?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()?)
        };
        let (Some((kb, ks, kn, kl, ksrc)), Some((db, ds, dn, _, dsrc))) =
            (load(keep_id)?, load(drop_id)?)
        else {
            return Ok(false);
        };
        let merged = fold_mean(&decode_vec(&kb), ks as f32, &decode_vec(&db), ds as f32);
        // The kept name wins; its provenance travels with it.
        let (name, source) = match kn {
            Some(n) => (Some(n), ksrc),
            None => (dn, dsrc),
        };
        let labeled = if kl == 1 || name.is_some() { 1 } else { 0 };
        tx.execute(
            "UPDATE speakers SET centroid = ?1, samples = ?2, name = ?3, labeled = ?4, \
             name_source = ?5, updated_at = ?6 WHERE id = ?7",
            params![
                encode_vec(&merged),
                ks + ds,
                name,
                labeled,
                source,
                now(),
                keep_id
            ],
        )?;
        tx.execute("DELETE FROM speakers WHERE id = ?1", params![drop_id])?;
        super::aliases::record(&tx, drop_id, Some(keep_id))?;
        tx.commit().context("committing speaker merge")?;
        Ok(true)
    }

    /// Number of stored profiles.
    pub fn count(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row("SELECT COUNT(*) FROM speakers", [], |r| r.get::<_, i64>(0))? as usize)
    }

    fn next_id(&self) -> String {
        self.ids.mint("spk-")
    }
}

/// How [`SpeakerRegistry::identify_or_create`] attributed an utterance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attribution {
    /// Confident match; the profile's centroid absorbed the sample.
    Matched,
    /// Borderline (or ambiguous) match; attributed without touching the centroid.
    Tentative,
    /// Below the match threshold but a clear household winner (see
    /// [`MatchPolicy::clear_margin`]): identified, centroid untouched.
    Distinct,
    /// No usable match; a new anonymous cluster was created.
    Created,
}

/// The outcome of [`SpeakerRegistry::identify_or_create`].
#[derive(Debug, Clone, PartialEq)]
pub struct Identified {
    /// The attributed profile's id.
    pub id: String,
    /// Its name, if the cluster has been named.
    pub name: Option<String>,
    /// Where that name came from.
    pub name_source: Option<NameSource>,
    /// Cosine similarity to the matched centroid (`0.0` for a new cluster).
    pub score: f32,
    /// Lead over the closest other named person (`None` with no rival or a new cluster).
    pub margin: Option<f32>,
    pub attribution: Attribution,
}

/// The nearest profile to an embedding **and its closest named rival** — the input to
/// household-relative scoring.
#[derive(Debug, Clone, PartialEq)]
pub struct Ranked {
    /// The best-scoring profile.
    pub id: String,
    /// Its cosine similarity.
    pub score: f32,
    /// Whether that profile has been named by a person (enrollment / admin / voice).
    pub labeled: bool,
    /// The best-scoring **labeled** profile other than `id`, with its cosine. `None`
    /// when no other named person exists. Anonymous clusters are deliberately not
    /// rivals: they are often fragments of a known person's voice, and a near-tie with
    /// your own fragment is not evidence that someone else is speaking.
    pub runner_up: Option<(String, f32)>,
}

impl Ranked {
    /// How far the best match leads its named rival (`None` with no rival).
    pub fn margin(&self) -> Option<f32> {
        self.runner_up.as_ref().map(|(_, s)| self.score - s)
    }
}

/// How a cosine score — and its lead over the rest of the household — turns into an
/// [`Attribution`]. The household is a small, closed set, so *relative* evidence ("much
/// closer to Dana than to anyone else") complements the absolute thresholds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MatchPolicy {
    /// Cosine ≥ this ⇒ a confident match (if also unambiguous).
    pub match_threshold: f32,
    /// Cosine below this ⇒ no usable match.
    pub new_threshold: f32,
    /// A score ≥ `match_threshold` whose lead over the named runner-up is below this
    /// is **ambiguous**: downgraded to tentative so neither person's voiceprint absorbs
    /// a sample that could be the other's. `0` disables the check.
    pub min_margin: f32,
    /// A score in `[new_threshold, match_threshold)` for a **named** profile that leads
    /// another named person by at least this is a clear household winner:
    /// [`Attribution::Distinct`] (identified, centroid untouched). `0` disables it.
    pub clear_margin: f32,
}

impl MatchPolicy {
    /// Absolute thresholds only (no household-relative scoring).
    pub fn absolute(match_threshold: f32, new_threshold: f32) -> Self {
        Self {
            match_threshold,
            new_threshold,
            min_margin: 0.0,
            clear_margin: 0.0,
        }
    }

    /// Decide how `r` is attributed. Never returns [`Attribution::Created`] for a score
    /// ≥ `new_threshold`; below it, `Created` means "no usable match" (the caller
    /// decides whether that mints a cluster).
    pub fn decide(&self, r: &Ranked) -> Attribution {
        let margin = r.margin();
        if r.score >= self.match_threshold {
            if margin.is_some_and(|m| m < self.min_margin) {
                Attribution::Tentative
            } else {
                Attribution::Matched
            }
        } else if r.score >= self.new_threshold {
            if self.clear_margin > 0.0
                && r.labeled
                && margin.is_some_and(|m| m >= self.clear_margin)
            {
                Attribution::Distinct
            } else {
                Attribution::Tentative
            }
        } else {
            Attribution::Created
        }
    }
}

/// Nearest profile to `embedding` by cosine (`None` when there are no profiles).
fn best_match(conn: &Connection, embedding: &[f32]) -> Result<Option<(String, f32)>> {
    Ok(rank(conn, embedding)?.map(|r| (r.id, r.score)))
}

/// Score `embedding` against every profile: the best match plus its best **named**
/// rival (`None` when there are no profiles).
fn rank(conn: &Connection, embedding: &[f32]) -> Result<Option<Ranked>> {
    let mut stmt = conn.prepare("SELECT id, centroid, labeled FROM speakers")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, i64>(2)? != 0,
        ))
    })?;
    let mut scored = Vec::new();
    for row in rows {
        let (id, blob, labeled) = row?;
        scored.push((id, cosine(embedding, &decode_vec(&blob)), labeled));
    }
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut it = scored.into_iter();
    let Some((id, score, labeled)) = it.next() else {
        return Ok(None);
    };
    let runner_up = it.find(|(_, _, l)| *l).map(|(id, s, _)| (id, s));
    Ok(Some(Ranked {
        id,
        score,
        labeled,
        runner_up,
    }))
}

/// Insert a new anonymous one-sample cluster `id` seeded with `embedding`.
fn insert_cluster(conn: &Connection, id: &str, embedding: &[f32]) -> Result<()> {
    let now = now();
    conn.execute(
        "INSERT INTO speakers (id, name, labeled, centroid, dims, samples, created_at, updated_at) \
         VALUES (?1, NULL, 0, ?2, ?3, 1, ?4, ?4)",
        params![id, encode_vec(embedding), embedding.len() as i64, now],
    )
    .context("inserting speaker cluster")?;
    Ok(())
}

/// Fold `embedding` into `id`'s centroid (online mean, renormalized) and bump its
/// sample count. A no-op if the id is unknown.
fn fold_into(conn: &Connection, id: &str, embedding: &[f32]) -> Result<()> {
    let existing: Option<(Vec<u8>, i64)> = conn
        .query_row(
            "SELECT centroid, samples FROM speakers WHERE id = ?1",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((blob, samples)) = existing else {
        return Ok(());
    };
    let merged = fold_mean(&decode_vec(&blob), samples as f32, embedding, 1.0);
    conn.execute(
        "UPDATE speakers SET centroid = ?1, samples = samples + 1, updated_at = ?2 WHERE id = ?3",
        params![encode_vec(&merged), now(), id],
    )?;
    Ok(())
}

/// A profile's name and its provenance (`(None, None)` when unnamed or unknown).
fn name_of(conn: &Connection, id: &str) -> Result<(Option<String>, Option<NameSource>)> {
    Ok(conn
        .query_row(
            "SELECT name, name_source FROM speakers WHERE id = ?1",
            params![id],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .optional()?
        .map(|(n, s)| (n, s.as_deref().and_then(NameSource::parse)))
        .unwrap_or((None, None)))
}

fn row_to_profile(row: &rusqlite::Row) -> rusqlite::Result<SpeakerProfile> {
    let name: Option<String> = row.get(1)?;
    let source: Option<String> = row.get(6)?;
    Ok(SpeakerProfile {
        id: row.get(0)?,
        name_source: name
            .as_ref()
            .and(source.as_deref().and_then(NameSource::parse)),
        name,
        labeled: row.get::<_, i64>(2)? != 0,
        samples: row.get::<_, i64>(3)? as u64,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

/// Add `speakers.name_source` to a pre-provenance registry. Legacy named profiles are
/// backfilled by how they were most plausibly named: one backed by at least
/// [`super::MIN_ENROLL_SAMPLES`] samples went through guided enrollment (or was trained
/// up after a People-screen rename) → `enrolled`; a thinner one was named by voice →
/// `spoken`. Idempotent: a no-op once the column exists.
fn migrate_name_source(conn: &Connection) -> Result<()> {
    let has_column = conn
        .prepare("PRAGMA table_info(speakers)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .any(|name| name == "name_source");
    if has_column {
        return Ok(());
    }
    conn.execute("ALTER TABLE speakers ADD COLUMN name_source TEXT", [])
        .context("adding speakers.name_source")?;
    conn.execute(
        "UPDATE speakers SET name_source = CASE WHEN samples >= ?1 THEN 'enrolled' ELSE 'spoken' END \
         WHERE name IS NOT NULL AND TRIM(name) <> ''",
        params![super::MIN_ENROLL_SAMPLES as i64],
    )
    .context("backfilling speakers.name_source")?;
    Ok(())
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
    l2_normalize(out)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speaker::embed::{MockSpeakerEmbedder, SpeakerEmbedder};

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
        assert!(
            score > 0.6,
            "same voice matches above threshold (got {score})"
        );

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
        let id = reg
            .create_cluster(&e.embed(&tone(200.0, 1500)).unwrap())
            .unwrap();
        reg.update_centroid(&id, &e.embed(&tone(205.0, 1500)).unwrap())
            .unwrap();
        let p = reg.get(&id).unwrap().unwrap();
        assert_eq!(p.samples, 2);
        // Read the centroid back and confirm it is still ~unit length.
        let conn = reg.conn.lock().unwrap();
        let blob: Vec<u8> = conn
            .query_row(
                "SELECT centroid FROM speakers WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        drop(conn);
        let c = decode_vec(&blob);
        let norm: f32 = c.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-4,
            "centroid stays normalized (got {norm})"
        );
    }

    #[test]
    fn rename_marks_labeled() {
        let reg = SpeakerRegistry::open_in_memory().unwrap();
        let id = reg
            .create_cluster(&l2_normalize(vec![1.0, 0.0, 0.0]))
            .unwrap();
        assert!(!reg.get(&id).unwrap().unwrap().labeled);
        assert!(reg.rename(&id, "Sam").unwrap());
        let p = reg.get(&id).unwrap().unwrap();
        assert_eq!(p.name.as_deref(), Some("Sam"));
        assert!(p.labeled);
    }

    #[test]
    fn name_provenance_guards_enrollment_by_name() {
        let reg = SpeakerRegistry::open_in_memory().unwrap();
        // A spoken label is never a fold target for enrollment-by-name …
        let spoken = reg.create_cluster(&l2_normalize(vec![1.0, 0.0])).unwrap();
        assert!(reg.name_from_voice(&spoken, "Brendon").unwrap());
        let p = reg.get(&spoken).unwrap().unwrap();
        assert_eq!(p.name_source, Some(NameSource::Spoken));
        assert_eq!(reg.find_by_name("brendon").unwrap(), None);
        assert_eq!(
            reg.find_named("brendon").unwrap().map(|p| p.id),
            Some(spoken.clone())
        );
        // … a voice never overwrites an existing name …
        assert!(!reg.name_from_voice(&spoken, "Kristina").unwrap());
        assert_eq!(
            reg.get(&spoken).unwrap().unwrap().name.as_deref(),
            Some("Brendon")
        );
        // … but an admin rename and an enrollment are trusted.
        assert!(reg.rename(&spoken, "Brendon").unwrap());
        assert_eq!(reg.find_by_name("Brendon").unwrap(), Some(spoken.clone()));
        let enrolled = reg
            .create_labeled("Kristina", &l2_normalize(vec![0.0, 1.0]), 3)
            .unwrap();
        assert_eq!(
            reg.get(&enrolled).unwrap().unwrap().name_source,
            Some(NameSource::Enrolled)
        );
        assert_eq!(reg.find_by_name("kristina").unwrap(), Some(enrolled));
    }

    #[test]
    fn legacy_names_are_backfilled_by_sample_support() {
        let conn = crate::db::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE speakers (id TEXT PRIMARY KEY, name TEXT, labeled INTEGER NOT NULL DEFAULT 0, \
             centroid BLOB NOT NULL, dims INTEGER NOT NULL, samples INTEGER NOT NULL DEFAULT 0, \
             created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
             INSERT INTO speakers VALUES ('spk-a', 'Christina', 1, x'0000803f', 1, 1, 1, 1);
             INSERT INTO speakers VALUES ('spk-b', 'Kristina', 1, x'0000803f', 1, 5, 2, 2);
             INSERT INTO speakers VALUES ('spk-c', NULL, 0, x'0000803f', 1, 1, 3, 3);",
        )
        .unwrap();
        let reg = SpeakerRegistry::from_conn(conn).unwrap();
        let src = |id: &str| reg.get(id).unwrap().unwrap().name_source;
        assert_eq!(src("spk-a"), Some(NameSource::Spoken));
        assert_eq!(src("spk-b"), Some(NameSource::Enrolled));
        assert_eq!(src("spk-c"), None);
    }

    #[test]
    fn merge_keeps_the_kept_names_provenance() {
        let reg = SpeakerRegistry::open_in_memory().unwrap();
        let keep = reg.create_cluster(&l2_normalize(vec![1.0, 0.0])).unwrap();
        let drop = reg.create_cluster(&l2_normalize(vec![0.0, 1.0])).unwrap();
        reg.name_from_voice(&drop, "Christina").unwrap();
        reg.merge(&keep, &drop).unwrap();
        assert_eq!(
            reg.get(&keep).unwrap().unwrap().name_source,
            Some(NameSource::Spoken),
            "a carried-over name keeps its (spoken) provenance"
        );
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

    /// A merge whose DELETE fails must not leave the kept profile half-updated.
    #[test]
    fn failed_merge_rolls_back_completely() {
        let reg = SpeakerRegistry::open_in_memory().unwrap();
        let keep = reg.create_cluster(&l2_normalize(vec![1.0, 0.0])).unwrap();
        let drop = reg.create_cluster(&l2_normalize(vec![0.0, 1.0])).unwrap();
        reg.rename(&drop, "Dana").unwrap();
        reg.conn
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER no_delete BEFORE DELETE ON speakers \
                 BEGIN SELECT RAISE(ABORT, 'simulated crash'); END;",
            )
            .unwrap();

        assert!(reg.merge(&keep, &drop).is_err());
        let k = reg.get(&keep).unwrap().unwrap();
        assert_eq!(k.samples, 1, "kept centroid/samples untouched");
        assert_eq!(k.name, None, "dropped name not carried over");
        assert_eq!(reg.count().unwrap(), 2);
    }

    #[test]
    fn identify_or_create_matches_tentatively_attributes_or_creates() {
        let reg = SpeakerRegistry::open_in_memory().unwrap();
        let a = l2_normalize(vec![1.0, 0.0]);
        let first = reg
            .identify_or_create(&a, &MatchPolicy::absolute(0.9, 0.5))
            .unwrap();
        assert_eq!(first.attribution, Attribution::Created);

        reg.rename(&first.id, "Sam").unwrap();
        let again = reg
            .identify_or_create(&a, &MatchPolicy::absolute(0.9, 0.5))
            .unwrap();
        assert_eq!(again.attribution, Attribution::Matched);
        assert_eq!(again.id, first.id);
        assert_eq!(again.name.as_deref(), Some("Sam"));
        assert_eq!(reg.get(&first.id).unwrap().unwrap().samples, 2);

        // cos(45°) ≈ 0.71: between the thresholds → attributed, centroid untouched.
        let near = reg
            .identify_or_create(
                &l2_normalize(vec![1.0, 1.0]),
                &MatchPolicy::absolute(0.9, 0.5),
            )
            .unwrap();
        assert_eq!(near.attribution, Attribution::Tentative);
        assert_eq!(near.id, first.id);
        assert_eq!(reg.get(&first.id).unwrap().unwrap().samples, 2);

        let other = reg
            .identify_or_create(
                &l2_normalize(vec![-1.0, 0.2]),
                &MatchPolicy::absolute(0.9, 0.5),
            )
            .unwrap();
        assert_eq!(other.attribution, Attribution::Created);
        assert_eq!(reg.count().unwrap(), 2);
    }

    /// A unit vector at `deg` degrees in the plane (cosine between two = cos of the gap).
    fn at(deg: f32) -> Vec<f32> {
        let r = deg.to_radians();
        vec![r.cos(), r.sin()]
    }

    fn ranked(score: f32, labeled: bool, rival: Option<f32>) -> Ranked {
        Ranked {
            id: "spk-a".into(),
            score,
            labeled,
            runner_up: rival.map(|s| ("spk-b".to_string(), s)),
        }
    }

    #[test]
    fn rank_reports_the_best_named_rival_and_ignores_anonymous_clusters() {
        let reg = SpeakerRegistry::open_in_memory().unwrap();
        let sam = reg.create_labeled("Sam", &at(0.0), 5).unwrap();
        // An anonymous fragment very close to Sam is not a rival …
        reg.create_cluster(&at(5.0)).unwrap();
        let dana = reg.create_labeled("Dana", &at(60.0), 5).unwrap();

        let r = reg.rank(&at(0.0)).unwrap().unwrap();
        assert_eq!(r.id, sam);
        assert!(r.labeled);
        let (rival, rival_score) = r.runner_up.clone().unwrap();
        assert_eq!(
            rival, dana,
            "the named runner-up, not the closer anonymous fragment"
        );
        assert!((rival_score - 0.5).abs() < 1e-4);
        assert!((r.margin().unwrap() - 0.5).abs() < 1e-4);
    }

    #[test]
    fn policy_downgrades_ambiguous_matches_and_promotes_clear_household_winners() {
        let p = MatchPolicy {
            match_threshold: 0.6,
            new_threshold: 0.4,
            min_margin: 0.05,
            clear_margin: 0.2,
        };
        // Above the match threshold and well ahead of everyone: confident.
        assert_eq!(
            p.decide(&ranked(0.7, true, Some(0.3))),
            Attribution::Matched
        );
        // No other named person to compare against: absolute thresholds alone.
        assert_eq!(p.decide(&ranked(0.7, true, None)), Attribution::Matched);
        // Above the match threshold but neck-and-neck with another person: ambiguous.
        assert_eq!(
            p.decide(&ranked(0.7, true, Some(0.68))),
            Attribution::Tentative
        );
        // Borderline score, but far closer to this person than to anyone else.
        assert_eq!(
            p.decide(&ranked(0.5, true, Some(0.2))),
            Attribution::Distinct
        );
        // … not with too small a lead,
        assert_eq!(
            p.decide(&ranked(0.5, true, Some(0.35))),
            Attribution::Tentative
        );
        // … not for an anonymous cluster,
        assert_eq!(
            p.decide(&ranked(0.5, false, Some(0.1))),
            Attribution::Tentative
        );
        // … and not without a second named person to be distinct from.
        assert_eq!(p.decide(&ranked(0.5, true, None)), Attribution::Tentative);
        // Below the new threshold nothing is promoted.
        assert_eq!(
            p.decide(&ranked(0.3, true, Some(-0.5))),
            Attribution::Created
        );

        // Margins of 0 reproduce the absolute-threshold behavior.
        let abs = MatchPolicy::absolute(0.6, 0.4);
        assert_eq!(
            abs.decide(&ranked(0.7, true, Some(0.69))),
            Attribution::Matched
        );
        assert_eq!(
            abs.decide(&ranked(0.5, true, Some(-0.5))),
            Attribution::Tentative
        );
    }

    #[test]
    fn ambiguous_match_does_not_fold_and_distinct_winner_is_identified_without_folding() {
        let reg = SpeakerRegistry::open_in_memory().unwrap();
        let sam = reg.create_labeled("Sam", &at(0.0), 5).unwrap();
        let dana = reg.create_labeled("Dana", &at(60.0), 5).unwrap();
        let p = MatchPolicy {
            match_threshold: 0.8,
            new_threshold: 0.5,
            min_margin: 0.05,
            clear_margin: 0.2,
        };

        // Halfway between them: cos 30° ≈ 0.87 to both — above match, but a tie.
        let tie = reg.identify_or_create(&at(30.0), &p).unwrap();
        assert_eq!(tie.attribution, Attribution::Tentative);
        assert!(tie.margin.unwrap().abs() < 1e-4);
        assert_eq!(reg.get(&sam).unwrap().unwrap().samples, 5);
        assert_eq!(reg.get(&dana).unwrap().unwrap().samples, 5);

        // 40° away from Sam on the far side: cos 40° ≈ 0.77 (below match) vs Dana
        // cos 100° ≈ -0.17 — a clear household winner.
        let clear = reg.identify_or_create(&at(-40.0), &p).unwrap();
        assert_eq!(clear.attribution, Attribution::Distinct);
        assert_eq!(clear.id, sam);
        assert_eq!(clear.name.as_deref(), Some("Sam"));
        assert_eq!(
            reg.get(&sam).unwrap().unwrap().samples,
            5,
            "centroid untouched"
        );
        assert_eq!(reg.count().unwrap(), 2, "no cluster minted");
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
