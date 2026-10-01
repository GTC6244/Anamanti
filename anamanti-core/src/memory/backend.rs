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
//! - `HelixRecall` — GraphRAG: embed the query, then vector KNN + graph expansion
//!   in the embedded HelixDB.
//!
//! The explicit/inferred memory *writes* and the Phase-6 control protocol still go
//! through the concrete [`MemoryStore`] regardless of which recall backend is
//! selected, so nothing about settings/voice management changes.

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use async_trait::async_trait;

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
    /// Query-embedding round-trip latency in ms (GraphRAG only; the OpenAI call).
    pub embed_ms: Option<u64>,
    /// Local vector KNN + graph-expansion latency in ms (GraphRAG only).
    pub search_ms: Option<u64>,
}

/// Produces the memory context lines for a turn. `Send + Sync` for sharing behind
/// an `Arc` across concurrent turns.
#[async_trait]
pub trait Recall: Send + Sync {
    /// Return up to `limit` context snippets relevant to `transcript` for the given
    /// speaker (`None` = shared/household scope), most relevant first, already
    /// de-duplicated, wrapped in a [`RecallResult`] whose optional per-stage timings
    /// the caller records for the timing logs. A speaker's own memories plus shared
    /// ones are in scope.
    async fn recall(
        &self,
        transcript: &str,
        speaker_id: Option<&str>,
        limit: usize,
    ) -> Result<RecallResult>;

    /// Short, stable id of this backend for the timing logs: `"helix"` (embedded
    /// HelixDB GraphRAG) or `"sqlite-fts"`. Defaults to FTS so any future backend
    /// opts in explicitly.
    fn backend(&self) -> &'static str {
        "sqlite-fts"
    }

    /// Whether a recall of a non-empty transcript issues a query-embedding network
    /// call (OpenAI embeddings). `true` for GraphRAG, `false` for pure-local FTS.
    /// This is the dominant, variable component of a recall's latency.
    fn embeds_query(&self) -> bool {
        false
    }

    /// Which embedder produced the query vector (`"local"`, `"openai"`, `"mock"`), or
    /// `None` for backends that don't embed (SQLite FTS). Recorded per turn so the
    /// `/chatlog` timing view labels the embedding stage with the real backend.
    fn embedder_label(&self) -> Option<&'static str> {
        None
    }
}

/// SQLite FTS recall — the default backend (unchanged Phase-4 behavior).
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
        let hits = self.memory.search_scoped(transcript, speaker_id, limit)?;
        Ok(RecallResult {
            hits: hits.into_iter().map(|m| m.content).collect(),
            // FTS is purely local: no embedding, no separate graph stage to split out.
            ..Default::default()
        })
    }
}

pub use helix_recall::HelixRecall;

mod helix_recall {
    use super::*;
    use crate::memory::embed::Embedder;
    use crate::memory::helix::HelixMemory;

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
            let hits = self.helix.recall(qvec, speaker_id, self.k, limit).await?;
            let search_ms = t1.elapsed().as_millis() as u64;
            Ok(RecallResult {
                hits,
                embed_ms: Some(embed_ms),
                search_ms: Some(search_ms),
            })
        }

        fn backend(&self) -> &'static str {
            "helix"
        }

        fn embeds_query(&self) -> bool {
            true
        }

        fn embedder_label(&self) -> Option<&'static str> {
            Some(self.embedder.label())
        }
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
        // FTS reports no per-stage split.
        assert_eq!(res.embed_ms, None);
        assert_eq!(res.search_ms, None);
    }
}
