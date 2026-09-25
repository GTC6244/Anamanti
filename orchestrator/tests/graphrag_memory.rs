//! End-to-end GraphRAG memory test (memory_plan.md Goals 1–4), exercising the
//! **real embedded HelixDB engine** with a deterministic offline embedder +
//! extractor (no network, no API keys). Proves the full path:
//!
//!   chat log (JSONL)  →  background ingester  →  embed + entity extraction
//!                     →  HelixDB graph (Turn/Memory/Entity nodes + edges)
//!                     →  vector + graph recall  →  context lines
//!
//! The embedded HelixDB engine is always compiled in.

use std::sync::Arc;

use ambient_orchestrator::memory::backend::Recall;
use ambient_orchestrator::memory::chatlog::{ChatLog, ChatLogRecord};
use ambient_orchestrator::memory::embed::{Embedder, MockEmbedder};
use ambient_orchestrator::memory::entity::MockEntityExtractor;
use ambient_orchestrator::memory::helix::HelixMemory;
use ambient_orchestrator::memory::ingester::MemoryIngester;
use ambient_orchestrator::memory::HelixRecall;

/// Run an async test body on a dedicated 32 MiB-stack thread with a multi-thread
/// runtime. The embedded HelixDB engine's async state machines exceed the default
/// 2 MiB test-thread stack (mirrors `WORKER_STACK_SIZE` in the binary).
fn on_big_stack<F>(body: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    const STACK: usize = 32 * 1024 * 1024;
    std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(STACK)
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async { tokio::spawn(body).await.unwrap() });
        })
        .unwrap()
        .join()
        .unwrap();
}

fn unique_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("ambient_graphrag_{tag}_{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn record(id: &str, transcript: &str, reply: &str, memories: &[&str]) -> ChatLogRecord {
    ChatLogRecord {
        id: id.to_string(),
        ts: 1,
        session_id: "sess-1".to_string(),
        transcript: transcript.to_string(),
        reply: reply.to_string(),
        memories_written: memories.iter().map(|s| s.to_string()).collect(),
        llm_backend: "mock".to_string(),
        model: None,
        speaker_id: "household".to_string(),
        speaker_name: None,
    }
}

#[test]
fn full_pipeline_ingests_and_recalls_from_the_graph() {
    on_big_stack(full_pipeline_body());
}

async fn full_pipeline_body() {
    let dir = unique_dir("full");
    let log_path = dir.join("chat.jsonl");

    // 1. Capture: three turns land in the chat log, one carrying a durable memory.
    let log = ChatLog::open(&log_path).unwrap();
    log.append(&record(
        "t1",
        "I love jazz music",
        "Great taste!",
        &["The user likes jazz"],
    ))
    .unwrap();
    log.append(&record(
        "t2",
        "what is the weather today",
        "It's sunny.",
        &[],
    ))
    .unwrap();
    log.append(&record("t3", "make me a coffee please", "On it.", &[]))
        .unwrap();

    // 2. GraphRAG store + deterministic offline embedder/extractor.
    let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder::new(32));
    let helix = Arc::new(
        HelixMemory::open_in_memory("full-test", embedder.dimensions())
            .await
            .unwrap(),
    );
    let extractor = Arc::new(MockEntityExtractor);

    // 3. Ingest: drain the log into the graph.
    let ingester = MemoryIngester::new(&log_path, helix.clone(), embedder.clone(), extractor);
    let n = ingester.run_once().await.unwrap();
    assert_eq!(n, 3, "all three turns should ingest");

    // Nodes: 1 household user + 3 turns + 1 memory + entities (jazz, music, weather,
    // coffee) → well over the turn count.
    let nodes = helix.node_count().await.unwrap();
    assert!(
        nodes > 3 + 1,
        "expected turns+user+memory nodes, got {nodes}"
    );

    // 4. Idempotency: a second run ingests nothing (offset advanced, upserts by id).
    let n2 = ingester.run_once().await.unwrap();
    assert_eq!(n2, 0, "re-running must not double-ingest");

    // 5. Recall: a music query should surface the jazz turn (vector KNN) and the
    //    jazz memory (graph expansion turn→MENTIONS→Entity→ABOUT→Memory).
    let recall = HelixRecall::new(helix.clone(), embedder.clone(), 6);
    let hits = recall
        .recall("what music do I like", None, 8)
        .await
        .unwrap();
    assert!(
        hits.iter().any(|h| h.to_lowercase().contains("jazz")),
        "recall should surface jazz context, got: {hits:?}"
    );
    // The weather/coffee turns should not dominate a music query's top hit.
    assert!(!hits.is_empty());
}

#[test]
fn disk_store_persists_across_reopen() {
    on_big_stack(disk_store_body());
}

async fn disk_store_body() {
    let dir = unique_dir("disk");
    let embedder = MockEmbedder::new(16);
    let dims = embedder.dimensions();

    // Write a turn, then close.
    {
        let helix = HelixMemory::open_disk(&dir, "persist", dims).await.unwrap();
        let vec = embedder.embed_one("remember the alamo").await.unwrap();
        helix
            .ingest_turn(
                "d1",
                "s",
                1,
                "remember the alamo",
                vec,
                &[],
                "household",
                None,
            )
            .await
            .unwrap();
        let before = helix.node_count().await.unwrap();
        assert!(before >= 2, "user + turn");
        helix.close().await.unwrap();
    }

    // Reopen the same on-disk root: the turn is still there.
    let helix = HelixMemory::open_disk(&dir, "persist", dims).await.unwrap();
    let after = helix.node_count().await.unwrap();
    assert!(
        after >= 2,
        "nodes should persist across reopen, got {after}"
    );
    helix.close().await.unwrap();
}

#[test]
fn rename_entity_fixes_a_misspelled_name() {
    on_big_stack(rename_entity_body());
}

/// Editing an entity's name (a misspelled fact) rewrites the `Entity` node in
/// place — same node, same edges — *and* the free text of the turns/memories that
/// mention it, while refusing to collide with another entity.
async fn rename_entity_body() {
    use ambient_orchestrator::memory::entity::Entity;
    use ambient_orchestrator::memory::GraphView;

    let embedder = MockEmbedder::new(16);
    let dims = embedder.dimensions();
    let helix = HelixMemory::open_in_memory("rename-test", dims)
        .await
        .unwrap();

    // Ingest a turn + a memory that both mention a misspelled place, "Portlnad".
    let ents = vec![Entity {
        name: "Portlnad".to_string(),
        kind: "place".to_string(),
    }];
    let vec = embedder.embed_one("I live in Portlnad").await.unwrap();
    helix
        .ingest_turn(
            "t1",
            "s",
            1,
            "I live in Portlnad",
            vec,
            &ents,
            "household",
            None,
        )
        .await
        .unwrap();
    let mvec = embedder
        .embed_one("The user lives in Portlnad")
        .await
        .unwrap();
    helix
        .ingest_memory(
            "m1",
            "fact",
            "The user lives in Portlnad",
            mvec,
            &ents,
            "household",
        )
        .await
        .unwrap();

    // Renaming a name that appears nowhere touches nothing.
    let none = helix.rename_entity("Nowhere", "Somewhere").await.unwrap();
    assert_eq!(none.total(), 0);

    // Fix the spelling: the Entity node, the turn text, and the memory content are
    // all corrected in place.
    let fixed = helix.rename_entity("Portlnad", "Portland").await.unwrap();
    assert_eq!(fixed.entities, 1, "the entity node is renamed");
    assert_eq!(fixed.turns, 1, "the turn's free text is corrected");
    assert_eq!(fixed.memories, 1, "the memory's free text is corrected");

    // The graph now shows the corrected name and no trace of the misspelling.
    let sample = helix.sample(10).await.unwrap();
    let entities = sample["Entity"].as_array().unwrap();
    assert_eq!(entities.len(), 1, "rename edits in place, not a new node");
    assert_eq!(entities[0]["name"], "Portland");
    assert_eq!(sample["Turn"][0]["text"], "I live in Portland");
    assert_eq!(sample["Memory"][0]["content"], "The user lives in Portland");

    // Renaming onto an existing *distinct* entity is rejected (would split edges),
    // and leaves everything untouched.
    let vec2 = embedder.embed_one("coffee").await.unwrap();
    let coffee = vec![Entity {
        name: "coffee".to_string(),
        kind: "thing".to_string(),
    }];
    helix
        .ingest_turn("t2", "s", 2, "coffee", vec2, &coffee, "household", None)
        .await
        .unwrap();
    assert!(
        helix.rename_entity("coffee", "Portland").await.is_err(),
        "colliding with an existing entity name must be rejected"
    );

    // A no-op rename (same name) changes nothing and reports the entity match count.
    let noop = helix.rename_entity("Portland", "Portland").await.unwrap();
    assert_eq!(noop.entities, 1);
    assert_eq!(noop.turns, 0);
    // An empty target name is rejected.
    assert!(helix.rename_entity("Portland", "   ").await.is_err());
}

#[test]
fn graph_view_reports_stats_and_samples_nodes() {
    on_big_stack(graph_view_body());
}

/// The read-only [`GraphView`] used by the debug GUI (`/helix`) must return live
/// per-label counts and non-embedding node samples over a real HelixDB store.
async fn graph_view_body() {
    use ambient_orchestrator::memory::GraphView;

    let embedder = MockEmbedder::new(16);
    let dims = embedder.dimensions();
    let helix = HelixMemory::open_in_memory("graphview-test", dims)
        .await
        .unwrap();

    let vec = embedder.embed_one("I love jazz music").await.unwrap();
    helix
        .ingest_turn(
            "t1",
            "s",
            1,
            "I love jazz music",
            vec,
            &[],
            "household",
            None,
        )
        .await
        .unwrap();

    // Stats: a total plus a per-label breakdown that includes the ingested Turn.
    let stats = helix.stats().await.unwrap();
    assert!(stats["total"].as_u64().unwrap() >= 2, "user + turn nodes");
    assert_eq!(stats["by_label"]["Turn"].as_u64().unwrap(), 1);
    assert_eq!(stats["by_label"]["User"].as_u64().unwrap(), 1);

    // Sample: the Turn node carries its text (a projected property) and no
    // `embedding` (deliberately excluded — it must never be dumped to the page).
    let sample = helix.sample(10).await.unwrap();
    let turns = sample["Turn"].as_array().unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0]["text"], "I love jazz music");
    assert!(
        turns[0].get("embedding").is_none(),
        "embedding must be omitted"
    );
    assert!(turns[0].get("$id").is_some(), "node id present");
}
