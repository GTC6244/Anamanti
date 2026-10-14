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

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::jsonl::{self, JsonlLog};
pub use super::jsonl::{cursor_at_line, LogCursor};

/// One completed conversation turn, as persisted to the JSONL log.
// `PartialEq` (not `Eq`) because the embedded `TurnTiming` carries an `f64` confidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatLogRecord {
    /// Monotonic-ish unique id (`{unix_millis}-{counter}`), stable across restarts
    /// only in ordering, not value. Used as the HelixDB `Turn.ext_id` for
    /// idempotent upserts.
    pub id: String,
    /// The turn this record belongs to (`t-{unix_millis}-{n}`, minted when the turn
    /// starts). Shared with the turn's [`super::promptlog::PromptLogRecord`] so the
    /// config page's Conversations view can show the exact prompt for a turn. Absent
    /// on records logged before the link shipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
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
    /// The identified speaker id for this turn (`spk-…`), or `"household"` when
    /// unattributed. Defaulted so pre-speaker-ID logs still parse (speaker_id_plan.md
    /// §3.4).
    #[serde(default)]
    pub speaker_id: String,
    /// The speaker's name, if their cluster has been named.
    #[serde(default)]
    pub speaker_name: Option<String>,
    /// Per-stage latency breakdown for this turn (STT, System-1 decision, fast-path
    /// fetch, System-2 recall/prompt/LLM, TTS, total). Absent for turns logged before
    /// timing capture shipped (the `/chatlog` UI shows "no timing recorded" for those).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<TurnTiming>,
}

/// Per-stage server-side latency for one turn, in milliseconds. Every field is
/// optional because the stages that run depend on the path taken: a System-1 fast-path
/// turn has `system1_ms` + `fetch_ms` + `tts_ms` but no `recall_ms`/`prompt_ms`/`llm_ms`,
/// a System-2 turn has the reverse, and a memory command has neither. Serialized with
/// `skip_serializing_if` so a record only carries the stages that actually happened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TurnTiming {
    /// Which path answered the turn: `"command"`, `"system1"`, or `"system2"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// End-of-speech (VAD finalize) → STT transcript returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stt_ms: Option<u64>,
    /// System-1 decision-engine latency (jev/laya round-trip).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system1_ms: Option<u64>,
    /// System-1 outcome: `"resolve"`, `"defer"`, `"error"`, or `"no-handler"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system1_outcome: Option<String>,
    /// The resolved System-1 intent (e.g. `"weather"`), when it resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system1_intent: Option<String>,
    /// The System-1 confidence (0.0–1.0), when it resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system1_confidence: Option<f64>,
    /// A fast-path tool fetch (e.g. the weather forecast HTTP call).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_ms: Option<u64>,
    /// System-2 memory recall (GraphRAG/FTS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall_ms: Option<u64>,
    /// Which recall backend actually answered this turn: `"helix"` (embedded HelixDB
    /// GraphRAG) or `"sqlite-fts"` (the always-available keyword fallback). Recorded
    /// per turn so the `/chatlog` page shows whether a `memory_backend="helix"`
    /// instance really ran GraphRAG or silently fell back to FTS at boot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall_backend: Option<String>,
    /// Whether this recall embedded the query: `true` for the GraphRAG backend,
    /// `false` for SQLite FTS. This is the variable component of `recall_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall_embedded: Option<bool>,
    /// Which embedder produced the query vector this turn: `"local"` (on-device nomic),
    /// `"openai"` (cloud), or `"mock"`. Recorded per turn so the `/chatlog` page labels
    /// the embedding stage with the backend that actually ran. Absent on the FTS backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall_embedder: Option<String>,
    /// GraphRAG only: the query-embedding portion of `recall_ms`, broken out so the
    /// `/chatlog` page shows the embedding and the local graph search as two separate
    /// rows. Absent on the FTS backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall_embed_ms: Option<u64>,
    /// GraphRAG only: the local HelixDB vector KNN + graph-expansion portion of
    /// `recall_ms` (the non-network part). Absent on the FTS backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall_search_ms: Option<u64>,
    /// System-2 prompt assembly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_ms: Option<u64>,
    /// System-2 LLM completion round-trip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_ms: Option<u64>,
    /// TTS synthesis of the reply's first chunk (when playback begins).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tts_ms: Option<u64>,
    /// End-of-speech → reply ready: the whole server-side turn latency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_ms: Option<u64>,
}

/// Append-only JSONL chat log, safe to share across concurrent turns behind an
/// `Arc` (see [`JsonlLog`] for the write/read guarantees).
pub type ChatLog = JsonlLog<ChatLogRecord>;

/// Current unix-seconds timestamp helper for building records (re-exported from
/// [`crate::clock`] for existing callers).
pub use crate::clock::now_secs;

/// Read up to `limit` most-recent records from `path`, newest first, reading
/// backwards from EOF (cost bounded by `limit`, not by the log's length). Malformed
/// lines are skipped. A missing file yields an empty vec. Used by the debug GUI and
/// the follow-up history; the ingester tails the log with [`read_after`] instead.
pub fn read_tail(path: impl AsRef<Path>, limit: usize) -> Result<Vec<ChatLogRecord>> {
    jsonl::read_tail(path, limit)
}

/// Read every complete record after `from` (see [`jsonl::read_after`]).
pub fn read_after(
    path: impl AsRef<Path>,
    from: LogCursor,
) -> Result<(LogCursor, Vec<ChatLogRecord>)> {
    jsonl::read_after(path, from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: &str, transcript: &str) -> ChatLogRecord {
        ChatLogRecord {
            id: id.to_string(),
            turn_id: None,
            ts: 1,
            session_id: "s1".to_string(),
            transcript: transcript.to_string(),
            reply: "ok".to_string(),
            memories_written: vec![],
            llm_backend: "mock".to_string(),
            model: None,
            speaker_id: "household".to_string(),
            speaker_name: None,
            timing: None,
        }
    }

    #[test]
    fn append_then_read_after_cursor() {
        let dir = std::env::temp_dir().join(format!("chatlog_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("log.jsonl");
        let _ = std::fs::remove_file(&path);

        let log = ChatLog::open(&path).unwrap();
        log.append(&rec("a", "hello")).unwrap();
        log.append(&rec("b", "world")).unwrap();

        let (next, recs) = read_after(&path, LogCursor::default()).unwrap();
        assert_eq!(next.line, 2);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].transcript, "hello");

        // Reading from the high-water mark yields nothing new.
        let (next2, recs2) = read_after(&path, next).unwrap();
        assert_eq!(next2.line, 2);
        assert!(recs2.is_empty());

        // A new append is visible past the old mark.
        log.append(&rec("c", "again")).unwrap();
        let (next3, recs3) = read_after(&path, next).unwrap();
        assert_eq!(next3.line, 3);
        assert_eq!(recs3.len(), 1);
        assert_eq!(recs3[0].transcript, "again");

        // The tail reads newest first.
        let tail = read_tail(&path, 2).unwrap();
        assert_eq!(tail[0].transcript, "again");
        assert_eq!(tail[1].transcript, "world");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ids_are_unique() {
        let log = ChatLog::open(std::env::temp_dir().join("chatlog_ids.jsonl")).unwrap();
        let a = log.next_id();
        let b = log.next_id();
        assert_ne!(a, b);
    }

    #[test]
    fn timing_round_trips_and_omits_empty_stages() {
        let mut r = rec("t", "what's the weather");
        r.timing = Some(TurnTiming {
            path: Some("system1".to_string()),
            stt_ms: Some(466),
            system1_ms: Some(369),
            system1_outcome: Some("resolve".to_string()),
            system1_intent: Some("weather".to_string()),
            system1_confidence: Some(1.0),
            fetch_ms: Some(1261),
            tts_ms: Some(308),
            total_ms: Some(2409),
            ..Default::default()
        });
        let json = serde_json::to_string(&r).unwrap();
        // Present stages serialize…
        assert!(json.contains("\"fetch_ms\":1261"), "{json}");
        assert!(json.contains("\"system1_intent\":\"weather\""), "{json}");
        // …absent System-2 stages are skipped entirely (skip_serializing_if).
        assert!(!json.contains("recall_ms"), "{json}");
        assert!(!json.contains("llm_ms"), "{json}");
        // Round-trips losslessly.
        let back: ChatLogRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn old_records_without_timing_still_parse() {
        // A pre-feature JSONL line (no `timing` key) must deserialize with `timing: None`.
        let line = r#"{"id":"1-0","ts":1,"session_id":"s1","transcript":"hi","reply":"hey",
            "memories_written":[],"llm_backend":"mock","model":null,"speaker_id":"household",
            "speaker_name":null}"#;
        let r: ChatLogRecord = serde_json::from_str(line).unwrap();
        assert_eq!(r.timing, None);
        assert_eq!(r.transcript, "hi");
    }
}
