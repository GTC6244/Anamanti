//! The read-only debug views (chat log, prompts, SQLite, HelixDB) and the one debug
//! mutation, renaming a GraphRAG entity.

use std::collections::HashSet;

use serde_json::{json, Value};

use super::speakers::{tag_graph_nodes, SpeakerDirectory};
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

/// The decoded `?q=` search parameter (`+` and `%XX` URL-decoded, trimmed); `None`
/// when absent or blank.
pub(super) fn query_param(target: &str) -> Option<String> {
    let raw = target
        .split(['?', '#'])
        .nth(1)?
        .split('&')
        .find_map(|pair| pair.strip_prefix("q="))?;
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = match bytes.get(i..i + 3) {
            Some([b'%', hi, lo]) => hex(*hi).zip(hex(*lo)).map(|(h, l)| h << 4 | l),
            _ => None,
        };
        match (escaped, bytes[i]) {
            (Some(b), _) => {
                out.push(b);
                i += 3;
                continue;
            }
            (None, b'+') => out.push(b' '),
            (None, b) => out.push(b),
        }
        i += 1;
    }
    let q = String::from_utf8_lossy(&out).trim().to_string();
    (!q.is_empty()).then_some(q)
}

/// `{ ok, records: [ChatLogRecord…] }` for the newest `limit` turns — or, with a
/// `query`, the newest `limit` turns **anywhere in the log** whose transcript or reply
/// contains it (case-insensitive), so turns older than the page's window (e.g. ones
/// told before voice ID) can be found and shared. Each record keeps every raw field
/// and adds the resolved `speaker` tag, `speaker_override`, `memories_tagged`
/// ([`SpeakerDirectory::chatlog_record_json`]) and `shared` (whether the turn is in
/// `shared`, the record ids shared with the household).
pub(super) fn chatlog_json(
    debug: &DebugSources,
    dir: &SpeakerDirectory,
    shared: &HashSet<String>,
    limit: usize,
    query: Option<&str>,
) -> String {
    let read = match query {
        // Loopback admin page: scanning the whole log for a search is fine.
        Some(q) => chatlog::read_tail(&debug.chatlog_path, usize::MAX).map(|all| {
            let q = q.to_lowercase();
            all.into_iter()
                .filter(|r| {
                    r.transcript.to_lowercase().contains(&q) || r.reply.to_lowercase().contains(&q)
                })
                .take(limit)
                .collect::<Vec<_>>()
        }),
        None => chatlog::read_tail(&debug.chatlog_path, limit),
    };
    match read {
        Ok(records) => {
            let records: Vec<Value> = records
                .iter()
                .map(|r| {
                    let mut v = dir.chatlog_record_json(r);
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert("shared".into(), json!(shared.contains(&r.id)));
                    }
                    v
                })
                .collect();
            json!({ "ok": true, "records": records }).to_string()
        }
        Err(e) => fail(format!("{e:#}")),
    }
}

/// `{ ok, records: [PromptLogRecord…] }` for the newest `limit` prompts, each with
/// its resolved `speaker` tag.
pub(super) fn prompts_json(debug: &DebugSources, dir: &SpeakerDirectory, limit: usize) -> String {
    match promptlog::read_tail(&debug.promptlog_path, limit) {
        Ok(records) => {
            let records: Vec<Value> = records.iter().map(|r| dir.prompt_record_json(r)).collect();
            json!({ "ok": true, "records": records }).to_string()
        }
        Err(e) => fail(format!("{e:#}")),
    }
}

/// `{ ok, count, unknown_count, db_path, memory_backend, memories: [...] }` — the whole
/// memory store. Each row keeps its raw fields and adds the resolved `speaker` (who it
/// is for), `said_by` (provenance), `candidate`, `scope`, `turn_id` and the write-time
/// resolution/confidence ([`SpeakerDirectory::memory_fields`]).
pub(super) fn sqlite_json(
    memory: &MemoryStore,
    debug: &DebugSources,
    dir: &SpeakerDirectory,
) -> String {
    match memory.list() {
        Ok(items) => {
            let memories: Vec<Value> = items
                .iter()
                .map(|m| {
                    let mut v = json!({
                        "id": m.id,
                        "kind": m.kind.as_str(),
                        "content": m.content,
                        "source": m.source.as_str(),
                        "created_at": m.created_at,
                        "speaker_id": m.speaker_id,
                    });
                    if let (Some(obj), Value::Object(extra)) =
                        (v.as_object_mut(), dir.memory_fields(m))
                    {
                        obj.extend(extra);
                    }
                    v
                })
                .collect();
            let unknown = items
                .iter()
                .filter(|m| m.scope == crate::memory::ScopeKind::Unknown)
                .count();
            json!({
                "ok": true,
                "count": memories.len(),
                "unknown_count": unknown,
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
///
/// Sampled nodes that carry a `speaker_id` (User / Turn / Memory) gain a resolved
/// `$speaker` tag when a directory is given, so the graph view shows names.
pub(super) async fn helix_json(debug: &DebugSources, dir: Option<&SpeakerDirectory>) -> String {
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
    let mut nodes = match graph.sample(50).await {
        Ok(v) => serde_json::to_value(v).unwrap_or(Value::Null),
        Err(e) => return fail(format!("{e:#}")),
    };
    if let Some(dir) = dir {
        tag_graph_nodes(dir, &mut nodes);
    }
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
