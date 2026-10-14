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

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::jsonl::{self, JsonlLog};

/// The prompt sent to the LLM for one turn, as persisted to the JSONL log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptLogRecord {
    /// Monotonic-ish unique id (`{unix_millis}-{counter}`).
    pub id: String,
    /// The turn this prompt was assembled for — the same id as that turn's
    /// [`super::chatlog::ChatLogRecord::turn_id`]. Absent on older records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// Unix seconds when the prompt was assembled.
    pub ts: i64,
    /// Which LLM backend received the prompt (e.g. `"ollama"`, `"anthropic"`).
    pub llm_backend: String,
    /// The model name, if the backend has one.
    #[serde(default)]
    pub model: Option<String>,
    /// The resolved speaker id for this turn (`spk-…`), or `"unknown"` (older records:
    /// `"household"`, also meaning unattributed).
    #[serde(default)]
    pub speaker_id: String,
    /// The speaker's name, if their cluster has been named.
    #[serde(default)]
    pub speaker_name: Option<String>,
    /// How confidently the speaker was resolved (`identified | tentative | new |
    /// unknown`) — which identity line + memory blocks the prompt carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker_resolution: Option<String>,
    /// The full assembled system prompt (persona + datetime + identity + memory).
    pub system_prompt: String,
    /// The user message (the turn's final transcript) sent alongside it.
    pub user_message: String,
}

/// Append-only JSONL prompt log, safe to share across concurrent turns behind an
/// `Arc` (see [`JsonlLog`] for the write/read guarantees).
pub type PromptLog = JsonlLog<PromptLogRecord>;

/// Read up to `limit` most-recent records from `path`, newest first, reading
/// backwards from EOF. Malformed lines are skipped. A missing file yields an empty
/// vec (nothing logged yet).
pub fn read_tail(path: impl AsRef<Path>, limit: usize) -> Result<Vec<PromptLogRecord>> {
    jsonl::read_tail(path, limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: &str, prompt: &str) -> PromptLogRecord {
        PromptLogRecord {
            id: id.to_string(),
            turn_id: None,
            ts: 1,
            llm_backend: "mock".to_string(),
            model: None,
            speaker_id: "unknown".to_string(),
            speaker_name: None,
            speaker_resolution: None,
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
