//! Background ingestion (memory_plan.md Goal 3, Q3 "batch/background").
//!
//! The ingester tails the JSONL chat log, and for each new batch of turns:
//! 1. embeds them (one batched OpenAI call per run),
//! 2. runs entity/topic extraction (Claude Haiku 4.5) to build the graph,
//! 3. upserts `Turn`/`Memory`/`Entity` nodes + edges into HelixDB,
//! 4. advances a committed high-water mark (a sidecar `<log>.offset` file) so
//!    re-runs are cheap and crash-safe (upserts are idempotent by `ext_id`).
//!
//! This keeps embedding/extraction off the live turn path entirely — the only
//! per-turn cost is the query embedding at recall time. Requires the `helix`
//! feature (it writes into the embedded HelixDB).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use super::chatlog::{self, ChatLogRecord};
use super::embed::Embedder;
use super::entity::EntityExtractor;
use super::helix::HelixMemory;

/// Drains the chat log into the HelixDB graph memory.
pub struct MemoryIngester {
    chatlog_path: PathBuf,
    offset_path: PathBuf,
    helix: Arc<HelixMemory>,
    embedder: Arc<dyn Embedder>,
    extractor: Arc<dyn EntityExtractor>,
}

impl MemoryIngester {
    /// Build an ingester over `chatlog_path`. The offset sidecar lives next to it
    /// (`<chatlog_path>.offset`).
    pub fn new(
        chatlog_path: impl Into<PathBuf>,
        helix: Arc<HelixMemory>,
        embedder: Arc<dyn Embedder>,
        extractor: Arc<dyn EntityExtractor>,
    ) -> Self {
        let chatlog_path = chatlog_path.into();
        let offset_path = offset_sidecar(&chatlog_path);
        Self {
            chatlog_path,
            offset_path,
            helix,
            embedder,
            extractor,
        }
    }

    /// Process every chat-log record past the committed offset. Returns how many
    /// new turns were ingested. Safe to call repeatedly.
    pub async fn run_once(&self) -> Result<usize> {
        let from = self.read_offset();
        let (next, records) = chatlog::read_from(&self.chatlog_path, from)?;
        if records.is_empty() {
            return Ok(0);
        }

        // 1. Batch-embed all turn texts (transcript + reply) and memory strings in a
        //    single call. Track which vector belongs to which item.
        let mut texts: Vec<String> = Vec::new();
        // (record index, kind) where kind = Turn or Memory(mem_index)
        enum Item {
            Turn(usize),
            Memory(usize, usize),
        }
        let mut items: Vec<Item> = Vec::new();
        for (ri, r) in records.iter().enumerate() {
            texts.push(turn_text(r));
            items.push(Item::Turn(ri));
            for (mi, m) in r.memories_written.iter().enumerate() {
                texts.push(m.clone());
                items.push(Item::Memory(ri, mi));
            }
        }
        let vectors = self
            .embedder
            .embed(&texts)
            .await
            .context("embedding chat-log batch")?;
        anyhow::ensure!(
            vectors.len() == items.len(),
            "embedder returned {} vectors for {} inputs",
            vectors.len(),
            items.len()
        );

        // 2 + 3. Extract entities and upsert, in log order.
        let mut ingested = 0usize;
        for (item, vector) in items.into_iter().zip(vectors) {
            match item {
                Item::Turn(ri) => {
                    let r = &records[ri];
                    let entities =
                        self.extractor
                            .extract(&turn_text(r))
                            .await
                            .unwrap_or_else(|e| {
                                log::warn!("entity extraction failed for turn {}: {e}", r.id);
                                Vec::new()
                            });
                    self.helix
                        .ingest_turn(
                            &r.id,
                            &r.session_id,
                            r.ts,
                            &turn_text(r),
                            vector,
                            &entities,
                            speaker_of(r),
                            r.speaker_name.as_deref(),
                        )
                        .await
                        .with_context(|| format!("ingesting turn {}", r.id))?;
                    ingested += 1;
                }
                Item::Memory(ri, mi) => {
                    let r = &records[ri];
                    let content = &r.memories_written[mi];
                    let entities = self.extractor.extract(content).await.unwrap_or_default();
                    let ext_id = format!("{}-mem-{}", r.id, mi);
                    self.helix
                        .ingest_memory(&ext_id, "memory", content, vector, &entities, speaker_of(r))
                        .await
                        .with_context(|| format!("ingesting memory {ext_id}"))?;
                }
            }
        }

        // Flush so the new nodes are durable and picked up by the index-build
        // worker before the next recall.
        self.helix.flush().await.context("flushing after ingest")?;

        // 4. Commit the new high-water mark only after successful upserts.
        self.write_offset(next)?;
        log::info!("memory ingester: ingested {ingested} new turn(s), offset now {next}");
        Ok(ingested)
    }

    /// Spawn a background loop that ingests every `interval`. Returns the join
    /// handle; the loop ends when the ingester `Arc` is the only ref and the task
    /// is aborted (or the process exits).
    pub fn spawn(self: Arc<Self>, interval: Duration) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                if let Err(e) = self.run_once().await {
                    log::warn!("memory ingester run failed: {e:#}");
                }
                tokio::time::sleep(interval).await;
            }
        })
    }

    fn read_offset(&self) -> u64 {
        std::fs::read_to_string(&self.offset_path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    fn write_offset(&self, offset: u64) -> Result<()> {
        std::fs::write(&self.offset_path, offset.to_string())
            .with_context(|| format!("writing offset {}", self.offset_path.display()))
    }
}

/// The speaker id to attribute a record to, normalizing a missing id (pre-speaker-ID
/// logs) to the shared household user.
fn speaker_of(r: &ChatLogRecord) -> &str {
    if r.speaker_id.is_empty() {
        "household"
    } else {
        &r.speaker_id
    }
}

/// Combined turn text used for embedding + extraction (Q8: transcript + reply).
fn turn_text(r: &ChatLogRecord) -> String {
    if r.reply.trim().is_empty() {
        r.transcript.clone()
    } else {
        format!("{}\n{}", r.transcript, r.reply)
    }
}

fn offset_sidecar(chatlog_path: &Path) -> PathBuf {
    let mut s = chatlog_path.to_path_buf().into_os_string();
    s.push(".offset");
    PathBuf::from(s)
}
