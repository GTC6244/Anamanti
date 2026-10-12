//! Embedded HelixDB GraphRAG memory (memory_plan.md Goals 1 & 4).
//!
//! HelixDB's engine crate (`db`) is compiled **in-process** — no server, no
//! Docker (see memory_plan.md "SPIKE RESULT"). This module owns a [`db::HelixDB`]
//! handle opened against a local on-disk object store and exposes the two
//! operations the rest of the system needs:
//!
//! - **ingest** — the background ingester calls [`HelixMemory::ingest_turn`] /
//!   [`HelixMemory::ingest_memory`] to write vector-bearing nodes plus the graph
//!   edges (`SAID`, `MENTIONS`, `ABOUT`, `FOLLOWS`, `KNOWS`) that make this a
//!   graph and not just a vector store.
//! - **recall** — [`HelixMemory::recall`] does the GraphRAG read: vector KNN over
//!   `Turn` and `Memory` embeddings, then a graph hop (nearest turns → their
//!   entities → memories about those entities) to surface related context a pure
//!   vector search would miss.
//!
//! ## Graph model
//! Nodes: `User` (household), `Turn`, `Memory`, `Entity`. Every `Turn`/`Memory`
//! carries an `embedding` vector property (indexed) and a stable `ext_id` string
//! (the chat-log record id / SQLite row id) for idempotent upserts.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde_json::Value;

use db::{HelixDB, HelixDbSource};
use helix_ast::expr::Predicate;
use helix_ast::index::{IndexSpec, VectorDistanceMetric};
use helix_ast::{batch, graph::NodeRef, query::QueryRequest, traversal::g, value::PropertyInput};

use super::entity::Entity;

// Node labels.
const L_USER: &str = "User";
const L_TURN: &str = "Turn";
const L_MEMORY: &str = "Memory";
const L_ENTITY: &str = "Entity";
// Edge labels.
const E_SAID: &str = "SAID";
const E_MENTIONS: &str = "MENTIONS";
const E_ABOUT: &str = "ABOUT";
const E_FOLLOWS: &str = "FOLLOWS";
const E_KNOWS: &str = "KNOWS";
// The shared/unattributed user, used for turns without a specific speaker (and as
// the sentinel `speaker_id` on such nodes). Per-person `User` nodes are keyed by
// their `spk-…` id (speaker_id_plan.md Phase D).
const HOUSEHOLD: &str = "household";

/// Per-kind tally of what a [`HelixMemory::rename_entity`] edit touched: `Entity`
/// nodes renamed, plus `Turn`/`Memory` nodes whose free text was corrected.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct RenameOutcome {
    /// `Entity` nodes whose `name` property was rewritten.
    pub entities: u64,
    /// `Turn` nodes whose `text` had the old name corrected.
    pub turns: u64,
    /// `Memory` nodes whose `content` had the old name corrected.
    pub memories: u64,
}

impl RenameOutcome {
    /// Total nodes touched across all kinds — `0` means the name was found nowhere.
    pub fn total(&self) -> u64 {
        self.entities + self.turns + self.memories
    }
}

/// Embedded GraphRAG memory backend.
pub struct HelixMemory {
    db: HelixDB,
    dims: usize,
    /// Most recent `Turn` node id per session, for the temporal `FOLLOWS` chain.
    last_turn: Mutex<HashMap<String, u64>>,
}

impl HelixMemory {
    /// Open an on-disk store rooted at `root` (local object store; persists across
    /// restarts). `dims` is the embedder's vector dimensionality.
    pub async fn open_disk(root: impl Into<PathBuf>, database: &str, dims: usize) -> Result<Self> {
        let root = root.into();
        // The local object store canonicalizes its root, so it must exist first.
        std::fs::create_dir_all(&root)
            .with_context(|| format!("creating HelixDB store dir {}", root.display()))?;

        // Dimensionality guard. The vector indexes bake in `dims` at creation and
        // `create_index_if_not_exists` is a no-op on an existing store — so reopening
        // with a different embedder (e.g. switching OpenAI 1536 ↔ local nomic 768, or
        // changing Matryoshka width) would silently mismatch inserts/searches against
        // the old index. We stamp the chosen dims in a marker file and fail loud on a
        // mismatch rather than corrupt recall.
        let marker = root.join("embedding_dims");
        if let Ok(prev) = std::fs::read_to_string(&marker) {
            if let Ok(prev) = prev.trim().parse::<usize>() {
                anyhow::ensure!(
                    prev == dims,
                    "HelixDB store at {} was built for {prev}-dim embeddings but the current \
                     embedder produces {dims}-dim vectors. Switching embedders or dimensions \
                     needs a fresh store: remove or rename {} and restart — the chat log \
                     re-ingests memory into the new store.",
                    root.display(),
                    root.display(),
                );
            }
        }

        let db = HelixDB::open(HelixDbSource::Disk {
            root: root.clone(),
            database: database.to_string(),
        })
        .await
        .context("opening embedded HelixDB (disk)")?;
        let me = Self::init(db, dims).await?;
        // Record the dims for future opens (first creation, or re-affirm on match).
        std::fs::write(&marker, dims.to_string())
            .with_context(|| format!("writing embedding-dims marker {}", marker.display()))?;
        Ok(me)
    }

    /// Open an ephemeral in-memory store (tests).
    pub async fn open_in_memory(database: &str, dims: usize) -> Result<Self> {
        let db = HelixDB::open(HelixDbSource::InMemory {
            database: database.to_string(),
        })
        .await
        .context("opening embedded HelixDB (in-memory)")?;
        Self::init(db, dims).await
    }

    /// Create the vector indexes (idempotent) and ensure the household user exists.
    async fn init(db: HelixDB, dims: usize) -> Result<Self> {
        let dim = NonZeroUsize::new(dims).context("embedding dimensions must be > 0")?;
        for label in [L_TURN, L_MEMORY] {
            db.query(QueryRequest::write(
                batch::write_batch()
                    .var_as(
                        "_idx",
                        g().create_index_if_not_exists(IndexSpec::node_vector(
                            label,
                            "embedding",
                            dim,
                            VectorDistanceMetric::Cosine,
                            None::<String>,
                        )),
                    )
                    .returning(["_idx"]),
            ))
            .await
            .with_context(|| format!("creating vector index on {label}.embedding"))?;
        }

        let me = Self {
            db,
            dims,
            last_turn: Mutex::new(HashMap::new()),
        };
        // Vector indexes are built asynchronously by the secondary-index worker;
        // block until they're queryable so the first recall can't race the build.
        me.ensure_indexes_ready().await?;
        // Ensure the shared household user exists (keyed by speaker_id).
        me.ensure_user(HOUSEHOLD, None).await?;
        Ok(me)
    }

    /// Flush the writer so recent mutations are durable and picked up by the
    /// index-build worker. Call after an ingest batch.
    pub async fn flush(&self) -> Result<()> {
        self.db
            .flush_writer()
            .await
            .context("flushing HelixDB writer")?;
        Ok(())
    }

    /// Block (bounded) until both vector indexes answer a probe query — i.e. the
    /// async index build has settled. A newly created index reports
    /// `index_not_found` until the worker finishes building it.
    async fn ensure_indexes_ready(&self) -> Result<()> {
        self.flush().await?;
        let probe = {
            let mut v = vec![0.0f32; self.dims];
            v[0] = 1.0;
            v
        };
        for label in [L_TURN, L_MEMORY] {
            let mut ready = false;
            for _ in 0..60 {
                let r = self
                    .read(
                        batch::read_batch()
                            .var_as(
                                "p",
                                g().vector_search_nodes(label, "embedding", probe.clone(), 1, None)
                                    .value_map(None::<Vec<String>>),
                            )
                            .returning(["p"]),
                    )
                    .await;
                if r.is_ok() {
                    ready = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            anyhow::ensure!(
                ready,
                "vector index on {label}.embedding never became ready"
            );
        }
        Ok(())
    }

    /// The embedding dimensionality this store was opened with.
    pub fn dimensions(&self) -> usize {
        self.dims
    }

    // ---- writes -----------------------------------------------------------

    /// Ingest one completed turn: a `Turn` node (with embedding), a `SAID` edge
    /// from the speaking user, `MENTIONS`/`KNOWS` edges to each entity, and a
    /// `FOLLOWS` edge from the previous turn in the same session. Idempotent by
    /// `ext_id` (a re-ingested record returns the existing node id).
    // A turn genuinely carries this many independent fields; grouping them into a
    // struct would only move the list around, so the arg-count lint isn't worth it.
    #[allow(clippy::too_many_arguments)]
    pub async fn ingest_turn(
        &self,
        ext_id: &str,
        session: &str,
        ts: i64,
        text: &str,
        embedding: Vec<f32>,
        entities: &[Entity],
        speaker_id: &str,
        speaker_name: Option<&str>,
    ) -> Result<u64> {
        if let Some(existing) = self.find_by_ext_id(L_TURN, ext_id).await? {
            return Ok(existing);
        }
        self.check_dims(&embedding)?;

        // Resolve the speaking user (get-or-create) so SAID/KNOWS originate from
        // the right person, and pre-resolve entity ids so the batch links by id.
        let user_id = self.ensure_user(speaker_id, speaker_name).await?;
        let mut entity_ids = Vec::with_capacity(entities.len());
        for e in entities {
            entity_ids.push(self.ensure_entity(e).await?);
        }
        let prev = self.last_turn.lock().unwrap().get(session).copied();

        // One write batch: create the turn, then all its edges by var/id.
        let mut b = batch::write_batch().var_as(
            "t",
            g().add_n(
                L_TURN,
                vec![
                    ("ext_id", PropertyInput::from(ext_id)),
                    ("session", PropertyInput::from(session)),
                    ("ts", PropertyInput::from(ts)),
                    ("text", PropertyInput::from(text)),
                    ("speaker_id", PropertyInput::from(speaker_id)),
                    ("embedding", PropertyInput::from(embedding)),
                ],
            ),
        );
        b = b.var_as(
            "_said",
            g().n(NodeRef::id(user_id))
                .add_e(E_SAID, NodeRef::var("t"), no_props()),
        );
        for (i, eid) in entity_ids.iter().enumerate() {
            b = b.var_as(
                &format!("_m{i}"),
                g().n(NodeRef::var("t"))
                    .add_e(E_MENTIONS, NodeRef::id(*eid), no_props()),
            );
            b = b.var_as(
                &format!("_k{i}"),
                g().n(NodeRef::id(user_id))
                    .add_e(E_KNOWS, NodeRef::id(*eid), no_props()),
            );
        }
        if let Some(prev_id) = prev {
            b = b.var_as(
                "_f",
                g().n(NodeRef::id(prev_id))
                    .add_e(E_FOLLOWS, NodeRef::var("t"), no_props()),
            );
        }
        let res = self.write(b.returning(["t"])).await?;
        let id = first_id(&res, "t").context("ingest_turn: no turn id returned")?;
        self.last_turn
            .lock()
            .unwrap()
            .insert(session.to_string(), id);
        Ok(id)
    }

    /// Ingest one durable memory (fact/preference): a `Memory` node (with
    /// embedding) linked via `ABOUT` to each entity. Idempotent by `ext_id`.
    pub async fn ingest_memory(
        &self,
        ext_id: &str,
        kind: &str,
        content: &str,
        embedding: Vec<f32>,
        entities: &[Entity],
        speaker_id: &str,
    ) -> Result<u64> {
        if let Some(existing) = self.find_by_ext_id(L_MEMORY, ext_id).await? {
            return Ok(existing);
        }
        self.check_dims(&embedding)?;

        let mut entity_ids = Vec::with_capacity(entities.len());
        for e in entities {
            entity_ids.push(self.ensure_entity(e).await?);
        }
        let mut b = batch::write_batch().var_as(
            "m",
            g().add_n(
                L_MEMORY,
                vec![
                    ("ext_id", PropertyInput::from(ext_id)),
                    ("kind", PropertyInput::from(kind)),
                    ("content", PropertyInput::from(content)),
                    ("speaker_id", PropertyInput::from(speaker_id)),
                    ("embedding", PropertyInput::from(embedding)),
                ],
            ),
        );
        for (i, eid) in entity_ids.iter().enumerate() {
            b = b.var_as(
                &format!("_a{i}"),
                g().n(NodeRef::var("m"))
                    .add_e(E_ABOUT, NodeRef::id(*eid), no_props()),
            );
        }
        let res = self.write(b.returning(["m"])).await?;
        first_id(&res, "m").context("ingest_memory: no memory id returned")
    }

    // ---- edits ------------------------------------------------------------

    /// Rename an entity — fixing a spelling mistake — everywhere it appears:
    ///
    /// 1. every `Entity` node whose `name` exactly equals `old_name` (id + all
    ///    `MENTIONS`/`ABOUT`/`KNOWS` edges preserved — only `name` is rewritten);
    /// 2. every whole-word occurrence of `old_name` in the **free text** of `Turn`
    ///    nodes (`text`) and `Memory` nodes (`content`), so the misspelling is gone
    ///    from the stored transcript and remembered facts too.
    ///
    /// Matching is case-sensitive and whole-word: `"Sam"` is corrected but the
    /// `"Sam"` inside `"Samsung"` is left alone. Returns a [`RenameOutcome`] with
    /// the per-kind counts.
    ///
    /// A no-op rename (`old_name == new_name` after trimming) changes nothing and
    /// reports how many entity nodes already carry the name. Rejects a rename whose
    /// target already names a *different* entity: the graph keys entities by `name`
    /// (get-or-create in [`Self::ensure_entity`]), so a collision would create two
    /// nodes sharing a name and quietly split that entity's edges — merging is out
    /// of scope. Nothing is written when the rename is rejected.
    ///
    /// Note: the stale vector `embedding` on a rewritten `Turn`/`Memory` is left as
    /// is — a one-token spelling fix barely moves the embedding, and re-embedding
    /// would need the embedder (which this store does not hold). Recall stays
    /// correct; the vector just isn't recomputed for the corrected spelling.
    pub async fn rename_entity(&self, old_name: &str, new_name: &str) -> Result<RenameOutcome> {
        let old_name = old_name.trim();
        let new_name = new_name.trim();
        anyhow::ensure!(!new_name.is_empty(), "new entity name must not be empty");

        // No-op rename: nothing to write, but report the current match count so the
        // caller can still distinguish "entity exists" from "no such entity".
        if old_name == new_name {
            return Ok(RenameOutcome {
                entities: self.count_entities_named(new_name).await?,
                ..Default::default()
            });
        }

        // Rename the Entity node(s), guarding the identity key first.
        let entities = if self.count_entities_named(old_name).await? > 0 {
            if self.count_entities_named(new_name).await? > 0 {
                anyhow::bail!("an entity named {new_name:?} already exists");
            }
            let res = self
                .write(
                    batch::write_batch()
                        .var_as(
                            "n",
                            g().n_with_label(L_ENTITY)
                                .where_(Predicate::eq("name", old_name))
                                .set_property("name", new_name)
                                .value_map(Some(vec!["name"])),
                        )
                        .returning(["n"]),
                )
                .await?;
            res.get("n")
                .and_then(|v| v.as_array())
                .map(|a| a.len() as u64)
                .unwrap_or(0)
        } else {
            0
        };

        // Rewrite the free-text occurrences in turns and memories.
        let turns = self
            .rewrite_free_text(L_TURN, "text", old_name, new_name)
            .await?;
        let memories = self
            .rewrite_free_text(L_MEMORY, "content", old_name, new_name)
            .await?;

        // Persist the edits so they survive a restart and are visible to recall.
        self.flush().await?;
        Ok(RenameOutcome {
            entities,
            turns,
            memories,
        })
    }

    /// Count `Entity` nodes whose `name` exactly equals `name`.
    async fn count_entities_named(&self, name: &str) -> Result<u64> {
        let v = self
            .read(
                batch::read_batch()
                    .var_as(
                        "c",
                        g().n_with_label(L_ENTITY)
                            .where_(Predicate::eq("name", name))
                            .count(),
                    )
                    .returning(["c"]),
            )
            .await?;
        Ok(v["c"].as_u64().unwrap_or(0))
    }

    /// Replace every whole-word occurrence of `old` with `new` in the string
    /// property `prop` of `label` nodes. `Predicate::contains` narrows the write to
    /// candidate nodes; [`replace_whole_word`] then decides the precise, boundary-
    /// aware edit (so a substring inside a larger token is never touched). Returns
    /// the number of nodes actually changed.
    async fn rewrite_free_text(
        &self,
        label: &str,
        prop: &str,
        old: &str,
        new: &str,
    ) -> Result<u64> {
        let found = self
            .read(
                batch::read_batch()
                    .var_as(
                        "n",
                        g().n_with_label(label)
                            .where_(Predicate::contains(prop, old))
                            .value_map(None::<Vec<String>>),
                    )
                    .returning(["n"]),
            )
            .await?;
        let items = found
            .get("n")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut changed = 0u64;
        for item in items {
            let (Some(id), Some(text)) = (
                item.get("$id").and_then(|v| v.as_u64()),
                item.get(prop).and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            if let Some(updated) = replace_whole_word(text, old, new) {
                self.write(
                    batch::write_batch()
                        .var_as("_u", g().n(NodeRef::id(id)).set_property(prop, updated))
                        .returning(["_u"]),
                )
                .await?;
                changed += 1;
            }
        }
        Ok(changed)
    }

    /// Delete every `Turn` node whose **reply** reads as a non-answer, as judged by the
    /// caller-supplied `reply_is_non_answer` predicate (the ingester's `is_non_answer`).
    ///
    /// A `Turn`'s `text` is the stored `"{transcript}\n{reply}"`; we classify only the
    /// reply half, so a user *asking* "I don't know where the school is" is never mistaken
    /// for the assistant failing to answer. A turn with no reply (no newline) is kept.
    ///
    /// This is the one-time cleanup for graphs built before the ingester's ingestion gate
    /// landed: a stored "I couldn't find that" turn otherwise resurfaces under "What you
    /// remember about this person" and few-shot-primes the model to repeat the failure.
    /// Dropping a `Turn` also removes its incident edges (`SAID`/`MENTIONS`/`KNOWS`/
    /// `FOLLOWS`) and its vector-index entry; durable `Memory` facts are never touched.
    /// Returns the number of turns removed.
    pub async fn purge_non_answer_turns(
        &self,
        reply_is_non_answer: impl Fn(&str) -> bool,
    ) -> Result<u64> {
        // Full value_map (not a projection) so each item carries its `$id`; we need the
        // `text` to classify. The large `embedding` comes along but is simply ignored.
        let found = self
            .read(
                batch::read_batch()
                    .var_as("t", g().n_with_label(L_TURN).value_map(None::<Vec<String>>))
                    .returning(["t"]),
            )
            .await?;
        let items = found
            .get("t")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut removed = 0u64;
        for item in items {
            let (Some(id), Some(text)) = (
                item.get("$id").and_then(|v| v.as_u64()),
                item.get("text").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            // Classify only the reply half (see `turn_reply_half`). No reply → keep.
            let Some(reply) = turn_reply_half(text) else {
                continue;
            };
            if reply_is_non_answer(reply) {
                self.write(
                    batch::write_batch()
                        .var_as("_d", g().n(NodeRef::id(id)).drop())
                        .returning(["_d"]),
                )
                .await?;
                removed += 1;
            }
        }
        if removed > 0 {
            self.flush().await?;
        }
        Ok(removed)
    }

    // ---- reads ------------------------------------------------------------

    /// GraphRAG recall for a query embedding. Combines:
    /// 1. nearest `Turn` texts (vector KNN),
    /// 2. nearest `Memory` contents (vector KNN),
    /// 3. graph-expanded memories: nearest turns → their entities → memories
    ///    `ABOUT` those entities.
    ///
    /// Deduplicated, capped to `max_items`, most-relevant-first-ish.
    pub async fn recall(
        &self,
        query: Vec<f32>,
        speaker_id: Option<&str>,
        k: usize,
        max_items: usize,
    ) -> Result<Vec<String>> {
        self.check_dims(&query)?;
        let mut out: Vec<String> = Vec::new();

        // 1. nearest turns (scoped to this speaker + shared)
        let turns = self
            .read(
                batch::read_batch()
                    .var_as(
                        "t",
                        g().vector_search_nodes(L_TURN, "embedding", query.clone(), k, None)
                            .value_map(Some(vec!["text", "speaker_id"])),
                    )
                    .returning(["t"]),
            )
            .await?;
        push_scoped(&mut out, &turns, "t", "text", speaker_id);

        // 2. nearest memories (direct vector, scoped)
        let mems = self
            .read(
                batch::read_batch()
                    .var_as(
                        "m",
                        g().vector_search_nodes(L_MEMORY, "embedding", query.clone(), k, None)
                            .value_map(Some(vec!["content", "speaker_id"])),
                    )
                    .returning(["m"]),
            )
            .await?;
        push_scoped(&mut out, &mems, "m", "content", speaker_id);

        // 3. graph hop: nearest turns → MENTIONS → Entity → (ABOUT, incoming) → Memory
        let expanded = self
            .read(
                batch::read_batch()
                    .var_as(
                        "e",
                        g().vector_search_nodes(L_TURN, "embedding", query, k, None)
                            .out(Some(E_MENTIONS))
                            .in_(Some(E_ABOUT))
                            .value_map(Some(vec!["content", "speaker_id"])),
                    )
                    .returning(["e"]),
            )
            .await?;
        push_scoped(&mut out, &expanded, "e", "content", speaker_id);

        // dedupe (stable, keep first occurrence) and cap
        let mut seen = std::collections::HashSet::new();
        out.retain(|s| !s.trim().is_empty() && seen.insert(s.clone()));
        out.truncate(max_items);
        Ok(out)
    }

    /// Total node count (tests/logging).
    pub async fn node_count(&self) -> Result<u64> {
        let v = self
            .read(
                batch::read_batch()
                    .var_as("c", g().n(NodeRef::all()).count())
                    .returning(["c"]),
            )
            .await?;
        Ok(v["c"].as_u64().unwrap_or(0))
    }

    /// Count nodes carrying a given label.
    async fn label_count(&self, label: &str) -> Result<u64> {
        let v = self
            .read(
                batch::read_batch()
                    .var_as("c", g().n_with_label(label).count())
                    .returning(["c"]),
            )
            .await?;
        Ok(v["c"].as_u64().unwrap_or(0))
    }

    /// Up to `limit` nodes of `label` as an array of objects (each with its `$id`
    /// and properties). The large `embedding` vector is stripped so it is never
    /// dumped to the debug page. A projected `value_map` would omit `$id`, so we
    /// fetch the full map and drop just `embedding`.
    async fn sample_label(&self, label: &str, limit: usize) -> Result<Value> {
        let v = self
            .read(
                batch::read_batch()
                    .var_as(
                        "n",
                        g().n_with_label(label)
                            .limit(limit)
                            .value_map(None::<Vec<String>>),
                    )
                    .returning(["n"]),
            )
            .await?;
        let mut arr = v
            .get("n")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        if let Some(items) = arr.as_array_mut() {
            for item in items.iter_mut() {
                if let Some(obj) = item.as_object_mut() {
                    obj.remove("embedding");
                }
            }
        }
        Ok(arr)
    }

    /// Browse stored `Turn` nodes for the device Memory screen, newest first.
    ///
    /// Filters (all optional, AND-combined):
    /// - `query` — case-insensitive substring match on the turn `text`
    ///   (`"{transcript}\n{reply}"`).
    /// - `speaker_id` — keep only turns said by this speaker (`spk-…`); a turn with no
    ///   `speaker_id` is treated as `household`.
    /// - `entity` — keep only turns that `MENTIONS` an `Entity` with this exact `name`
    ///   (the graph hop `Entity ← MENTIONS ← Turn`).
    ///
    /// Each returned turn carries its `$id`, `ext_id`, `session`, `ts`, the split
    /// `transcript`/`reply`, `speaker_id`, and its mentioned `entities` (`[{name,kind}]`).
    /// Sorted by `ts` descending, then `offset`/`limit` paginated. Entities are fetched
    /// only for the returned page (bounded by `limit`), not for every candidate.
    pub async fn list_turns(
        &self,
        query: Option<&str>,
        speaker_id: Option<&str>,
        entity: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Value> {
        // Candidate set: either all Turn nodes, or just those mentioning `entity`.
        let found = match entity.filter(|s| !s.is_empty()) {
            Some(ent) => {
                self.read(
                    batch::read_batch()
                        .var_as(
                            "t",
                            g().n_with_label(L_ENTITY)
                                .where_(Predicate::eq("name", ent))
                                .in_(Some(E_MENTIONS))
                                .value_map(None::<Vec<String>>),
                        )
                        .returning(["t"]),
                )
                .await?
            }
            None => {
                self.read(
                    batch::read_batch()
                        .var_as("t", g().n_with_label(L_TURN).value_map(None::<Vec<String>>))
                        .returning(["t"]),
                )
                .await?
            }
        };
        let items = found
            .get("t")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let q = query.filter(|s| !s.is_empty()).map(|s| s.to_lowercase());
        let want_speaker = speaker_id.filter(|s| !s.is_empty());

        // First pass: filter + capture (ts, id, base fields) — no per-turn entity reads yet.
        let mut matched: Vec<(i64, u64, Value)> = Vec::new();
        for item in items {
            let Some(id) = item.get("$id").and_then(|v| v.as_u64()) else {
                continue;
            };
            let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let sid = item
                .get("speaker_id")
                .and_then(|v| v.as_str())
                .unwrap_or(HOUSEHOLD);
            if let Some(want) = want_speaker {
                if sid != want {
                    continue;
                }
            }
            if let Some(ref needle) = q {
                if !text.to_lowercase().contains(needle.as_str()) {
                    continue;
                }
            }
            let ts = item.get("ts").and_then(|v| v.as_i64()).unwrap_or(0);
            let (transcript, reply) = split_turn_text(text);
            let base = serde_json::json!({
                "id": id,
                "ext_id": item.get("ext_id").and_then(|v| v.as_str()).unwrap_or(""),
                "session": item.get("session").and_then(|v| v.as_str()).unwrap_or(""),
                "ts": ts,
                "transcript": transcript,
                "reply": reply,
                "speaker_id": sid,
            });
            matched.push((ts, id, base));
        }

        // Newest first, then paginate, then fetch entities only for the page.
        matched.sort_by(|a, b| b.0.cmp(&a.0));
        let mut turns: Vec<Value> = Vec::new();
        for (_ts, id, mut base) in matched.into_iter().skip(offset).take(limit) {
            let entities = self.turn_entities(id).await?;
            if let Some(obj) = base.as_object_mut() {
                obj.insert("entities".to_string(), entities);
            }
            turns.push(base);
        }
        Ok(serde_json::json!({ "turns": turns }))
    }

    /// The entities a given `Turn` mentions, as `[{name,kind}]` (the `Turn → MENTIONS →
    /// Entity` hop). Used to decorate each row in [`Self::list_turns`].
    async fn turn_entities(&self, turn_id: u64) -> Result<Value> {
        let v = self
            .read(
                batch::read_batch()
                    .var_as(
                        "e",
                        g().n(NodeRef::id(turn_id))
                            .out(Some(E_MENTIONS))
                            .value_map(Some(vec!["name", "kind"])),
                    )
                    .returning(["e"]),
            )
            .await?;
        let items = v
            .get("e")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let out: Vec<Value> = items
            .into_iter()
            .filter_map(|item| {
                let name = item.get("name").and_then(|v| v.as_str())?;
                let kind = item.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                Some(serde_json::json!({ "name": name, "kind": kind }))
            })
            .collect();
        Ok(Value::Array(out))
    }

    /// Browse stored `Entity` nodes ("keywords"/people/places/topics) for the device
    /// Memory screen, as `{ "entities": [ {id,name,kind}, ... ] }`. `query` is an optional
    /// case-insensitive substring match on `name`; results are sorted by `name` and capped
    /// to `limit`.
    pub async fn list_entities(&self, query: Option<&str>, limit: usize) -> Result<Value> {
        let found = self
            .read(
                batch::read_batch()
                    .var_as(
                        "e",
                        g().n_with_label(L_ENTITY).value_map(None::<Vec<String>>),
                    )
                    .returning(["e"]),
            )
            .await?;
        let items = found
            .get("e")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let q = query.filter(|s| !s.is_empty()).map(|s| s.to_lowercase());
        let mut entities: Vec<(String, Value)> = Vec::new();
        for item in items {
            let Some(id) = item.get("$id").and_then(|v| v.as_u64()) else {
                continue;
            };
            let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(ref needle) = q {
                if !name.to_lowercase().contains(needle.as_str()) {
                    continue;
                }
            }
            let kind = item.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            entities.push((
                name.to_string(),
                serde_json::json!({ "id": id, "name": name, "kind": kind }),
            ));
        }
        entities.sort_by(|a, b| a.0.cmp(&b.0));
        let out: Vec<Value> = entities.into_iter().take(limit).map(|(_, v)| v).collect();
        Ok(serde_json::json!({ "entities": out }))
    }

    /// Drop a single node by its `$id` (a `Turn`, `Entity`, or `Memory` chosen in the
    /// device Memory screen). Dropping a node also removes its incident edges and its
    /// vector-index entry — the same write `purge_non_answer_turns` performs per turn.
    ///
    /// This is a **Helix-index-only** delete: the durable SQLite facts and the chat-log
    /// JSONL (the source of truth) are untouched, so a full index rebuild / embedding-
    /// backend switch can re-import a deleted `Turn` from the chat log. Returns `true`
    /// once the drop + flush succeed (idempotent — dropping a missing id is a no-op).
    pub async fn delete_node(&self, id: u64) -> Result<bool> {
        self.write(
            batch::write_batch()
                .var_as("_d", g().n(NodeRef::id(id)).drop())
                .returning(["_d"]),
        )
        .await?;
        self.flush().await?;
        Ok(true)
    }

    /// Close the store, flushing durable state.
    pub async fn close(self) -> Result<()> {
        self.db.close().await.context("closing embedded HelixDB")
    }

    // ---- helpers ----------------------------------------------------------

    async fn write(&self, b: batch::WriteBatch) -> Result<Value> {
        self.db
            .query(QueryRequest::write(b))
            .await
            .context("HelixDB write query failed")
    }

    async fn read(&self, b: batch::ReadBatch) -> Result<Value> {
        self.db
            .query(QueryRequest::read(b))
            .await
            .context("HelixDB read query failed")
    }

    /// Find a node id by its `ext_id` property, if present.
    async fn find_by_ext_id(&self, label: &str, ext_id: &str) -> Result<Option<u64>> {
        let v = self
            .read(
                batch::read_batch()
                    .var_as(
                        "n",
                        g().n_with_label(label)
                            .where_(Predicate::eq("ext_id", ext_id))
                            .value_map(None::<Vec<String>>),
                    )
                    .returning(["n"]),
            )
            .await?;
        Ok(first_id(&v, "n"))
    }

    /// Get-or-create an entity node by name (carrying its `kind`), returning its id.
    async fn ensure_entity(&self, e: &Entity) -> Result<u64> {
        self.ensure_node(L_ENTITY, "name", &e.name, &[("kind", &e.kind)])
            .await
    }

    /// Get-or-create a `User` node keyed by `speaker_id` (`spk-…`, or `household`
    /// for the shared user), tagging its display `name` when provided. The name is
    /// set on creation; a later rename updates the SQLite registry (the source of
    /// truth for the spoken reply), so the graph property is best-effort.
    async fn ensure_user(&self, speaker_id: &str, name: Option<&str>) -> Result<u64> {
        let extra: Vec<(&str, &str)> = match name {
            Some(n) if !n.is_empty() => vec![("name", n)],
            _ => Vec::new(),
        };
        self.ensure_node(L_USER, "speaker_id", speaker_id, &extra)
            .await
    }

    /// Get-or-create a node identified by `(key_prop == key_val)` under `label`,
    /// setting `extra` properties only when creating. Returns the node id.
    async fn ensure_node(
        &self,
        label: &str,
        key_prop: &str,
        key_val: &str,
        extra: &[(&str, &str)],
    ) -> Result<u64> {
        let found = self
            .read(
                batch::read_batch()
                    .var_as(
                        "n",
                        g().n_with_label(label)
                            .where_(Predicate::eq(key_prop, key_val))
                            .value_map(None::<Vec<String>>),
                    )
                    .returning(["n"]),
            )
            .await?;
        if let Some(id) = first_id(&found, "n") {
            return Ok(id);
        }
        let mut props: Vec<(&str, PropertyInput)> = vec![(key_prop, PropertyInput::from(key_val))];
        for (k, val) in extra {
            props.push((k, PropertyInput::from(*val)));
        }
        let res = self
            .write(
                batch::write_batch()
                    .var_as("n", g().add_n(label, props))
                    .returning(["n"]),
            )
            .await?;
        first_id(&res, "n").with_context(|| format!("ensure_node: no id creating {label}"))
    }

    fn check_dims(&self, v: &[f32]) -> Result<()> {
        anyhow::ensure!(
            v.len() == self.dims,
            "embedding dimensionality {} != index dimensionality {}",
            v.len(),
            self.dims
        );
        Ok(())
    }
}

/// Read-only introspection for the debug GUI (`webconfig.rs` → `/helix`).
#[async_trait::async_trait]
impl super::graphview::GraphView for HelixMemory {
    async fn stats(&self) -> Result<Value> {
        let mut by_label = serde_json::Map::new();
        for label in [L_USER, L_TURN, L_MEMORY, L_ENTITY] {
            by_label.insert(
                label.to_string(),
                Value::from(self.label_count(label).await?),
            );
        }
        Ok(serde_json::json!({
            "total": self.node_count().await?,
            "by_label": Value::Object(by_label),
        }))
    }

    async fn sample(&self, limit: usize) -> Result<Value> {
        let mut out = serde_json::Map::new();
        for label in [L_USER, L_TURN, L_MEMORY, L_ENTITY] {
            out.insert(label.to_string(), self.sample_label(label, limit).await?);
        }
        Ok(Value::Object(out))
    }

    async fn rename_entity(&self, old_name: &str, new_name: &str) -> Result<Value> {
        let outcome = HelixMemory::rename_entity(self, old_name, new_name).await?;
        Ok(serde_json::json!({
            "entities": outcome.entities,
            "turns": outcome.turns,
            "memories": outcome.memories,
            "total": outcome.total(),
        }))
    }

    async fn list_turns(
        &self,
        query: Option<&str>,
        speaker_id: Option<&str>,
        entity: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Value> {
        HelixMemory::list_turns(self, query, speaker_id, entity, limit, offset).await
    }

    async fn list_entities(&self, query: Option<&str>, limit: usize) -> Result<Value> {
        HelixMemory::list_entities(self, query, limit).await
    }

    async fn delete_node(&self, id: u64) -> Result<bool> {
        HelixMemory::delete_node(self, id).await
    }
}

/// Empty edge-property list with the concrete type the builder needs.
fn no_props() -> Vec<(String, PropertyInput)> {
    Vec::new()
}

/// Replace every **whole-word** occurrence of `from` with `to` in `text`,
/// case-sensitively. An occurrence counts only when both sides are word
/// boundaries — the start/end of the string or a non-alphanumeric char — so
/// correcting `"Portlnad"` never mangles `"Portlnadia"`, and `"Sam"` never
/// touches `"Samsung"`. Returns `Some(new_text)` if anything changed, else `None`
/// (so the caller can skip a no-op write). An empty `from` never matches.
fn replace_whole_word(text: &str, from: &str, to: &str) -> Option<String> {
    if from.is_empty() {
        return None;
    }
    let is_word = |c: char| c.is_alphanumeric();
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    let mut rest = text;
    // The source char immediately preceding `rest` (needed when a match sits at the
    // very start of `rest`, i.e. right after the previous match we advanced past).
    let mut prev_char: Option<char> = None;
    while let Some(pos) = rest.find(from) {
        let before_char = if pos == 0 {
            prev_char
        } else {
            rest[..pos].chars().next_back()
        };
        let before_ok = before_char.is_none_or(|c| !is_word(c));
        let after_idx = pos + from.len();
        let after_ok = rest[after_idx..].chars().next().is_none_or(|c| !is_word(c));
        out.push_str(&rest[..pos]);
        if before_ok && after_ok {
            out.push_str(to);
            changed = true;
        } else {
            out.push_str(from);
        }
        // For the next iteration's boundary test, the relevant source char is the
        // last char of the matched `from` (whether or not we replaced it).
        prev_char = from.chars().next_back();
        rest = &rest[after_idx..];
    }
    out.push_str(rest);
    changed.then_some(out)
}

/// The reply half of a stored `Turn.text` (`"{transcript}\n{reply}"`, as built by the
/// ingester's `turn_text`). The transcript is a single STT utterance (no newline), so the
/// first newline is the boundary and everything after it — even a multi-line reply — is the
/// reply. Returns `None` when there is no newline (the reply was empty at ingest), so such a
/// turn is never judged a non-answer. This keeps `purge_non_answer_turns` classifying only
/// what the *assistant* said, never a user transcript that happens to contain "I don't know".
fn turn_reply_half(text: &str) -> Option<&str> {
    text.split_once('\n').map(|(_transcript, reply)| reply)
}

/// Split a stored `Turn.text` (`"{transcript}\n{reply}"`) into its `(transcript, reply)`
/// halves on the first newline. With no newline the whole string is the transcript and
/// the reply is empty. Used to present the two halves separately on the device Memory
/// screen (the ingester joins them for embedding; see `turn_text`).
fn split_turn_text(text: &str) -> (&str, &str) {
    match text.split_once('\n') {
        Some((transcript, reply)) => (transcript, reply),
        None => (text, ""),
    }
}

/// Extract the `$id` of the first element of `value[var]`, if any.
fn first_id(value: &Value, var: &str) -> Option<u64> {
    value.get(var)?.as_array()?.first()?.get("$id")?.as_u64()
}

/// Append `prop` from every element of `value[var]` to `out`, keeping only nodes
/// in scope for `want`: a specific speaker sees their own nodes plus shared
/// (`household`) ones; `None` (household scope) sees only shared. A node missing a
/// `speaker_id` property is treated as shared (covers pre-Phase-D nodes).
fn push_scoped(out: &mut Vec<String>, value: &Value, var: &str, prop: &str, want: Option<&str>) {
    if let Some(arr) = value.get(var).and_then(|v| v.as_array()) {
        for item in arr {
            let sid = item
                .get("speaker_id")
                .and_then(|v| v.as_str())
                .unwrap_or(HOUSEHOLD);
            let in_scope = match want {
                Some(w) => sid == w || sid == HOUSEHOLD,
                None => sid == HOUSEHOLD,
            };
            if in_scope {
                if let Some(s) = item.get(prop).and_then(|v| v.as_str()) {
                    out.push(s.to_string());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{replace_whole_word, turn_reply_half};

    #[test]
    fn turn_reply_half_classifies_only_the_assistant_reply() {
        // Splits on the first newline (the transcript/reply boundary).
        assert_eq!(
            turn_reply_half("where is the library?\nIt's on Main Street."),
            Some("It's on Main Street.")
        );
        // A *user* "I don't know" stays in the transcript, out of the reply half — so
        // purge, which classifies the returned reply, never deletes this turn.
        assert_eq!(
            turn_reply_half("I don't know the capital of France, what is it?\nIt's Paris."),
            Some("It's Paris.")
        );
        // A multi-line reply is returned whole (everything after the first newline).
        assert_eq!(
            turn_reply_half("directions?\nHead north.\nThen turn left."),
            Some("Head north.\nThen turn left.")
        );
        // No newline → reply was empty at ingest → never a non-answer.
        assert_eq!(turn_reply_half("just a transcript"), None);
    }

    #[test]
    fn corrects_a_standalone_token_only() {
        assert_eq!(
            replace_whole_word("I live in Portlnad now", "Portlnad", "Portland").as_deref(),
            Some("I live in Portland now")
        );
        // Leading/trailing position and punctuation boundaries both count.
        assert_eq!(
            replace_whole_word("Portlnad.", "Portlnad", "Portland").as_deref(),
            Some("Portland.")
        );
    }

    #[test]
    fn leaves_substrings_inside_larger_words_alone() {
        assert_eq!(
            replace_whole_word("Portlnadia", "Portlnad", "Portland"),
            None
        );
        assert_eq!(
            replace_whole_word("aPortlnad", "Portlnad", "Portland"),
            None
        );
    }

    #[test]
    fn handles_repeats_and_adjacent_word_chars() {
        // Two whole-word hits replaced; the "Sam" inside "Samsung" is untouched.
        assert_eq!(
            replace_whole_word("Sam, Sam and Samsung", "Sam", "Sameer").as_deref(),
            Some("Sameer, Sameer and Samsung")
        );
        // Adjacent word chars on the inner side block both (no whole word here).
        assert_eq!(replace_whole_word("SamSam", "Sam", "Sameer"), None);
    }

    #[test]
    fn no_match_and_empty_needle_yield_none() {
        assert_eq!(
            replace_whole_word("nothing here", "Portlnad", "Portland"),
            None
        );
        assert_eq!(replace_whole_word("anything", "", "x"), None);
    }

    #[test]
    fn split_turn_text_separates_transcript_and_reply() {
        use super::split_turn_text;
        assert_eq!(
            split_turn_text("where is the library?\nIt's on Main Street."),
            ("where is the library?", "It's on Main Street.")
        );
        // Multi-line reply: everything after the first newline is the reply.
        assert_eq!(
            split_turn_text("directions?\nHead north.\nThen left."),
            ("directions?", "Head north.\nThen left.")
        );
        // No newline → all transcript, empty reply.
        assert_eq!(split_turn_text("just a transcript"), ("just a transcript", ""));
    }

    use super::HelixMemory;
    use crate::memory::entity::Entity;

    fn ent(name: &str, kind: &str) -> Entity {
        Entity {
            name: name.to_string(),
            kind: kind.to_string(),
        }
    }

    /// Browse/filter/delete the graph end-to-end against an in-memory store.
    ///
    /// Run on a generous-stack thread with its own current-thread runtime: the
    /// embedded HelixDB engine's in-memory query execution recurses deeply and
    /// overflows the ~2 MB default test-thread stack.
    #[test]
    fn list_turns_entities_and_delete_round_trip() {
        std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(list_turns_entities_and_delete_round_trip_body());
            })
            .unwrap()
            .join()
            .unwrap();
    }

    async fn list_turns_entities_and_delete_round_trip_body() {
        let mem = HelixMemory::open_in_memory("test", 3).await.unwrap();
        let emb = vec![0.1f32, 0.2, 0.3];

        let t1 = mem
            .ingest_turn(
                "r1",
                "sess",
                100,
                "where is the library?\nIt's on Main Street.",
                emb.clone(),
                &[ent("library", "place")],
                "household",
                None,
            )
            .await
            .unwrap();
        let _t2 = mem
            .ingest_turn(
                "r2",
                "sess",
                200,
                "what time is it?\nIt's 3pm.",
                emb.clone(),
                &[],
                "spk-dana",
                Some("Dana"),
            )
            .await
            .unwrap();

        // All turns, newest first (t2 ts=200 before t1 ts=100).
        let turns = mem.list_turns(None, None, None, 50, 0).await.unwrap();
        let arr = turns["turns"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["ts"].as_i64(), Some(200));
        assert_eq!(arr[0]["transcript"], "what time is it?");
        assert_eq!(arr[0]["reply"], "It's 3pm.");

        // The first turn carries its mentioned entity.
        let lib = arr
            .iter()
            .find(|t| t["ext_id"] == "r1")
            .expect("r1 present");
        assert_eq!(lib["entities"][0]["name"], "library");
        assert_eq!(lib["entities"][0]["kind"], "place");

        // query filter (case-insensitive substring over the text).
        let q = mem.list_turns(Some("LIBRARY"), None, None, 50, 0).await.unwrap();
        assert_eq!(q["turns"].as_array().unwrap().len(), 1);
        assert_eq!(q["turns"][0]["ext_id"], "r1");

        // speaker filter.
        let s = mem.list_turns(None, Some("spk-dana"), None, 50, 0).await.unwrap();
        assert_eq!(s["turns"].as_array().unwrap().len(), 1);
        assert_eq!(s["turns"][0]["ext_id"], "r2");

        // entity filter (turns mentioning "library").
        let e = mem.list_turns(None, None, Some("library"), 50, 0).await.unwrap();
        assert_eq!(e["turns"].as_array().unwrap().len(), 1);
        assert_eq!(e["turns"][0]["ext_id"], "r1");

        // list_entities with and without a query filter.
        let ents = mem.list_entities(None, 50).await.unwrap();
        assert_eq!(ents["entities"].as_array().unwrap().len(), 1);
        assert_eq!(ents["entities"][0]["name"], "library");
        let ents_q = mem.list_entities(Some("zzz"), 50).await.unwrap();
        assert_eq!(ents_q["entities"].as_array().unwrap().len(), 0);

        // delete the library turn → gone from the browse list.
        assert!(mem.delete_node(t1).await.unwrap());
        let after = mem.list_turns(None, None, None, 50, 0).await.unwrap();
        let after = after["turns"].as_array().unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0]["ext_id"], "r2");
    }
}
