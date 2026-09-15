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

pub use extract::{infer_memories, parse_command, MemoryCommand};

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
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS memories (
                id         INTEGER PRIMARY KEY,
                kind       TEXT NOT NULL,
                content    TEXT NOT NULL,
                source     TEXT NOT NULL,
                created_at INTEGER NOT NULL
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
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Add an entry, returning its row id. Duplicate content of the same kind is
    /// coalesced (returns the existing id) so repeated turns don't pile up copies.
    pub fn add(&self, kind: MemoryKind, content: &str, source: MemorySource) -> Result<i64> {
        let content = content.trim();
        let conn = self.conn.lock().unwrap();
        if let Ok(existing) = conn.query_row(
            "SELECT id FROM memories WHERE kind = ?1 AND content = ?2",
            params![kind.as_str(), content],
            |row| row.get::<_, i64>(0),
        ) {
            return Ok(existing);
        }
        conn.execute(
            "INSERT INTO memories (kind, content, source, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![kind.as_str(), content, source.as_str(), now()],
        )
        .context("inserting memory")?;
        Ok(conn.last_insert_rowid())
    }

    /// All entries, most recent first (the settings list view).
    pub fn list(&self) -> Result<Vec<Memory>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, kind, content, source, created_at FROM memories ORDER BY created_at DESC, id DESC",
        )?;
        let rows = stmt.query_map([], row_to_memory)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Full-text search for entries relevant to `query`, most relevant first. An
    /// empty/degenerate query returns the most recent entries so the LLM always
    /// gets some standing context.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Memory>> {
        let match_expr = fts_match_expr(query);
        let conn = self.conn.lock().unwrap();
        if let Some(expr) = match_expr {
            let mut stmt = conn.prepare(
                "SELECT m.id, m.kind, m.content, m.source, m.created_at \
                 FROM memories_fts f JOIN memories m ON m.id = f.rowid \
                 WHERE memories_fts MATCH ?1 ORDER BY rank LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![expr, limit as i64], row_to_memory)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, kind, content, source, created_at FROM memories \
                 ORDER BY created_at DESC, id DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![limit as i64], row_to_memory)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        }
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
    })
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
