//! Bridge from the (`!Send`, real-time) capture/playback thread to the async
//! Wyoming turn (Plan.MD §3, Phases 3 & 5; architecture.md §4).
//!
//! The engine's capture/inference loop runs on a dedicated std thread because the
//! `cpal` streams are `!Send`. The Wyoming client, by contrast, is async `tokio`
//! networking. [`Network`] owns a small single-worker tokio runtime and marshals
//! between the two worlds:
//!
//! - On a wake-word detection the capture thread calls [`Network::on_wake_word`].
//!   If no turn is active it spawns one: resolve the host over mDNS → dial → run
//!   the [`crate::wyoming`] state machine. If a turn *is* active, the call is a
//!   **barge-in**: it flushes playback, interrupts the running turn, and requests
//!   a fresh turn once the current one tears down (Plan.MD §3, Phase 5).
//! - While a turn is active the capture thread calls [`Network::push_pcm`] with
//!   each resampled 16 kHz block; the samples are forwarded over a bounded channel
//!   to the turn task, which frames them as Wyoming `audio-chunk`s.
//! - Returned TTS audio is handed to the shared [`PlaybackSink`] so the same
//!   `cpal` layer that captured the mic plays the reply back.
//! - Turn progress (connecting / streaming / transcript / reply-token / speaking /
//!   disconnected) is emitted to Dart on the same [`StreamSink`] as wake-word
//!   events.
//!
//! Full-duplex is preserved: wake-word scoring keeps running on the capture thread
//! throughout a turn (the engine raises the confidence threshold while
//! `is_active()` — the AEC-interim mitigation).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::runtime::{Handle, Runtime};
use tokio::sync::mpsc;

use crate::api::engine::{WakeWordConfig, WakeWordEvent};
use crate::audio::playback::PlaybackSink;
use crate::engine::timer::TimerManager;
use crate::frb_generated::StreamSink;
use crate::wyoming::{
    self, AudioFormat, EndpointCache, TurnUpdate, WyomingConnection, DEFAULT_DISCOVERY_TIMEOUT,
    DEFAULT_TURN_TIMEOUT,
};

/// Depth of the capture→turn PCM channel (in blocks). A handful of ~256 ms worth
/// of blocks absorbs network jitter; on overflow the newest block is dropped
/// rather than back-pressuring the real-time capture thread.
const PCM_CHANNEL_DEPTH: usize = 32;

/// How often the post-turn watcher polls the playback ring to see whether the reply
/// audio has finished draining (so the UI can drop the on-screen text).
const PLAYBACK_DRAIN_POLL: Duration = Duration::from_millis(100);

/// Safety cap on the drain watch. The playback ring holds at most a handful of tens
/// of seconds of audio, so if it hasn't drained by now the stream is wedged; emit
/// `SpeakingDone` anyway rather than leak the task or pin the text on screen forever.
const PLAYBACK_DRAIN_CAP: Duration = Duration::from_secs(45);

/// State shared between the capture thread and every (possibly restarted) turn
/// task. Held in an `Arc` so a barge-in restart, spawned from the finishing
/// turn's own cleanup, can re-enter [`Shared::spawn_turn`].
struct Shared {
    sink: StreamSink<WakeWordEvent>,
    cache: Arc<EndpointCache>,
    /// True while a turn task is in flight (gates re-triggers + PCM forwarding).
    active: AtomicBool,
    /// Sender to the current turn's PCM channel, present only while active.
    pcm_tx: Mutex<Option<mpsc::Sender<Vec<i16>>>>,
    /// Barge-in signal to the current turn, present only while active.
    interrupt_tx: Mutex<Option<mpsc::Sender<()>>>,
    /// Set when a wake word barges in mid-turn; the finishing turn starts a fresh
    /// one when it sees this.
    pending_restart: AtomicBool,
    /// Speaker playback sink for returned TTS frames (absent if no output device).
    playback: Option<Arc<PlaybackSink>>,
    /// On-device timers (Phase 2). Long-lived so countdowns outlive the turn socket.
    timers: TimerManager,
    discovery_timeout: Duration,
    turn_timeout: Duration,
    /// Stable selection key of the pinned orchestrator; `None` = Auto (first
    /// available). Threaded into every `resolve` so voice turns hit the selected
    /// Mac (strict: stay offline if it's unreachable).
    orchestrator_key: Option<String>,
}

/// Owns the tokio runtime and the shared turn state.
pub struct Network {
    runtime: Runtime,
    shared: Arc<Shared>,
}

impl Network {
    /// Build the network bridge and its runtime. A single worker thread keeps the
    /// footprint small on the Echo Show's ~1 GB budget while still driving spawned
    /// turn tasks without an explicit `block_on`. `playback`, when present, is the
    /// shared sink used to play returned TTS audio.
    pub fn new(
        config: &WakeWordConfig,
        sink: StreamSink<WakeWordEvent>,
        playback: Option<Arc<PlaybackSink>>,
    ) -> anyhow::Result<Self> {
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
        // Empty selection key = Auto (first available orchestrator).
        let orchestrator_key = {
            let k = config.orchestrator_key.trim();
            if k.is_empty() {
                None
            } else {
                Some(k.to_string())
            }
        };

        // Shared mDNS endpoint cache: used both by the turn path and by a fired
        // timer voicing its announcement through the orchestrator.
        let cache = Arc::new(EndpointCache::new());

        // The timer manager shares the event sink + playback and spawns countdown
        // tasks on this runtime (so they outlive any single turn's socket). It also
        // shares the endpoint cache so a fired timer can reach the orchestrator for
        // its Piper "Time's up …" announcement (bell-only when offline).
        let timers = TimerManager::new(
            sink.clone(),
            playback.clone(),
            runtime.handle().clone(),
            cache.clone(),
            discovery_timeout,
            orchestrator_key.clone(),
        );

        Ok(Self {
            runtime,
            shared: Arc::new(Shared {
                sink,
                cache,
                active: AtomicBool::new(false),
                pcm_tx: Mutex::new(None),
                interrupt_tx: Mutex::new(None),
                pending_restart: AtomicBool::new(false),
                playback,
                timers,
                discovery_timeout,
                turn_timeout,
                orchestrator_key,
            }),
        })
    }

    /// Whether a Wyoming turn is currently in flight. The engine uses this to pick
    /// the (higher) active wake-word threshold and to decide whether to forward
    /// PCM.
    pub fn is_active(&self) -> bool {
        self.shared.active.load(Ordering::SeqCst)
    }

    /// Called by the capture thread when the wake word fires.
    ///
    /// **Always flush playback first.** The orchestrator relays a whole reply faster
    /// than real-time, so a turn reaches `Idle` (its TTS `audio-stop` arrives) within
    /// a second or two, while the audio itself keeps playing out of the multi-second
    /// playback ring for much longer. So a wake word spoken *while the reply is still
    /// audible* usually lands when no turn is technically active — yet the user
    /// clearly means "stop talking and listen." Clearing the playback ring on every
    /// wake word makes that barge-in feel instant whether or not a turn is in flight.
    ///
    /// If a turn *is* still active (STT/relay window), also interrupt it (sending the
    /// `anamanti-interrupt` frame so the orchestrator aborts generation/TTS) and
    /// request a restart so the finishing turn spawns a fresh one.
    pub fn on_wake_word(&self) {
        // Silence any in-flight or still-draining reply immediately.
        if let Some(pb) = &self.shared.playback {
            pb.clear();
        }

        if self
            .shared
            .active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            // A wake word always starts an ordinary turn (depth 0), never a follow-up.
            Shared::spawn_turn(self.shared.clone(), self.runtime.handle().clone(), 0, 0);
        } else {
            self.shared.pending_restart.store(true, Ordering::SeqCst);
            if let Some(tx) = self.shared.interrupt_tx.lock().unwrap().as_ref() {
                let _ = tx.try_send(());
            }
        }
    }

    /// Forward a resampled 16 kHz mono block to the active turn, if any. Never
    /// blocks the capture thread: a full or closed channel just drops the block.
    pub fn push_pcm(&self, samples: &[i16]) {
        if !self.is_active() {
            return;
        }
        if let Some(tx) = self.shared.pcm_tx.lock().unwrap().as_ref() {
            let _ = tx.try_send(samples.to_vec());
        }
    }
}

impl Drop for Network {
    fn drop(&mut self) {
        // Signal any in-flight turn to stop forwarding + restart, then let the
        // runtime drop abort outstanding tasks. `shutdown_background` (implicit on
        // `Runtime` drop) avoids blocking the engine thread on a parked socket read.
        self.shared.active.store(false, Ordering::SeqCst);
        self.shared.pending_restart.store(false, Ordering::SeqCst);
        *self.shared.pcm_tx.lock().unwrap() = None;
        if let Some(tx) = self.shared.interrupt_tx.lock().unwrap().as_ref() {
            let _ = tx.try_send(());
        }
    }
}

impl Shared {
    /// Spawn one turn task on `handle`, wiring up its PCM + interrupt channels. On
    /// completion it clears the per-turn state and, if a barge-in requested a
    /// restart, immediately spawns the next turn. `followup_depth` is 0 for an
    /// ordinary turn and >0 for a follow-up turn the device auto-opened after a reply
    /// (stamped, with `followup_wait_secs`, on the turn's `audio-start`).
    fn spawn_turn(shared: Arc<Self>, handle: Handle, followup_depth: u32, followup_wait_secs: u32) {
        let (pcm_tx, pcm_rx) = mpsc::channel::<Vec<i16>>(PCM_CHANNEL_DEPTH);
        let (int_tx, int_rx) = mpsc::channel::<()>(1);
        *shared.pcm_tx.lock().unwrap() = Some(pcm_tx);
        *shared.interrupt_tx.lock().unwrap() = Some(int_tx);

        let shared_task = shared.clone();
        handle.spawn(async move {
            run_turn_task(
                &shared_task,
                pcm_rx,
                int_rx,
                followup_depth,
                followup_wait_secs,
            )
            .await;
            *shared_task.pcm_tx.lock().unwrap() = None;
            *shared_task.interrupt_tx.lock().unwrap() = None;
            shared_task.active.store(false, Ordering::SeqCst);

            // Barge-in restart: a wake word fired mid-turn → begin a fresh (ordinary)
            // turn, unless the engine is shutting down (Drop cleared `pending_restart`).
            if shared_task.pending_restart.swap(false, Ordering::SeqCst)
                && shared_task
                    .active
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                Shared::spawn_turn(shared_task.clone(), Handle::current(), 0, 0);
            }
        });
    }
}

/// The body of one Wyoming turn: discover → connect → drive the state machine,
/// translating [`TurnUpdate`]s into Dart events and returned TTS audio into
/// playback. Errors are reported as a `Disconnected` event and swallowed so a
/// failed turn never poisons the engine.
async fn run_turn_task(
    shared: &Arc<Shared>,
    pcm_rx: mpsc::Receiver<Vec<i16>>,
    interrupt: mpsc::Receiver<()>,
    followup_depth: u32,
    followup_wait_secs: u32,
) {
    let sink = &shared.sink;

    // A follow-up turn (the device reopened the mic after a reply, no wake word) tells
    // the UI so it can show a "listening for your reply" affordance.
    if followup_depth > 0 {
        log::info!(
            "follow-up turn: listening for input for {followup_wait_secs}s (depth {followup_depth})"
        );
        let _ = sink.add(WakeWordEvent::listening_followup());
    }

    let endpoint = match wyoming::resolve(
        &shared.cache,
        shared.discovery_timeout,
        shared.orchestrator_key.as_deref(),
    )
    .await
    {
        Ok(ep) => ep,
        Err(e) => {
            log::warn!("turn: no Wyoming host: {e}");
            let _ = sink.add(WakeWordEvent::disconnected(format!("no Wyoming host: {e}")));
            return;
        }
    };
    log::info!("turn: connecting to {endpoint}");
    let _ = sink.add(WakeWordEvent::connecting(format!(
        "connecting to {endpoint}"
    )));

    let mut conn = match WyomingConnection::connect(&endpoint, AudioFormat::default()).await {
        Ok(c) => c,
        Err(e) => {
            // A stale cached endpoint may be why the dial failed; drop it so the
            // next turn re-browses instead of retrying a dead host.
            shared.cache.clear();
            log::warn!("turn: connect failed: {e}");
            let _ = sink.add(WakeWordEvent::disconnected(format!("connect failed: {e}")));
            return;
        }
    };
    // On a follow-up turn, stamp the chain depth + echo the listen window so the
    // orchestrator seeds the prompt with recent history, bounds the chain, and sizes
    // its no-speech window (no-op when depth is 0).
    conn.set_followup(followup_depth, followup_wait_secs);
    // Stamp the current display context (e.g. recipe tab/scroll state) on this turn's
    // `audio-start`, so the orchestrator's LLM knows what the display is showing and can
    // drive it (switch tabs / scroll / close). `None` on an idle screen.
    conn.set_display_context(crate::engine::display_context());

    // Idle watchdog for this turn: a follow-up turn keeps the mic open for the listen
    // window; give the device a small margin past it so the orchestrator's no-speech
    // finalize (at `followup_wait_secs`) ends the turn first, with this as a backstop.
    let turn_timeout = if followup_depth > 0 && followup_wait_secs > 0 {
        Duration::from_secs(followup_wait_secs as u64) + shared.turn_timeout
    } else {
        shared.turn_timeout
    };

    let playback = shared.playback.clone();
    let timers = shared.timers.clone();
    // Set when this turn reaches SPEAKING, so we only watch for playback drain on a
    // turn that actually produced audio (see the drain watcher below).
    let spoke = AtomicBool::new(false);
    // Set when the orchestrator asks (mid-turn) to listen for a follow-up after its
    // reply: the chain depth and the listen window the follow-up turn should use. The
    // mic is reopened only after the reply audio drains (see the drain watcher below).
    let pending_followup_depth = AtomicU32::new(0);
    let pending_followup_wait = AtomicU32::new(0);
    let on_update = |update: TurnUpdate| {
        let event = match update {
            TurnUpdate::Streaming => {
                log::info!("turn: streaming mic to STT");
                WakeWordEvent::streaming()
            }
            TurnUpdate::Transcript(text) => {
                log::info!("turn: transcript {text:?}");
                WakeWordEvent::transcript(text)
            }
            TurnUpdate::ReplyToken(text) => WakeWordEvent::reply_token(text),
            TurnUpdate::Speaking => {
                log::info!("turn: speaking (TTS playback started)");
                spoke.store(true, Ordering::SeqCst);
                WakeWordEvent::speaking()
            }
            // A device action: hand it to the long-lived timer manager, which emits
            // its own timer events + alarm. Nothing to add on the sink here.
            TurnUpdate::Timer(cmd) => {
                log::info!("turn: timer command {cmd:?}");
                timers.apply(cmd);
                return;
            }
            // A recipe command: surface it to the UI (which owns the recipe screen).
            // `Show` carries the recipe object, serialized to a JSON string for the
            // FRB boundary; `Dismiss` closes the screen.
            TurnUpdate::Recipe(cmd) => match cmd {
                wyoming::protocol::RecipeCommand::Show(recipe) => {
                    log::info!("turn: show recipe");
                    WakeWordEvent::show_recipe(recipe.to_string())
                }
                wyoming::protocol::RecipeCommand::Dismiss => {
                    log::info!("turn: dismiss recipe");
                    WakeWordEvent::dismiss_recipe()
                }
                wyoming::protocol::RecipeCommand::Navigate(target) => {
                    log::info!("turn: recipe navigate to {target}");
                    WakeWordEvent::recipe_navigate(target)
                }
                wyoming::protocol::RecipeCommand::Scroll(direction) => {
                    log::info!("turn: recipe scroll {direction}");
                    WakeWordEvent::recipe_scroll(direction)
                }
            },
            // Record the request to reopen the mic after this reply. Acted on only after
            // this turn's reply audio drains (drain watcher below), so the follow-up mic
            // never records the tail of the TTS. Nothing to add here.
            TurnUpdate::ListenFollowup(depth, wait_secs) => {
                log::info!("turn: follow-up listen requested (depth {depth}, {wait_secs}s)");
                pending_followup_depth.store(depth, Ordering::SeqCst);
                pending_followup_wait.store(wait_secs, Ordering::SeqCst);
                return;
            }
            TurnUpdate::Finished => {
                log::info!("turn: finished cleanly");
                WakeWordEvent::disconnected("turn complete".to_string())
            }
        };
        let _ = sink.add(event);
    };
    let on_audio = |pcm: &[i16], rate: u32| {
        if let Some(pb) = playback.as_ref() {
            pb.submit_pcm(pcm, rate);
        }
    };

    if let Err(e) = wyoming::run_turn(
        &mut conn,
        pcm_rx,
        on_update,
        on_audio,
        interrupt,
        turn_timeout,
    )
    .await
    {
        log::error!("turn error (aborting turn): {e:#}");
        let _ = sink.add(WakeWordEvent::disconnected(format!("turn error: {e}")));
    }

    // The turn has reached idle, but — because the orchestrator relays a whole reply
    // faster than real-time — the reply audio is usually still draining from the
    // playback ring for seconds after this point. The UI keeps the reply text on
    // screen until the audio actually stops, so watch the ring drain (or a barge-in
    // flush) and then emit `SpeakingDone` so the UI can remove the text. Runs
    // detached, holding no turn state, so a barge-in during drain still sees an
    // inactive turn and simply flushes.
    //
    // The same drain point gates a **follow-up listen**: if the orchestrator asked to
    // reopen the mic (`pending_followup_depth > 0`), do so with no wake word — but only
    // once the reply audio has drained, so the follow-up turn's mic never records the
    // tail of our own TTS.
    let followup = pending_followup_depth.load(Ordering::SeqCst);
    let followup_wait = pending_followup_wait.load(Ordering::SeqCst);
    let did_speak = spoke.load(Ordering::SeqCst);
    if did_speak || followup > 0 {
        let watch_shared = shared.clone();
        let playback = shared.playback.clone();
        tokio::spawn(async move {
            if let Some(pb) = &playback {
                let deadline = tokio::time::Instant::now() + PLAYBACK_DRAIN_CAP;
                while pb.pending() > 0 && tokio::time::Instant::now() < deadline {
                    tokio::time::sleep(PLAYBACK_DRAIN_POLL).await;
                }
                if did_speak {
                    let _ = watch_shared.sink.add(WakeWordEvent::speaking_done());
                }
            }

            // Reopen the mic for the follow-up. The `active` compare-exchange makes
            // this lose to a barge-in restart (which already spawned a fresh turn),
            // so we never double-start; `!pending_restart` skips it outright when a
            // barge-in is pending.
            if followup > 0
                && !watch_shared.pending_restart.load(Ordering::SeqCst)
                && watch_shared
                    .active
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                log::info!(
                    "follow-up: reopening mic for input ({followup_wait}s, depth {followup})"
                );
                Shared::spawn_turn(
                    watch_shared.clone(),
                    Handle::current(),
                    followup,
                    followup_wait,
                );
            }
        });
    }
}
