//! Background ingestion (memory_plan.md Goal 3, Q3 "batch/background").
//!
//! The ingester tails the JSONL chat log, and for each new batch of turns:
//! 1. embeds them (one batched embedder call per run),
//! 2. runs entity/topic extraction (Claude Haiku 4.5) to build the graph,
//! 3. upserts `Turn`/`Memory`/`Entity` nodes + edges into HelixDB,
//! 4. advances a committed high-water mark (a sidecar `<log>.offset` file) so
//!    re-runs are cheap and crash-safe (upserts are idempotent by `ext_id`).
//!
//! This keeps embedding/extraction off the live turn path entirely — the only
//! per-turn cost is the query embedding at recall time.
//!
//! ## The high-water mark
//! The sidecar holds a **line index** (the number of chat-log lines already
//! ingested) — the format every existing install has on disk, and still the one
//! written. In memory the ingester also caches the matching **byte position**
//! ([`LogCursor`]), so each tick seeks straight to the new lines and counts them
//! while streaming instead of re-reading the whole log; the byte position is found
//! once per process by one streaming pass over the already-ingested prefix. Only
//! complete (`\n`-terminated) lines are consumed, so a record still being appended
//! is picked up whole on the next tick. The file I/O runs on the blocking pool.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};

use super::chatlog::{self, ChatLogRecord, LogCursor};
use super::embed::Embedder;
use super::entity::EntityExtractor;
use super::helix::{turn_text, HelixMemory, NewTurn};

/// Drains the chat log into the HelixDB graph memory.
pub struct MemoryIngester {
    chatlog_path: PathBuf,
    offset_path: PathBuf,
    helix: Arc<HelixMemory>,
    embedder: Arc<dyn Embedder>,
    extractor: Arc<dyn EntityExtractor>,
    /// The cached read position matching the committed line index; `None` until the
    /// first run locates it.
    cursor: Mutex<Option<LogCursor>>,
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
        Self::new_with_offset(chatlog_path, offset_path, helix, embedder, extractor)
    }

    /// Like [`MemoryIngester::new`] but with an explicit offset sidecar path. Used by
    /// the live-swappable GraphRAG controller to keep a **per-embedding-backend**
    /// high-water mark (`<log>.<signature>.offset`), so each backend re-ingests the
    /// chat log independently and switching back resumes where that backend left off.
    pub fn new_with_offset(
        chatlog_path: impl Into<PathBuf>,
        offset_path: impl Into<PathBuf>,
        helix: Arc<HelixMemory>,
        embedder: Arc<dyn Embedder>,
        extractor: Arc<dyn EntityExtractor>,
    ) -> Self {
        Self {
            chatlog_path: chatlog_path.into(),
            offset_path: offset_path.into(),
            helix,
            embedder,
            extractor,
            cursor: Mutex::new(None),
        }
    }

    /// Process every chat-log record past the committed offset. Returns how many
    /// new turns were ingested. Safe to call repeatedly.
    pub async fn run_once(&self) -> Result<usize> {
        let cached = *self.cursor.lock().unwrap();
        let chatlog_path = self.chatlog_path.clone();
        let offset_path = self.offset_path.clone();
        let (from, next, records) = tokio::task::spawn_blocking(move || {
            let from = match cached {
                Some(c) => c,
                None => chatlog::cursor_at_line(&chatlog_path, read_offset(&offset_path))?,
            };
            let (next, records) = chatlog::read_after(&chatlog_path, from)?;
            anyhow::Ok((from, next, records))
        })
        .await
        .context("joining the chat-log read task")??;
        *self.cursor.lock().unwrap() = Some(from);
        if records.is_empty() {
            if next != from {
                // Only blank/malformed lines arrived: still move past them.
                self.commit(next).await?;
            }
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
            // Gate: don't make a *non-answer* turn ("I don't know", "I can't find
            // that", "could you clarify…") recallable. Re-surfacing it under "What you
            // remember about this person" few-shot-primes the model to repeat the same
            // failure next time — a self-reinforcing loop (a mis-transcribed proper noun
            // that fails once then keeps failing). The turn still lives in the chat log
            // (the auditable source of truth); only its raw transcript+reply Turn node is
            // withheld from recall. Explicit/inferred facts written this turn are still
            // ingested below.
            if is_recallable_turn(r) {
                texts.push(turn_text(&r.transcript, &r.reply));
                items.push(Item::Turn(ri));
            } else {
                log::debug!(
                    "memory ingester: withholding non-answer turn {} from recall (reply: {:?})",
                    r.id,
                    truncate_for_log(&r.reply),
                );
            }
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
        for ((item, vector), text) in items.into_iter().zip(vectors).zip(&texts) {
            match item {
                Item::Turn(ri) => {
                    let r = &records[ri];
                    let entities = self.extractor.extract(text).await.unwrap_or_else(|e| {
                        log::warn!("entity extraction failed for turn {}: {e}", r.id);
                        Vec::new()
                    });
                    self.helix
                        .ingest_turn(NewTurn {
                            ext_id: &r.id,
                            session: &r.session_id,
                            ts: r.ts,
                            transcript: &r.transcript,
                            reply: &r.reply,
                            embedding: vector,
                            entities: &entities,
                            speaker_id: speaker_of(r),
                            speaker_name: r.speaker_name.as_deref(),
                        })
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
        self.commit(next).await?;
        log::info!(
            "memory ingester: ingested {ingested} new turn(s), offset now {}",
            next.line
        );
        Ok(ingested)
    }

    /// Spawn a background loop that ingests every `interval`. Returns the join
    /// handle; the loop ends when the ingester `Arc` is the only ref and the task
    /// is aborted (or the process exits).
    pub fn spawn(self: Arc<Self>, interval: Duration) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            // One-time cleanup of legacy non-answer turns ingested before the gate in
            // `run_once` existed. Idempotent: once purged — and with new non-answers no
            // longer ingested — later starts find nothing to remove, so this is cheap
            // after the first run.
            match self.helix.purge_non_answer_turns(is_non_answer).await {
                Ok(0) => {}
                Ok(n) => {
                    log::info!("memory ingester: purged {n} legacy non-answer turn(s) from recall")
                }
                Err(e) => log::warn!("memory ingester: non-answer purge failed: {e:#}"),
            }
            loop {
                if let Err(e) = self.run_once().await {
                    log::warn!("memory ingester run failed: {e:#}");
                }
                tokio::time::sleep(interval).await;
            }
        })
    }

    /// Persist `next` (its line index, the on-disk format) and cache its position.
    async fn commit(&self, next: LogCursor) -> Result<()> {
        let path = self.offset_path.clone();
        tokio::task::spawn_blocking(move || write_offset(&path, next.line))
            .await
            .context("joining the offset write task")??;
        *self.cursor.lock().unwrap() = Some(next);
        Ok(())
    }
}

/// The committed line index from the sidecar (`0` when absent or unreadable — the
/// upserts are idempotent by `ext_id`, so starting over never duplicates nodes).
fn read_offset(path: &Path) -> u64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Write the line index atomically (temp file + rename), so a crash mid-write can
/// never leave a truncated sidecar that would restart ingestion from line 0.
fn write_offset(path: &Path, line: u64) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, line.to_string())
        .and_then(|_| std::fs::rename(&tmp, path))
        .with_context(|| format!("writing offset {}", path.display()))
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

/// Phrases that mark a reply as a *non-answer*: the assistant declining, failing to find
/// something, or asking the user to clarify. Matched case-insensitively against the reply
/// with curly apostrophes normalized to ASCII. Kept deliberately conservative — it should
/// only catch replies that clearly convey "I didn't answer this," never a real answer that
/// happens to contain a hedge.
const NON_ANSWER_MARKERS: &[&str] = &[
    "i don't know",
    "i do not know",
    "i'm not sure which",
    "i am not sure which",
    "i'm not sure what",
    "i'm not certain",
    "i'm not finding",
    "i am not finding",
    "i'm still not finding",
    "not finding a",
    "not finding any",
    "i couldn't find",
    "i could not find",
    "i can't find",
    "i cannot find",
    "couldn't find a",
    "could not find a",
    "couldn't find any",
    "i'm not able to find",
    "i am not able to find",
    "i don't have that",
];

/// Whether a reply reads as a non-answer (see [`NON_ANSWER_MARKERS`]).
fn is_non_answer(reply: &str) -> bool {
    let normalized = reply.replace('\u{2019}', "'").to_lowercase();
    NON_ANSWER_MARKERS
        .iter()
        .any(|marker| normalized.contains(marker))
}

/// Whether a turn should be ingested as a recallable `Turn` node. False for non-answers,
/// which would otherwise prime the model to repeat the failure (see the gate in
/// [`MemoryIngester::run_once`]).
fn is_recallable_turn(r: &ChatLogRecord) -> bool {
    !is_non_answer(&r.reply)
}

/// Shorten a string for a log line (char-safe, won't split a UTF-8 boundary).
fn truncate_for_log(s: &str) -> String {
    const MAX: usize = 80;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        let head: String = s.chars().take(MAX).collect();
        format!("{head}…")
    }
}

fn offset_sidecar(chatlog_path: &Path) -> PathBuf {
    let mut s = chatlog_path.to_path_buf().into_os_string();
    s.push(".offset");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::chatlog::ChatLog;
    use crate::memory::embed::MockEmbedder;
    use crate::memory::entity::NoopEntityExtractor;

    #[test]
    fn flags_non_answers() {
        for reply in [
            "I'm not sure which high school you mean — could you provide the full name?",
            "I'm still not finding a high school by that name.",
            "I couldn't find a route to that school — could you double-check the name?",
            "I'm not finding a school by that name.",
            "I'm not sure which secondary school you're referring to.",
            "I don't know.",
            "Sorry, I can't find that.",
        ] {
            assert!(is_non_answer(reply), "should flag as non-answer: {reply}");
        }
    }

    #[test]
    fn keeps_real_answers() {
        for reply in [
            "It's about a 20 minute drive north on Highway 8.",
            "The weather in Kitchener is 12 degrees and sunny.",
            "Sure, I've set a timer for ten minutes.",
            "Jacob Hespeler Secondary School is at 355 Holiday Inn Drive in Cambridge.",
            "Your next event is a dentist appointment on Thursday at 2 PM.",
        ] {
            assert!(
                !is_non_answer(reply),
                "should keep as a real answer: {reply}"
            );
        }
    }

    #[test]
    fn normalizes_curly_apostrophe() {
        // TTS/LLM output often uses U+2019; the gate must still catch it.
        assert!(is_non_answer(
            "I\u{2019}m not finding a school by that name."
        ));
    }

    fn rec(id: &str, transcript: &str) -> ChatLogRecord {
        ChatLogRecord {
            id: id.to_string(),
            ts: 1,
            session_id: "s".to_string(),
            transcript: transcript.to_string(),
            reply: "ok".to_string(),
            memories_written: vec![],
            llm_backend: "mock".to_string(),
            model: None,
            speaker_id: String::new(),
            speaker_name: None,
            timing: None,
        }
    }

    /// An existing install's sidecar holds a plain LINE index. The ingester must
    /// resume exactly after that line — never re-ingesting or skipping a turn — keep
    /// writing the same format, and pick up a partially-appended record only once it
    /// is complete.
    #[test]
    fn honours_a_legacy_line_index_sidecar() {
        std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(legacy_sidecar_body());
            })
            .unwrap()
            .join()
            .unwrap();
    }

    async fn legacy_sidecar_body() {
        let dir = std::env::temp_dir().join(format!(
            "anamanti-ingest-{}-{}",
            std::process::id(),
            crate::clock::now_millis()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let log_path = dir.join("anamanti_chatlog.jsonl");
        let sidecar = dir.join("anamanti_chatlog.jsonl.nomic-768.offset");

        let log = ChatLog::open(&log_path).unwrap();
        for (id, t) in [("a", "first"), ("b", "second"), ("c", "third")] {
            log.append(&rec(id, t)).unwrap();
        }
        // Production state: two lines already ingested, recorded as a line index.
        std::fs::write(&sidecar, "2").unwrap();

        let helix = Arc::new(HelixMemory::open_in_memory("legacy", 8).await.unwrap());
        let ingester = MemoryIngester::new_with_offset(
            &log_path,
            &sidecar,
            helix.clone(),
            Arc::new(MockEmbedder::new(8)),
            Arc::new(NoopEntityExtractor),
        );
        assert_eq!(ingester.run_once().await.unwrap(), 1, "only line 3 is new");
        let turns = helix
            .list_turns(None, None, None, 10, 0)
            .await
            .unwrap()
            .turns;
        let ids: Vec<&str> = turns.iter().map(|t| t.ext_id.as_str()).collect();
        assert_eq!(ids, vec!["c"], "lines 1-2 were not re-ingested");
        assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), "3");

        // Nothing new → nothing ingested.
        assert_eq!(ingester.run_once().await.unwrap(), 0);

        // A record mid-append (no newline yet) is left for the next tick.
        let line = serde_json::to_string(&rec("d", "fourth")).unwrap();
        let (head, tail) = line.split_at(10);
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&log_path)
                .unwrap();
            f.write_all(head.as_bytes()).unwrap();
            assert_eq!(ingester.run_once().await.unwrap(), 0);
            assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), "3");
            f.write_all(format!("{tail}\n").as_bytes()).unwrap();
        }
        assert_eq!(ingester.run_once().await.unwrap(), 1);
        assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), "4");

        // A fresh process (no cached position) resumes from the line index alone.
        let restarted = MemoryIngester::new_with_offset(
            &log_path,
            &sidecar,
            helix.clone(),
            Arc::new(MockEmbedder::new(8)),
            Arc::new(NoopEntityExtractor),
        );
        log.append(&rec("e", "fifth")).unwrap();
        assert_eq!(restarted.run_once().await.unwrap(), 1);
        let turns = helix
            .list_turns(None, None, None, 10, 0)
            .await
            .unwrap()
            .turns;
        assert_eq!(turns.len(), 3, "c, d, e — each exactly once");
        std::fs::remove_dir_all(&dir).ok();
    }
}
