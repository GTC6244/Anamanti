//! Persistent conversation memory (Plan.MD Phase 4, bullet 3; §4 "Persistent
//! memory"). A **SQLite** database on the Mac holds long-term facts and
//! preferences across sessions, with **FTS5** full-text search (no embedding model
//! in v1). Policy is **explicit + inferred**:
//!
//! - *explicit* — voice commands ("remember …", "forget …") add/remove entries;
//! - *inferred* — light heuristics auto-extract facts/prefs from ordinary turns.
//!
//! Entries are kept until cleared and are managed from settings (list/delete,
//! Phase 6) or by voice. The store is synchronous (rusqlite); calls are tiny and
//! run once per turn, and the connection is guarded by a `Mutex` so one `Arc` is
//! shared across concurrent turns.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

mod extract;
mod scope;

pub mod backend;
pub mod chatlog;
pub mod embed;
pub mod entity;
pub mod graphrag_control;
pub mod graphview;
pub mod jsonl;
pub mod promptlog;

// GraphRAG memory (embedded HelixDB) — always compiled. See memory_plan.md.
pub mod helix;
pub mod ingester;

pub use backend::HelixRecall;
pub use backend::{Recall, RecallResult, SqliteRecall};
pub use chatlog::{ChatLog, ChatLogRecord, TurnTiming, WrittenMemory};
pub use extract::{infer_memories, parse_command, MemoryCommand};
pub use graphrag_control::{EmbedStatus, ExtractStatus, GraphRagController};
pub use graphview::GraphView;
pub use promptlog::{PromptLog, PromptLogRecord};
pub use scope::{classify_scope, FactScope, MemoryScope, Provenance, ScopeKind};

/// Whether an entry is a discrete fact or a standing preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryKind {
    Fact,
    Preference,
}

impl MemoryKind {
    /// Stable wire/storage label (also used by the Phase-6 control protocol).
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryKind::Fact => "fact",
            MemoryKind::Preference => "preference",
        }
    }
    fn from_str(s: &str) -> MemoryKind {
        match s {
            "preference" => MemoryKind::Preference,
            _ => MemoryKind::Fact,
        }
    }
}

/// How an entry was captured: at the user's explicit request, or auto-inferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySource {
    Explicit,
    Inferred,
}

impl MemorySource {
    /// Stable wire/storage label (also used by the Phase-6 control protocol).
    pub fn as_str(self) -> &'static str {
        match self {
            MemorySource::Explicit => "explicit",
            MemorySource::Inferred => "inferred",
        }
    }
    fn from_str(s: &str) -> MemorySource {
        match s {
            "inferred" => MemorySource::Inferred,
            _ => MemorySource::Explicit,
        }
    }
}

/// One stored memory row.
#[derive(Debug, Clone, PartialEq)]
pub struct Memory {
    pub id: i64,
    pub kind: MemoryKind,
    pub content: String,
    pub source: MemorySource,
    pub created_at: i64,
    /// Who the memory is true for (SpeakerScopedMemoryPlan.md §2.1).
    pub scope: ScopeKind,
    /// The person this memory belongs to (`spk-…`) — set iff `scope` is `Person`.
    pub speaker_id: Option<String>,
    /// For an `Unknown`-scope memory: the best-guess person, if any.
    pub candidate_speaker_id: Option<String>,
    /// Provenance: the turn's resolved speaker (`None` = unidentified / not recorded).
    pub said_by_speaker_id: Option<String>,
    /// Provenance: `identified | tentative | new | unknown` at write time.
    pub speaker_resolution: Option<String>,
    /// Provenance: voice-match cosine at write time.
    pub speaker_confidence: Option<f32>,
    /// Provenance: the conversation turn that wrote it.
    pub turn_id: Option<String>,
}

/// Who is asking to forget something by voice — scopes "forget …" / "forget that" so
/// one person can never delete another person's memories (plan root cause 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forgetter<'a> {
    /// A confidently identified person: their own memories, plus anything they said.
    Identified(&'a str),
    /// A probable match: only memories that this voice itself said.
    Tentative(&'a str),
    /// An unrecognized voice: only memories an unrecognized voice said recently
    /// (within [`UNKNOWN_FORGET_WINDOW_SECS`]).
    Unknown,
}

/// How far back an unrecognized voice may "forget" what an unrecognized voice said —
/// long enough to undo a just-made mistake, short enough not to reach other people's.
pub const UNKNOWN_FORGET_WINDOW_SECS: i64 = 10 * 60;

/// The columns every row read selects, in [`row_to_memory`] order.
const MEMORY_COLS: &str =
    "m.id, m.kind, m.content, m.source, m.created_at, m.speaker_id, m.scope, \
     m.candidate_speaker_id, m.said_by_speaker_id, m.speaker_resolution, m.speaker_confidence, \
     m.turn_id";

/// The persistent memory store.
pub struct MemoryStore {
    conn: Mutex<Connection>,
}

impl MemoryStore {
    /// Open (creating if needed) the SQLite database at `path` and run migrations.
    ///
    /// The connection is opened through [`crate::db::open`] (busy timeout + WAL),
    /// the same policy as the speaker registry that shares this file.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = crate::db::open(path).context("opening memory database")?;
        Self::from_conn(conn)
    }

    /// An ephemeral in-memory store (tests).
    pub fn open_in_memory() -> Result<Self> {
        Self::from_conn(crate::db::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS memories (
                id         INTEGER PRIMARY KEY,
                kind       TEXT NOT NULL,
                content    TEXT NOT NULL,
                source     TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                speaker_id TEXT
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts
                USING fts5(content, content='memories', content_rowid='id');
            CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
                INSERT INTO memories_fts(rowid, content) VALUES (new.id, new.content);
            END;
            CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, content)
                    VALUES ('delete', old.id, old.content);
            END;
            CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, content)
                    VALUES ('delete', old.id, old.content);
                INSERT INTO memories_fts(rowid, content) VALUES (new.id, new.content);
            END;
            CREATE TABLE IF NOT EXISTS turn_attribution_overrides (
                turn_id    TEXT PRIMARY KEY,
                speaker_id TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            "#,
        )
        .context("initializing memory schema")?;
        migrate_speaker_column(&conn)?;
        migrate_scope_columns(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Add a **household** entry (true for everyone; no provenance). Used by the
    /// device/admin "add a memory" paths and tests.
    pub fn add(&self, kind: MemoryKind, content: &str, source: MemorySource) -> Result<i64> {
        self.add_in(
            kind,
            content,
            source,
            &MemoryScope::Household,
            &Provenance::default(),
        )
    }

    /// Add an entry owned by `speaker_id` (`None` = household). A compatibility
    /// wrapper over [`add_in`](Self::add_in) with no provenance.
    pub fn add_scoped(
        &self,
        kind: MemoryKind,
        content: &str,
        source: MemorySource,
        speaker_id: Option<&str>,
    ) -> Result<i64> {
        let scope = match speaker_id {
            Some(id) => MemoryScope::Person(id.to_string()),
            None => MemoryScope::Household,
        };
        self.add_in(kind, content, source, &scope, &Provenance::default())
    }

    /// Add an entry in `scope` with its `provenance`, returning its row id. Duplicate
    /// content of the same kind **in the same scope** (and, for `Person`, the same
    /// person) is coalesced — the existing id is returned — so repeated turns don't
    /// pile up copies; the same words said by two people are kept separately.
    pub fn add_in(
        &self,
        kind: MemoryKind,
        content: &str,
        source: MemorySource,
        scope: &MemoryScope,
        provenance: &Provenance,
    ) -> Result<i64> {
        let content = content.trim();
        let conn = self.conn.lock().unwrap();
        // `IS` (not `=`) so a NULL speaker matches NULL — SQLite's null-safe compare.
        if let Ok(existing) = conn.query_row(
            "SELECT id FROM memories WHERE kind = ?1 AND content = ?2 AND scope = ?3 \
             AND speaker_id IS ?4",
            params![
                kind.as_str(),
                content,
                scope.kind().as_str(),
                scope.speaker_id()
            ],
            |row| row.get::<_, i64>(0),
        ) {
            return Ok(existing);
        }
        conn.execute(
            "INSERT INTO memories (kind, content, source, created_at, speaker_id, scope, \
             candidate_speaker_id, said_by_speaker_id, speaker_resolution, speaker_confidence, \
             turn_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                kind.as_str(),
                content,
                source.as_str(),
                crate::clock::now_secs(),
                scope.speaker_id(),
                scope.kind().as_str(),
                scope.candidate(),
                provenance.said_by,
                provenance.resolution,
                provenance.confidence.map(f64::from),
                provenance.turn_id,
            ],
        )
        .context("inserting memory")?;
        Ok(conn.last_insert_rowid())
    }

    /// All entries, most recent first (the settings list view) — every scope.
    pub fn list(&self) -> Result<Vec<Memory>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {MEMORY_COLS} FROM memories m ORDER BY m.created_at DESC, m.id DESC"
        ))?;
        let rows = stmt.query_map([], row_to_memory)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The unattributed (`Unknown`-scope) entries awaiting review, most recent first.
    pub fn list_unknown(&self) -> Result<Vec<Memory>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {MEMORY_COLS} FROM memories m WHERE m.scope = 'unknown' \
             ORDER BY m.created_at DESC, m.id DESC"
        ))?;
        let rows = stmt.query_map([], row_to_memory)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// How many `Unknown`-scope entries name `speaker_id` as their best guess — the
    /// "N things might be yours" count after that person enrolls.
    pub fn unknown_count_for_candidate(&self, speaker_id: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM memories WHERE scope = 'unknown' AND candidate_speaker_id = ?1",
            params![speaker_id],
            |r| r.get::<_, i64>(0),
        )? as usize)
    }

    /// One entry by id.
    pub fn get(&self, id: i64) -> Result<Option<Memory>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                &format!("SELECT {MEMORY_COLS} FROM memories m WHERE m.id = ?1"),
                params![id],
                row_to_memory,
            )
            .optional()?)
    }

    /// Full-text search across **all** scopes (unscoped) — used by the settings list.
    /// For per-turn recall use [`search_scoped`](Self::search_scoped).
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Memory>> {
        let match_expr = fts_match_expr(query);
        let conn = self.conn.lock().unwrap();
        if let Some(expr) = match_expr {
            let mut stmt = conn.prepare(&format!(
                "SELECT {MEMORY_COLS} FROM memories_fts f JOIN memories m ON m.id = f.rowid \
                 WHERE memories_fts MATCH ?1 ORDER BY rank LIMIT ?2"
            ))?;
            let rows = stmt.query_map(params![expr, limit as i64], row_to_memory)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        } else {
            let mut stmt = conn.prepare(&format!(
                "SELECT {MEMORY_COLS} FROM memories m ORDER BY m.created_at DESC, m.id DESC LIMIT ?1"
            ))?;
            let rows = stmt.query_map(params![limit as i64], row_to_memory)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        }
    }

    /// Full-text search for per-turn recall (SpeakerScopedMemoryPlan.md §3.1): a
    /// recognized reader (`Some(spk-…)`) gets **their own** entries plus **household**
    /// ones; an unrecognized reader (`None`) gets household entries only. `Unknown`-
    /// scope entries are **never** recalled. A degenerate query falls back to recent
    /// entries in the same scope so the LLM always gets some standing context.
    pub fn search_scoped(
        &self,
        query: &str,
        reader: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Memory>> {
        let match_expr = fts_match_expr(query);
        // `:reader` NULL matches no person row, leaving household entries only.
        let in_scope = "(m.scope = 'household' OR (m.scope = 'person' AND m.speaker_id = :reader))";
        let conn = self.conn.lock().unwrap();
        let rows = match match_expr {
            Some(expr) => {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {MEMORY_COLS} FROM memories_fts f JOIN memories m ON m.id = f.rowid \
                     WHERE memories_fts MATCH :expr AND {in_scope} ORDER BY rank LIMIT :limit"
                ))?;
                let mapped = stmt.query_map(
                    rusqlite::named_params! { ":expr": expr, ":limit": limit as i64, ":reader": reader },
                    row_to_memory,
                )?;
                mapped.collect::<rusqlite::Result<Vec<_>>>()?
            }
            None => {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {MEMORY_COLS} FROM memories m WHERE {in_scope} \
                     ORDER BY m.created_at DESC, m.id DESC LIMIT :limit"
                ))?;
                let mapped = stmt.query_map(
                    rusqlite::named_params! { ":limit": limit as i64, ":reader": reader },
                    row_to_memory,
                )?;
                mapped.collect::<rusqlite::Result<Vec<_>>>()?
            }
        };
        Ok(rows)
    }

    /// Delete one entry by id; returns whether a row was removed.
    pub fn delete(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("DELETE FROM memories WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    /// Delete every entry matching `query` regardless of scope (admin paths); returns
    /// the count. Voice "forget …" uses [`forget_matching_for`](Self::forget_matching_for).
    pub fn forget_matching(&self, query: &str) -> Result<usize> {
        let ids: Vec<i64> = self.search(query, 100)?.into_iter().map(|m| m.id).collect();
        self.delete_ids(&ids)
    }

    /// Voice "forget …": delete entries matching `query` that `who` may forget (their
    /// own, see [`Forgetter`]) — plus household entries when `include_household` (the
    /// request explicitly said "we"/"our"/"everyone"). Returns the count.
    pub fn forget_matching_for(
        &self,
        query: &str,
        who: Forgetter<'_>,
        include_household: bool,
    ) -> Result<usize> {
        let now = crate::clock::now_secs();
        let ids: Vec<i64> = self
            .search(query, 200)?
            .into_iter()
            .filter(|m| may_forget(m, who, include_household, now))
            .map(|m| m.id)
            .collect();
        self.delete_ids(&ids)
    }

    /// Delete the single most recently added entry regardless of scope (admin/tests);
    /// returns its content. Voice "forget that" uses [`forget_last_for`](Self::forget_last_for).
    pub fn forget_last(&self) -> Result<Option<String>> {
        let last = self.list()?.into_iter().next();
        self.forget_one(last)
    }

    /// Voice "forget that": delete the most recent entry `who` may forget; returns its
    /// content, if any.
    pub fn forget_last_for(&self, who: Forgetter<'_>) -> Result<Option<String>> {
        let now = crate::clock::now_secs();
        let last = self
            .list()?
            .into_iter()
            .find(|m| may_forget(m, who, false, now));
        self.forget_one(last)
    }

    fn forget_one(&self, m: Option<Memory>) -> Result<Option<String>> {
        match m {
            Some(m) => {
                self.delete(m.id)?;
                Ok(Some(m.content))
            }
            None => Ok(None),
        }
    }

    fn delete_ids(&self, ids: &[i64]) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let mut removed = 0;
        for id in ids {
            removed += conn.execute("DELETE FROM memories WHERE id = ?1", params![id])?;
        }
        Ok(removed)
    }

    /// Move entry `id` into `scope` (the admin review queue's *Attribute to → person* /
    /// *Make household*). Clears the best-guess candidate. Returns whether a row changed.
    pub fn attribute(&self, id: i64, scope: &MemoryScope) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "UPDATE memories SET scope = ?1, speaker_id = ?2, candidate_speaker_id = ?3 WHERE id = ?4",
            params![scope.kind().as_str(), scope.speaker_id(), scope.candidate(), id],
        )? > 0)
    }

    /// Re-attribute every memory written by the conversation turn `turn_id` that was
    /// about its speaker (`Person` or `Unknown` scope) to `to` — the conversations
    /// page's "This was → person" correction. Provenance is rewritten to match.
    /// Household facts keep their scope. Returns the number re-scoped.
    pub fn reattribute_turn(&self, turn_id: &str, to: &MemoryScope) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE memories SET said_by_speaker_id = ?1, speaker_resolution = 'corrected' \
             WHERE turn_id = ?2",
            params![to.speaker_id(), turn_id],
        )?;
        Ok(conn.execute(
            "UPDATE memories SET scope = ?1, speaker_id = ?2, candidate_speaker_id = ?3 \
             WHERE turn_id = ?4 AND scope IN ('person', 'unknown')",
            params![to.kind().as_str(), to.speaker_id(), to.candidate(), turn_id],
        )?)
    }

    /// Reassign every memory owned by `from` to `to` (used when two speaker
    /// clusters are merged, speaker_id_plan.md §4.5) — ownership, best-guess
    /// candidates and provenance alike. Returns the number of owned rows moved.
    pub fn reassign_speaker(&self, from: &str, to: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE memories SET candidate_speaker_id = ?1 WHERE candidate_speaker_id = ?2",
            params![to, from],
        )?;
        conn.execute(
            "UPDATE memories SET said_by_speaker_id = ?1 WHERE said_by_speaker_id = ?2",
            params![to, from],
        )?;
        Ok(conn.execute(
            "UPDATE memories SET speaker_id = ?1 WHERE speaker_id = ?2",
            params![to, from],
        )?)
    }

    /// Detach every memory owned by `id` from that person — used when their voiceprint
    /// is forgotten. Their memories are kept but move to the **unknown** scope (never
    /// recalled), not the household: a forgotten person's private facts must not
    /// become everyone's. Returns the number of owned rows changed.
    pub fn unassign_speaker(&self, id: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE memories SET candidate_speaker_id = NULL WHERE candidate_speaker_id = ?1",
            params![id],
        )?;
        conn.execute(
            "UPDATE memories SET said_by_speaker_id = NULL WHERE said_by_speaker_id = ?1",
            params![id],
        )?;
        Ok(conn.execute(
            "UPDATE memories SET speaker_id = NULL, scope = 'unknown' WHERE speaker_id = ?1",
            params![id],
        )?)
    }

    /// Record a conversations-page correction: turn `turn_id` was really said by
    /// `speaker_id` (`spk-…`, or `"unknown"`). Upserts.
    pub fn set_turn_override(&self, turn_id: &str, speaker_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO turn_attribution_overrides (turn_id, speaker_id, created_at) \
             VALUES (?1, ?2, ?3) ON CONFLICT(turn_id) DO UPDATE SET \
             speaker_id = excluded.speaker_id, created_at = excluded.created_at",
            params![turn_id, speaker_id, crate::clock::now_secs()],
        )?;
        Ok(())
    }

    /// Every turn-attribution correction, `turn_id → speaker_id`.
    pub fn turn_overrides(&self) -> Result<std::collections::HashMap<String, String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT turn_id, speaker_id FROM turn_attribution_overrides")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Remove all entries.
    pub fn clear(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM memories", [])?)
    }

    /// Total number of stored entries.
    pub fn count(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row("SELECT COUNT(*) FROM memories", [], |r| r.get::<_, i64>(0))? as usize)
    }
}

/// Whether `who` may delete `m` by voice (see [`Forgetter`]).
fn may_forget(m: &Memory, who: Forgetter<'_>, include_household: bool, now: i64) -> bool {
    if include_household && m.scope == ScopeKind::Household {
        return true;
    }
    match who {
        Forgetter::Identified(id) => {
            (m.scope == ScopeKind::Person && m.speaker_id.as_deref() == Some(id))
                || m.said_by_speaker_id.as_deref() == Some(id)
        }
        Forgetter::Tentative(id) => m.said_by_speaker_id.as_deref() == Some(id),
        // Only rows an unrecognized voice wrote (provenance recorded, no speaker), and
        // only recent ones.
        Forgetter::Unknown => {
            m.said_by_speaker_id.is_none()
                && m.speaker_resolution.is_some()
                && now - m.created_at <= UNKNOWN_FORGET_WINDOW_SECS
        }
    }
}

fn row_to_memory(row: &rusqlite::Row) -> rusqlite::Result<Memory> {
    Ok(Memory {
        id: row.get(0)?,
        kind: MemoryKind::from_str(&row.get::<_, String>(1)?),
        content: row.get(2)?,
        source: MemorySource::from_str(&row.get::<_, String>(3)?),
        created_at: row.get(4)?,
        speaker_id: row.get(5)?,
        scope: ScopeKind::parse(&row.get::<_, String>(6)?),
        candidate_speaker_id: row.get(7)?,
        said_by_speaker_id: row.get(8)?,
        speaker_resolution: row.get(9)?,
        speaker_confidence: row.get::<_, Option<f64>>(10)?.map(|v| v as f32),
        turn_id: row.get(11)?,
    })
}

/// The column names of `memories`.
fn memory_columns(conn: &Connection) -> Result<Vec<String>> {
    Ok(conn
        .prepare("PRAGMA table_info(memories)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .collect())
}

/// Add the `speaker_id` column to a pre-speaker-ID `memories` table. Fresh DBs get
/// it from `CREATE TABLE`; this backfills older ones. Idempotent.
fn migrate_speaker_column(conn: &Connection) -> Result<()> {
    if !memory_columns(conn)?.iter().any(|c| c == "speaker_id") {
        conn.execute("ALTER TABLE memories ADD COLUMN speaker_id TEXT", [])
            .context("adding memories.speaker_id column")?;
    }
    Ok(())
}

/// Add the scope + provenance columns (SpeakerScopedMemoryPlan.md §3.1) and, **once**
/// (when `scope` is first added), classify the existing rows: an owned row → `person`;
/// a speaker-less row was *unattributed*, not deliberately shared, so a
/// speaker-relative one ("The user's name is Brendan") → `unknown` and a house-wide one
/// ("the wifi password is …") → `household` ([`classify_scope`], decision D2).
/// Idempotent: later opens add nothing and reclassify nothing.
fn migrate_scope_columns(conn: &Connection) -> Result<()> {
    let cols = memory_columns(conn)?;
    let has = |c: &str| cols.iter().any(|x| x == c);
    let first_time = !has("scope");
    for (col, ty) in [
        ("scope", "TEXT NOT NULL DEFAULT 'household'"),
        ("candidate_speaker_id", "TEXT"),
        ("said_by_speaker_id", "TEXT"),
        ("speaker_resolution", "TEXT"),
        ("speaker_confidence", "REAL"),
        ("turn_id", "TEXT"),
    ] {
        if !has(col) {
            conn.execute(&format!("ALTER TABLE memories ADD COLUMN {col} {ty}"), [])
                .with_context(|| format!("adding memories.{col} column"))?;
        }
    }
    if !first_time {
        return Ok(());
    }
    conn.execute(
        "UPDATE memories SET scope = 'person' WHERE speaker_id IS NOT NULL",
        [],
    )?;
    let legacy: Vec<(i64, String)> = conn
        .prepare("SELECT id, content FROM memories WHERE speaker_id IS NULL")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, content) in legacy {
        let scope = match classify_scope(&content) {
            FactScope::Personal => ScopeKind::Unknown,
            FactScope::Household => ScopeKind::Household,
        };
        conn.execute(
            "UPDATE memories SET scope = ?1 WHERE id = ?2",
            params![scope.as_str(), id],
        )?;
    }
    Ok(())
}

/// Build a safe FTS5 `MATCH` expression from free text: keep alphanumeric tokens,
/// OR them together (quoted so FTS5 never sees an operator it can't parse).
/// Returns `None` when there's nothing searchable.
fn fts_match_expr(query: &str) -> Option<String> {
    let tokens: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 2)
        .map(|t| format!("\"{}\"", t.to_lowercase()))
        .collect();
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" OR "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_list_and_dedupe() {
        let store = MemoryStore::open_in_memory().unwrap();
        let a = store
            .add(
                MemoryKind::Fact,
                "The user's name is Sam",
                MemorySource::Explicit,
            )
            .unwrap();
        let b = store
            .add(
                MemoryKind::Fact,
                "The user's name is Sam",
                MemorySource::Inferred,
            )
            .unwrap();
        assert_eq!(a, b, "duplicate content of same kind coalesces");
        store
            .add(
                MemoryKind::Preference,
                "The user likes jazz",
                MemorySource::Inferred,
            )
            .unwrap();
        assert_eq!(store.count().unwrap(), 2);
        assert_eq!(store.list().unwrap().len(), 2);
    }

    #[test]
    fn full_text_search_finds_relevant_entries() {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .add(
                MemoryKind::Fact,
                "The user lives in Portland",
                MemorySource::Inferred,
            )
            .unwrap();
        store
            .add(
                MemoryKind::Preference,
                "The user likes jazz music",
                MemorySource::Inferred,
            )
            .unwrap();

        let hits = store.search("what music do I like", 5).unwrap();
        assert!(hits.iter().any(|m| m.content.contains("jazz")));

        // Degenerate query falls back to recent entries rather than erroring.
        let recent = store.search("??", 5).unwrap();
        assert_eq!(recent.len(), 2);
    }

    #[test]
    fn delete_and_forget_variants() {
        let store = MemoryStore::open_in_memory().unwrap();
        let id = store
            .add(
                MemoryKind::Fact,
                "The user drives a red car",
                MemorySource::Explicit,
            )
            .unwrap();
        store
            .add(
                MemoryKind::Fact,
                "The user has a cat named Milo",
                MemorySource::Inferred,
            )
            .unwrap();

        assert!(store.delete(id).unwrap());
        assert!(!store.delete(id).unwrap(), "second delete is a no-op");

        assert_eq!(store.forget_matching("Milo").unwrap(), 1);
        assert_eq!(store.count().unwrap(), 0);
    }

    #[test]
    fn scoped_search_isolates_speakers_but_shares_household() {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .add_scoped(
                MemoryKind::Preference,
                "The user likes jazz",
                MemorySource::Inferred,
                Some("spk-sam"),
            )
            .unwrap();
        store
            .add_scoped(
                MemoryKind::Preference,
                "The user hates jazz",
                MemorySource::Inferred,
                Some("spk-dana"),
            )
            .unwrap();
        store
            .add_scoped(
                MemoryKind::Fact,
                "The house wifi is FastNet",
                MemorySource::Explicit,
                None, // shared/household
            )
            .unwrap();

        // Sam sees his own jazz pref + the shared wifi fact, not Dana's.
        let sam = store
            .search_scoped("jazz wifi", Some("spk-sam"), 10)
            .unwrap();
        assert!(sam.iter().any(|m| m.content.contains("likes jazz")));
        assert!(sam.iter().any(|m| m.content.contains("FastNet")));
        assert!(!sam.iter().any(|m| m.content.contains("hates jazz")));

        // Household scope sees only shared entries.
        let shared = store.search_scoped("jazz wifi", None, 10).unwrap();
        assert!(shared.iter().all(|m| m.speaker_id.is_none()));
        assert!(shared.iter().any(|m| m.content.contains("FastNet")));

        // The same words from two people are stored as distinct rows.
        assert_eq!(store.count().unwrap(), 3);
    }

    #[test]
    fn same_content_different_speakers_not_coalesced() {
        let store = MemoryStore::open_in_memory().unwrap();
        let a = store
            .add_scoped(
                MemoryKind::Fact,
                "likes tea",
                MemorySource::Explicit,
                Some("spk-a"),
            )
            .unwrap();
        let b = store
            .add_scoped(
                MemoryKind::Fact,
                "likes tea",
                MemorySource::Explicit,
                Some("spk-b"),
            )
            .unwrap();
        let a2 = store
            .add_scoped(
                MemoryKind::Fact,
                "likes tea",
                MemorySource::Inferred,
                Some("spk-a"),
            )
            .unwrap();
        assert_ne!(a, b, "different speakers kept separate");
        assert_eq!(a, a2, "same speaker + content coalesces");
        assert_eq!(store.count().unwrap(), 2);
    }

    fn prov(said_by: Option<&str>, resolution: &str) -> Provenance {
        Provenance {
            said_by: said_by.map(str::to_string),
            resolution: Some(resolution.to_string()),
            confidence: Some(0.7),
            turn_id: Some("t-1".to_string()),
        }
    }

    #[test]
    fn unknown_scope_is_kept_but_never_recalled() {
        let store = MemoryStore::open_in_memory().unwrap();
        let unknown = MemoryScope::Unknown {
            candidate: Some("spk-b".to_string()),
        };
        let id = store
            .add_in(
                MemoryKind::Fact,
                "The user's name is Brendan",
                MemorySource::Explicit,
                &unknown,
                &prov(None, "unknown"),
            )
            .unwrap();
        store
            .add_in(
                MemoryKind::Fact,
                "The user's name is Kristina",
                MemorySource::Explicit,
                &MemoryScope::Person("spk-k".into()),
                &prov(Some("spk-k"), "identified"),
            )
            .unwrap();
        // Nobody — not a recognized person, not an unknown voice — recalls it.
        for reader in [Some("spk-k"), Some("spk-b"), None] {
            let hits = store.search_scoped("name", reader, 10).unwrap();
            assert!(
                !hits.iter().any(|m| m.content.contains("Brendan")),
                "{reader:?} must not recall an unknown-speaker fact"
            );
        }
        // Kristina sees her own; an unknown voice sees neither.
        assert!(store
            .search_scoped("name", Some("spk-k"), 10)
            .unwrap()
            .iter()
            .any(|m| m.content.contains("Kristina")));
        assert!(store.search_scoped("name", None, 10).unwrap().is_empty());
        // It is listed for review, with its candidate + provenance.
        let pending = store.list_unknown().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].candidate_speaker_id.as_deref(), Some("spk-b"));
        assert_eq!(pending[0].speaker_resolution.as_deref(), Some("unknown"));
        assert_eq!(pending[0].turn_id.as_deref(), Some("t-1"));
        assert_eq!(store.unknown_count_for_candidate("spk-b").unwrap(), 1);
        // Attributing it makes it Brendon's.
        assert!(store
            .attribute(id, &MemoryScope::Person("spk-b".into()))
            .unwrap());
        assert!(store
            .search_scoped("name", Some("spk-b"), 10)
            .unwrap()
            .iter()
            .any(|m| m.content.contains("Brendan")));
        assert!(store.list_unknown().unwrap().is_empty());
    }

    #[test]
    fn legacy_speakerless_rows_are_classified_once() {
        let conn = crate::db::open_in_memory().unwrap();
        // The pre-scope schema, FTS index + triggers included (as every install has).
        conn.execute_batch(
            "CREATE TABLE memories (id INTEGER PRIMARY KEY, kind TEXT NOT NULL, content TEXT NOT NULL, \
             source TEXT NOT NULL, created_at INTEGER NOT NULL, speaker_id TEXT);
             CREATE VIRTUAL TABLE memories_fts USING fts5(content, content='memories', content_rowid='id');
             CREATE TRIGGER memories_ai AFTER INSERT ON memories BEGIN
                 INSERT INTO memories_fts(rowid, content) VALUES (new.id, new.content); END;
             CREATE TRIGGER memories_ad AFTER DELETE ON memories BEGIN
                 INSERT INTO memories_fts(memories_fts, rowid, content) VALUES ('delete', old.id, old.content); END;
             CREATE TRIGGER memories_au AFTER UPDATE ON memories BEGIN
                 INSERT INTO memories_fts(memories_fts, rowid, content) VALUES ('delete', old.id, old.content);
                 INSERT INTO memories_fts(rowid, content) VALUES (new.id, new.content); END;
             INSERT INTO memories VALUES (1, 'preference', 'The user likes the words from songs', 'inferred', 1, NULL);
             INSERT INTO memories VALUES (2, 'fact', 'The user''s name is Brendan', 'explicit', 2, NULL);
             INSERT INTO memories VALUES (3, 'fact', 'The user''s name is Christina', 'explicit', 3, 'spk-k');
             INSERT INTO memories VALUES (4, 'fact', 'the wifi password is FastNet', 'explicit', 4, NULL);",
        )
        .unwrap();
        let store = MemoryStore::from_conn(conn).unwrap();
        let scope_of = |id: i64| store.get(id).unwrap().unwrap().scope;
        assert_eq!(scope_of(1), ScopeKind::Unknown);
        assert_eq!(scope_of(2), ScopeKind::Unknown);
        assert_eq!(scope_of(3), ScopeKind::Person);
        assert_eq!(scope_of(4), ScopeKind::Household);
        // The headline bug: an unrecognized "who am I" no longer sees "Brendan".
        assert!(!store
            .search_scoped("name", None, 10)
            .unwrap()
            .iter()
            .any(|m| m.content.contains("Brendan")));
        // A deliberate household row written later is not reclassified on reopen.
        store
            .add(MemoryKind::Fact, "The user is tall", MemorySource::Explicit)
            .unwrap();
        let conn = store.conn.into_inner().unwrap();
        let store = MemoryStore::from_conn(conn).unwrap();
        assert!(store
            .list()
            .unwrap()
            .iter()
            .any(|m| m.content == "The user is tall" && m.scope == ScopeKind::Household));
    }

    #[test]
    fn voice_forget_only_reaches_your_own_memories() {
        let store = MemoryStore::open_in_memory().unwrap();
        let person = |id: &str| MemoryScope::Person(id.to_string());
        store
            .add_in(
                MemoryKind::Preference,
                "The user likes jazz",
                MemorySource::Inferred,
                &person("spk-b"),
                &prov(Some("spk-b"), "identified"),
            )
            .unwrap();
        store
            .add_in(
                MemoryKind::Preference,
                "The user likes jazz",
                MemorySource::Inferred,
                &person("spk-k"),
                &prov(Some("spk-k"), "identified"),
            )
            .unwrap();
        store
            .add_in(
                MemoryKind::Fact,
                "our jazz club meets Friday",
                MemorySource::Explicit,
                &MemoryScope::Household,
                &prov(Some("spk-b"), "identified"),
            )
            .unwrap();
        // Kristina forgets jazz: only her row goes; Brendon's and the household's stay.
        assert_eq!(
            store
                .forget_matching_for("jazz", Forgetter::Identified("spk-k"), false)
                .unwrap(),
            1
        );
        assert_eq!(store.count().unwrap(), 2);
        // An unknown voice can't forget anyone's.
        assert_eq!(
            store
                .forget_matching_for("jazz", Forgetter::Unknown, false)
                .unwrap(),
            0
        );
        assert_eq!(store.forget_last_for(Forgetter::Unknown).unwrap(), None);
        // Brendon's "forget that" removes the latest thing *he* said (the household fact).
        assert_eq!(
            store
                .forget_last_for(Forgetter::Identified("spk-b"))
                .unwrap()
                .as_deref(),
            Some("our jazz club meets Friday")
        );
        // An explicit "our …" forget may reach household rows.
        store
            .add_in(
                MemoryKind::Fact,
                "our jazz club meets Friday",
                MemorySource::Explicit,
                &MemoryScope::Household,
                &prov(Some("spk-x"), "identified"),
            )
            .unwrap();
        assert_eq!(
            store
                .forget_matching_for("jazz club", Forgetter::Unknown, true)
                .unwrap(),
            1
        );
    }

    #[test]
    fn forgotten_speaker_memories_become_unknown_not_household() {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .add_in(
                MemoryKind::Fact,
                "The user's name is Brendon",
                MemorySource::Explicit,
                &MemoryScope::Person("spk-b".into()),
                &prov(Some("spk-b"), "identified"),
            )
            .unwrap();
        assert_eq!(store.unassign_speaker("spk-b").unwrap(), 1);
        let m = &store.list().unwrap()[0];
        assert_eq!(m.scope, ScopeKind::Unknown);
        assert_eq!(m.said_by_speaker_id, None);
        assert!(store.search_scoped("name", None, 10).unwrap().is_empty());
    }

    #[test]
    fn turn_correction_rescopes_its_personal_memories() {
        let store = MemoryStore::open_in_memory().unwrap();
        let unknown = MemoryScope::Unknown { candidate: None };
        store
            .add_in(
                MemoryKind::Fact,
                "The user's name is Brendan",
                MemorySource::Explicit,
                &unknown,
                &prov(None, "unknown"),
            )
            .unwrap();
        store
            .add_in(
                MemoryKind::Fact,
                "the wifi is FastNet",
                MemorySource::Explicit,
                &MemoryScope::Household,
                &prov(None, "unknown"),
            )
            .unwrap();
        let n = store
            .reattribute_turn("t-1", &MemoryScope::Person("spk-b".into()))
            .unwrap();
        assert_eq!(n, 1, "only the speaker-relative fact moves");
        let all = store.list().unwrap();
        let name = all.iter().find(|m| m.content.contains("Brendan")).unwrap();
        assert_eq!(name.scope, ScopeKind::Person);
        assert_eq!(name.speaker_id.as_deref(), Some("spk-b"));
        assert_eq!(name.said_by_speaker_id.as_deref(), Some("spk-b"));
        let wifi = all.iter().find(|m| m.content.contains("wifi")).unwrap();
        assert_eq!(wifi.scope, ScopeKind::Household);
        store.set_turn_override("t-1", "spk-b").unwrap();
        store.set_turn_override("t-1", "spk-k").unwrap();
        assert_eq!(store.turn_overrides().unwrap()["t-1"], "spk-k");
    }

    #[test]
    fn forget_last_removes_most_recent() {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .add(MemoryKind::Fact, "first", MemorySource::Explicit)
            .unwrap();
        store
            .add(MemoryKind::Fact, "second", MemorySource::Explicit)
            .unwrap();
        assert_eq!(store.forget_last().unwrap().as_deref(), Some("second"));
        assert_eq!(store.count().unwrap(), 1);
    }
}
