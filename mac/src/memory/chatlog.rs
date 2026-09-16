//! Append-only chat log (Plan.MD "memory_plan.md" Goal 2).
//!
//! Every completed turn is appended as one JSON object per line (**JSONL**) to a
//! file on the Mac. This is the durable, human-auditable source of truth **and**
//! the ingestion queue for the GraphRAG memory: the background [`crate::memory::
//! ingester::MemoryIngester`] tails this file, embeds new records, and upserts them
//! into HelixDB. Decoupling capture from embedding means we can embed in batches
//! and re-embed later if the model changes.
//!
//! Writes never block a turn: a logging failure is logged and swallowed by the
//! caller (`orchestrator.rs`), never turned into a turn error.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// One completed conversation turn, as persisted to the JSONL log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatLogRecord {
    /// Monotonic-ish unique id (`{unix_millis}-{counter}`), stable across restarts
    /// only in ordering, not value. Used as the HelixDB `Turn.ext_id` for
    /// idempotent upserts.
    pub id: String,
    /// Unix seconds when the turn completed.
    pub ts: i64,
    /// Session this turn belongs to (one per device connection).
    pub session_id: String,
    /// The user's final transcript.
    pub transcript: String,
    /// The assistant's full reply text.
    pub reply: String,
    /// Contents of any memory entries stored during this turn (explicit or inferred).
    #[serde(default)]
    pub memories_written: Vec<String>,
    /// Which LLM backend produced the reply (e.g. `"ollama"`, `"anthropic"`).
    pub llm_backend: String,
    /// The model name, if the backend has one.
    #[serde(default)]
    pub model: Option<String>,
}

/// Append-only JSONL writer, safe to share across concurrent turns behind an `Arc`.
pub struct ChatLog {
    path: PathBuf,
    file: Mutex<File>,
    counter: AtomicU64,
}

impl ChatLog {
    /// Open (creating if needed) the log at `path` for appending.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening chat log at {}", path.display()))?;
        Ok(Self {
            path,
            file: Mutex::new(file),
            counter: AtomicU64::new(0),
        })
    }

    /// The log's path (the ingester reads from it).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Mint a fresh record id (`{unix_millis}-{counter}`).
    pub fn next_id(&self) -> String {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("{millis}-{n}")
    }

    /// Append one record as a single JSON line, flushing so the ingester (a separate
    /// reader) sees it promptly.
    pub fn append(&self, record: &ChatLogRecord) -> Result<()> {
        let mut line = serde_json::to_string(record).context("serializing chat log record")?;
        line.push('\n');
        let mut file = self.file.lock().unwrap();
        file.write_all(line.as_bytes())
            .context("appending to chat log")?;
        file.flush().context("flushing chat log")?;
        Ok(())
    }
}

/// Current unix-seconds timestamp helper for building records.
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Read every record from `path` whose 0-based line index is `>= from_line`.
/// Returns `(next_line, records)` so a caller can persist the new high-water mark.
/// Malformed lines are skipped (logged) rather than aborting ingestion.
pub fn read_from(path: impl AsRef<Path>, from_line: u64) -> Result<(u64, Vec<ChatLogRecord>)> {
    let path = path.as_ref();
    let file = match File::open(path) {
        Ok(f) => f,
        // No log yet → nothing to ingest.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((from_line, Vec::new())),
        Err(e) => return Err(e).with_context(|| format!("opening chat log {}", path.display())),
    };
    let reader = BufReader::new(file);
    let mut idx: u64 = 0;
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line.context("reading chat log line")?;
        if idx >= from_line && !line.trim().is_empty() {
            match serde_json::from_str::<ChatLogRecord>(&line) {
                Ok(rec) => out.push(rec),
                Err(e) => log::warn!("skipping malformed chat log line {idx}: {e}"),
            }
        }
        idx += 1;
    }
    Ok((idx, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: &str, transcript: &str) -> ChatLogRecord {
        ChatLogRecord {
            id: id.to_string(),
            ts: 1,
            session_id: "s1".to_string(),
            transcript: transcript.to_string(),
            reply: "ok".to_string(),
            memories_written: vec![],
            llm_backend: "mock".to_string(),
            model: None,
        }
    }

    #[test]
    fn append_then_read_from_offset() {
        let dir = std::env::temp_dir().join(format!("chatlog_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("log.jsonl");
        let _ = std::fs::remove_file(&path);

        let log = ChatLog::open(&path).unwrap();
        log.append(&rec("a", "hello")).unwrap();
        log.append(&rec("b", "world")).unwrap();

        let (next, recs) = read_from(&path, 0).unwrap();
        assert_eq!(next, 2);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].transcript, "hello");

        // Reading from the high-water mark yields nothing new.
        let (next2, recs2) = read_from(&path, next).unwrap();
        assert_eq!(next2, 2);
        assert!(recs2.is_empty());

        // A new append is visible past the old mark.
        log.append(&rec("c", "again")).unwrap();
        let (next3, recs3) = read_from(&path, next).unwrap();
        assert_eq!(next3, 3);
        assert_eq!(recs3.len(), 1);
        assert_eq!(recs3[0].transcript, "again");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ids_are_unique() {
        let log = ChatLog::open(std::env::temp_dir().join("chatlog_ids.jsonl")).unwrap();
        let a = log.next_id();
        let b = log.next_id();
        assert_ne!(a, b);
    }
}
