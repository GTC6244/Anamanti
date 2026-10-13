//! The Memory → Embeddings tab: the live GraphRAG embedding backend and its hot-swap.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::config::EmbedBackend;
use crate::memory::{EmbedStatus, GraphRagController};

/// Current embedding-backend status for the Embeddings page. `available:false` when
/// GraphRAG memory isn't active (SQLite backend, or init failed at boot).
pub(super) async fn embeddings_status_json(graphrag: Option<&Arc<GraphRagController>>) -> String {
    match graphrag {
        None => json!({
            "available": false,
            "reason": "GraphRAG memory is not active (memory_backend is sqlite, or init failed at boot)",
        })
        .to_string(),
        Some(ctrl) => embed_status_payload(true, &ctrl.status().await, None),
    }
}

/// Hot-swap the embedding backend from a `{ "backend": "local"|"openai" }` body
/// (rebuilds the embedder + per-backend HelixDB store + ingester and swaps them in with
/// no restart). A malformed body reads as a missing `backend`.
pub(super) async fn embeddings_switch_json(
    graphrag: Option<&Arc<GraphRagController>>,
    body: &[u8],
) -> String {
    let Some(ctrl) = graphrag else {
        return json!({ "ok": false, "error": "GraphRAG memory is not active" }).to_string();
    };
    let backend = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v["backend"].as_str().map(str::to_string));
    let target = match backend.as_deref() {
        Some("local") | Some("nomic") => EmbedBackend::Local,
        Some("openai") => EmbedBackend::OpenAi,
        Some(other) => {
            return json!({ "ok": false, "error": format!("unknown backend {other:?}") })
                .to_string()
        }
        None => {
            return json!({ "ok": false, "error": "missing 'backend' (local|openai)" }).to_string()
        }
    };
    match ctrl.switch(target).await {
        Ok(status) => embed_status_payload(true, &status, Some(true)),
        Err(e) => json!({ "ok": false, "error": format!("{e:#}") }).to_string(),
    }
}

/// Shared JSON shape for the status + switch responses.
fn embed_status_payload(available: bool, s: &EmbedStatus, ok: Option<bool>) -> String {
    let mut v = json!({
        "available": available,
        "active": s.active,
        "dims": s.dims,
        "local": { "available": s.local_available, "detail": s.local_detail.clone() },
        "openai": { "available": s.openai_available, "detail": s.openai_detail.clone() },
    });
    if let Some(ok) = ok {
        v["ok"] = json!(ok);
    }
    v.to_string()
}
