//! One way to open the Core's SQLite database.
//!
//! The memory store ([`crate::memory::MemoryStore`]) and the speaker registry
//! ([`crate::speaker::SpeakerRegistry`]) each hold their own connection to the
//! same `db_path` file. Both must agree on the connection policy, so both open it
//! here:
//!
//! - **`busy_timeout`** — the other connection (or another process sharing the
//!   file) may hold a transient write lock; wait for it rather than failing with
//!   `SQLITE_BUSY`.
//! - **`journal_mode=WAL`** — lets several processes share the file (e.g. a
//!   production Core plus a short-lived test Core) with concurrent readers and a
//!   single writer. Switching an existing rollback-journal file to WAL is a
//!   persistent, in-place change SQLite performs itself; a database already in WAL
//!   mode is unaffected. In-memory databases stay in `memory` journal mode.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::Connection;

/// How long a connection waits on another writer's lock before giving up.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Open (creating if needed) the SQLite database at `path` with the shared
/// connection policy applied (see the module docs).
pub fn open(path: impl AsRef<Path>) -> Result<Connection> {
    let path = path.as_ref();
    let conn = Connection::open(path)
        .with_context(|| format!("opening SQLite database {}", path.display()))?;
    configure(&conn);
    Ok(conn)
}

/// An ephemeral in-memory database with the same policy (tests).
pub fn open_in_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory().context("opening in-memory SQLite database")?;
    configure(&conn);
    Ok(conn)
}

/// Apply the shared policy. Best-effort: a failure to set either pragma leaves the
/// connection usable with SQLite's defaults, exactly as before this helper existed.
fn configure(conn: &Connection) {
    if let Err(e) = conn.busy_timeout(BUSY_TIMEOUT) {
        log::warn!("sqlite: setting busy_timeout failed: {e}");
    }
    if let Err(e) = conn.execute_batch("PRAGMA journal_mode=WAL;") {
        log::warn!("sqlite: enabling WAL failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_database_opens_in_wal_mode() {
        let dir = std::env::temp_dir().join(format!(
            "anamanti-db-{}-{}",
            std::process::id(),
            crate::clock::now_millis()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = open(dir.join("x.sqlite")).unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        drop(conn);
        std::fs::remove_dir_all(&dir).ok();
    }
}
