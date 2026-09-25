//! Introspection + light-editing seam for the GraphRAG store, used by the
//! orchestrator's debug GUI (`webconfig.rs` → `/helix`).
//!
//! [`GraphView`] is a tiny window onto whatever graph backend is live so the web
//! page can render node counts and a sample of nodes — and fix an entity's name
//! (extracted facts sometimes carry a spelling mistake) — **without** the
//! `webconfig` module depending on the feature-gated HelixDB engine. When the
//! `helix` feature is off (or the SQLite backend is selected) no implementation is
//! attached and the page reports the graph as disabled.
//!
//! The embedded HelixDB implementation lives in [`super::helix`].

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

/// A read-only view of the graph memory for the debug GUI. `Send + Sync` for
/// sharing behind an `Arc` across connections.
#[async_trait]
pub trait GraphView: Send + Sync {
    /// Total node count plus a per-label breakdown, as
    /// `{ "total": N, "by_label": { "Turn": n, "Memory": n, ... } }`.
    async fn stats(&self) -> Result<Value>;

    /// Up to `limit` nodes per label with their (non-embedding) properties, as
    /// `{ "Turn": [ {..}, .. ], "Memory": [ .. ], ... }`.
    async fn sample(&self, limit: usize) -> Result<Value>;

    /// Rename an entity everywhere it appears — correcting e.g. a spelling mistake
    /// in an extracted fact — matching its exact current `name`. Rewrites the
    /// `Entity` node's `name` (id + `MENTIONS`/`ABOUT`/`KNOWS` edges preserved) and
    /// every whole-word occurrence of the name in `Turn.text` / `Memory.content`.
    /// Returns `{ "entities": n, "turns": n, "memories": n, "total": n }`; a `total`
    /// of `0` means the name was found nowhere.
    async fn rename_entity(&self, old_name: &str, new_name: &str) -> Result<Value>;
}
