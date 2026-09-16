// The embedded HelixDB engine (feature `helix`) has deeply nested generic types;
// computing the layout of the async runtime that awaits them needs a higher
// recursion limit than the default 128 (the `db` crate sets the same).
#![recursion_limit = "512"]
//! Ambient Smart Display — Mac Mini assistant orchestrator (Plan.MD Phase 4).
//!
//! Runs the "brain": a Wyoming server the Echo Show discovers over mDNS, wiring
//! downstream Whisper (STT) → a pluggable LLM + persistent SQLite memory → Piper
//! (TTS), and streaming the synthesized reply back to the device.
//!
//! Configuration is environment-driven (see `config.rs`); with the defaults it
//! advertises `_wyoming._tcp` on port 10700 and talks to a local Whisper (10300),
//! Piper (10200), and Ollama (11434).

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::TcpListener;

use ambient_orchestrator::config::{Config, MemoryBackendChoice};
use ambient_orchestrator::discovery::MdnsAdvertiser;
use ambient_orchestrator::memory::{ChatLog, MemoryStore};
use ambient_orchestrator::orchestrator::{self, Pipeline, TcpConnector};
use ambient_orchestrator::server;

/// Worker-thread stack size. The embedded HelixDB engine (feature `helix`) builds
/// deep async state machines whose stack usage exceeds tokio's 2 MiB default,
/// especially in debug builds; 16 MiB gives comfortable headroom.
const WORKER_STACK_SIZE: usize = 16 * 1024 * 1024;

fn main() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(WORKER_STACK_SIZE)
        .build()
        .context("building tokio runtime")?;
    // Run on a spawned task so all of `run` (including the embedded-HelixDB init)
    // executes on a large-stack worker thread rather than the main thread.
    runtime.block_on(async {
        tokio::spawn(run())
            .await
            .context("orchestrator task panicked")?
    })
}

async fn run() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut config = Config::from_env().context("loading configuration")?;
    log::info!(
        "starting orchestrator: bind={} stt={} tts={} llm={} db={}",
        config.bind_addr,
        config.stt_addr,
        config.tts_addr,
        config.llm_label(),
        config.db_path.display(),
    );

    // Startup model check: when using Ollama, verify the configured model is
    // actually installed. A missing model otherwise fails silently as a per-turn
    // 404 (transcript shows, no reply). Surface it loudly at boot, and fall back
    // to an installed model so the assistant still responds.
    ensure_ollama_model(&mut config).await;

    let memory = Arc::new(MemoryStore::open(&config.db_path).context("opening memory store")?);
    log::info!("memory store holds {} entries", memory.count()?);

    // Runtime-swappable settings (Phase 6): the initial backend/voice come from
    // config; the on-device settings screen can change them between turns.
    let settings = config
        .shared_settings()
        .context("initializing LLM backend")?;

    // Chat log (always on): every completed turn is recorded here, both as the
    // durable source of truth and as the ingestion queue for GraphRAG memory.
    let chatlog = Arc::new(
        ChatLog::open(&config.chatlog_path)
            .with_context(|| format!("opening chat log at {}", config.chatlog_path.display()))?,
    );
    log::info!("chat log at {}", config.chatlog_path.display());

    let mut pipeline = Pipeline::with_settings(
        settings,
        memory,
        config.system_prompt.clone(),
        config.turn_timeout,
    )
    .with_chatlog(chatlog.clone());

    // Memory retrieval backend: SQLite FTS (default) or embedded HelixDB GraphRAG.
    if config.memory_backend == MemoryBackendChoice::Helix {
        match build_graphrag_recall(&config, &chatlog).await {
            Ok(recall) => {
                log::info!("memory backend: HelixDB GraphRAG (embedded, in-process)");
                pipeline = pipeline.with_recall(recall);
            }
            Err(e) => {
                log::error!("GraphRAG init failed ({e:#}); falling back to SQLite FTS recall");
            }
        }
    } else {
        log::info!("memory backend: SQLite FTS");
    }

    let connector: Arc<dyn orchestrator::ServiceConnector> = Arc::new(TcpConnector {
        stt_addr: config.stt_addr,
        tts_addr: config.tts_addr,
    });

    let listener = TcpListener::bind(config.bind_addr)
        .await
        .with_context(|| format!("binding device-facing server to {}", config.bind_addr))?;
    let local = listener.local_addr()?;

    // Advertise over mDNS so the Echo Show discovers us without a hardcoded IP.
    // Held for the process lifetime; unregisters on drop.
    let _mdns = MdnsAdvertiser::advertise(&config.service_name, local.port())
        .context("advertising Wyoming service over mDNS")?;

    log::info!("orchestrator ready on {local}; waiting for the device");

    tokio::select! {
        res = server::serve(listener, pipeline, connector) => {
            res.context("device-facing server stopped")?;
        }
        _ = tokio::signal::ctrl_c() => {
            log::info!("shutdown signal received; stopping");
        }
    }

    Ok(())
}

/// Startup model check for the Ollama backend: verify the configured model is
/// installed; if not, fall back to an installed one (with a loud warning) so a
/// misconfigured/absent model fails visibly at boot instead of as a silent
/// per-turn 404. No-op for non-Ollama backends or if Ollama is unreachable.
async fn ensure_ollama_model(config: &mut Config) {
    use ambient_orchestrator::config::LlmChoice;
    use ambient_orchestrator::llm::ollama::{self, ModelCheck};

    let LlmChoice::Ollama { url, model } = &config.llm else {
        return;
    };
    let (url, model) = (url.clone(), model.clone());
    match ollama::available_models(&url).await {
        Ok(models) => match ollama::resolve_model(&model, &models) {
            ModelCheck::Available => log::info!("ollama model '{model}' is installed"),
            ModelCheck::FallBack(fallback) => {
                log::warn!(
                    "ollama model '{model}' is not installed at {url}; falling back to '{fallback}'. \
                     Installed: {models:?}. Set AMBIENT_OLLAMA_MODEL or run `ollama pull {model}`."
                );
                config.llm = LlmChoice::Ollama {
                    url,
                    model: fallback,
                };
            }
            ModelCheck::NonePulled => log::error!(
                "no models are installed in ollama at {url}; LLM turns will fail until you \
                 `ollama pull {model}` (or start Ollama)."
            ),
        },
        Err(e) => log::warn!(
            "could not query ollama models at {url} ({e:#}); proceeding with '{model}' \
             (turns will fail if it isn't installed)"
        ),
    }
}

/// Build the HelixDB GraphRAG recall backend and spawn the background ingester.
/// Requires `OPENAI_API_KEY` (embeddings); `ANTHROPIC_API_KEY` enables Claude
/// Haiku entity extraction (absent → pure-vector recall). Only available when the
/// binary is built with the `helix` feature.
#[cfg(feature = "helix")]
async fn build_graphrag_recall(
    config: &Config,
    chatlog: &Arc<ChatLog>,
) -> Result<Arc<dyn ambient_orchestrator::memory::Recall>> {
    use ambient_orchestrator::memory::embed::{Embedder, OpenAiEmbedder};
    use ambient_orchestrator::memory::entity::{
        AnthropicEntityExtractor, EntityExtractor, NoopEntityExtractor,
    };
    use ambient_orchestrator::memory::helix::HelixMemory;
    use ambient_orchestrator::memory::ingester::MemoryIngester;
    use ambient_orchestrator::memory::HelixRecall;

    let g = &config.graphrag;
    let openai_key = std::env::var("OPENAI_API_KEY")
        .context("AMBIENT_MEMORY_BACKEND=helix requires OPENAI_API_KEY for embeddings")?;
    let embedder: Arc<dyn Embedder> = Arc::new(OpenAiEmbedder::new(
        g.openai_base_url.clone(),
        openai_key,
        g.embed_model.clone(),
        g.embed_dims,
    ));

    let helix = Arc::new(
        HelixMemory::open_disk(config.helix_path.clone(), "ambient", embedder.dimensions())
            .await
            .context("opening embedded HelixDB store")?,
    );
    log::info!(
        "HelixDB store at {} ({} nodes)",
        config.helix_path.display(),
        helix.node_count().await.unwrap_or(0)
    );

    let extractor: Arc<dyn EntityExtractor> = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(key) if !key.is_empty() => Arc::new(AnthropicEntityExtractor::new(
            g.anthropic_base_url.clone(),
            key,
            g.extract_model.clone(),
        )),
        _ => {
            log::warn!(
                "ANTHROPIC_API_KEY absent; entity extraction disabled (recall is pure vector KNN)"
            );
            Arc::new(NoopEntityExtractor)
        }
    };

    let ingester = Arc::new(MemoryIngester::new(
        chatlog.path().to_path_buf(),
        helix.clone(),
        embedder.clone(),
        extractor,
    ));
    ingester.spawn(g.ingest_interval);
    log::info!(
        "background memory ingester running every {}s",
        g.ingest_interval.as_secs()
    );

    Ok(Arc::new(HelixRecall::new(helix, embedder, g.recall_k)))
}

/// When built without the `helix` feature, the GraphRAG backend is unavailable.
#[cfg(not(feature = "helix"))]
async fn build_graphrag_recall(
    _config: &Config,
    _chatlog: &Arc<ChatLog>,
) -> Result<Arc<dyn ambient_orchestrator::memory::Recall>> {
    anyhow::bail!("binary built without the `helix` feature; rebuild with --features helix")
}
