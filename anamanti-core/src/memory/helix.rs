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
        let db = HelixDB::open(HelixDbSource::Disk {
            root,
            database: database.to_string(),
        })
        .await
        .context("opening embedded HelixDB (disk)")?;
        Self::init(db, dims).await
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
    use super::replace_whole_word;

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
}
