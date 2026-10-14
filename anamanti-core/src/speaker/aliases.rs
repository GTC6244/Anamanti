//! Persisted speaker-id aliases: where a merged-away or forgotten speaker id now
//! points.
//!
//! [`super::SpeakerService::merge`] / [`super::SpeakerService::delete`] rewrite the
//! SQLite memories and the **active** GraphRAG store, but two things can still carry
//! the old id: the chat log (`anamanti_chatlog.jsonl`, the append-only source of
//! truth, which keeps each turn's original `speaker_id`) and the stores of the other,
//! inactive embedding backends (`helix_path/<label>-<dims>/`). So every merge/forget
//! also records an alias in the `speaker_aliases` table of the same SQLite database:
//!
//! - `merge(keep, drop)` records `drop → keep` and re-points any alias that pointed at
//!   `drop` (so chains stay one hop);
//! - `delete(id)` records `id → NULL` (= the unknown speaker: a forgotten person's
//!   history is kept but attributed to no one) and re-points aliases
//!   that pointed at `id` likewise.
//!
//! The alias is written in the same transaction as the registry change. Consumers:
//! the GraphRAG ingester resolves every record's speaker id through the map before it
//! writes to any store, and the GraphRAG controller replays the map onto a store each
//! time it becomes active (boot or a backend switch), so an inactive store catches up
//! before it serves recall. The table is created idempotently; an install without it
//! simply has no aliases.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use super::UNKNOWN_SPEAKER;

/// Create the alias table if it does not exist (idempotent).
pub(crate) fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS speaker_aliases (
            from_id    TEXT PRIMARY KEY,
            to_id      TEXT,
            created_at INTEGER NOT NULL
        );
        "#,
    )
    .context("initializing speaker alias schema")
}

/// Record `from → to` (`None` = the unknown speaker) and re-point every alias that pointed
/// at `from`. Call inside the registry transaction that merges/deletes `from`.
pub(crate) fn record(conn: &Connection, from: &str, to: Option<&str>) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO speaker_aliases (from_id, to_id, created_at) VALUES (?1, ?2, ?3)",
        params![from, to, crate::clock::now_secs()],
    )
    .context("recording speaker alias")?;
    conn.execute(
        "UPDATE speaker_aliases SET to_id = ?2 WHERE to_id = ?1",
        params![from, to],
    )
    .context("re-pointing speaker aliases")?;
    Ok(())
}

/// Every alias in the table.
pub(crate) fn load(conn: &Connection) -> Result<AliasMap> {
    let mut stmt = conn.prepare("SELECT from_id, to_id FROM speaker_aliases")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    Ok(AliasMap(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?))
}

/// A snapshot of the alias table: `from_id → Some(to_id)` for a merge,
/// `from_id → None` for a forgotten speaker (now the unknown speaker).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AliasMap(HashMap<String, Option<String>>);

impl AliasMap {
    /// The surviving id for `id`: itself when it has no alias, the merge target, or
    /// [`UNKNOWN_SPEAKER`] for a forgotten speaker. Follows (bounded) chains, though
    /// [`record`] keeps them one hop.
    pub fn resolve<'a>(&'a self, id: &'a str) -> &'a str {
        let mut cur = id;
        for _ in 0..8 {
            match self.0.get(cur) {
                Some(Some(next)) if next != cur => cur = next,
                Some(None) => return UNKNOWN_SPEAKER,
                _ => break,
            }
        }
        cur
    }

    /// Every alias as `(from, Some(to))` / `(from, None)` (unknown), in no
    /// particular order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, Option<&str>)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_deref()))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Read access to the alias table for consumers outside the speaker service (the
/// GraphRAG ingester and controller), on its own connection to the shared database —
/// available even when speaker identification is currently disabled, so aliases
/// recorded earlier are still honored.
pub struct SpeakerAliases {
    conn: Mutex<Connection>,
}

impl SpeakerAliases {
    /// Open the alias table in the SQLite database at `path` (creating it if needed).
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = crate::db::open(path).context("opening speaker alias database")?;
        ensure_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// The current alias map.
    pub fn load(&self) -> Result<AliasMap> {
        load(&self.conn.lock().unwrap())
    }

    /// The admin console's per-turn speaker corrections (`turn_id → speaker_id`, the
    /// memory store's `turn_attribution_overrides` table in the same database). Empty
    /// when the table does not exist yet.
    pub fn load_turn_overrides(&self) -> Result<HashMap<String, String>> {
        let conn = self.conn.lock().unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
                 AND name = 'turn_attribution_overrides'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)?;
        if !exists {
            return Ok(HashMap::new());
        }
        let mut stmt =
            conn.prepare("SELECT turn_id, speaker_id FROM turn_attribution_overrides")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, Option<&str>)]) -> AliasMap {
        AliasMap(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.map(str::to_string)))
                .collect(),
        )
    }

    #[test]
    fn resolve_follows_merges_and_forgets() {
        let m = map(&[("a", Some("b")), ("c", None), ("d", Some("a"))]);
        assert_eq!(m.resolve("a"), "b");
        assert_eq!(m.resolve("c"), UNKNOWN_SPEAKER);
        assert_eq!(m.resolve("d"), "b", "a chain is followed");
        assert_eq!(m.resolve("z"), "z", "unknown ids are themselves");
        assert_eq!(m.resolve(UNKNOWN_SPEAKER), UNKNOWN_SPEAKER);
        // A malformed cycle can't spin.
        let cyc = map(&[("x", Some("y")), ("y", Some("x"))]);
        let _ = cyc.resolve("x");
    }

    #[test]
    fn record_repoints_existing_aliases() {
        let conn = crate::db::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        ensure_schema(&conn).unwrap(); // idempotent
        record(&conn, "a", Some("b")).unwrap();
        // b is later merged into c: a must now point straight at c.
        record(&conn, "b", Some("c")).unwrap();
        let m = load(&conn).unwrap();
        assert_eq!(m, map(&[("a", Some("c")), ("b", Some("c"))]));
        // c is forgotten: everything that was c is now the unknown speaker.
        record(&conn, "c", None).unwrap();
        let m = load(&conn).unwrap();
        assert_eq!(m, map(&[("a", None), ("b", None), ("c", None)]));
        assert_eq!(m.resolve("a"), UNKNOWN_SPEAKER);
    }
}
