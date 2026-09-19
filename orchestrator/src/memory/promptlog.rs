//! Append-only **prompt log** — a debug/audit record of the exact prompt sent to
//! the LLM for each turn.
//!
//! The assembled system prompt (base persona + datetime + speaker identity +
//! recalled memory context) and the user message are built transiently per turn in
//! `orchestrator.rs` and would otherwise vanish once the reply streams. This log
//! captures them so the orchestrator's debug GUI (`webconfig.rs` → `/prompts`) can
//! show precisely what the model saw — separate from the [`super::chatlog`], which
//! records only the transcript/reply and feeds the GraphRAG ingester.
//!
//! One JSON object per line (**JSONL**), same shape and never-block-a-turn
//! semantics as the chat log: an append failure is logged and swallowed by the
//! caller, never turned into a turn error.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The prompt sent to the LLM for one turn, as persisted to the JSONL log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptLogRecord {
    /// Monotonic-ish unique id (`{unix_millis}-{counter}`).
    pub id: String,
    /// Unix seconds when the prompt was assembled.
    pub ts: i64,
    /// Which LLM backend received the prompt (e.g. `"ollama"`, `"anthropic"`).
    pub llm_backend: String,
    /// The model name, if the backend has one.
    #[serde(default)]
    pub model: Option<String>,
    /// The identified speaker id for this turn (`spk-…`), or `"household"`.
    #[serde(default)]
    pub speaker_id: String,
    /// The speaker's name, if their cluster has been named.
    #[serde(default)]
    pub speaker_name: Option<String>,
    /// The full assembled system prompt (persona + datetime + identity + memory).
    pub system_prompt: String,
    /// The user message (the turn's final transcript) sent alongside it.
    pub user_message: String,
}

/// Append-only JSONL writer, safe to share across concurrent turns behind an `Arc`.
pub struct PromptLog {
    path: PathBuf,
    file: Mutex<File>,
    counter: AtomicU64,
}

impl PromptLog {
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
            .with_context(|| format!("opening prompt log at {}", path.display()))?;
        Ok(Self {
            path,
            file: Mutex::new(file),
            counter: AtomicU64::new(0),
        })
    }

    /// The log's path.
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

    /// Append one record as a single JSON line, flushing so a viewer sees it promptly.
    pub fn append(&self, record: &PromptLogRecord) -> Result<()> {
        let mut line = serde_json::to_string(record).context("serializing prompt log record")?;
        line.push('\n');
        let mut file = self.file.lock().unwrap();
        file.write_all(line.as_bytes())
            .context("appending to prompt log")?;
        file.flush().context("flushing prompt log")?;
        Ok(())
    }
}

/// Read up to `limit` most-recent records from `path`, newest first. Malformed
/// lines are skipped. A missing file yields an empty vec (nothing logged yet).
pub fn read_tail(path: impl AsRef<Path>, limit: usize) -> Result<Vec<PromptLogRecord>> {
    let path = path.as_ref();
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("opening prompt log {}", path.display())),
    };
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line.context("reading prompt log line")?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<PromptLogRecord>(&line) {
            Ok(rec) => out.push(rec),
            Err(e) => log::warn!("skipping malformed prompt log line: {e}"),
        }
    }
    out.reverse(); // newest first
    out.truncate(limit);
    Ok(out)
}

/// Current unix-seconds timestamp helper for building records.
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: &str, prompt: &str) -> PromptLogRecord {
        PromptLogRecord {
            id: id.to_string(),
            ts: 1,
            llm_backend: "mock".to_string(),
            model: None,
            speaker_id: "household".to_string(),
            speaker_name: None,
            system_prompt: prompt.to_string(),
            user_message: "hi".to_string(),
        }
    }

    #[test]
    fn append_then_tail_is_newest_first_and_capped() {
        let dir = std::env::temp_dir().join(format!("promptlog_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prompts.jsonl");
        let _ = std::fs::remove_file(&path);

        let log = PromptLog::open(&path).unwrap();
        log.append(&rec("a", "first")).unwrap();
        log.append(&rec("b", "second")).unwrap();
        log.append(&rec("c", "third")).unwrap();

        let tail = read_tail(&path, 2).unwrap();
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].system_prompt, "third");
        assert_eq!(tail[1].system_prompt, "second");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tail_of_missing_file_is_empty() {
        let path = std::env::temp_dir().join("promptlog_missing_xyz.jsonl");
        let _ = std::fs::remove_file(&path);
        assert!(read_tail(&path, 10).unwrap().is_empty());
    }
}
