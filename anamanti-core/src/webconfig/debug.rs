//! The read-only debug views (chat log, prompts, SQLite, HelixDB) and the one debug
//! mutation, renaming a GraphRAG entity.

use serde_json::{json, Value};

use super::{fail, parse_json_body, DebugSources};
use crate::memory::{chatlog, promptlog, MemoryStore};

/// Default cap on how many log records a debug page returns per request.
pub(super) const DEFAULT_LOG_LIMIT: usize = 100;
/// Hard cap, so a hand-typed `?limit=` can't ask the server to buffer the world.
pub(super) const MAX_LOG_LIMIT: usize = 1000;

/// Parse a `?limit=N` query parameter, clamped to `[1, MAX_LOG_LIMIT]`; absent or
/// unparseable falls back to [`DEFAULT_LOG_LIMIT`].
pub(super) fn limit_param(target: &str) -> usize {
    target
        .split(['?', '#'])
        .nth(1)
        .into_iter()
        .flat_map(|q| q.split('&'))
        .find_map(|pair| pair.strip_prefix("limit=")?.parse::<usize>().ok())
        .map(|n| n.clamp(1, MAX_LOG_LIMIT))
        .unwrap_or(DEFAULT_LOG_LIMIT)
}

/// `{ ok, records: [ChatLogRecord…] }` for the newest `limit` turns.
pub(super) fn chatlog_json(debug: &DebugSources, limit: usize) -> String {
    match chatlog::read_tail(&debug.chatlog_path, limit) {
        Ok(records) => json!({ "ok": true, "records": records }).to_string(),
        Err(e) => fail(format!("{e:#}")),
    }
}

/// `{ ok, records: [PromptLogRecord…] }` for the newest `limit` prompts.
pub(super) fn prompts_json(debug: &DebugSources, limit: usize) -> String {
    match promptlog::read_tail(&debug.promptlog_path, limit) {
        Ok(records) => json!({ "ok": true, "records": records }).to_string(),
        Err(e) => fail(format!("{e:#}")),
    }
}

/// `{ ok, count, db_path, memory_backend, memories: [...] }` — the whole memory store.
pub(super) fn sqlite_json(memory: &MemoryStore, debug: &DebugSources) -> String {
    match memory.list() {
        Ok(items) => {
            let memories: Vec<Value> = items
                .iter()
                .map(|m| {
                    json!({
                        "id": m.id,
                        "kind": m.kind.as_str(),
                        "content": m.content,
                        "source": m.source.as_str(),
                        "created_at": m.created_at,
                        "speaker_id": m.speaker_id,
                    })
                })
                .collect();
            json!({
                "ok": true,
                "count": memories.len(),
                "db_path": debug.db_path.display().to_string(),
                "memory_backend": debug.memory_backend,
                "memories": memories,
            })
            .to_string()
        }
        Err(e) => fail(format!("{e:#}")),
    }
}

/// `{ ok, enabled, helix_path, stats, nodes }` — GraphRAG store overview, or
/// `{ ok, enabled: false, message }` when the graph backend is not live. The
/// message distinguishes "not configured for HelixDB" from "configured for HelixDB
/// but init failed, so recall fell back to SQLite FTS" (commonly a missing
/// `OPENAI_API_KEY`), so the page doesn't tell you to set a value that is already set.
pub(super) async fn helix_json(debug: &DebugSources) -> String {
    let Some(graph) = &debug.graph else {
        let message = if debug.memory_backend.eq_ignore_ascii_case("helix") {
            "GraphRAG (HelixDB) is configured (memory_backend=\"helix\") but failed to \
             initialize, so recall fell back to SQLite FTS. The usual cause is a missing \
             OPENAI_API_KEY (required for embeddings); a HelixDB store that could not be \
             opened — e.g. two orchestrators sharing one helix_path — does this too. Check \
             the orchestrator log for \"GraphRAG init failed\"."
        } else {
            "GraphRAG (HelixDB) is not active. Set memory_backend=\"helix\" in \
             anamanti.json to enable it."
        };
        return json!({
            "ok": true,
            "enabled": false,
            "message": message,
        })
        .to_string();
    };
    let stats = match graph.stats().await {
        Ok(v) => v,
        Err(e) => return fail(format!("{e:#}")),
    };
    let nodes = match graph.sample(50).await {
        Ok(v) => v,
        Err(e) => return fail(format!("{e:#}")),
    };
    json!({
        "ok": true,
        "enabled": true,
        "helix_path": debug.helix_path.display().to_string(),
        "stats": stats,
        "nodes": nodes,
    })
    .to_string()
}

/// POST `/helix/rename-entity` — body `{ "old_name": "...", "new_name": "..." }`.
/// Fixes a misspelled entity everywhere: the `Entity` node's `name` (id + edges
/// preserved) plus whole-word occurrences in `Turn.text` / `Memory.content`.
/// Returns `{ ok: true, entities, turns, memories, total }`, or `{ ok: false,
/// message }` when the graph is inactive, the request is malformed, the name was
/// found nowhere, or the target already names a different entity.
pub(super) async fn helix_rename_json(debug: &DebugSources, body: &[u8]) -> String {
    let Some(graph) = &debug.graph else {
        return fail("GraphRAG (HelixDB) is not active.");
    };
    let data = match parse_json_body(body) {
        Ok(v) => v,
        Err(invalid) => return invalid,
    };
    let name = |key: &str| {
        data.get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let (old_name, new_name) = (name("old_name"), name("new_name"));
    if old_name.is_empty() || new_name.is_empty() {
        return fail("rename requires non-empty `old_name` and `new_name`");
    }
    match graph.rename_entity(&old_name, &new_name).await {
        Ok(counts) => {
            let total = counts.total();
            if total == 0 {
                fail(format!("no occurrences of {old_name:?} found"))
            } else {
                json!({
                    "ok": true,
                    "entities": counts.entities,
                    "turns": counts.turns,
                    "memories": counts.memories,
                    "total": total,
                })
                .to_string()
            }
        }
        Err(e) => fail(format!("{e:#}")),
    }
}
