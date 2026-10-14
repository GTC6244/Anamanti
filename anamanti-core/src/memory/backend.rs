//! Retrieval backend seam (memory_plan.md Q2: HelixDB runs *alongside* SQLite
//! behind a trait; recall defaults to HelixDB GraphRAG, with SQLite FTS as the
//! always-available fallback).
//!
//! [`Recall`] is the one operation the orchestrator's context builder needs:
//! given the incoming transcript, return the memory snippets to inject into the
//! system prompt. Two implementations:
//!
//! - [`SqliteRecall`] — FTS keyword search over the SQLite [`MemoryStore`]. Always
//!   available; the fallback when GraphRAG can't initialize.
//! - [`HelixRecall`] — GraphRAG: embed the query, then vector KNN + graph expansion
//!   in the embedded HelixDB.
//!
//! The explicit/inferred memory *writes* and the Phase-6 control protocol still go
//! through the concrete [`MemoryStore`] regardless of which recall backend is
//! selected, so nothing about settings/voice management changes.

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use async_trait::async_trait;

use super::embed::Embedder;
use super::helix::HelixMemory;
use super::MemoryStore;

/// The outcome of a recall: the context snippets plus an optional per-stage latency
/// split. Only the GraphRAG backend fills the two timings — the query-embedding
/// network round-trip (`embed_ms`) versus the local HelixDB vector KNN + graph
/// expansion (`search_ms`) — so the `/chatlog` page can show them as two separate
/// rows. SQLite FTS leaves both `None` (it does neither).
#[derive(Debug, Clone, Default)]
pub struct RecallResult {
    /// Context snippets, most relevant first, de-duplicated, capped to `limit`.
    pub hits: Vec<String>,
    /// The subset of `hits` that are **household** facts (true for everyone) rather
    /// than the reader's own — the prompt presents the two separately.
    pub household: Vec<String>,
    /// Query-embedding round-trip latency in ms (GraphRAG only; the OpenAI call).
    pub embed_ms: Option<u64>,
    /// Local vector KNN + graph-expansion latency in ms (GraphRAG only).
    pub search_ms: Option<u64>,
}

/// Produces the memory context lines for a turn. `Send + Sync` for sharing behind
/// an `Arc` across concurrent turns.
///
/// Besides recall, the seam carries the **speaker-attribution hooks** a speaker
/// merge/forget needs ([`reassign_speaker`](Recall::reassign_speaker),
/// [`delete_speaker`](Recall::delete_speaker)): a backend that keeps its own copy of
/// `speaker_id` (the HelixDB graph) must rewrite it, or the person's recall loses
/// their history. Backends that read attribution straight from the SQLite
/// [`MemoryStore`] (FTS) keep the no-op defaults — the caller updates SQLite itself.
#[async_trait]
pub trait Recall: Send + Sync {
    /// Return up to `limit` context snippets relevant to `transcript` for the given
    /// reader (`Some(spk-…)` = that person's own memories plus household ones; `None` =
    /// household only; the unknown speaker's are never returned), most relevant first,
    /// already de-duplicated, wrapped in a [`RecallResult`] whose optional per-stage
    /// timings the caller records for the timing logs.
    async fn recall(
        &self,
        transcript: &str,
        speaker_id: Option<&str>,
        limit: usize,
    ) -> Result<RecallResult>;

    /// Short, stable id of this backend for the timing logs: `"helix"` (embedded
    /// HelixDB GraphRAG) or `"sqlite-fts"`.
    fn backend(&self) -> &'static str;

    /// Which embedder produced the query vector (`"local"`, `"openai"`, `"mock"`), or
    /// `None` for backends that don't embed the query (SQLite FTS). Recorded per turn
    /// so the `/chatlog` timing view labels the embedding stage with the real
    /// backend; `is_some()` also says whether a recall pays a query-embedding cost.
    fn embedder_label(&self) -> Option<&'static str> {
        None
    }

    /// Re-attribute everything this backend stores for speaker `from` to speaker `to`
    /// (two voice clusters were merged). Returns how many stored items moved.
    async fn reassign_speaker(&self, from: &str, to: &str) -> Result<u64> {
        let _ = (from, to);
        Ok(0)
    }

    /// Detach everything this backend stores for speaker `id` from that person (their
    /// voiceprint was forgotten): it moves to the unknown speaker (never recalled),
    /// matching the SQLite side. Returns how many stored items changed.
    async fn delete_speaker(&self, id: &str) -> Result<u64> {
        let _ = id;
        Ok(0)
    }

    /// Re-attribute one logged turn (chat-log record `ext_id`) and the person/unknown
    /// facts it wrote to speaker `to` — the admin console's turn correction
    /// (SpeakerScopedMemoryPlan.md §8.4). Returns how many stored items changed.
    async fn reattribute_turn(&self, ext_id: &str, to: &str) -> Result<u64> {
        let _ = (ext_id, to);
        Ok(0)
    }

    /// Move stored facts with exactly `content` from owner `from` to owner `to`
    /// (`spk-…` / `household` / `unknown`) — the admin review queue's attribute action.
    async fn retag_memory(&self, content: &str, from: &str, to: &str) -> Result<u64> {
        let _ = (content, from, to);
        Ok(0)
    }

    /// Drop stored facts with exactly `content` owned by `owner` — the admin console's
    /// delete, so a removed fact stops being recalled from the graph too.
    async fn forget_memory(&self, content: &str, owner: &str) -> Result<u64> {
        let _ = (content, owner);
        Ok(0)
    }
}

/// SQLite FTS recall — the always-available keyword backend.
pub struct SqliteRecall {
    memory: Arc<MemoryStore>,
}

impl SqliteRecall {
    pub fn new(memory: Arc<MemoryStore>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl Recall for SqliteRecall {
    async fn recall(
        &self,
        transcript: &str,
        speaker_id: Option<&str>,
        limit: usize,
    ) -> Result<RecallResult> {
        // rusqlite is blocking (and serialized behind the store's mutex), so the FTS
        // query runs on the blocking pool rather than stalling a tokio worker.
        let memory = self.memory.clone();
        let transcript = transcript.to_string();
        let speaker_id = speaker_id.map(str::to_string);
        let hits = tokio::task::spawn_blocking(move || {
            memory.search_scoped(&transcript, speaker_id.as_deref(), limit)
        })
        .await
        .context("joining the SQLite recall task")??;
        Ok(RecallResult {
            household: hits
                .iter()
                .filter(|m| m.scope == super::ScopeKind::Household)
                .map(|m| m.content.clone())
                .collect(),
            hits: hits.into_iter().map(|m| m.content).collect(),
            // FTS is purely local: no embedding, no separate graph stage to split out.
            ..Default::default()
        })
    }

    fn backend(&self) -> &'static str {
        "sqlite-fts"
    }
}

/// GraphRAG recall over the embedded HelixDB: embed the query with the same
/// embedder used at ingest time, then vector KNN + graph expansion.
pub struct HelixRecall {
    helix: Arc<HelixMemory>,
    embedder: Arc<dyn Embedder>,
    /// KNN fan-out per vector search (before graph expansion + capping).
    k: usize,
}

impl HelixRecall {
    pub fn new(helix: Arc<HelixMemory>, embedder: Arc<dyn Embedder>, k: usize) -> Self {
        Self { helix, embedder, k }
    }
}

#[async_trait]
impl Recall for HelixRecall {
    async fn recall(
        &self,
        transcript: &str,
        speaker_id: Option<&str>,
        limit: usize,
    ) -> Result<RecallResult> {
        if transcript.trim().is_empty() {
            return Ok(RecallResult::default());
        }
        // Time the two stages separately: the query-embedding cost (local nomic
        // inference, or an OpenAI round-trip) vs. the local vector KNN + graph hop.
        // `embed_query` (not `embed_one`) so asymmetric models — nomic — apply the
        // `search_query:` prefix rather than the `search_document:` one used at ingest.
        let t0 = Instant::now();
        let qvec = self.embedder.embed_query(transcript).await?;
        let embed_ms = t0.elapsed().as_millis() as u64;
        let t1 = Instant::now();
        let (hits, household) = self
            .helix
            .recall_scoped(qvec, speaker_id, self.k, limit)
            .await?;
        let search_ms = t1.elapsed().as_millis() as u64;
        Ok(RecallResult {
            hits,
            household,
            embed_ms: Some(embed_ms),
            search_ms: Some(search_ms),
        })
    }

    fn backend(&self) -> &'static str {
        "helix"
    }

    fn embedder_label(&self) -> Option<&'static str> {
        Some(self.embedder.label())
    }

    async fn reassign_speaker(&self, from: &str, to: &str) -> Result<u64> {
        self.helix.reassign_speaker(from, to).await
    }

    async fn delete_speaker(&self, id: &str) -> Result<u64> {
        self.helix.delete_speaker(id).await
    }

    async fn reattribute_turn(&self, ext_id: &str, to: &str) -> Result<u64> {
        self.helix.reattribute_turn(ext_id, to).await
    }

    async fn retag_memory(&self, content: &str, from: &str, to: &str) -> Result<u64> {
        self.helix.retag_memory(content, from, to).await
    }

    async fn forget_memory(&self, content: &str, owner: &str) -> Result<u64> {
        self.helix.forget_memory(content, owner).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryKind, MemorySource};

    #[tokio::test]
    async fn sqlite_recall_returns_relevant_contents() {
        let store = Arc::new(MemoryStore::open_in_memory().unwrap());
        store
            .add(
                MemoryKind::Preference,
                "The user likes jazz music",
                MemorySource::Inferred,
            )
            .unwrap();
        store
            .add(
                MemoryKind::Fact,
                "The user lives in Portland",
                MemorySource::Inferred,
            )
            .unwrap();
        let recall = SqliteRecall::new(store);
        let res = recall
            .recall("what music do I like", None, 5)
            .await
            .unwrap();
        assert!(res.hits.iter().any(|h| h.contains("jazz")));
        // FTS reports no per-stage split and no query embedder.
        assert_eq!(res.embed_ms, None);
        assert_eq!(res.search_ms, None);
        assert_eq!(recall.backend(), "sqlite-fts");
        assert!(recall.embedder_label().is_none());
        // The speaker hooks are no-ops: FTS reads attribution from SQLite directly.
        assert_eq!(recall.reassign_speaker("spk-a", "spk-b").await.unwrap(), 0);
        assert_eq!(recall.delete_speaker("spk-a").await.unwrap(), 0);
    }
}
