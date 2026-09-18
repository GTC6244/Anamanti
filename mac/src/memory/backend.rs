//! Retrieval backend seam (memory_plan.md Q2: HelixDB runs *alongside* SQLite
//! behind a trait; SQLite stays the default, Helix is opt-in).
//!
//! [`Recall`] is the one operation the orchestrator's context builder needs:
//! given the incoming transcript, return the memory snippets to inject into the
//! system prompt. Two implementations:
//!
//! - [`SqliteRecall`] — today's behavior: FTS keyword search over the SQLite
//!   [`MemoryStore`]. Always available; the default.
//! - `HelixRecall` (feature `helix`) — GraphRAG: embed the query, then vector KNN
//!   + graph expansion in the embedded HelixDB.
//!
//! The explicit/inferred memory *writes* and the Phase-6 control protocol still go
//! through the concrete [`MemoryStore`] regardless of which recall backend is
//! selected, so nothing about settings/voice management changes.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use super::MemoryStore;

/// Produces the memory context lines for a turn. `Send + Sync` for sharing behind
/// an `Arc` across concurrent turns.
#[async_trait]
pub trait Recall: Send + Sync {
    /// Return up to `limit` context snippets relevant to `transcript` for the given
    /// speaker (`None` = shared/household scope), most relevant first, already
    /// de-duplicated. A speaker's own memories plus shared ones are in scope.
    async fn recall(
        &self,
        transcript: &str,
        speaker_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>>;
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
    ) -> Result<Vec<String>> {
        let hits = self.memory.search_scoped(transcript, speaker_id, limit)?;
        Ok(hits.into_iter().map(|m| m.content).collect())
    }
}

#[cfg(feature = "helix")]
pub use helix_recall::HelixRecall;

#[cfg(feature = "helix")]
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
        ) -> Result<Vec<String>> {
            if transcript.trim().is_empty() {
                return Ok(Vec::new());
            }
            let qvec = self.embedder.embed_one(transcript).await?;
            self.helix.recall(qvec, speaker_id, self.k, limit).await
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
        let hits = recall
            .recall("what music do I like", None, 5)
            .await
            .unwrap();
        assert!(hits.iter().any(|h| h.contains("jazz")));
    }
}
