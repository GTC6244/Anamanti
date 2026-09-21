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
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

mod extract;

pub mod backend;
pub mod chatlog;
pub mod embed;
pub mod entity;
pub mod graphview;
pub mod promptlog;

// GraphRAG memory (embedded HelixDB) — always compiled. See memory_plan.md.
pub mod helix;
pub mod ingester;

pub use backend::HelixRecall;
pub use backend::{Recall, SqliteRecall};
pub use chatlog::{ChatLog, ChatLogRecord};
pub use extract::{infer_memories, parse_command, MemoryCommand};
pub use graphview::GraphView;
pub use promptlog::{PromptLog, PromptLogRecord};

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Memory {
    pub id: i64,
    pub kind: MemoryKind,
    pub content: String,
    pub source: MemorySource,
    pub created_at: i64,
    /// The person this memory belongs to (`spk-…`), or `None` for a
    /// shared/household entry visible to everyone (speaker_id_plan.md §3.2).
    pub speaker_id: Option<String>,
}

/// The persistent memory store.
pub struct MemoryStore {
    conn: Mutex<Connection>,
}

impl MemoryStore {
    /// Open (creating if needed) the SQLite database at `path` and run migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path).context("opening memory database")?;
        Self::from_conn(conn)
    }

    /// An ephemeral in-memory store (tests).
    pub fn open_in_memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Self> {
        // A second connection (the speaker registry) may touch the same file;
        // wait on a transient write lock rather than failing.
        conn.busy_timeout(std::time::Duration::from_secs(5)).ok();
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
            "#,
        )
        .context("initializing memory schema")?;
        migrate_speaker_column(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Add a shared/household entry (no speaker attribution). Convenience wrapper
    /// over [`add_scoped`](Self::add_scoped) preserving the pre-speaker-ID signature.
    pub fn add(&self, kind: MemoryKind, content: &str, source: MemorySource) -> Result<i64> {
        self.add_scoped(kind, content, source, None)
    }

    /// Add an entry owned by `speaker_id` (`None` = shared/household), returning its
    /// row id. Duplicate content of the same kind **and speaker** is coalesced
    /// (returns the existing id) so repeated turns don't pile up copies; the same
    /// content said by two people is kept separately.
    pub fn add_scoped(
        &self,
        kind: MemoryKind,
        content: &str,
        source: MemorySource,
        speaker_id: Option<&str>,
    ) -> Result<i64> {
        let content = content.trim();
        let conn = self.conn.lock().unwrap();
        // `IS` (not `=`) so a NULL speaker matches NULL — SQLite's null-safe compare.
        if let Ok(existing) = conn.query_row(
            "SELECT id FROM memories WHERE kind = ?1 AND content = ?2 AND speaker_id IS ?3",
            params![kind.as_str(), content, speaker_id],
            |row| row.get::<_, i64>(0),
        ) {
            return Ok(existing);
        }
        conn.execute(
            "INSERT INTO memories (kind, content, source, created_at, speaker_id) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![kind.as_str(), content, source.as_str(), now(), speaker_id],
        )
        .context("inserting memory")?;
        Ok(conn.last_insert_rowid())
    }

    /// All entries, most recent first (the settings list view). Every speaker's
    /// entries plus shared ones — the settings screen shows the whole store.
    pub fn list(&self) -> Result<Vec<Memory>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, kind, content, source, created_at, speaker_id FROM memories \
             ORDER BY created_at DESC, id DESC",
        )?;
        let rows = stmt.query_map([], row_to_memory)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Full-text search across **all** speakers (unscoped) — used by the settings
    /// list and by "forget …". For per-turn recall use [`search_scoped`](Self::search_scoped).
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Memory>> {
        let match_expr = fts_match_expr(query);
        let conn = self.conn.lock().unwrap();
        if let Some(expr) = match_expr {
            let mut stmt = conn.prepare(
                "SELECT m.id, m.kind, m.content, m.source, m.created_at, m.speaker_id \
                 FROM memories_fts f JOIN memories m ON m.id = f.rowid \
                 WHERE memories_fts MATCH ?1 ORDER BY rank LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![expr, limit as i64], row_to_memory)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, kind, content, source, created_at, speaker_id FROM memories \
                 ORDER BY created_at DESC, id DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![limit as i64], row_to_memory)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        }
    }

    /// Full-text search scoped to a speaker for per-turn recall (speaker_id_plan.md
    /// §4.2): returns `speaker_id`'s own entries **plus** shared (`NULL`) entries,
    /// most relevant first. `speaker_id = None` (household/unattributed) returns
    /// only shared entries. A degenerate query falls back to recent entries in the
    /// same scope so the LLM always gets some standing context.
    pub fn search_scoped(
        &self,
        query: &str,
        speaker_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Memory>> {
        let match_expr = fts_match_expr(query);
        let conn = self.conn.lock().unwrap();
        let rows = match (match_expr, speaker_id) {
            (Some(expr), Some(id)) => {
                let mut stmt = conn.prepare(
                    "SELECT m.id, m.kind, m.content, m.source, m.created_at, m.speaker_id \
                     FROM memories_fts f JOIN memories m ON m.id = f.rowid \
                     WHERE memories_fts MATCH ?1 AND (m.speaker_id IS NULL OR m.speaker_id = ?2) \
                     ORDER BY rank LIMIT ?3",
                )?;
                let mapped = stmt.query_map(params![expr, id, limit as i64], row_to_memory)?;
                mapped.collect::<rusqlite::Result<Vec<_>>>()?
            }
            (Some(expr), None) => {
                let mut stmt = conn.prepare(
                    "SELECT m.id, m.kind, m.content, m.source, m.created_at, m.speaker_id \
                     FROM memories_fts f JOIN memories m ON m.id = f.rowid \
                     WHERE memories_fts MATCH ?1 AND m.speaker_id IS NULL \
                     ORDER BY rank LIMIT ?2",
                )?;
                let mapped = stmt.query_map(params![expr, limit as i64], row_to_memory)?;
                mapped.collect::<rusqlite::Result<Vec<_>>>()?
            }
            (None, Some(id)) => {
                let mut stmt = conn.prepare(
                    "SELECT id, kind, content, source, created_at, speaker_id FROM memories \
                     WHERE speaker_id IS NULL OR speaker_id = ?1 \
                     ORDER BY created_at DESC, id DESC LIMIT ?2",
                )?;
                let mapped = stmt.query_map(params![id, limit as i64], row_to_memory)?;
                mapped.collect::<rusqlite::Result<Vec<_>>>()?
            }
            (None, None) => {
                let mut stmt = conn.prepare(
                    "SELECT id, kind, content, source, created_at, speaker_id FROM memories \
                     WHERE speaker_id IS NULL ORDER BY created_at DESC, id DESC LIMIT ?1",
                )?;
                let mapped = stmt.query_map(params![limit as i64], row_to_memory)?;
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

    /// Delete every entry matching `query` (voice "forget …"); returns the count.
    pub fn forget_matching(&self, query: &str) -> Result<usize> {
        let ids: Vec<i64> = self.search(query, 100)?.into_iter().map(|m| m.id).collect();
        let conn = self.conn.lock().unwrap();
        let mut removed = 0;
        for id in ids {
            removed += conn.execute("DELETE FROM memories WHERE id = ?1", params![id])?;
        }
        Ok(removed)
    }

    /// Delete the single most recently added entry (voice "forget that"); returns
    /// the deleted entry's content, if any.
    pub fn forget_last(&self) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let last: Option<(i64, String)> = conn
            .query_row(
                "SELECT id, content FROM memories ORDER BY created_at DESC, id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();
        if let Some((id, content)) = last {
            conn.execute("DELETE FROM memories WHERE id = ?1", params![id])?;
            Ok(Some(content))
        } else {
            Ok(None)
        }
    }

    /// Reassign every memory owned by `from` to `to` (used when two speaker
    /// clusters are merged, speaker_id_plan.md §4.5). Returns the number moved.
    pub fn reassign_speaker(&self, from: &str, to: &str) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "UPDATE memories SET speaker_id = ?1 WHERE speaker_id = ?2",
            params![to, from],
        )?)
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

fn row_to_memory(row: &rusqlite::Row) -> rusqlite::Result<Memory> {
    Ok(Memory {
        id: row.get(0)?,
        kind: MemoryKind::from_str(&row.get::<_, String>(1)?),
        content: row.get(2)?,
        source: MemorySource::from_str(&row.get::<_, String>(3)?),
        created_at: row.get(4)?,
        speaker_id: row.get(5)?,
    })
}

/// Add the `speaker_id` column to a pre-speaker-ID `memories` table. Fresh DBs get
/// it from `CREATE TABLE`; this backfills older ones (existing rows read as `NULL`
/// = shared/household, so nothing is lost). Idempotent: a no-op when present.
fn migrate_speaker_column(conn: &Connection) -> Result<()> {
    let has_column: bool = conn
        .prepare("PRAGMA table_info(memories)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .any(|name| name == "speaker_id");
    if !has_column {
        conn.execute("ALTER TABLE memories ADD COLUMN speaker_id TEXT", [])
            .context("adding memories.speaker_id column")?;
    }
    Ok(())
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
