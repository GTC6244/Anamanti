//! Embedded HelixDB GraphRAG memory (memory_plan.md Goals 1 & 4).
//!
//! HelixDB's engine crate (`db`) is compiled **in-process** — no server, no
//! Docker (see memory_plan.md "SPIKE RESULT"). This module owns a [`db::HelixDB`]
//! handle opened against a local on-disk object store and exposes the operations
//! the rest of the system needs:
//!
//! - **ingest** — the background ingester calls [`HelixMemory::ingest_turn`] /
//!   [`HelixMemory::ingest_memory`] to write vector-bearing nodes plus the graph
//!   edges (`SAID`, `MENTIONS`, `ABOUT`, `FOLLOWS`, `KNOWS`) that make this a
//!   graph and not just a vector store.
//! - **recall** — [`HelixMemory::recall`] does the GraphRAG read: vector KNN over
//!   `Turn` and `Memory` embeddings, then a graph hop (nearest turns → their
//!   entities → memories about those entities) to surface related context a pure
//!   vector search would miss.
//! - **speaker attribution** — [`HelixMemory::reassign_speaker`] /
//!   [`HelixMemory::delete_speaker`] keep the graph's per-person `speaker_id`s in
//!   step with a voice-cluster merge or forget.
//!
//! ## Graph model
//! Nodes: `User` (one per speaker, plus the shared household), `Turn`, `Memory`,
//! `Entity`. Every `Turn`/`Memory` carries an `embedding` vector property (indexed)
//! and a stable `ext_id` string (the chat-log record id) for idempotent upserts.
//!
//! A `Turn` stores `text` — the embedded `"{transcript}\n{reply}"` join — and, for
//! turns ingested since the two were split, separate `transcript` and `reply`
//! properties. Readers use the separate properties when present and fall back to
//! splitting `text` for older nodes.
//!
//! A `User` whose speaker was merged into another (or forgotten) is replaced by a
//! tombstone carrying `merged_into`, so a late ingest of a chat-log record still
//! tagged with the old id is attributed to the surviving person.
//!
//! Reads project only the properties they use: the `embedding` vectors never leave
//! the store except through the vector index itself.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde_json::{Map, Value};

use db::{HelixDB, HelixDbSource};
use helix_ast::expr::Predicate;
use helix_ast::index::{IndexSpec, VectorDistanceMetric};
use helix_ast::{batch, graph::NodeRef, query::QueryRequest, traversal::g, value::PropertyInput};

use super::entity::Entity;
use super::graphview::{
    EntityList, EntityRef, EntityRow, GraphSample, GraphStats, NodeId, NodeSample, RenameOutcome,
    TurnList, TurnRow,
};

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
// Property on a tombstoned `User`: the speaker id it was merged into.
const P_MERGED_INTO: &str = "merged_into";
// The node id pseudo-property in a projection.
const P_ID: &str = "$id";

/// Every property each label is written with (except the `embedding` vector). Used
/// to project node samples for the debug page without pulling embeddings.
const USER_PROPS: &[&str] = &[P_ID, "speaker_id", "name", P_MERGED_INTO];
const TURN_PROPS: &[&str] = &[
    P_ID,
    "ext_id",
    "session",
    "ts",
    "text",
    "transcript",
    "reply",
    "speaker_id",
];
const MEMORY_PROPS: &[&str] = &[P_ID, "ext_id", "kind", "content", "speaker_id"];
const ENTITY_PROPS: &[&str] = &[P_ID, "name", "kind"];

/// Over-fetch factor for recall's vector searches: the speaker scope is applied
/// after the KNN (the vector index has no speaker partition), so fetch more than
/// `k` to keep a quiet speaker's own nodes from being crowded out by others'.
const RECALL_OVERFETCH: usize = 4;
/// Upper bound on a single recall KNN fetch.
const RECALL_MAX_FETCH: usize = 128;

/// One completed turn to ingest (see [`HelixMemory::ingest_turn`]).
#[derive(Debug, Clone)]
pub struct NewTurn<'a> {
    /// The chat-log record id (idempotency key).
    pub ext_id: &'a str,
    pub session: &'a str,
    pub ts: i64,
    /// The user's transcript.
    pub transcript: &'a str,
    /// The assistant's reply (may be empty).
    pub reply: &'a str,
    /// Embedding of [`turn_text`]`(transcript, reply)`.
    pub embedding: Vec<f32>,
    pub entities: &'a [Entity],
    /// `spk-…`, or `household` when unattributed.
    pub speaker_id: &'a str,
    pub speaker_name: Option<&'a str>,
}

/// The combined turn text that is embedded, extracted from and stored as
/// `Turn.text` (Q8: transcript + reply, newline-joined; just the transcript when
/// the reply is empty).
pub fn turn_text(transcript: &str, reply: &str) -> String {
    if reply.trim().is_empty() {
        transcript.to_string()
    } else {
        format!("{transcript}\n{reply}")
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
                                    .value_map(Some(vec![P_ID])),
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

    // ---- writes -----------------------------------------------------------

    /// Ingest one completed turn: a `Turn` node (with embedding, the joined `text`
    /// and the separate `transcript`/`reply`), a `SAID` edge from the speaking user,
    /// `MENTIONS`/`KNOWS` edges to each entity, and a `FOLLOWS` edge from the
    /// previous turn in the same session. Idempotent by `ext_id` (a re-ingested
    /// record returns the existing node id). A speaker id that was merged away (see
    /// [`Self::reassign_speaker`]) is attributed to the surviving speaker.
    pub async fn ingest_turn(&self, turn: NewTurn<'_>) -> Result<u64> {
        if let Some(existing) = self.find_by_ext_id(L_TURN, turn.ext_id).await? {
            return Ok(existing);
        }
        self.check_dims(&turn.embedding)?;

        // Resolve the speaking user (get-or-create) so SAID/KNOWS originate from
        // the right person, and pre-resolve entity ids so the batch links by id.
        let (user_id, speaker_id) = self.ensure_user(turn.speaker_id, turn.speaker_name).await?;
        let mut entity_ids = Vec::with_capacity(turn.entities.len());
        for e in turn.entities {
            entity_ids.push(self.ensure_entity(e).await?);
        }
        let prev = self.last_turn.lock().unwrap().get(turn.session).copied();

        // One write batch: create the turn, then all its edges by var/id.
        let mut b = batch::write_batch().var_as(
            "t",
            g().add_n(
                L_TURN,
                vec![
                    ("ext_id", PropertyInput::from(turn.ext_id)),
                    ("session", PropertyInput::from(turn.session)),
                    ("ts", PropertyInput::from(turn.ts)),
                    (
                        "text",
                        PropertyInput::from(turn_text(turn.transcript, turn.reply)),
                    ),
                    ("transcript", PropertyInput::from(turn.transcript)),
                    ("reply", PropertyInput::from(turn.reply)),
                    ("speaker_id", PropertyInput::from(speaker_id.as_str())),
                    ("embedding", PropertyInput::from(turn.embedding)),
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
            .insert(turn.session.to_string(), id);
        Ok(id)
    }

    /// Ingest one durable memory (fact/preference): a `Memory` node (with
    /// embedding) linked via `ABOUT` to each entity. Idempotent by `ext_id`. A speaker
    /// id that was merged away is attributed to the surviving speaker.
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
        let speaker_id = self.resolve_speaker(speaker_id).await?;

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
                    ("speaker_id", PropertyInput::from(speaker_id.as_str())),
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

    // ---- speaker attribution ------------------------------------------------

    /// Re-attribute everything speaker `from` owns in the graph to speaker `to` (two
    /// voice clusters were merged): `speaker_id` on their `Turn` and `Memory` nodes is
    /// rewritten, their `SAID`/`KNOWS` edges are re-pointed to `to`'s `User` node, and
    /// `from`'s `User` node is replaced by a tombstone (`merged_into = to`) so a later
    /// ingest of a record still tagged `from` lands on `to`. Returns the number of
    /// `Turn` + `Memory` nodes re-attributed. A no-op when `from == to`.
    pub async fn reassign_speaker(&self, from: &str, to: &str) -> Result<u64> {
        // Follow any earlier merge of the target so we never point at a tombstone.
        let to = self.resolve_speaker(to).await?;
        if from == to || from == HOUSEHOLD {
            return Ok(0);
        }

        // 1. Rewrite the speaker_id property on the person's turns and memories.
        let res = self
            .write(
                batch::write_batch()
                    .var_as(
                        "t",
                        g().n_with_label(L_TURN)
                            .where_(Predicate::eq("speaker_id", from))
                            .set_property("speaker_id", to.as_str())
                            .value_map(Some(vec![P_ID])),
                    )
                    .var_as(
                        "m",
                        g().n_with_label(L_MEMORY)
                            .where_(Predicate::eq("speaker_id", from))
                            .set_property("speaker_id", to.as_str())
                            .value_map(Some(vec![P_ID])),
                    )
                    .returning(["t", "m"]),
            )
            .await?;
        let moved = array_len(&res, "t") + array_len(&res, "m");

        // 2. Re-point SAID/KNOWS from the old User node to the surviving one, then
        //    replace the old node by a tombstone.
        if let Some((from_user, from_name)) = self.find_user(from).await? {
            let (to_user, _) = self.ensure_user(&to, from_name.as_deref()).await?;
            let edges = self
                .read(
                    batch::read_batch()
                        .var_as(
                            "said",
                            g().n(NodeRef::id(from_user))
                                .out(Some(E_SAID))
                                .value_map(Some(vec![P_ID])),
                        )
                        .var_as(
                            "knows",
                            g().n(NodeRef::id(from_user))
                                .out(Some(E_KNOWS))
                                .value_map(Some(vec![P_ID])),
                        )
                        .var_as(
                            "known",
                            g().n(NodeRef::id(to_user))
                                .out(Some(E_KNOWS))
                                .value_map(Some(vec![P_ID])),
                        )
                        .returning(["said", "knows", "known"]),
                )
                .await?;
            let said = ids_of(&edges, "said");
            let already: HashSet<u64> = ids_of(&edges, "known").into_iter().collect();
            let mut seen = HashSet::new();
            let knows: Vec<u64> = ids_of(&edges, "knows")
                .into_iter()
                .filter(|id| !already.contains(id) && seen.insert(*id))
                .collect();

            let mut b = batch::write_batch();
            for (i, tid) in said.iter().enumerate() {
                b = b.var_as(
                    &format!("_s{i}"),
                    g().n(NodeRef::id(to_user))
                        .add_e(E_SAID, NodeRef::id(*tid), no_props()),
                );
            }
            for (i, eid) in knows.iter().enumerate() {
                b = b.var_as(
                    &format!("_k{i}"),
                    g().n(NodeRef::id(to_user))
                        .add_e(E_KNOWS, NodeRef::id(*eid), no_props()),
                );
            }
            // Dropping the old node removes its (now duplicated) edges.
            b = b.var_as("_d", g().n(NodeRef::id(from_user)).drop());
            let mut tomb: Vec<(&str, PropertyInput)> = vec![
                ("speaker_id", PropertyInput::from(from)),
                (P_MERGED_INTO, PropertyInput::from(to.as_str())),
            ];
            if let Some(name) = from_name.as_deref() {
                tomb.push(("name", PropertyInput::from(name)));
            }
            b = b.var_as("_tomb", g().add_n(L_USER, tomb));
            self.write(b.returning(["_tomb"])).await?;
        }

        self.flush().await?;
        Ok(moved)
    }

    /// Detach everything speaker `id` owns in the graph from that person (their
    /// voiceprint was forgotten): their turns and memories become shared household
    /// nodes, mirroring the SQLite side where their memories become unattributed.
    /// Returns the number of `Turn` + `Memory` nodes changed.
    pub async fn delete_speaker(&self, id: &str) -> Result<u64> {
        self.reassign_speaker(id, HOUSEHOLD).await
    }

    // ---- edits ------------------------------------------------------------

    /// Rename an entity — fixing a spelling mistake — everywhere it appears:
    ///
    /// 1. every `Entity` node whose `name` exactly equals `old_name` (id + all
    ///    `MENTIONS`/`ABOUT`/`KNOWS` edges preserved — only `name` is rewritten);
    /// 2. every whole-word occurrence of `old_name` in the **free text** of `Turn`
    ///    nodes (`text`, and `transcript`/`reply` where stored) and `Memory` nodes
    ///    (`content`), so the misspelling is gone from the stored transcript and
    ///    remembered facts too.
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
            array_len(&res, "n")
        } else {
            0
        };

        // Rewrite the free-text occurrences in turns and memories.
        let turns = self
            .rewrite_free_text(L_TURN, &["text", "transcript", "reply"], old_name, new_name)
            .await?;
        let memories = self
            .rewrite_free_text(L_MEMORY, &["content"], old_name, new_name)
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
    /// properties `props` of `label` nodes. `Predicate::contains` narrows the read to
    /// candidate nodes; [`replace_whole_word`] then decides the precise, boundary-
    /// aware edit (so a substring inside a larger token is never touched). Returns
    /// the number of nodes actually changed (a node counts once however many of its
    /// properties changed).
    async fn rewrite_free_text(
        &self,
        label: &str,
        props: &[&str],
        old: &str,
        new: &str,
    ) -> Result<u64> {
        let mut projection = vec![P_ID];
        projection.extend_from_slice(props);
        let found = self
            .read(
                batch::read_batch()
                    .var_as(
                        "n",
                        g().n_with_label(label)
                            .where_(Predicate::or(
                                props.iter().map(|p| Predicate::contains(*p, old)).collect(),
                            ))
                            .value_map(Some(projection)),
                    )
                    .returning(["n"]),
            )
            .await?;
        let mut changed = 0u64;
        for item in items_of(&found, "n") {
            let Some(id) = item.get(P_ID).and_then(Value::as_u64) else {
                continue;
            };
            let updates: Vec<(&str, String)> = props
                .iter()
                .filter_map(|p| {
                    let text = item.get(*p).and_then(Value::as_str)?;
                    replace_whole_word(text, old, new).map(|u| (*p, u))
                })
                .collect();
            let Some(((first_prop, first_val), rest)) = updates.split_first() else {
                continue;
            };
            let mut t = g()
                .n(NodeRef::id(id))
                .set_property(*first_prop, first_val.as_str());
            for (p, v) in rest {
                t = t.set_property(*p, v.as_str());
            }
            self.write(batch::write_batch().var_as("_u", t).returning(["_u"]))
                .await?;
            changed += 1;
        }
        Ok(changed)
    }

    /// Delete every `Turn` node whose **reply** reads as a non-answer, as judged by the
    /// caller-supplied `reply_is_non_answer` predicate (the ingester's `is_non_answer`).
    ///
    /// Only the reply is classified, so a user *asking* "I don't know where the school
    /// is" is never mistaken for the assistant failing to answer. The reply is the
    /// stored `reply` property, or — for turns stored before transcript and reply were
    /// split — the part of `text` after its first newline. A turn with no reply is kept.
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
        let found = self
            .read(
                batch::read_batch()
                    .var_as(
                        "t",
                        g().n_with_label(L_TURN)
                            .value_map(Some(vec![P_ID, "text", "reply"])),
                    )
                    .returning(["t"]),
            )
            .await?;
        let mut removed = 0u64;
        for item in items_of(&found, "t") {
            let Some(id) = item.get(P_ID).and_then(Value::as_u64) else {
                continue;
            };
            let Some(reply) = stored_reply(item) else {
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

    /// GraphRAG recall for a query embedding. Combines, in this order:
    /// 1. the `k` nearest in-scope `Turn` texts (vector KNN),
    /// 2. the `k` nearest in-scope `Memory` contents (vector KNN),
    /// 3. graph-expanded memories: those nearest turns → `MENTIONS` → entities →
    ///    in-scope memories `ABOUT` those entities.
    ///
    /// In scope means the speaker's own nodes plus shared household ones (`None` =
    /// household only). The vector index has no speaker partition, so each KNN
    /// over-fetches (`k ×` [`RECALL_OVERFETCH`], capped at [`RECALL_MAX_FETCH`]) before
    /// scoping, so a quiet speaker isn't crowded out by everyone else's turns. Both
    /// searches run in one read; the entity hop reuses the turn hits (no second KNN).
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
        let fetch = k
            .saturating_mul(RECALL_OVERFETCH)
            .min(RECALL_MAX_FETCH)
            .max(k);

        // 1 + 2. One read: both vector searches, projecting only what we use.
        let hits = self
            .read(
                batch::read_batch()
                    .var_as(
                        "t",
                        g().vector_search_nodes(L_TURN, "embedding", query.clone(), fetch, None)
                            .value_map(Some(vec![P_ID, "text", "speaker_id"])),
                    )
                    .var_as(
                        "m",
                        g().vector_search_nodes(L_MEMORY, "embedding", query, fetch, None)
                            .value_map(Some(vec!["content", "speaker_id"])),
                    )
                    .returning(["t", "m"]),
            )
            .await?;

        let mut out: Vec<String> = Vec::new();
        let mut turn_ids: Vec<u64> = Vec::new();
        for item in items_of(&hits, "t")
            .iter()
            .filter(|i| in_scope(i, speaker_id))
            .take(k)
        {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                out.push(text.to_string());
            }
            if let Some(id) = item.get(P_ID).and_then(Value::as_u64) {
                turn_ids.push(id);
            }
        }
        out.extend(
            items_of(&hits, "m")
                .iter()
                .filter(|i| in_scope(i, speaker_id))
                .take(k)
                .filter_map(|i| i.get("content").and_then(Value::as_str))
                .map(str::to_string),
        );

        // 3. Graph hop from the turns already found: → MENTIONS → Entity → (ABOUT,
        //    incoming) → Memory.
        if !turn_ids.is_empty() {
            let expanded = self
                .read(
                    batch::read_batch()
                        .var_as(
                            "e",
                            g().n(NodeRef::ids(turn_ids))
                                .out(Some(E_MENTIONS))
                                .in_(Some(E_ABOUT))
                                .value_map(Some(vec!["content", "speaker_id"])),
                        )
                        .returning(["e"]),
                )
                .await?;
            out.extend(
                items_of(&expanded, "e")
                    .iter()
                    .filter(|i| in_scope(i, speaker_id))
                    .filter_map(|i| i.get("content").and_then(Value::as_str))
                    .map(str::to_string),
            );
        }

        // dedupe (stable, keep first occurrence) and cap
        let mut seen = HashSet::new();
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

    /// Up to `limit` nodes of `label` with their `$id` and stored properties,
    /// projected to the label's known property set so the large `embedding` vector
    /// is never read (nor dumped to the debug page).
    async fn sample_label(&self, label: &str, limit: usize) -> Result<Vec<NodeSample>> {
        let props = match label {
            L_USER => USER_PROPS,
            L_TURN => TURN_PROPS,
            L_MEMORY => MEMORY_PROPS,
            _ => ENTITY_PROPS,
        };
        let v = self
            .read(
                batch::read_batch()
                    .var_as(
                        "n",
                        g().n_with_label(label)
                            .limit(limit)
                            .value_map(Some(props.to_vec())),
                    )
                    .returning(["n"]),
            )
            .await?;
        Ok(items_of(&v, "n")
            .iter()
            .filter_map(|item| {
                let mut properties: Map<String, Value> = item.as_object()?.clone();
                let id = properties.remove(P_ID)?.as_u64()?;
                Some(NodeSample {
                    id: NodeId::from_raw(id),
                    properties,
                })
            })
            .collect())
    }

    /// Browse stored `Turn` nodes for the device Memory screen, newest first.
    ///
    /// Filters (all optional, AND-combined):
    /// - `query` — case-insensitive substring match on the turn text
    ///   (`"{transcript}\n{reply}"`).
    /// - `speaker_id` — keep only turns said by this speaker (`spk-…`); a turn with no
    ///   `speaker_id` is treated as `household`.
    /// - `entity` — keep only turns that `MENTIONS` an `Entity` with this exact `name`
    ///   (the graph hop `Entity ← MENTIONS ← Turn`).
    ///
    /// Each returned turn carries its id, `ext_id`, `session`, `ts`, the
    /// `transcript`/`reply` (the stored properties, or the split `text` for older
    /// turns), `speaker_id`, and its mentioned `entities`. Sorted by `ts` descending,
    /// then `offset`/`limit` paginated. Entities are fetched in one batched read for
    /// the returned page only, not per turn and not for every candidate.
    pub async fn list_turns(
        &self,
        query: Option<&str>,
        speaker_id: Option<&str>,
        entity: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<TurnList> {
        let fields = vec![
            P_ID,
            "ext_id",
            "session",
            "ts",
            "text",
            "transcript",
            "reply",
            "speaker_id",
        ];
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
                                .value_map(Some(fields)),
                        )
                        .returning(["t"]),
                )
                .await?
            }
            None => {
                self.read(
                    batch::read_batch()
                        .var_as("t", g().n_with_label(L_TURN).value_map(Some(fields)))
                        .returning(["t"]),
                )
                .await?
            }
        };

        let q = query.filter(|s| !s.is_empty()).map(|s| s.to_lowercase());
        let want_speaker = speaker_id.filter(|s| !s.is_empty());

        // First pass: filter + capture the base rows — no entity reads yet.
        let mut matched: Vec<TurnRow> = Vec::new();
        for item in items_of(&found, "t") {
            let Some(id) = item.get(P_ID).and_then(Value::as_u64) else {
                continue;
            };
            let text = item.get("text").and_then(Value::as_str).unwrap_or("");
            let sid = item
                .get("speaker_id")
                .and_then(Value::as_str)
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
            let (transcript, reply) = turn_parts(item);
            matched.push(TurnRow {
                id: NodeId::from_raw(id),
                ext_id: str_prop(item, "ext_id"),
                session: str_prop(item, "session"),
                ts: item.get("ts").and_then(Value::as_i64).unwrap_or(0),
                transcript: transcript.to_string(),
                reply: reply.to_string(),
                speaker_id: sid.to_string(),
                entities: Vec::new(),
            });
        }

        // Newest first, then paginate, then decorate the page with its entities.
        matched.sort_by(|a, b| b.ts.cmp(&a.ts));
        let mut turns: Vec<TurnRow> = matched.into_iter().skip(offset).take(limit).collect();
        let page_ids: Vec<u64> = turns.iter().map(|t| t.id.raw()).collect();
        let mut by_turn = self.entities_of_turns(&page_ids).await?;
        for t in &mut turns {
            t.entities = by_turn.remove(&t.id.raw()).unwrap_or_default();
        }
        Ok(TurnList { turns })
    }

    /// The entities each of `turn_ids` mentions (the `Turn → MENTIONS → Entity` hop),
    /// keyed by turn id, in edge order — one read for the whole set: the `MENTIONS`
    /// edge endpoints plus the distinct entity nodes they reach.
    async fn entities_of_turns(&self, turn_ids: &[u64]) -> Result<HashMap<u64, Vec<EntityRef>>> {
        let mut out: HashMap<u64, Vec<EntityRef>> = HashMap::new();
        if turn_ids.is_empty() {
            return Ok(out);
        }
        let v = self
            .read(
                batch::read_batch()
                    .var_as(
                        "edges",
                        g().n(NodeRef::ids(turn_ids.iter().copied()))
                            .out_e(Some(E_MENTIONS))
                            .edge_properties(),
                    )
                    .var_as(
                        "ents",
                        g().n(NodeRef::ids(turn_ids.iter().copied()))
                            .out(Some(E_MENTIONS))
                            .dedup()
                            .value_map(Some(ENTITY_PROPS.to_vec())),
                    )
                    .returning(["edges", "ents"]),
            )
            .await?;
        let entities: HashMap<u64, EntityRef> = items_of(&v, "ents")
            .iter()
            .filter_map(|item| {
                let id = item.get(P_ID).and_then(Value::as_u64)?;
                let name = item.get("name").and_then(Value::as_str)?;
                Some((
                    id,
                    EntityRef {
                        name: name.to_string(),
                        kind: str_prop(item, "kind"),
                    },
                ))
            })
            .collect();
        for edge in items_of(&v, "edges") {
            let (Some(from), Some(to)) = (
                edge.get("$from").and_then(Value::as_u64),
                edge.get("$to").and_then(Value::as_u64),
            ) else {
                continue;
            };
            if let Some(e) = entities.get(&to) {
                out.entry(from).or_default().push(e.clone());
            }
        }
        Ok(out)
    }

    /// Browse stored `Entity` nodes ("keywords"/people/places/topics) for the device
    /// Memory screen. `query` is an optional case-insensitive substring match on
    /// `name`; results are sorted by `name` and capped to `limit`.
    pub async fn list_entities(&self, query: Option<&str>, limit: usize) -> Result<EntityList> {
        let found = self
            .read(
                batch::read_batch()
                    .var_as(
                        "e",
                        g().n_with_label(L_ENTITY)
                            .value_map(Some(ENTITY_PROPS.to_vec())),
                    )
                    .returning(["e"]),
            )
            .await?;
        let q = query.filter(|s| !s.is_empty()).map(|s| s.to_lowercase());
        let mut entities: Vec<EntityRow> = Vec::new();
        for item in items_of(&found, "e") {
            let Some(id) = item.get(P_ID).and_then(Value::as_u64) else {
                continue;
            };
            let name = str_prop(item, "name");
            if let Some(ref needle) = q {
                if !name.to_lowercase().contains(needle.as_str()) {
                    continue;
                }
            }
            entities.push(EntityRow {
                id: NodeId::from_raw(id),
                name,
                kind: str_prop(item, "kind"),
            });
        }
        entities.sort_by(|a, b| a.name.cmp(&b.name));
        entities.truncate(limit);
        Ok(EntityList { entities })
    }

    /// Drop a single node (a `Turn`, `Entity`, or `Memory` chosen in the device
    /// Memory screen). Dropping a node also removes its incident edges and its
    /// vector-index entry — the same write `purge_non_answer_turns` performs per turn.
    ///
    /// This is a **Helix-index-only** delete: the durable SQLite facts and the chat-log
    /// JSONL (the source of truth) are untouched, so a full index rebuild / embedding-
    /// backend switch can re-import a deleted `Turn` from the chat log. Returns `true`
    /// once the drop + flush succeed (idempotent — dropping a missing id is a no-op).
    pub async fn delete_node(&self, id: NodeId) -> Result<bool> {
        self.write(
            batch::write_batch()
                .var_as("_d", g().n(NodeRef::id(id.raw())).drop())
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
                            .value_map(Some(vec![P_ID])),
                    )
                    .returning(["n"]),
            )
            .await?;
        Ok(first_id(&v, "n"))
    }

    /// Get-or-create an entity node by name (carrying its `kind`), returning its id.
    async fn ensure_entity(&self, e: &Entity) -> Result<u64> {
        if let Some(id) = self.find_node(L_ENTITY, "name", &e.name).await? {
            return Ok(id);
        }
        self.create_node(
            L_ENTITY,
            vec![
                ("name", PropertyInput::from(e.name.as_str())),
                ("kind", PropertyInput::from(e.kind.as_str())),
            ],
        )
        .await
    }

    /// The live (non-tombstone) `User` node for `speaker_id` and its display name.
    async fn find_user(&self, speaker_id: &str) -> Result<Option<(u64, Option<String>)>> {
        let v = self
            .read(
                batch::read_batch()
                    .var_as(
                        "n",
                        g().n_with_label(L_USER)
                            .where_(Predicate::eq("speaker_id", speaker_id))
                            .value_map(Some(vec![P_ID, "name", P_MERGED_INTO])),
                    )
                    .returning(["n"]),
            )
            .await?;
        Ok(items_of(&v, "n")
            .iter()
            .filter(|i| i.get(P_MERGED_INTO).is_none())
            .find_map(|i| {
                let id = i.get(P_ID).and_then(Value::as_u64)?;
                let name = i.get("name").and_then(Value::as_str).map(str::to_string);
                Some((id, name))
            }))
    }

    /// Follow `merged_into` tombstones from `speaker_id` to the surviving speaker id
    /// (bounded, so a malformed cycle can't spin). Unknown ids resolve to themselves.
    async fn resolve_speaker(&self, speaker_id: &str) -> Result<String> {
        let mut sid = speaker_id.to_string();
        for _ in 0..8 {
            let v = self
                .read(
                    batch::read_batch()
                        .var_as(
                            "n",
                            g().n_with_label(L_USER)
                                .where_(Predicate::eq("speaker_id", sid.as_str()))
                                .value_map(Some(vec![P_MERGED_INTO])),
                        )
                        .returning(["n"]),
                )
                .await?;
            let next = items_of(&v, "n")
                .iter()
                .find_map(|i| i.get(P_MERGED_INTO).and_then(Value::as_str))
                .map(str::to_string);
            match next {
                Some(target) if target != sid => sid = target,
                _ => break,
            }
        }
        Ok(sid)
    }

    /// Get-or-create the `User` node for `speaker_id` (`spk-…`, or `household` for
    /// the shared user), following any merge tombstone first. Returns the node id and
    /// the resolved speaker id. The display `name` is set on creation only; a later
    /// rename updates the SQLite registry (the source of truth for the spoken reply),
    /// so the graph property is best-effort.
    async fn ensure_user(&self, speaker_id: &str, name: Option<&str>) -> Result<(u64, String)> {
        let sid = self.resolve_speaker(speaker_id).await?;
        if let Some((id, _)) = self.find_user(&sid).await? {
            return Ok((id, sid));
        }
        let mut props = vec![("speaker_id", PropertyInput::from(sid.as_str()))];
        if let Some(n) = name.filter(|n| !n.is_empty()) {
            props.push(("name", PropertyInput::from(n)));
        }
        let id = self.create_node(L_USER, props).await?;
        Ok((id, sid))
    }

    /// The id of the first `label` node whose `key_prop == key_val`, if any.
    async fn find_node(&self, label: &str, key_prop: &str, key_val: &str) -> Result<Option<u64>> {
        let found = self
            .read(
                batch::read_batch()
                    .var_as(
                        "n",
                        g().n_with_label(label)
                            .where_(Predicate::eq(key_prop, key_val))
                            .value_map(Some(vec![P_ID])),
                    )
                    .returning(["n"]),
            )
            .await?;
        Ok(first_id(&found, "n"))
    }

    /// Create a `label` node with `props`, returning its id.
    async fn create_node(&self, label: &str, props: Vec<(&str, PropertyInput)>) -> Result<u64> {
        let res = self
            .write(
                batch::write_batch()
                    .var_as("n", g().add_n(label, props))
                    .returning(["n"]),
            )
            .await?;
        first_id(&res, "n").with_context(|| format!("no id returned creating a {label} node"))
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

    /// Write a `Turn` the way builds before the transcript/reply split did (only the
    /// joined `text`), to exercise the legacy-node fallbacks.
    #[cfg(test)]
    pub(crate) async fn ingest_legacy_turn(
        &self,
        ext_id: &str,
        ts: i64,
        text: &str,
        embedding: Vec<f32>,
        speaker_id: &str,
    ) -> Result<u64> {
        let (user_id, _) = self.ensure_user(speaker_id, None).await?;
        let b = batch::write_batch()
            .var_as(
                "t",
                g().add_n(
                    L_TURN,
                    vec![
                        ("ext_id", PropertyInput::from(ext_id)),
                        ("session", PropertyInput::from("legacy")),
                        ("ts", PropertyInput::from(ts)),
                        ("text", PropertyInput::from(text)),
                        ("speaker_id", PropertyInput::from(speaker_id)),
                        ("embedding", PropertyInput::from(embedding)),
                    ],
                ),
            )
            .var_as(
                "_said",
                g().n(NodeRef::id(user_id))
                    .add_e(E_SAID, NodeRef::var("t"), no_props()),
            );
        let res = self.write(b.returning(["t"])).await?;
        first_id(&res, "t").context("legacy turn: no id returned")
    }
}

/// Introspection + browsing for the debug GUI (`/helix`) and the device Memory screen.
#[async_trait::async_trait]
impl super::graphview::GraphView for HelixMemory {
    async fn stats(&self) -> Result<GraphStats> {
        let mut by_label = std::collections::BTreeMap::new();
        for label in [L_USER, L_TURN, L_MEMORY, L_ENTITY] {
            by_label.insert(label.to_string(), self.label_count(label).await?);
        }
        Ok(GraphStats {
            total: self.node_count().await?,
            by_label,
        })
    }

    async fn sample(&self, limit: usize) -> Result<GraphSample> {
        let mut out = std::collections::BTreeMap::new();
        for label in [L_USER, L_TURN, L_MEMORY, L_ENTITY] {
            out.insert(label.to_string(), self.sample_label(label, limit).await?);
        }
        Ok(GraphSample(out))
    }

    async fn rename_entity(&self, old_name: &str, new_name: &str) -> Result<RenameOutcome> {
        HelixMemory::rename_entity(self, old_name, new_name).await
    }

    async fn list_turns(
        &self,
        query: Option<&str>,
        speaker_id: Option<&str>,
        entity: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<TurnList> {
        HelixMemory::list_turns(self, query, speaker_id, entity, limit, offset).await
    }

    async fn list_entities(&self, query: Option<&str>, limit: usize) -> Result<EntityList> {
        HelixMemory::list_entities(self, query, limit).await
    }

    async fn delete_node(&self, id: NodeId) -> Result<bool> {
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

/// The reply half of a legacy `Turn.text` (`"{transcript}\n{reply}"`, as built by
/// [`turn_text`]). Assumes a single-line transcript, so the first newline is the
/// boundary and everything after it — even a multi-line reply — is the reply.
/// Returns `None` when there is no newline (the reply was empty at ingest), so such a
/// turn is never judged a non-answer. Only used for turns stored before `reply` was a
/// separate property.
fn turn_reply_half(text: &str) -> Option<&str> {
    text.split_once('\n').map(|(_transcript, reply)| reply)
}

/// Split a legacy `Turn.text` (`"{transcript}\n{reply}"`) into its `(transcript,
/// reply)` halves on the first newline. With no newline the whole string is the
/// transcript and the reply is empty. Only used for turns stored before the two were
/// separate properties.
fn split_turn_text(text: &str) -> (&str, &str) {
    match text.split_once('\n') {
        Some((transcript, reply)) => (transcript, reply),
        None => (text, ""),
    }
}

/// A turn node's `(transcript, reply)`: the stored properties when present, else the
/// legacy split of its `text`.
fn turn_parts(item: &Value) -> (&str, &str) {
    match (
        item.get("transcript").and_then(Value::as_str),
        item.get("reply").and_then(Value::as_str),
    ) {
        (Some(t), Some(r)) => (t, r),
        _ => split_turn_text(item.get("text").and_then(Value::as_str).unwrap_or("")),
    }
}

/// A turn node's reply for non-answer classification: the stored `reply` property
/// when present (an empty reply is still "present"), else the legacy reply half of
/// its `text` (`None` when that has no reply).
fn stored_reply(item: &Value) -> Option<&str> {
    match item.get("reply").and_then(Value::as_str) {
        Some(r) => Some(r),
        None => turn_reply_half(item.get("text").and_then(Value::as_str)?),
    }
}

/// The elements of `value[var]` (empty when absent).
fn items_of<'a>(value: &'a Value, var: &str) -> &'a [Value] {
    value
        .get(var)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// How many elements `value[var]` holds.
fn array_len(value: &Value, var: &str) -> u64 {
    items_of(value, var).len() as u64
}

/// The `$id`s of the elements of `value[var]`.
fn ids_of(value: &Value, var: &str) -> Vec<u64> {
    items_of(value, var)
        .iter()
        .filter_map(|i| i.get(P_ID).and_then(Value::as_u64))
        .collect()
}

/// A string property, or `""` when absent.
fn str_prop(item: &Value, key: &str) -> String {
    item.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Extract the `$id` of the first element of `value[var]`, if any.
fn first_id(value: &Value, var: &str) -> Option<u64> {
    items_of(value, var).first()?.get(P_ID)?.as_u64()
}

/// Whether a node is in scope for `want`: a specific speaker sees their own nodes plus
/// shared (`household`) ones; `None` (household scope) sees only shared. A node missing
/// a `speaker_id` property is treated as shared (covers pre-Phase-D nodes).
fn in_scope(item: &Value, want: Option<&str>) -> bool {
    let sid = item
        .get("speaker_id")
        .and_then(Value::as_str)
        .unwrap_or(HOUSEHOLD);
    match want {
        Some(w) => sid == w || sid == HOUSEHOLD,
        None => sid == HOUSEHOLD,
    }
}

#[cfg(test)]
mod tests {
    use super::{replace_whole_word, split_turn_text, turn_reply_half};

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
        assert_eq!(
            split_turn_text("just a transcript"),
            ("just a transcript", "")
        );
    }

    use super::{turn_text, HelixMemory, NewTurn};
    use crate::memory::entity::Entity;

    fn ent(name: &str, kind: &str) -> Entity {
        Entity {
            name: name.to_string(),
            kind: kind.to_string(),
        }
    }

    /// Run an async body on a generous-stack thread with its own current-thread
    /// runtime: the embedded HelixDB engine's in-memory query execution recurses
    /// deeply and overflows the ~2 MB default test-thread stack.
    fn on_big_stack<F: std::future::Future<Output = ()> + Send + 'static>(body: F) {
        std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(body);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[allow(clippy::too_many_arguments)]
    fn turn<'a>(
        ext_id: &'a str,
        ts: i64,
        transcript: &'a str,
        reply: &'a str,
        embedding: Vec<f32>,
        entities: &'a [Entity],
        speaker_id: &'a str,
        speaker_name: Option<&'a str>,
    ) -> NewTurn<'a> {
        NewTurn {
            ext_id,
            session: "sess",
            ts,
            transcript,
            reply,
            embedding,
            entities,
            speaker_id,
            speaker_name,
        }
    }

    /// Browse/filter/delete the graph end-to-end against an in-memory store.
    #[test]
    fn list_turns_entities_and_delete_round_trip() {
        on_big_stack(list_turns_entities_and_delete_round_trip_body());
    }

    async fn list_turns_entities_and_delete_round_trip_body() {
        let mem = HelixMemory::open_in_memory("test", 3).await.unwrap();
        let emb = vec![0.1f32, 0.2, 0.3];
        let library = [ent("library", "place"), ent("Main Street", "place")];

        let t1 = mem
            .ingest_turn(turn(
                "r1",
                100,
                "where is the library?",
                "It's on Main Street.",
                emb.clone(),
                &library,
                "household",
                None,
            ))
            .await
            .unwrap();
        let _t2 = mem
            .ingest_turn(turn(
                "r2",
                200,
                "what time is it?",
                "It's 3pm.",
                emb.clone(),
                &[],
                "spk-dana",
                Some("Dana"),
            ))
            .await
            .unwrap();

        // All turns, newest first (t2 ts=200 before t1 ts=100).
        let turns = mem.list_turns(None, None, None, 50, 0).await.unwrap().turns;
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].ts, 200);
        assert_eq!(turns[0].transcript, "what time is it?");
        assert_eq!(turns[0].reply, "It's 3pm.");
        assert!(turns[0].entities.is_empty());

        // The first turn carries its mentioned entities, in mention order.
        let lib = turns.iter().find(|t| t.ext_id == "r1").expect("r1 present");
        let names: Vec<&str> = lib.entities.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["library", "Main Street"]);
        assert_eq!(lib.entities[0].kind, "place");

        // query filter (case-insensitive substring over the text).
        let q = mem
            .list_turns(Some("LIBRARY"), None, None, 50, 0)
            .await
            .unwrap()
            .turns;
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].ext_id, "r1");

        // speaker filter.
        let s = mem
            .list_turns(None, Some("spk-dana"), None, 50, 0)
            .await
            .unwrap()
            .turns;
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].ext_id, "r2");

        // entity filter (turns mentioning "library").
        let e = mem
            .list_turns(None, None, Some("library"), 50, 0)
            .await
            .unwrap()
            .turns;
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].ext_id, "r1");
        assert_eq!(e[0].entities.len(), 2);

        // list_entities with and without a query filter (sorted by name).
        let ents = mem.list_entities(None, 50).await.unwrap().entities;
        let names: Vec<&str> = ents.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["Main Street", "library"]);
        assert!(mem
            .list_entities(Some("zzz"), 50)
            .await
            .unwrap()
            .entities
            .is_empty());

        // delete the library turn → gone from the browse list.
        assert!(mem.delete_node(super::NodeId::from_raw(t1)).await.unwrap());
        let after = mem.list_turns(None, None, None, 50, 0).await.unwrap().turns;
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].ext_id, "r2");
    }

    /// A26: new turns keep transcript and reply apart, so a transcript containing a
    /// newline is neither mis-split for display nor misclassified by the non-answer
    /// purge; legacy combined-text turns still fall back to the first-newline split.
    #[test]
    fn separate_transcript_and_reply_with_legacy_fallback() {
        on_big_stack(separate_transcript_and_reply_body());
    }

    async fn separate_transcript_and_reply_body() {
        let mem = HelixMemory::open_in_memory("split", 3).await.unwrap();
        let emb = vec![0.3f32, 0.2, 0.1];
        let is_non_answer = |r: &str| r.to_lowercase().contains("i don't know");

        // New turn: a two-line transcript whose second line *sounds* like a non-answer.
        let transcript = "dinner ideas?\nI don't know what to cook";
        mem.ingest_turn(turn(
            "new",
            200,
            transcript,
            "Try a mushroom risotto.",
            emb.clone(),
            &[],
            "household",
            None,
        ))
        .await
        .unwrap();
        // Legacy turns: combined text only.
        mem.ingest_legacy_turn("old-ok", 100, "hi there\nHello!", emb.clone(), "household")
            .await
            .unwrap();
        mem.ingest_legacy_turn(
            "old-bad",
            50,
            "who won?\nI don't know.",
            emb.clone(),
            "household",
        )
        .await
        .unwrap();

        // The purge reads the stored reply for the new turn (kept) and the legacy
        // split for old ones (the non-answer is removed).
        assert_eq!(mem.purge_non_answer_turns(is_non_answer).await.unwrap(), 1);

        let turns = mem.list_turns(None, None, None, 10, 0).await.unwrap().turns;
        let ids: Vec<&str> = turns.iter().map(|t| t.ext_id.as_str()).collect();
        assert_eq!(ids, vec!["new", "old-ok"]);
        // Display split: the stored properties for the new turn…
        assert_eq!(turns[0].transcript, transcript);
        assert_eq!(turns[0].reply, "Try a mushroom risotto.");
        // …and the first-newline split for the legacy one.
        assert_eq!(turns[1].transcript, "hi there");
        assert_eq!(turns[1].reply, "Hello!");

        // The joined text is what's embedded and recalled.
        let hits = mem.recall(emb, None, 5, 10).await.unwrap();
        assert!(hits.contains(&turn_text(transcript, "Try a mushroom risotto.")));
    }

    /// A25: a speaker with few turns still recalls their own turns when many other
    /// speakers' turns are nearer to the query (the KNN over-fetches before scoping).
    #[test]
    fn quiet_speaker_is_not_starved_by_scope_filter() {
        on_big_stack(quiet_speaker_body());
    }

    async fn quiet_speaker_body() {
        let mem = HelixMemory::open_in_memory("quiet", 3).await.unwrap();
        let query = vec![1.0f32, 0.0, 0.0];
        // Five turns from someone else, all nearer to the query than the quiet one's.
        for i in 0..5 {
            let ext = format!("loud-{i}");
            mem.ingest_turn(turn(
                &ext,
                i,
                &format!("loud turn {i}"),
                "ok",
                vec![1.0, 0.01 * i as f32, 0.0],
                &[],
                "spk-loud",
                None,
            ))
            .await
            .unwrap();
        }
        mem.ingest_turn(turn(
            "quiet-1",
            10,
            "my favourite tea is oolong",
            "Noted.",
            vec![0.6, 0.8, 0.0],
            &[],
            "spk-quiet",
            None,
        ))
        .await
        .unwrap();
        mem.flush().await.unwrap();

        // k = 2: a plain top-2 holds only the loud speaker's turns.
        let hits = mem.recall(query, Some("spk-quiet"), 2, 8).await.unwrap();
        assert!(
            hits.iter().any(|h| h.contains("oolong")),
            "the quiet speaker's own turn must survive scoping, got {hits:?}"
        );
        assert!(!hits.iter().any(|h| h.contains("loud")));
    }

    /// A8: a merge re-attributes turns and memories (so the kept speaker's scoped
    /// recall finds the dropped speaker's history), re-points SAID, and leaves a
    /// tombstone so a late ingest under the old id lands on the kept speaker. A forget
    /// moves the person's nodes to the household.
    #[test]
    fn reassign_and_delete_speaker_rewrite_graph_attribution() {
        on_big_stack(reassign_body());
    }

    async fn reassign_body() {
        let mem = HelixMemory::open_in_memory("reassign", 3).await.unwrap();
        let emb = vec![0.0f32, 1.0, 0.0];
        mem.ingest_turn(turn(
            "d1",
            1,
            "I play the cello",
            "Lovely.",
            emb.clone(),
            &[ent("cello", "thing")],
            "spk-drop",
            Some("Dee"),
        ))
        .await
        .unwrap();
        mem.ingest_memory(
            "d1-mem-0",
            "memory",
            "The user plays cello",
            emb.clone(),
            &[],
            "spk-drop",
        )
        .await
        .unwrap();
        mem.flush().await.unwrap();

        // Before: the kept speaker can't see it.
        let before = mem
            .recall(emb.clone(), Some("spk-keep"), 5, 8)
            .await
            .unwrap();
        assert!(!before.iter().any(|h| h.contains("cello")));

        let moved = mem.reassign_speaker("spk-drop", "spk-keep").await.unwrap();
        assert_eq!(moved, 2, "one turn + one memory re-attributed");
        let after = mem
            .recall(emb.clone(), Some("spk-keep"), 5, 8)
            .await
            .unwrap();
        assert!(after.iter().any(|h| h.contains("I play the cello")));
        assert!(after.iter().any(|h| h.contains("plays cello")));
        let kept = mem
            .list_turns(None, Some("spk-keep"), None, 10, 0)
            .await
            .unwrap()
            .turns;
        assert_eq!(kept.len(), 1);

        // SAID now originates from the kept user; the old user is a tombstone.
        let said = mem
            .read(
                helix_ast::batch::read_batch()
                    .var_as(
                        "s",
                        helix_ast::traversal::g()
                            .n_with_label(super::L_USER)
                            .where_(helix_ast::expr::Predicate::eq("speaker_id", "spk-keep"))
                            .out(Some(super::E_SAID))
                            .value_map(Some(vec!["ext_id"])),
                    )
                    .returning(["s"]),
            )
            .await
            .unwrap();
        assert_eq!(said["s"][0]["ext_id"], "d1");
        assert!(mem.find_user("spk-drop").await.unwrap().is_none());

        // A late ingest of a record still tagged with the dropped id follows the merge.
        mem.ingest_turn(turn(
            "d2",
            2,
            "cello lesson today",
            "Have fun.",
            emb.clone(),
            &[],
            "spk-drop",
            None,
        ))
        .await
        .unwrap();
        let kept = mem
            .list_turns(None, Some("spk-keep"), None, 10, 0)
            .await
            .unwrap()
            .turns;
        assert_eq!(kept.len(), 2);

        // Forgetting the kept speaker makes their nodes shared household ones.
        assert_eq!(mem.delete_speaker("spk-keep").await.unwrap(), 3);
        let shared = mem.recall(emb, None, 5, 8).await.unwrap();
        assert!(shared.iter().any(|h| h.contains("cello")));
        assert!(mem
            .list_turns(None, Some("spk-keep"), None, 10, 0)
            .await
            .unwrap()
            .turns
            .is_empty());
    }
}
