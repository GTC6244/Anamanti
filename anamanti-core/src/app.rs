//! The long-lived service handles every front door shares.
//!
//! The device-facing Wyoming server ([`crate::server`]), its control protocol
//! ([`crate::control`]) and the loopback config page ([`crate::webconfig`]) all need
//! the same handful of services — the turn pipeline (and through it the settings,
//! memory and speaker service), the model catalog, the STT/TTS connector, the
//! notification + weather push registries, the GraphRAG controller, music. They are
//! built once in `main`, bundled into one [`AppServices`], and shared as an
//! `Arc<AppServices>`: a connection clones one `Arc`, and a new service is one new
//! field here rather than a new argument threaded through every signature.

use std::path::PathBuf;
use std::sync::Arc;

use crate::llm::catalog::ModelCatalog;
use crate::memory::{GraphRagController, MemoryStore};
use crate::music::MusicHub;
use crate::notify::NotificationService;
use crate::orchestrator::{Pipeline, ServiceConnector};
use crate::server::Ducker;
use crate::settings::SharedSettings;
use crate::speaker::SpeakerService;
use crate::weather::WeatherService;
use crate::webconfig::DebugSources;

/// Every shared service handle, built once at boot (see the module docs).
///
/// [`AppServices::new`] takes the three handles with no sensible default and fills
/// the rest with inert ones (no voices dir, no ducking, no music, no GraphRAG, fresh
/// push registries, empty debug sources); the `with_*` builders override them. That
/// keeps tests to the services they exercise.
pub struct AppServices {
    /// The turn pipeline. Also the owner of the runtime settings, the SQLite memory
    /// store and the speaker service (see [`Self::settings`], [`Self::memory`],
    /// [`Self::speaker`]), so there is exactly one of each.
    pub pipeline: Pipeline,
    /// Dials STT/TTS for a turn (and Piper's `describe` for the voice list).
    pub connector: Arc<dyn ServiceConnector>,
    /// The selectable-model catalog (device dropdown + config page).
    pub catalog: Arc<ModelCatalog>,
    /// Where Piper voices are installed, to filter its catalog to installed ones.
    pub voices_dir: Option<PathBuf>,
    /// Proactive-notification channels (Approach A).
    pub notify: Arc<NotificationService>,
    /// Ambient weather push channels.
    pub weather: Arc<WeatherService>,
    /// The live-swappable GraphRAG controller; `None` on the SQLite backend or when
    /// GraphRAG failed to start.
    pub graphrag: Option<Arc<GraphRagController>>,
    /// Lowers the house music while a reply speaks; `None` when ducking is off.
    pub ducker: Option<Arc<dyn Ducker>>,
    /// The Music tab's process supervisor + Snapcast/mpv control; `None` when music
    /// is disabled.
    pub music: Option<MusicHub>,
    /// Read-only sources for the config page's debug views.
    pub debug: DebugSources,
}

impl AppServices {
    /// Bundle the required handles; everything else starts inert (see the type docs).
    pub fn new(
        pipeline: Pipeline,
        connector: Arc<dyn ServiceConnector>,
        catalog: Arc<ModelCatalog>,
    ) -> Self {
        Self {
            pipeline,
            connector,
            catalog,
            voices_dir: None,
            notify: Arc::new(NotificationService::new()),
            weather: Arc::new(WeatherService::new()),
            graphrag: None,
            ducker: None,
            music: None,
            debug: DebugSources::default(),
        }
    }

    pub fn with_voices_dir(mut self, voices_dir: Option<PathBuf>) -> Self {
        self.voices_dir = voices_dir;
        self
    }

    pub fn with_notify(mut self, notify: Arc<NotificationService>) -> Self {
        self.notify = notify;
        self
    }

    pub fn with_weather(mut self, weather: Arc<WeatherService>) -> Self {
        self.weather = weather;
        self
    }

    pub fn with_graphrag(mut self, graphrag: Option<Arc<GraphRagController>>) -> Self {
        self.graphrag = graphrag;
        self
    }

    pub fn with_ducker(mut self, ducker: Option<Arc<dyn Ducker>>) -> Self {
        self.ducker = ducker;
        self
    }

    pub fn with_music(mut self, music: Option<MusicHub>) -> Self {
        self.music = music;
        self
    }

    pub fn with_debug(mut self, debug: DebugSources) -> Self {
        self.debug = debug;
        self
    }

    /// The runtime-swappable settings (owned by the pipeline).
    pub fn settings(&self) -> &Arc<SharedSettings> {
        self.pipeline.settings()
    }

    /// The SQLite memory store (owned by the pipeline).
    pub fn memory(&self) -> &Arc<MemoryStore> {
        self.pipeline.memory()
    }

    /// The speaker service, when speaker identification is enabled.
    pub fn speaker(&self) -> Option<&SpeakerService> {
        self.pipeline.speaker().map(Arc::as_ref)
    }
}
