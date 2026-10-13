//! Live-swappable GraphRAG memory backend (embedding-backend hot-swap).
//!
//! The GraphRAG recall backend (embedder + HelixDB store + background ingester) is
//! wired once at boot. To let the config page switch the embedding backend
//! (local nomic ↔ OpenAI) **without a restart**, the orchestrator and the debug GUI
//! hold *proxies* ([`SwappableRecall`], [`SwappableGraphView`]) whose inner target is
//! swapped atomically by [`GraphRagController::switch`].
//!
//! **Per-backend stores (no data loss on switch):** each embedding backend gets its
//! own on-disk HelixDB store and ingest offset, keyed by an embedding *signature*
//! (`<label>-<dims>`, e.g. `nomic-768`, `openai-1536`). The first switch to a backend
//! builds its index from the chat log; switching back reuses the already-built index
//! and just catches up new turns. The durable SQLite store and the chat log (the
//! source of truth) are never touched, so **no memory is ever lost** — only the
//! derived vector index differs per backend.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::config::{Config, EmbedBackend, GraphRagConfig};

use super::backend::{HelixRecall, Recall, RecallResult};
use super::chatlog::ChatLog;
use super::embed::{Embedder, OpenAiEmbedder};
use super::entity::{AnthropicEntityExtractor, EntityExtractor, NoopEntityExtractor};
use super::graphview::{
    EntityList, GraphSample, GraphStats, GraphView, NodeId, RenameOutcome, TurnList,
};
use super::helix::HelixMemory;
use super::ingester::MemoryIngester;
use crate::speaker::{AliasMap, SpeakerAliases, HOUSEHOLD_SPEAKER};

/// A [`Recall`] that forwards to an inner backend which can be swapped at runtime.
/// Reads clone the current `Arc` under a short read lock and release it before the
/// (async) recall, so a concurrent swap never blocks the turn path.
pub struct SwappableRecall {
    inner: RwLock<Arc<dyn Recall>>,
}

impl SwappableRecall {
    fn new(inner: Arc<dyn Recall>) -> Self {
        Self {
            inner: RwLock::new(inner),
        }
    }
    fn set(&self, inner: Arc<dyn Recall>) {
        *self.inner.write().expect("recall proxy lock") = inner;
    }
    fn current(&self) -> Arc<dyn Recall> {
        self.inner.read().expect("recall proxy lock").clone()
    }
}

#[async_trait]
impl Recall for SwappableRecall {
    async fn recall(
        &self,
        transcript: &str,
        speaker_id: Option<&str>,
        limit: usize,
    ) -> Result<RecallResult> {
        self.current().recall(transcript, speaker_id, limit).await
    }
    fn backend(&self) -> &'static str {
        self.current().backend()
    }
    fn embedder_label(&self) -> Option<&'static str> {
        self.current().embedder_label()
    }
    async fn reassign_speaker(&self, from: &str, to: &str) -> Result<u64> {
        self.current().reassign_speaker(from, to).await
    }
    async fn delete_speaker(&self, id: &str) -> Result<u64> {
        self.current().delete_speaker(id).await
    }
}

/// A [`GraphView`] that forwards to a swappable inner store (debug GUI `/helix`).
pub struct SwappableGraphView {
    inner: RwLock<Arc<dyn GraphView>>,
}

impl SwappableGraphView {
    fn new(inner: Arc<dyn GraphView>) -> Self {
        Self {
            inner: RwLock::new(inner),
        }
    }
    fn set(&self, inner: Arc<dyn GraphView>) {
        *self.inner.write().expect("graphview proxy lock") = inner;
    }
    fn current(&self) -> Arc<dyn GraphView> {
        self.inner.read().expect("graphview proxy lock").clone()
    }
}

#[async_trait]
impl GraphView for SwappableGraphView {
    async fn stats(&self) -> Result<GraphStats> {
        self.current().stats().await
    }
    async fn sample(&self, limit: usize) -> Result<GraphSample> {
        self.current().sample(limit).await
    }
    async fn rename_entity(&self, old_name: &str, new_name: &str) -> Result<RenameOutcome> {
        self.current().rename_entity(old_name, new_name).await
    }
    async fn list_turns(
        &self,
        query: Option<&str>,
        speaker_id: Option<&str>,
        entity: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<TurnList> {
        self.current()
            .list_turns(query, speaker_id, entity, limit, offset)
            .await
    }
    async fn list_entities(&self, query: Option<&str>, limit: usize) -> Result<EntityList> {
        self.current().list_entities(query, limit).await
    }
    async fn delete_node(&self, id: NodeId) -> Result<bool> {
        self.current().delete_node(id).await
    }
}

/// Status of the embedding backend, for the config page.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EmbedStatus {
    /// The active backend label (`"local"` or `"openai"`).
    pub active: &'static str,
    /// Dimensionality of the active backend's vectors.
    pub dims: usize,
    /// Whether the local nomic backend can be selected (feature compiled + model files
    /// present on disk).
    pub local_available: bool,
    /// Why local is unavailable, if it is (for the GUI to show).
    pub local_detail: String,
    /// Whether the OpenAI backend can be selected (`OPENAI_API_KEY` present).
    pub openai_available: bool,
    /// Why OpenAI is unavailable, if it is.
    pub openai_detail: String,
}

/// Owns the live GraphRAG backend and performs hot-swaps.
pub struct GraphRagController {
    chatlog_path: PathBuf,
    helix_base: PathBuf,
    cfg: GraphRagConfig,
    extractor: Arc<dyn EntityExtractor>,
    /// Speaker merges/forgets (`crate::speaker::aliases`), replayed onto a store each
    /// time it becomes active and applied by its ingester. `None` when the alias table
    /// could not be opened.
    aliases: Option<Arc<SpeakerAliases>>,
    recall: Arc<SwappableRecall>,
    graph: Arc<SwappableGraphView>,
    live: Mutex<Live>,
}

struct Live {
    backend: EmbedBackend,
    ingester: JoinHandle<()>,
    dims: usize,
}

/// A freshly built (not-yet-spawned) backend.
struct Built {
    recall: Arc<dyn Recall>,
    graph: Arc<dyn GraphView>,
    ingester: Arc<MemoryIngester>,
    dims: usize,
}

impl GraphRagController {
    /// Build the GraphRAG backend for the active embedding backend and start its
    /// ingester. The active backend is the last one persisted to disk (the
    /// `active_backend` marker), else `config.graphrag.embed_backend`. If the chosen
    /// backend can't be built (e.g. missing model or key) it falls back to the config
    /// default; if that also fails the error propagates (caller → SQLite FTS).
    pub async fn start(config: &Config, chatlog: &Arc<ChatLog>) -> Result<Arc<Self>> {
        let cfg = config.graphrag.clone();
        let helix_base = config.helix_path.clone();
        let chatlog_path = chatlog.path().to_path_buf();
        let extractor = build_extractor(&cfg);
        // The alias table lives in the SQLite store of record; it exists (empty) even
        // when speaker identification is off.
        let aliases = match SpeakerAliases::open(&config.db_path) {
            Ok(a) => Some(Arc::new(a)),
            Err(e) => {
                log::warn!("speaker aliases unavailable ({e:#}); graph ids are used as logged");
                None
            }
        };

        let configured = cfg.embed_backend;
        let initial = read_marker(&marker_path(&helix_base)).unwrap_or(configured);
        let build = |backend| {
            build_backend(
                &cfg,
                &helix_base,
                &chatlog_path,
                &extractor,
                aliases.clone(),
                backend,
            )
        };

        let (backend, built) = match build(initial).await {
            Ok(b) => (initial, b),
            Err(e) if initial != configured => {
                log::warn!(
                    "persisted embedding backend {} failed to start ({e:#}); \
                     falling back to the configured default {}",
                    label(initial),
                    label(configured)
                );
                (configured, build(configured).await?)
            }
            Err(e) => return Err(e),
        };

        log::info!(
            "memory embeddings: {} backend active ({} dims); store {}",
            label(backend),
            built.dims,
            helix_base.join(signature(backend, built.dims)).display(),
        );
        let handle = built.ingester.clone().spawn(cfg.ingest_interval);
        let ctrl = Arc::new(Self {
            chatlog_path,
            helix_base,
            cfg,
            extractor,
            aliases,
            recall: Arc::new(SwappableRecall::new(built.recall)),
            graph: Arc::new(SwappableGraphView::new(built.graph)),
            live: Mutex::new(Live {
                backend,
                ingester: handle,
                dims: built.dims,
            }),
        });
        let _ = write_marker(&marker_path(&ctrl.helix_base), backend);
        Ok(ctrl)
    }

    /// The swappable recall proxy handed to the orchestrator pipeline.
    pub fn recall(&self) -> Arc<dyn Recall> {
        self.recall.clone()
    }

    /// The swappable graph-view proxy handed to the debug GUI.
    pub fn graph_view(&self) -> Arc<dyn GraphView> {
        self.graph.clone()
    }

    /// Hot-swap the embedding backend. Builds the new backend first (its per-signature
    /// store and ingester); only if that succeeds does it swap the proxies and stop the
    /// old ingester, so a failed switch leaves the current backend running untouched.
    pub async fn switch(&self, backend: EmbedBackend) -> Result<EmbedStatus> {
        let mut live = self.live.lock().await;
        if live.backend == backend {
            return Ok(self.status_for(backend, live.dims));
        }
        let built = build_backend(
            &self.cfg,
            &self.helix_base,
            &self.chatlog_path,
            &self.extractor,
            self.aliases.clone(),
            backend,
        )
        .await
        .with_context(|| format!("switching embeddings to the {} backend", label(backend)))?;

        let new_handle = built.ingester.clone().spawn(self.cfg.ingest_interval);
        self.recall.set(built.recall);
        self.graph.set(built.graph);
        live.ingester.abort();
        live.ingester = new_handle;
        live.backend = backend;
        live.dims = built.dims;
        let _ = write_marker(&marker_path(&self.helix_base), backend);
        log::info!(
            "memory embeddings: hot-swapped to the {} backend ({} dims)",
            label(backend),
            built.dims
        );
        Ok(self.status_for(backend, built.dims))
    }

    /// Current status (active backend + which options are selectable).
    pub async fn status(&self) -> EmbedStatus {
        let live = self.live.lock().await;
        self.status_for(live.backend, live.dims)
    }

    fn status_for(&self, backend: EmbedBackend, dims: usize) -> EmbedStatus {
        let (local_available, local_detail) = local_availability(&self.cfg);
        let (openai_available, openai_detail) = openai_availability();
        EmbedStatus {
            active: label(backend),
            dims,
            local_available,
            local_detail,
            openai_available,
            openai_detail,
        }
    }
}

/// The embedding signature that keys a backend's store dir + offset sidecar.
fn signature(backend: EmbedBackend, dims: usize) -> String {
    format!("{}-{dims}", store_label(backend))
}

/// Short backend label for the API/GUI (`local`/`openai`).
fn label(backend: EmbedBackend) -> &'static str {
    match backend {
        EmbedBackend::Local => "local",
        EmbedBackend::OpenAi => "openai",
    }
}

/// Backend label used in on-disk store paths (the model family, not the API name).
fn store_label(backend: EmbedBackend) -> &'static str {
    match backend {
        EmbedBackend::Local => "nomic",
        EmbedBackend::OpenAi => "openai",
    }
}

fn marker_path(helix_base: &Path) -> PathBuf {
    helix_base.join("active_backend")
}

fn read_marker(path: &Path) -> Option<EmbedBackend> {
    match std::fs::read_to_string(path).ok()?.trim() {
        "local" | "nomic" => Some(EmbedBackend::Local),
        "openai" => Some(EmbedBackend::OpenAi),
        _ => None,
    }
}

fn write_marker(path: &Path, backend: EmbedBackend) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    std::fs::write(path, label(backend))
        .with_context(|| format!("writing active-backend marker {}", path.display()))
}

fn offset_path(chatlog_path: &Path, sig: &str) -> PathBuf {
    let mut s = chatlog_path.to_path_buf().into_os_string();
    s.push(format!(".{sig}.offset"));
    PathBuf::from(s)
}

fn build_extractor(cfg: &GraphRagConfig) -> Arc<dyn EntityExtractor> {
    match std::env::var("ANTHROPIC_API_KEY") {
        Ok(key) if !key.is_empty() => Arc::new(AnthropicEntityExtractor::new(
            cfg.anthropic_base_url.clone(),
            key,
            cfg.extract_model.clone(),
        )),
        _ => {
            log::warn!(
                "ANTHROPIC_API_KEY absent; entity extraction disabled (recall is pure vector KNN)"
            );
            Arc::new(NoopEntityExtractor)
        }
    }
}

/// Build a backend about to become active (boot or a switch): open its store, bring
/// it up to date with the speaker aliases, and wire its ingester to resolve them.
async fn build_backend(
    cfg: &GraphRagConfig,
    helix_base: &Path,
    chatlog_path: &Path,
    extractor: &Arc<dyn EntityExtractor>,
    aliases: Option<Arc<SpeakerAliases>>,
    backend: EmbedBackend,
) -> Result<Built> {
    let embedder = build_embedder(cfg, backend)?;
    let dims = embedder.dimensions();
    let sig = signature(backend, dims);
    let root = helix_base.join(&sig);
    let helix = Arc::new(
        HelixMemory::open_disk(root, "ambient", dims)
            .await
            .context("opening embedded HelixDB store")?,
    );
    // Speaker attribution must never take recall down: on any alias problem, warn and
    // serve the store as-is. The ingester resolves aliases as it writes, and this replay
    // is idempotent and runs again on the next boot/switch.
    if let Some(aliases) = &aliases {
        match aliases.load() {
            Ok(map) => match apply_speaker_aliases(&helix, &map).await {
                Ok(moved) if moved > 0 => log::info!(
                    "memory store {sig}: re-attributed {moved} node(s) per speaker merges/forgets"
                ),
                Ok(_) => {}
                Err(e) => log::warn!(
                    "memory store {sig}: applying speaker aliases failed ({e:#}); serving \
                     the store as-is, will retry on the next boot"
                ),
            },
            Err(e) => log::warn!(
                "memory store {sig}: loading speaker aliases failed ({e:#}); serving the \
                 store as-is, will retry on the next boot"
            ),
        }
    }
    let ingester = Arc::new(
        MemoryIngester::new_with_offset(
            chatlog_path.to_path_buf(),
            offset_path(chatlog_path, &sig),
            helix.clone(),
            embedder.clone(),
            extractor.clone(),
        )
        .with_aliases(aliases),
    );
    let recall: Arc<dyn Recall> = Arc::new(HelixRecall::new(helix.clone(), embedder, cfg.recall_k));
    let graph: Arc<dyn GraphView> = helix;
    Ok(Built {
        recall,
        graph,
        ingester,
        dims,
    })
}

/// Replay every speaker merge/forget onto `helix`: a store that was inactive when the
/// change happened still attributes nodes to the old id. Idempotent (an id with no
/// nodes left is a no-op), so it runs every time a store becomes active. Returns the
/// number of `Turn` + `Memory` nodes re-attributed.
async fn apply_speaker_aliases(helix: &HelixMemory, aliases: &AliasMap) -> Result<u64> {
    let mut moved = 0;
    for (from, _) in aliases.iter() {
        // Skip aliases this store already applied (tombstoned) or never saw (no User
        // node ⇒ no turns/memories to move), so boot cost doesn't grow with history.
        if !helix.has_live_speaker(from).await? {
            continue;
        }
        moved += match aliases.resolve(from) {
            HOUSEHOLD_SPEAKER => helix.delete_speaker(from).await?,
            to => helix.reassign_speaker(from, to).await?,
        };
    }
    Ok(moved)
}

fn build_embedder(cfg: &GraphRagConfig, backend: EmbedBackend) -> Result<Arc<dyn Embedder>> {
    match backend {
        EmbedBackend::Local => {
            #[cfg(feature = "embed-local")]
            {
                use super::embed::LocalNomicEmbedder;
                Ok(Arc::new(
                    LocalNomicEmbedder::open(
                        &cfg.embed_model_path,
                        &cfg.embed_tokenizer_dir,
                        cfg.embed_dims,
                    )
                    .context(
                        "loading the local nomic embedder — provision the model with \
                         scripts/fetch-embed-model.sh",
                    )?,
                ))
            }
            #[cfg(not(feature = "embed-local"))]
            {
                let _ = cfg;
                anyhow::bail!("the local embedder needs the `embed-local` build feature")
            }
        }
        EmbedBackend::OpenAi => {
            let key = std::env::var("OPENAI_API_KEY")
                .ok()
                .filter(|s| !s.is_empty())
                .context("the OpenAI embedding backend requires OPENAI_API_KEY")?;
            Ok(Arc::new(OpenAiEmbedder::new(
                cfg.openai_base_url.clone(),
                key,
                cfg.embed_model.clone(),
                cfg.embed_dims,
            )))
        }
    }
}

/// Whether the local backend can be selected right now, plus a reason if not.
fn local_availability(cfg: &GraphRagConfig) -> (bool, String) {
    #[cfg(not(feature = "embed-local"))]
    {
        let _ = cfg;
        return (
            false,
            "not compiled (build without `embed-local`)".to_string(),
        );
    }
    #[cfg(feature = "embed-local")]
    {
        if !cfg.embed_model_path.exists() {
            return (
                false,
                format!("model missing at {}", cfg.embed_model_path.display()),
            );
        }
        if !cfg.embed_tokenizer_dir.join("tokenizer.json").exists() {
            return (
                false,
                format!("tokenizer missing in {}", cfg.embed_tokenizer_dir.display()),
            );
        }
        (true, String::new())
    }
}

/// Whether the OpenAI backend can be selected right now, plus a reason if not.
fn openai_availability() -> (bool, String) {
    match std::env::var("OPENAI_API_KEY") {
        Ok(k) if !k.is_empty() => (true, String::new()),
        _ => (false, "OPENAI_API_KEY not set".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_and_labels_are_per_backend() {
        assert_eq!(signature(EmbedBackend::Local, 768), "nomic-768");
        assert_eq!(signature(EmbedBackend::OpenAi, 1536), "openai-1536");
        // Even at equal dims the two backends get distinct store signatures.
        assert_ne!(
            signature(EmbedBackend::Local, 768),
            signature(EmbedBackend::OpenAi, 768)
        );
        assert_eq!(label(EmbedBackend::Local), "local");
        assert_eq!(label(EmbedBackend::OpenAi), "openai");
    }

    /// A fake GraphRAG backend that records the speaker hooks it receives.
    #[derive(Default)]
    struct FakeGraphRecall {
        calls: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Recall for FakeGraphRecall {
        async fn recall(&self, _: &str, _: Option<&str>, _: usize) -> Result<RecallResult> {
            Ok(RecallResult::default())
        }
        fn backend(&self) -> &'static str {
            "helix"
        }
        fn embedder_label(&self) -> Option<&'static str> {
            Some("mock")
        }
        async fn reassign_speaker(&self, from: &str, to: &str) -> Result<u64> {
            self.calls.lock().unwrap().push(format!("{from}->{to}"));
            Ok(3)
        }
        async fn delete_speaker(&self, id: &str) -> Result<u64> {
            self.calls.lock().unwrap().push(format!("del {id}"));
            Ok(1)
        }
    }

    /// The proxy reports and forwards to whatever backend is live, never a
    /// hardcoded "helix" — including the speaker hooks a merge/forget relies on.
    #[tokio::test]
    async fn swappable_recall_delegates_to_the_live_backend() {
        use crate::memory::{MemoryStore, SqliteRecall};

        let fts: Arc<dyn Recall> = Arc::new(SqliteRecall::new(Arc::new(
            MemoryStore::open_in_memory().unwrap(),
        )));
        let proxy = SwappableRecall::new(fts);
        assert_eq!(proxy.backend(), "sqlite-fts");
        assert_eq!(proxy.embedder_label(), None);
        assert_eq!(proxy.reassign_speaker("a", "b").await.unwrap(), 0);

        let graph = Arc::new(FakeGraphRecall::default());
        proxy.set(graph.clone());
        assert_eq!(proxy.backend(), "helix");
        assert_eq!(proxy.embedder_label(), Some("mock"));
        assert_eq!(proxy.reassign_speaker("spk-a", "spk-b").await.unwrap(), 3);
        assert_eq!(proxy.delete_speaker("spk-b").await.unwrap(), 1);
        assert_eq!(
            *graph.calls.lock().unwrap(),
            vec!["spk-a->spk-b".to_string(), "del spk-b".to_string()]
        );
    }

    // ---- speaker aliases across stores ------------------------------------------

    use crate::memory::chatlog::ChatLogRecord;
    use crate::memory::embed::MockEmbedder;
    use crate::memory::entity::NoopEntityExtractor;
    use crate::memory::{HelixRecall, MemoryStore};
    use crate::speaker::{MockSpeakerEmbedder, SpeakerRegistry, SpeakerService, SpeakerThresholds};

    /// Run an async test body on a big-stack thread (the embedded HelixDB engine's
    /// futures overflow the default test-thread stack).
    fn on_big_stack<F: std::future::Future<Output = ()> + Send + 'static>(body: F) {
        std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(body)
            })
            .unwrap()
            .join()
            .unwrap();
    }

    fn logged(id: &str, speaker: &str, name: &str, transcript: &str) -> ChatLogRecord {
        ChatLogRecord {
            id: id.to_string(),
            ts: 1,
            session_id: "s".to_string(),
            transcript: transcript.to_string(),
            reply: "Noted.".to_string(),
            memories_written: vec![],
            llm_backend: "mock".to_string(),
            model: None,
            speaker_id: speaker.to_string(),
            speaker_name: Some(name.to_string()),
            timing: None,
        }
    }

    /// A household with two voice clusters (`keep` = Ann, `drop` = an anonymous
    /// cluster that is really Ann too), one logged turn each, and the speaker service +
    /// alias reader over a real SQLite file. Returns the pieces the tests need.
    struct Household {
        dir: PathBuf,
        log_path: PathBuf,
        service: SpeakerService,
        aliases: Arc<SpeakerAliases>,
        memory: MemoryStore,
        keep: String,
        drop: String,
    }

    fn household(tag: &str) -> Household {
        let dir = std::env::temp_dir().join(format!(
            "anamanti-alias-{tag}-{}-{}",
            std::process::id(),
            crate::clock::now_millis()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("anamanti_memory.sqlite");
        let registry = SpeakerRegistry::open(&db).unwrap();
        let keep = registry
            .create_labeled("Ann", &[1.0, 0.0, 0.0, 0.0], 3)
            .unwrap();
        let drop = registry.create_cluster(&[0.0, 1.0, 0.0, 0.0]).unwrap();
        let service = SpeakerService::new(
            Arc::new(MockSpeakerEmbedder::new(4)),
            registry,
            SpeakerThresholds::from_ms(0.7, 0.5, 100),
        );
        let log_path = dir.join("anamanti_chatlog.jsonl");
        let log = ChatLog::open(&log_path).unwrap();
        log.append(&logged("t1", &keep, "Ann", "I like green tea"))
            .unwrap();
        log.append(&logged("t2", &drop, "Speaker 2", "I play the cello"))
            .unwrap();
        Household {
            aliases: Arc::new(SpeakerAliases::open(&db).unwrap()),
            memory: MemoryStore::open_in_memory().unwrap(),
            dir,
            log_path,
            service,
            keep,
            drop,
        }
    }

    /// A store fed from the chat log by its own ingester (one per embedding backend),
    /// with the production alias wiring.
    async fn ingested_store(h: &Household, sig: &str) -> Arc<HelixMemory> {
        let helix = Arc::new(HelixMemory::open_in_memory(sig, 8).await.unwrap());
        let ingester = MemoryIngester::new_with_offset(
            &h.log_path,
            offset_path(&h.log_path, sig),
            helix.clone(),
            Arc::new(MockEmbedder::new(8)),
            Arc::new(NoopEntityExtractor),
        )
        .with_aliases(Some(h.aliases.clone()));
        ingester.run_once().await.unwrap();
        helix
    }

    async fn turns_of(helix: &HelixMemory, speaker: &str) -> Vec<String> {
        let list = helix
            .list_turns(None, Some(speaker), None, 10, 0)
            .await
            .unwrap();
        list.turns
            .into_iter()
            .filter(|t| t.speaker_id == speaker)
            .map(|t| t.ext_id)
            .collect()
    }

    /// After a merge, re-ingesting the chat log (which still records the dropped
    /// speaker's id) into a fresh store attributes those turns to the kept speaker.
    #[test]
    fn reingest_after_merge_attributes_to_the_kept_speaker() {
        on_big_stack(async {
            let h = household("reingest");
            let active = ingested_store(&h, "nomic-8").await;
            let graph = HelixRecall::new(active.clone(), Arc::new(MockEmbedder::new(8)), 8);
            h.service
                .merge(&h.keep, &h.drop, &h.memory, Some(&graph))
                .await
                .unwrap()
                .expect("merged");

            // A store built from scratch from the chat log (a new backend's first use).
            let fresh = ingested_store(&h, "openai-8").await;
            assert!(turns_of(&fresh, &h.drop).await.is_empty());
            let mut kept = turns_of(&fresh, &h.keep).await;
            kept.sort();
            assert_eq!(kept, vec!["t1", "t2"]);

            // A forget re-points the merged id too: both now land on the household.
            h.service
                .delete(&h.keep, &h.memory, Some(&graph))
                .await
                .unwrap()
                .expect("deleted");
            let after_forget = ingested_store(&h, "other-8").await;
            assert!(turns_of(&after_forget, &h.keep).await.is_empty());
            assert!(turns_of(&after_forget, &h.drop).await.is_empty());
            assert_eq!(turns_of(&after_forget, "household").await.len(), 2);
            std::fs::remove_dir_all(&h.dir).ok();
        });
    }

    /// A store that was inactive during the merge still holds the old id; activating
    /// it (the alias replay `build_backend` runs on boot and on `switch`) re-attributes
    /// it before it serves recall, so the old id is not resurrected.
    #[test]
    fn activating_another_store_after_merge_does_not_resurrect_the_old_id() {
        on_big_stack(async {
            let h = household("switch");
            let active = ingested_store(&h, "nomic-8").await;
            let inactive = ingested_store(&h, "openai-8").await;
            assert_eq!(turns_of(&inactive, &h.drop).await, vec!["t2"]);

            let graph = HelixRecall::new(active.clone(), Arc::new(MockEmbedder::new(8)), 8);
            h.service
                .merge(&h.keep, &h.drop, &h.memory, Some(&graph))
                .await
                .unwrap()
                .expect("merged");
            // Only the active store was rewritten by the merge itself.
            assert_eq!(turns_of(&inactive, &h.drop).await, vec!["t2"]);

            // Switch: the newly active store is brought up to date first.
            let map = h.aliases.load().unwrap();
            assert_eq!(apply_speaker_aliases(&inactive, &map).await.unwrap(), 1);
            let recall = HelixRecall::new(inactive.clone(), Arc::new(MockEmbedder::new(8)), 8);
            let as_old = recall
                .recall("I play the cello", Some(&h.drop), 8)
                .await
                .unwrap();
            assert!(
                !as_old.hits.iter().any(|t| t.contains("cello")),
                "old id still scoped to the turn: {:?}",
                as_old.hits
            );
            let as_kept = recall
                .recall("I play the cello", Some(&h.keep), 8)
                .await
                .unwrap();
            assert!(as_kept.hits.iter().any(|t| t.contains("cello")));
            assert!(turns_of(&inactive, &h.drop).await.is_empty());

            // Idempotent: replaying again (the next boot) changes nothing.
            assert_eq!(apply_speaker_aliases(&inactive, &map).await.unwrap(), 0);
            std::fs::remove_dir_all(&h.dir).ok();
        });
    }

    #[test]
    fn offset_path_is_signature_scoped() {
        let p = offset_path(Path::new("/x/anamanti_chatlog.jsonl"), "nomic-768");
        assert_eq!(
            p,
            PathBuf::from("/x/anamanti_chatlog.jsonl.nomic-768.offset")
        );
    }

    #[test]
    fn marker_round_trips_and_ignores_garbage() {
        let dir = std::env::temp_dir().join(format!("anamanti-marker-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = marker_path(&dir);

        assert!(read_marker(&path).is_none()); // absent
        write_marker(&path, EmbedBackend::OpenAi).unwrap();
        assert_eq!(read_marker(&path), Some(EmbedBackend::OpenAi));
        write_marker(&path, EmbedBackend::Local).unwrap();
        assert_eq!(read_marker(&path), Some(EmbedBackend::Local));

        std::fs::write(&path, "garbage").unwrap();
        assert!(read_marker(&path).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Full controller boot against the provisioned local model: per-backend store is
    /// created under its signature, status reports the active backend, and the recall +
    /// graph proxies work. Skips cleanly when the model isn't provisioned.
    #[cfg(feature = "embed-local")]
    #[tokio::test]
    async fn controller_starts_local_and_exposes_working_proxies() {
        use crate::config::Config;
        use crate::memory::ChatLog;

        if !PathBuf::from("models/nomic-embed-text-v1.5.onnx").exists()
            || !PathBuf::from("models/nomic-tokenizer/tokenizer.json").exists()
        {
            eprintln!("skip: nomic model not provisioned (scripts/fetch-embed-model.sh)");
            return;
        }

        let tmp = std::env::temp_dir().join(format!("anamanti-ctrl-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let config = Config {
            helix_path: tmp.join("helix"),
            db_path: tmp.join("memory.sqlite"),
            ..Config::default()
        };
        let chatlog = Arc::new(ChatLog::open(tmp.join("chatlog.jsonl")).unwrap());

        let ctrl = GraphRagController::start(&config, &chatlog)
            .await
            .expect("controller should start on the local backend");

        let status = ctrl.status().await;
        assert_eq!(status.active, "local");
        assert_eq!(status.dims, 768);
        assert!(status.local_available);

        // Per-backend store dir was created under its signature.
        assert!(tmp.join("helix").join("nomic-768").exists());
        // Active-backend marker persisted.
        assert_eq!(
            read_marker(&marker_path(&config.helix_path)),
            Some(EmbedBackend::Local)
        );

        // The recall + graph proxies delegate to the live backend without panicking.
        let recall = ctrl.recall();
        assert_eq!(recall.backend(), "helix");
        let _ = recall.recall("hello", None, 3).await.unwrap();
        let _ = ctrl.graph_view().stats().await.unwrap();

        std::fs::remove_dir_all(&tmp).ok();
    }
}
