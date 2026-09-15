//! Bridge from the (`!Send`, real-time) capture thread to the async Wyoming turn
//! (Plan.MD §3, Phase 3; architecture.md §4).
//!
//! The engine's capture/inference loop runs on a dedicated std thread because the
//! `cpal` stream is `!Send`. The Wyoming client, by contrast, is async `tokio`
//! networking. [`Network`] owns a small single-worker tokio runtime and marshals
//! between the two worlds:
//!
//! - On a wake-word detection the capture thread calls [`Network::on_wake_word`],
//!   which (if no turn is already active) spawns a turn task: resolve the host
//!   over mDNS → dial → run the [`crate::wyoming`] state machine.
//! - While a turn is active the capture thread calls [`Network::push_pcm`] with
//!   each resampled 16 kHz block; the samples are forwarded over a bounded
//!   channel to the turn task, which frames them as Wyoming `audio-chunk`s.
//! - Turn progress (connecting / streaming / transcript / disconnected) is
//!   emitted to Dart on the same [`StreamSink`] the wake-word events use.
//!
//! Full-duplex is preserved: wake-word scoring keeps running on the capture
//! thread throughout a turn (the engine just raises the confidence threshold
//! while `is_active()` — the AEC-interim mitigation).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::runtime::Runtime;
use tokio::sync::mpsc;

use crate::api::engine::{WakeWordConfig, WakeWordEvent};
use crate::frb_generated::StreamSink;
use crate::wyoming::{
    self, AudioFormat, EndpointCache, TurnUpdate, WyomingConnection, DEFAULT_DISCOVERY_TIMEOUT,
    DEFAULT_TURN_TIMEOUT,
};

/// Depth of the capture→turn PCM channel (in blocks). A handful of ~256 ms worth
/// of blocks absorbs network jitter; on overflow the newest block is dropped
/// rather than back-pressuring the real-time capture thread.
const PCM_CHANNEL_DEPTH: usize = 32;

/// Owns the tokio runtime and the state shared between the capture thread and the
/// in-flight Wyoming turn.
pub struct Network {
    runtime: Runtime,
    sink: StreamSink<WakeWordEvent>,
    cache: Arc<EndpointCache>,
    /// True while a turn task is in flight (gates re-triggers + PCM forwarding).
    active: Arc<AtomicBool>,
    /// Sender to the current turn's PCM channel, present only while active.
    pcm_tx: Arc<Mutex<Option<mpsc::Sender<Vec<i16>>>>>,
    discovery_timeout: Duration,
    turn_timeout: Duration,
}

impl Network {
    /// Build the network bridge and its runtime. A single worker thread keeps the
    /// footprint small on the Echo Show's ~1 GB budget while still driving
    /// spawned turn tasks without an explicit `block_on`.
    pub fn new(config: &WakeWordConfig, sink: StreamSink<WakeWordEvent>) -> anyhow::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("wyoming-net")
            .enable_all()
            .build()?;

        let discovery_timeout = match config.discovery_timeout_secs {
            0 => DEFAULT_DISCOVERY_TIMEOUT,
            n => Duration::from_secs(n),
        };
        let turn_timeout = match config.turn_timeout_secs {
            0 => DEFAULT_TURN_TIMEOUT,
            n => Duration::from_secs(n),
        };

        Ok(Self {
            runtime,
            sink,
            cache: Arc::new(EndpointCache::new()),
            active: Arc::new(AtomicBool::new(false)),
            pcm_tx: Arc::new(Mutex::new(None)),
            discovery_timeout,
            turn_timeout,
        })
    }

    /// Whether a Wyoming turn is currently in flight. The engine uses this to pick
    /// the (higher) active wake-word threshold and to decide whether to forward
    /// PCM.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    /// Called by the capture thread when the wake word fires. Starts a turn unless
    /// one is already running (duplicate triggers mid-turn are ignored in Phase
    /// 3). Uses `compare_exchange` so only one task is ever spawned per turn.
    pub fn on_wake_word(&self) {
        if self
            .active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return; // a turn is already active
        }

        let (tx, rx) = mpsc::channel::<Vec<i16>>(PCM_CHANNEL_DEPTH);
        *self.pcm_tx.lock().unwrap() = Some(tx);

        let sink = self.sink.clone();
        let cache = self.cache.clone();
        let active = self.active.clone();
        let pcm_tx = self.pcm_tx.clone();
        let discovery_timeout = self.discovery_timeout;
        let turn_timeout = self.turn_timeout;

        self.runtime.spawn(async move {
            run_turn_task(sink, cache, rx, discovery_timeout, turn_timeout).await;
            // Whatever happened, the turn is over: clear the sender and release
            // the active flag so the next wake word can start a fresh turn.
            *pcm_tx.lock().unwrap() = None;
            active.store(false, Ordering::SeqCst);
        });
    }

    /// Forward a resampled 16 kHz mono block to the active turn, if any. Never
    /// blocks the capture thread: a full or closed channel just drops the block.
    pub fn push_pcm(&self, samples: &[i16]) {
        if !self.is_active() {
            return;
        }
        if let Some(tx) = self.pcm_tx.lock().unwrap().as_ref() {
            let _ = tx.try_send(samples.to_vec());
        }
    }
}

impl Drop for Network {
    fn drop(&mut self) {
        // Signal any in-flight turn to stop forwarding, then let the runtime drop
        // abort outstanding tasks. `shutdown_background` avoids blocking the
        // engine thread on a task that is parked in a socket read.
        self.active.store(false, Ordering::SeqCst);
        *self.pcm_tx.lock().unwrap() = None;
    }
}

/// The body of one Wyoming turn: discover → connect → drive the state machine,
/// translating [`TurnUpdate`]s into Dart events. Errors are reported as a
/// `Disconnected` event and swallowed so a failed turn never poisons the engine.
async fn run_turn_task(
    sink: StreamSink<WakeWordEvent>,
    cache: Arc<EndpointCache>,
    pcm_rx: mpsc::Receiver<Vec<i16>>,
    discovery_timeout: Duration,
    turn_timeout: Duration,
) {
    let endpoint = match wyoming::resolve(&cache, discovery_timeout).await {
        Ok(ep) => ep,
        Err(e) => {
            let _ = sink.add(WakeWordEvent::disconnected(format!("no Wyoming host: {e}")));
            return;
        }
    };
    let _ = sink.add(WakeWordEvent::connecting(format!(
        "connecting to {endpoint}"
    )));

    let mut conn = match WyomingConnection::connect(&endpoint, AudioFormat::default()).await {
        Ok(c) => c,
        Err(e) => {
            // A stale cached endpoint may be why the dial failed; drop it so the
            // next turn re-browses instead of retrying a dead host.
            cache.clear();
            let _ = sink.add(WakeWordEvent::disconnected(format!("connect failed: {e}")));
            return;
        }
    };

    let on_update = |update: TurnUpdate| {
        let event = match update {
            TurnUpdate::Streaming => WakeWordEvent::streaming(),
            TurnUpdate::Transcript(text) => WakeWordEvent::transcript(text),
            TurnUpdate::Finished => WakeWordEvent::disconnected("turn complete".to_string()),
        };
        let _ = sink.add(event);
    };

    if let Err(e) = wyoming::run_turn(&mut conn, pcm_rx, on_update, turn_timeout).await {
        let _ = sink.add(WakeWordEvent::disconnected(format!("turn error: {e}")));
    }
}
