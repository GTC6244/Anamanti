//! Introspection + light-editing seam for the GraphRAG store, used by the debug
//! GUI (`webconfig.rs` → `/helix`) and the device Memory screen (`control.rs`).
//!
//! [`GraphView`] is a small window onto whatever graph backend is live so the web
//! page can render node counts and a sample of nodes — and fix an entity's name
//! (extracted facts sometimes carry a spelling mistake) — and the device can browse
//! and prune turns/entities, **without** the callers depending on the HelixDB
//! engine. When the SQLite recall backend is selected (or GraphRAG failed to start)
//! no implementation is attached and the pages report the graph as disabled.
//!
//! Results are typed: every method returns a plain struct deriving `Serialize`
//! whose JSON is exactly the shape the pages and the Wyoming control replies carry.
//! Node ids are an opaque [`NodeId`]: callers pass back what they were given and
//! never interpret it.
//!
//! The embedded HelixDB implementation lives in [`super::helix`].

use std::collections::BTreeMap;

use anyhow::Result;
use async_trait::async_trait;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value};

/// An opaque graph-node id. Serializes as a bare JSON number (the wire shape the
/// device and the debug page already use); callers only ever hand it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(u64);

impl NodeId {
    /// Wrap a raw id received over the wire (e.g. a device delete request).
    pub fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The backend's raw id (for the backend implementation only).
    pub(crate) fn raw(self) -> u64 {
        self.0
    }
}

/// Node totals: `{ "total": N, "by_label": { "Turn": n, "Memory": n, ... } }`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct GraphStats {
    pub total: u64,
    pub by_label: BTreeMap<String, u64>,
}

/// One sampled node: its id (as `"$id"`) plus its non-embedding properties.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NodeSample {
    #[serde(rename = "$id")]
    pub id: NodeId,
    #[serde(flatten)]
    pub properties: Map<String, Value>,
}

/// A per-label node sample: `{ "Turn": [ {..}, .. ], "Memory": [ .. ], ... }`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(transparent)]
pub struct GraphSample(pub BTreeMap<String, Vec<NodeSample>>);

/// Per-kind tally of what a rename touched: `Entity` nodes renamed, plus
/// `Turn`/`Memory` nodes whose free text was corrected. Serializes as
/// `{ "entities": n, "turns": n, "memories": n, "total": n }`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenameOutcome {
    /// `Entity` nodes whose `name` property was rewritten.
    pub entities: u64,
    /// `Turn` nodes whose text had the old name corrected.
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

impl Serialize for RenameOutcome {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_struct("RenameOutcome", 4)?;
        s.serialize_field("entities", &self.entities)?;
        s.serialize_field("turns", &self.turns)?;
        s.serialize_field("memories", &self.memories)?;
        s.serialize_field("total", &self.total())?;
        s.end()
    }
}

/// An entity a turn mentions: `{ "name": .., "kind": .. }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EntityRef {
    pub name: String,
    pub kind: String,
}

/// One stored turn for the device Memory screen.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TurnRow {
    pub id: NodeId,
    pub ext_id: String,
    pub session: String,
    pub ts: i64,
    pub transcript: String,
    pub reply: String,
    pub speaker_id: String,
    pub entities: Vec<EntityRef>,
}

/// A page of turns: `{ "turns": [ ... ] }`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TurnList {
    pub turns: Vec<TurnRow>,
}

/// One stored entity ("keyword") for the device Memory screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EntityRow {
    pub id: NodeId,
    pub name: String,
    pub kind: String,
}

/// A list of entities: `{ "entities": [ ... ] }`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct EntityList {
    pub entities: Vec<EntityRow>,
}

/// A view of the graph memory for the debug GUI and the device Memory screen.
/// `Send + Sync` for sharing behind an `Arc` across connections.
#[async_trait]
pub trait GraphView: Send + Sync {
    /// Total node count plus a per-label breakdown.
    async fn stats(&self) -> Result<GraphStats>;

    /// Up to `limit` nodes per label with their (non-embedding) properties.
    async fn sample(&self, limit: usize) -> Result<GraphSample>;

    /// Rename an entity everywhere it appears — correcting e.g. a spelling mistake
    /// in an extracted fact — matching its exact current `name`. Rewrites the
    /// `Entity` node's `name` (id + `MENTIONS`/`ABOUT`/`KNOWS` edges preserved) and
    /// every whole-word occurrence of the name in turn and memory text. A `total()`
    /// of `0` means the name was found nowhere.
    async fn rename_entity(&self, old_name: &str, new_name: &str) -> Result<RenameOutcome>;

    /// Browse stored turns for the device Memory screen, newest first. All filters
    /// are optional and AND-combined: `query` (case-insensitive substring over the
    /// turn text), `speaker_id` (that speaker only), `entity` (turns mentioning that
    /// entity name). Paginated by `offset`/`limit`.
    async fn list_turns(
        &self,
        query: Option<&str>,
        speaker_id: Option<&str>,
        entity: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<TurnList>;

    /// Browse stored entities ("keywords") for the device Memory screen. `query` is an
    /// optional case-insensitive substring match on `name`; capped to `limit`, sorted
    /// by `name`.
    async fn list_entities(&self, query: Option<&str>, limit: usize) -> Result<EntityList>;

    /// Drop a single node (a turn/entity/memory picked on the device), with its
    /// incident edges and vector-index entry. Graph-index-only — the durable SQLite
    /// facts and the chat log (source of truth) are untouched. Returns `true` once the
    /// drop is flushed (idempotent for a missing id).
    async fn delete_node(&self, id: NodeId) -> Result<bool>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn typed_results_serialize_to_the_page_shapes() {
        let mut by_label = BTreeMap::new();
        by_label.insert("Turn".to_string(), 2);
        by_label.insert("User".to_string(), 1);
        let stats = GraphStats { total: 3, by_label };
        assert_eq!(
            serde_json::to_value(&stats).unwrap(),
            json!({ "total": 3, "by_label": { "Turn": 2, "User": 1 } })
        );

        let mut props = Map::new();
        props.insert("text".to_string(), json!("hi"));
        let mut sample = BTreeMap::new();
        sample.insert(
            "Turn".to_string(),
            vec![NodeSample {
                id: NodeId::from_raw(7),
                properties: props,
            }],
        );
        assert_eq!(
            serde_json::to_string(&GraphSample(sample)).unwrap(),
            r#"{"Turn":[{"$id":7,"text":"hi"}]}"#
        );

        let rename = RenameOutcome {
            entities: 1,
            turns: 2,
            memories: 0,
        };
        assert_eq!(
            serde_json::to_value(rename).unwrap(),
            json!({ "entities": 1, "turns": 2, "memories": 0, "total": 3 })
        );

        let turns = TurnList {
            turns: vec![TurnRow {
                id: NodeId::from_raw(4),
                ext_id: "r1".into(),
                session: "s".into(),
                ts: 100,
                transcript: "where?".into(),
                reply: "there".into(),
                speaker_id: "household".into(),
                entities: vec![EntityRef {
                    name: "library".into(),
                    kind: "place".into(),
                }],
            }],
        };
        assert_eq!(
            serde_json::to_value(&turns).unwrap(),
            json!({ "turns": [{
                "id": 4, "ext_id": "r1", "session": "s", "ts": 100,
                "transcript": "where?", "reply": "there", "speaker_id": "household",
                "entities": [{ "name": "library", "kind": "place" }],
            }]})
        );

        let ents = EntityList {
            entities: vec![EntityRow {
                id: NodeId::from_raw(9),
                name: "jazz".into(),
                kind: "topic".into(),
            }],
        };
        assert_eq!(
            serde_json::to_value(&ents).unwrap(),
            json!({ "entities": [{ "id": 9, "name": "jazz", "kind": "topic" }] })
        );
    }
}
