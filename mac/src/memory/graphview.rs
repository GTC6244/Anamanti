//! Read-only introspection seam for the GraphRAG store, used by the orchestrator's
//! debug GUI (`webconfig.rs` → `/helix`).
//!
//! [`GraphView`] is a tiny read-only window onto whatever graph backend is live so
//! the web page can render node counts and a sample of nodes **without** the
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
}
