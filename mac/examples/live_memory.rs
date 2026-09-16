// The embedded HelixDB engine's deeply nested async types exceed the default
// type-layout recursion limit (as in the binary + lib).
#![recursion_limit = "512"]
//! Live GraphRAG smoke test against the REAL providers (OpenAI embeddings +
//! Claude Haiku extraction) driving the embedded HelixDB memory end to end.
//!
//! Reads `OPENAI_API_KEY` and `ANTHROPIC_API_KEY` from the environment (map your
//! namespaced keys in at launch, e.g.
//!   OPENAI_API_KEY="$ORCHESTRATOR_TEXT_EMBEDDING_3_SMALL_API_KEY" \
//!   ANTHROPIC_API_KEY="$ORCHESTRATOR_HAIKU45_API_KEY" \
//!   cargo run --example live_memory
//! ). It NEVER prints key material — only shapes, scores, and extracted entities.
//!
//! Requires the `helix` feature (on by default).

#[cfg(not(feature = "helix"))]
fn main() {
    eprintln!("build with the `helix` feature to run this example");
}

#[cfg(feature = "helix")]
fn main() {
    // Large worker stack: the embedded engine's async types overflow tokio's 2 MiB
    // default (mirrors the binary's WORKER_STACK_SIZE).
    const STACK: usize = 32 * 1024 * 1024;
    std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(|| {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(STACK)
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                if let Err(e) = run().await {
                    eprintln!("LIVE TEST FAILED: {e:#}");
                    std::process::exit(1);
                }
            });
        })
        .unwrap()
        .join()
        .unwrap();
}

#[cfg(feature = "helix")]
async fn run() -> anyhow::Result<()> {
    use std::sync::Arc;

    use ambient_orchestrator::memory::backend::Recall;
    use ambient_orchestrator::memory::chatlog::{ChatLog, ChatLogRecord};
    use ambient_orchestrator::memory::embed::{Embedder, OpenAiEmbedder, OPENAI_SMALL_DIMS};
    use ambient_orchestrator::memory::entity::{AnthropicEntityExtractor, EntityExtractor};
    use ambient_orchestrator::memory::helix::HelixMemory;
    use ambient_orchestrator::memory::ingester::MemoryIngester;
    use ambient_orchestrator::memory::HelixRecall;

    let openai_key = std::env::var("OPENAI_API_KEY").map_err(|_| {
        anyhow::anyhow!("OPENAI_API_KEY not set (map ORCHESTRATOR_TEXT_EMBEDDING_3_SMALL_API_KEY)")
    })?;
    let anthropic_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| {
        anyhow::anyhow!("ANTHROPIC_API_KEY not set (map ORCHESTRATOR_HAIKU45_API_KEY)")
    })?;

    let embedder: Arc<dyn Embedder> = Arc::new(OpenAiEmbedder::new(
        "https://api.openai.com",
        openai_key,
        "text-embedding-3-small",
        OPENAI_SMALL_DIMS,
    ));
    let extractor: Arc<dyn EntityExtractor> = Arc::new(AnthropicEntityExtractor::new(
        "https://api.anthropic.com",
        anthropic_key,
        "claude-haiku-4-5",
    ));

    // 1) Live embeddings: batch of 3; check dims + that near texts score higher.
    println!("== 1. OpenAI text-embedding-3-small ==");
    let texts = vec![
        "I love listening to jazz music".to_string(),
        "jazz records are my favorite".to_string(),
        "what is the weather forecast tomorrow".to_string(),
    ];
    let vecs = embedder.embed(&texts).await?;
    let cos = |a: &[f32], b: &[f32]| {
        let d: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        d / (na * nb)
    };
    println!(
        "   returned {} vectors, dim = {}",
        vecs.len(),
        vecs[0].len()
    );
    println!("   cos(jazz, jazz)    = {:.4}", cos(&vecs[0], &vecs[1]));
    println!("   cos(jazz, weather) = {:.4}", cos(&vecs[0], &vecs[2]));
    anyhow::ensure!(
        vecs[0].len() == OPENAI_SMALL_DIMS,
        "unexpected embedding dim"
    );
    anyhow::ensure!(
        cos(&vecs[0], &vecs[1]) > cos(&vecs[0], &vecs[2]),
        "expected jazz/jazz to be more similar than jazz/weather"
    );
    println!("   ✓ dims correct and semantic similarity holds");

    // 2) Live entity extraction with Claude Haiku.
    println!("\n== 2. Claude Haiku 4.5 entity extraction ==");
    let sentence =
        "Sam mentioned he loves jazz and just moved to Portland for a coffee roasting job.";
    let entities = extractor.extract(sentence).await?;
    println!("   input: {sentence}");
    for e in &entities {
        println!("   - {} ({})", e.name, e.kind);
    }
    anyhow::ensure!(!entities.is_empty(), "Haiku returned no entities");
    println!("   ✓ extracted {} entities", entities.len());

    // 3) Full GraphRAG e2e: chat log → ingester (real embed + extract) → recall.
    println!("\n== 3. Full GraphRAG pipeline (embedded HelixDB, on disk) ==");
    let dir = std::env::temp_dir().join(format!(
        "ambient_live_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir)?;
    let log_path = dir.join("chat.jsonl");
    let log = ChatLog::open(&log_path)?;
    let mk = |id: &str, t: &str, r: &str, mem: &[&str]| ChatLogRecord {
        id: id.to_string(),
        ts: 1,
        session_id: "live".to_string(),
        transcript: t.to_string(),
        reply: r.to_string(),
        memories_written: mem.iter().map(|s| s.to_string()).collect(),
        llm_backend: "mock".to_string(),
        model: None,
    };
    log.append(&mk(
        "l1",
        "I really love jazz music, especially Coltrane",
        "Coltrane is a legend!",
        &["The user loves jazz, especially Coltrane"],
    ))?;
    log.append(&mk("l2", "remind me to buy coffee beans", "Will do.", &[]))?;
    log.append(&mk(
        "l3",
        "what time is my dentist appointment",
        "3pm on Tuesday.",
        &[],
    ))?;

    let helix =
        Arc::new(HelixMemory::open_disk(&dir.join("helix"), "live", embedder.dimensions()).await?);
    let ingester = MemoryIngester::new(
        &log_path,
        helix.clone(),
        embedder.clone(),
        extractor.clone(),
    );
    let n = ingester.run_once().await?;
    println!(
        "   ingested {n} turns; graph has {} nodes",
        helix.node_count().await?
    );

    let recall = HelixRecall::new(helix.clone(), embedder.clone(), 6);
    let query = "what kind of music am I into";
    let hits = recall.recall(query, 8).await?;
    println!("   query: {query:?}");
    for h in &hits {
        println!("   → {h}");
    }
    anyhow::ensure!(
        hits.iter().any(|h| h.to_lowercase().contains("jazz")),
        "recall did not surface the jazz context"
    );
    println!("   ✓ GraphRAG recall surfaced the relevant jazz memory");

    std::fs::remove_dir_all(&dir).ok();
    println!("\nALL LIVE CHECKS PASSED ✅");
    Ok(())
}
