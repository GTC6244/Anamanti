//! On-device timer manager (Phase 2 device actions).
//!
//! A tool the LLM calls on the Mac emits an `anamanti-timer` frame during the voice
//! turn; the Wyoming client surfaces it as [`TurnUpdate::Timer`](crate::wyoming::TurnUpdate)
//! and the network bridge ([`crate::engine::net`]) applies it here. Each running
//! timer is a task spawned on the **long-lived** network runtime, so it keeps
//! counting after the turn's short-lived socket closes — and even if the Mac
//! disconnects. Rust owns the countdown and the alarm sound (via the shared
//! `cpal`/`oboe` [`PlaybackSink`], no second audio path); Flutter renders the
//! countdown from the `TimerStarted` event's deadline.
//!
//! When a timer fires it rings a loud two-strike bell locally, then — if the Mac is
//! reachable — asks the orchestrator (over a fresh Wyoming socket, same mDNS path a
//! turn uses) to synthesize "Time's up for {name}" with Piper and plays that audio
//! through the same `PlaybackSink`, right after the bell. When the Mac is offline the
//! timer is bell-only. The alarm then **repeats every [`ALARM_REPEAT`]** (bell + the
//! cached voice, no further Mac round-trips) until the timer is turned off — by voice
//! (a `cancel` command, e.g. "stop"), or by the display's "Cancel timer" button
//! ([`dismiss_timer`]). A repeat is skipped while a voice turn is in flight so the
//! alarm never talks over the user or the reply.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::BufReader;
use tokio::net::TcpStream;
use tokio::runtime::Handle;
use tokio::task::AbortHandle;

use crate::api::engine::WakeWordEvent;
use crate::audio::playback::PlaybackSink;
use crate::frb_generated::StreamSink;
use crate::wyoming::protocol::{self, types, TimerCommand, WyomingEvent};
use crate::wyoming::{resolve, EndpointCache};

/// Defense-in-depth cap on a timer (24h); the Mac side already clamps.
const MAX_TIMER_SECS: u64 = 24 * 60 * 60;

/// How often a ringing timer re-sounds its alarm (start to start) until turned off.
/// Stretched to the alarm's own length if that is longer, so repeats never pile up.
const ALARM_REPEAT: Duration = Duration::from_secs(5);

/// One running timer's cancellation handle + label (for label-scoped cancel) + the
/// instant it will fire (so the manager can report time-remaining as turn context).
struct TimerEntry {
    label: Option<String>,
    abort: AbortHandle,
    /// When this timer fires. Used only to compute remaining time for [`TimerManager::snapshot`];
    /// the countdown itself is driven by the spawned task's `sleep`.
    deadline: Instant,
    /// Set once the timer has fired and its alarm is repeating. A ringing timer stays
    /// registered (so it counts as running in the turn context, and "stop"/"cancel"
    /// reach it) until it is cancelled or dismissed.
    ringing: Arc<AtomicBool>,
}

/// A point-in-time summary of the running timers, stamped into each turn's `audio-start`
/// so the Core (and System-1) can answer "how much time is left?" and route bare
/// "stop"/"cancel" to the timer. Orthogonal to the foreground widget — a timer counts in
/// the background regardless of what screen is up. See
/// `plans/system1-fast-decisions.md` §17/§19.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TimerSnapshot {
    /// How many timers are currently running.
    pub running: u32,
    /// Whole seconds until the soonest-to-fire timer (0 if already due), or `None` when
    /// nothing is running.
    pub next_remaining_secs: Option<u64>,
    /// Non-empty labels of the running timers (for future labeled query/cancel).
    pub labels: Vec<String>,
}

impl TimerSnapshot {
    /// Serialize to the `screen.timers` JSON object the device stamps on `audio-start`.
    /// Byte-shape must match the Core's `protocol::timer_context` parser.
    pub fn to_json(&self) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        obj.insert("running".into(), self.running.into());
        if let Some(secs) = self.next_remaining_secs {
            obj.insert("next_remaining_secs".into(), secs.into());
        }
        if !self.labels.is_empty() {
            obj.insert("labels".into(), self.labels.clone().into());
        }
        serde_json::Value::Object(obj)
    }
}

/// Owns all running on-device timers. Cheap to `clone` for the per-turn `on_update`
/// closure — it shares the registry behind an `Arc`.
#[derive(Clone)]
pub struct TimerManager {
    inner: Arc<Inner>,
}

struct Inner {
    sink: StreamSink<WakeWordEvent>,
    playback: Option<Arc<PlaybackSink>>,
    handle: Handle,
    /// Shared mDNS endpoint cache (same one the turn path uses) so a fired timer can
    /// reach the orchestrator to voice its announcement, and the discovery timeout.
    cache: Arc<EndpointCache>,
    discovery_timeout: Duration,
    /// Pinned orchestrator selection key (`None` = Auto); a fired timer's Piper
    /// announcement resolves the same Mac the turns use.
    orchestrator_key: Option<String>,
    /// True while a voice turn is in flight (the network bridge's flag); a ringing
    /// alarm skips its repeat then, so it never talks over the user or the reply.
    turn_active: Arc<AtomicBool>,
    next_id: AtomicU32,
    timers: Mutex<HashMap<u32, TimerEntry>>,
}

impl TimerManager {
    /// Build a manager that emits timer events on `sink`, sounds the alarm through
    /// `playback` (when an output device exists), and spawns countdown tasks on
    /// `handle` (the network runtime). `cache`/`discovery_timeout` let a fired timer
    /// reach the orchestrator (via the same mDNS path as a turn) to voice its
    /// "Time's up …" announcement through Piper; when the Mac is unreachable the
    /// timer still rings the bell locally.
    pub fn new(
        sink: StreamSink<WakeWordEvent>,
        playback: Option<Arc<PlaybackSink>>,
        handle: Handle,
        cache: Arc<EndpointCache>,
        discovery_timeout: Duration,
        orchestrator_key: Option<String>,
        turn_active: Arc<AtomicBool>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                sink,
                playback,
                handle,
                cache,
                discovery_timeout,
                orchestrator_key,
                turn_active,
                next_id: AtomicU32::new(1),
                timers: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Apply a timer command relayed from the orchestrator.
    pub fn apply(&self, cmd: TimerCommand) {
        match cmd {
            TimerCommand::Start {
                label,
                duration_secs,
            } => self.start(label, duration_secs),
            TimerCommand::Cancel { label } => self.cancel(label),
            TimerCommand::Adjust { label, delta_secs } => self.adjust(label, delta_secs),
        }
    }

    fn start(&self, label: Option<String>, duration_secs: u64) {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        self.spawn_countdown(id, label, duration_secs);
    }

    /// Spawn the countdown task for timer `id` and register its [`TimerEntry`], clamping
    /// `duration_secs` to the valid range. Shared by [`start`](Self::start) (a fresh id)
    /// and [`adjust`](Self::adjust) (re-arming an existing id with new remaining time).
    /// Emits `timer_started(id, …, remaining)` so Flutter (re-)anchors the countdown and
    /// progress ring to the new deadline.
    fn spawn_countdown(&self, id: u32, label: Option<String>, duration_secs: u64) {
        let secs = duration_secs.clamp(1, MAX_TIMER_SECS);
        let inner = self.inner.clone();
        let task_label = label.clone();
        let ringing = Arc::new(AtomicBool::new(false));
        let task_ringing = ringing.clone();
        let task = self.inner.handle.spawn(async move {
            let label_str = task_label.as_deref().unwrap_or("");
            let _ = inner
                .sink
                .add(WakeWordEvent::timer_started(id, label_str, secs as u32));
            tokio::time::sleep(Duration::from_secs(secs)).await;
            // Fired: the entry stays registered (ringing) until cancelled/dismissed,
            // which aborts this task and so stops the repeats.
            task_ringing.store(true, Ordering::SeqCst);
            let _ = inner.sink.add(WakeWordEvent::timer_finished(id, label_str));
            let Some(pb) = inner.playback.clone() else {
                return; // no speaker: the screen shows "Time's up" until dismissed
            };
            // Sound the alarm through the same cpal layer that plays TTS: the bell,
            // then the announcement in the assistant's Piper voice (fetched once from
            // the orchestrator and cached for the repeats; bell-only when offline).
            let (bell, bell_rate) = alarm_pcm();
            let phrase = announce_phrase(task_label.as_deref());
            let mut voice: Option<(Vec<i16>, u32)> = None;
            let mut first = true;
            loop {
                if !inner.turn_active.load(Ordering::SeqCst) {
                    pb.submit_pcm(&bell, bell_rate);
                    if first {
                        first = false;
                        match synthesize_via_orchestrator(
                            &inner.cache,
                            inner.discovery_timeout,
                            inner.orchestrator_key.as_deref(),
                            &phrase,
                        )
                        .await
                        {
                            Ok(v) => voice = Some(v),
                            Err(e) => {
                                log::info!("timer voice announcement skipped ({phrase:?}): {e:#}")
                            }
                        }
                    }
                    if let Some((pcm, rate)) = &voice {
                        pb.submit_pcm(pcm, *rate);
                    }
                }
                let played = pcm_duration(bell.len(), bell_rate)
                    + voice
                        .as_ref()
                        .map_or(Duration::ZERO, |(pcm, rate)| pcm_duration(pcm.len(), *rate));
                tokio::time::sleep(repeat_interval(played)).await;
            }
        });
        self.inner.timers.lock().unwrap().insert(
            id,
            TimerEntry {
                label,
                abort: task.abort_handle(),
                deadline: Instant::now() + Duration::from_secs(secs),
                ringing,
            },
        );
    }

    /// A snapshot of the running timers for stamping into a turn's `audio-start` context.
    /// Cheap; takes the registry lock briefly. Remaining time is clamped at 0 for a timer
    /// that is due but whose firing task hasn't yet removed it.
    pub fn snapshot(&self) -> TimerSnapshot {
        let now = Instant::now();
        let timers = self.inner.timers.lock().unwrap();
        let running = timers.len() as u32;
        let next_remaining_secs = timers
            .values()
            .map(|e| e.deadline.saturating_duration_since(now).as_secs())
            .min();
        let labels = timers
            .values()
            .filter_map(|e| e.label.clone())
            .filter(|l| !l.is_empty())
            .collect();
        TimerSnapshot {
            running,
            next_remaining_secs,
            labels,
        }
    }

    /// Cancel timers matching `label` (every timer when `None`). **While any timer is
    /// ringing, an unlabeled cancel ("stop", "cancel the timer") only silences the
    /// ringing ones** — the user means "turn the alarm off", not "also throw away the
    /// other countdowns"; a second unlabeled cancel then clears the rest.
    fn cancel(&self, label: Option<String>) {
        let mut timers = self.inner.timers.lock().unwrap();
        let ringing_only =
            label.is_none() && timers.values().any(|e| e.ringing.load(Ordering::SeqCst));
        let ids: Vec<u32> = ids_matching(timers.iter().map(|(id, e)| (*id, &e.label)), &label)
            .into_iter()
            .filter(|id| !ringing_only || timers[id].ringing.load(Ordering::SeqCst))
            .collect();
        for id in ids {
            if let Some(entry) = timers.remove(&id) {
                entry.abort.abort();
                let _ = self.inner.sink.add(WakeWordEvent::timer_cancelled(id));
            }
        }
    }

    /// Turn off one timer by id — the display's "Cancel timer" button. Stops a ringing
    /// alarm's repeats (or cancels a running countdown) and emits `timer_cancelled`.
    /// A no-op for an unknown id.
    pub fn dismiss(&self, id: u32) {
        let entry = self.inner.timers.lock().unwrap().remove(&id);
        if let Some(entry) = entry {
            entry.abort.abort();
            let _ = self.inner.sink.add(WakeWordEvent::timer_cancelled(id));
        }
    }

    /// Add (`delta_secs > 0`) or remove (`delta_secs < 0`) time on timers matching
    /// `label` (every timer when `label` is `None`). The device owns the authoritative
    /// deadline, so each matched timer's *remaining* time is recomputed from its own
    /// deadline, shifted by `delta_secs`, and floored at 1s (removing more than is left
    /// just rings it momentarily) — then its countdown task is aborted and re-armed with
    /// the new remaining time. `spawn_countdown` re-emits `timer_started` so Flutter
    /// re-anchors the countdown/ring to the new deadline.
    fn adjust(&self, label: Option<String>, delta_secs: i64) {
        let now = Instant::now();
        // Collect the re-arm plan under the lock (id, label, new remaining), aborting the
        // old tasks and removing their entries as we go; spawn outside the lock.
        let rearm: Vec<(u32, Option<String>, u64)> = {
            let mut timers = self.inner.timers.lock().unwrap();
            // A timer that already fired (ringing) isn't re-armed — only countdowns are.
            let ids: Vec<u32> = ids_matching(timers.iter().map(|(id, e)| (*id, &e.label)), &label)
                .into_iter()
                .filter(|id| !timers[id].ringing.load(Ordering::SeqCst))
                .collect();
            ids.into_iter()
                .filter_map(|id| {
                    let entry = timers.remove(&id)?;
                    entry.abort.abort();
                    let remaining = entry.deadline.saturating_duration_since(now).as_secs() as i64;
                    let new_secs = remaining.saturating_add(delta_secs).max(1) as u64;
                    Some((id, entry.label, new_secs))
                })
                .collect()
        };
        for (id, entry_label, new_secs) in rearm {
            self.spawn_countdown(id, entry_label, new_secs);
        }
    }
}

/// The running engine's timer manager, published so the display's "Cancel timer"
/// button ([`dismiss_timer`]) can reach it. Set by the network bridge on start and
/// cleared on teardown.
static MANAGER: OnceLock<Mutex<Option<TimerManager>>> = OnceLock::new();

fn manager_slot() -> &'static Mutex<Option<TimerManager>> {
    MANAGER.get_or_init(|| Mutex::new(None))
}

/// Publish (or, with `None`, withdraw) the running engine's timer manager.
pub fn set_manager(manager: Option<TimerManager>) {
    *manager_slot().lock().unwrap() = manager;
}

/// Turn off timer `id` on the running engine (a no-op when no engine is running).
pub fn dismiss_timer(id: u32) {
    let manager = manager_slot().lock().unwrap().clone();
    if let Some(m) = manager {
        m.dismiss(id);
    }
}

/// How long a buffer of `samples` mono samples at `rate` Hz plays for.
fn pcm_duration(samples: usize, rate: u32) -> Duration {
    Duration::from_secs_f64(samples as f64 / rate.max(1) as f64)
}

/// The wait before the next alarm repeat, given how long one alarm (bell + voice)
/// plays: every [`ALARM_REPEAT`], stretched so a long alarm still gets a short pause
/// instead of queueing repeats back to back.
fn repeat_interval(played: Duration) -> Duration {
    ALARM_REPEAT.max(played + Duration::from_millis(750))
}

/// Ids of the timers a cancel targets: those whose label equals `label`, or *every*
/// timer when `label` is `None`. Pure (no registry types) so it is unit-testable.
fn ids_matching<'a>(
    entries: impl Iterator<Item = (u32, &'a Option<String>)>,
    label: &Option<String>,
) -> Vec<u32> {
    entries
        .filter(|(_, entry_label)| match label {
            Some(want) => entry_label.as_deref() == Some(want.as_str()),
            None => true,
        })
        .map(|(id, _)| id)
        .collect()
}

/// Synthesize a bell alarm — two struck-bell tones — as mono `i16` PCM at the device
/// rate. Returns the samples plus their sample rate, suitable for
/// [`PlaybackSink::submit_pcm`].
///
/// Each strike is a sum of inharmonic partials (which gives the metallic, bell-like
/// timbre a pure sine lacks), each with an exponential decay — fast attack, long
/// ring-out — so it reads as a bell being rung rather than a beep. The whole alarm is
/// two such strikes separated by a short gap ("ring it twice").
/// The alarm bell's peak level as a fraction of full scale (was ~0.35 before the
/// louder alarm; ~0.95 is about +8.7 dB while leaving headroom against clipping).
const ALARM_PEAK: f32 = 0.95;

fn alarm_pcm() -> (Vec<i16>, u32) {
    const RATE: u32 = crate::audio::TARGET_SAMPLE_RATE;
    const STRIKES: u32 = 2;
    const STRIKE_MS: u32 = 1100; // strike + ring-out
    const GAP_MS: u32 = 260; // silence between the two rings
    const BASE_FREQ: f32 = 660.0;
    // (frequency multiplier, relative amplitude, decay rate in 1/s). Inharmonic
    // ratios + faster decay on the higher partials = a struck-bell timbre.
    const PARTIALS: [(f32, f32, f32); 4] = [
        (1.0, 1.0, 2.8),
        (2.0, 0.6, 3.8),
        (2.76, 0.4, 5.0),
        (5.4, 0.25, 7.0),
    ];

    let strike_len = (RATE * STRIKE_MS / 1000) as usize;
    let gap_len = (RATE * GAP_MS / 1000) as usize;
    let attack = (RATE as f32 * 0.004).max(1.0) as usize; // ~4 ms, avoids an onset click
                                                          // One strike, unscaled, then normalised so its loudest sample sits at
                                                          // [`ALARM_PEAK`] of full scale — a loud alarm that still never clips.
    let strike: Vec<f32> = (0..strike_len)
        .map(|n| {
            let t = n as f32 / RATE as f32;
            let mut s = 0.0f32;
            for (mult, amp, decay) in PARTIALS {
                s += amp
                    * (2.0 * std::f32::consts::PI * BASE_FREQ * mult * t).sin()
                    * (-t * decay).exp();
            }
            s * (n as f32 / attack as f32).clamp(0.0, 1.0) // soft attack
        })
        .collect();
    let peak = strike
        .iter()
        .fold(0.0f32, |m, s| m.max(s.abs()))
        .max(f32::EPSILON);
    let gain = ALARM_PEAK / peak;
    let mut out = Vec::with_capacity((strike_len + gap_len) * STRIKES as usize);
    for _ in 0..STRIKES {
        out.extend(
            strike
                .iter()
                .map(|s| ((s * gain).clamp(-1.0, 1.0) * i16::MAX as f32) as i16),
        );
        out.extend(std::iter::repeat_n(0i16, gap_len));
    }
    (out, RATE)
}

/// The phrase a fired timer asks the orchestrator to speak: named timers announce
/// their name ("Time's up for pasta"); an unnamed timer is just "Time's up".
fn announce_phrase(label: Option<&str>) -> String {
    match label.map(str::trim).filter(|s| !s.is_empty()) {
        Some(name) => format!("Time's up for {name}"),
        None => "Time's up".to_string(),
    }
}

/// Ask the orchestrator to synthesize `text` (Piper) and return the audio as mono
/// `i16` PCM + its sample rate, so a ringing alarm can replay it on every repeat
/// without another round-trip. Opens a fresh Wyoming socket to the mDNS-resolved host,
/// sends one `anamanti-speak` frame, then collects the `audio-start`/`audio-chunk`…/
/// `audio-stop` reply. Errors (offline, no host, dropped socket) are returned so the
/// caller can degrade to bell-only.
async fn synthesize_via_orchestrator(
    cache: &EndpointCache,
    discovery_timeout: Duration,
    preferred: Option<&str>,
    text: &str,
) -> anyhow::Result<(Vec<i16>, u32)> {
    let endpoint = resolve(cache, discovery_timeout, preferred).await?;
    let stream = TcpStream::connect(endpoint.socket_addr()).await?;
    stream.set_nodelay(true).ok();
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    protocol::write_event(&mut write_half, &WyomingEvent::speak(text)).await?;

    let mut rate = crate::audio::TARGET_SAMPLE_RATE;
    let mut pcm = Vec::new();
    while let Some(ev) = protocol::read_event(&mut reader).await? {
        match ev.event_type.as_str() {
            types::AUDIO_START => {
                if let Some(fmt) = protocol::audio_format(&ev.data) {
                    rate = fmt.rate;
                }
            }
            types::AUDIO_CHUNK => {
                if let Some(payload) = &ev.payload {
                    pcm.extend(decode_pcm_i16(payload));
                }
            }
            types::AUDIO_STOP => break,
            _ => {} // ignore any other frames on this out-of-band socket
        }
    }
    anyhow::ensure!(!pcm.is_empty(), "orchestrator returned no audio");
    Ok((pcm, rate))
}

/// Decode a little-endian `i16` PCM payload into samples (a trailing odd byte, never
/// expected from a well-formed server, is ignored).
fn decode_pcm_i16(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_matching_selects_by_label_or_all() {
        let entries = [
            (1u32, Some("pasta".to_string())),
            (2u32, None),
            (3u32, Some("pasta".to_string())),
            (4u32, Some("laundry".to_string())),
        ];
        // Label-scoped: both "pasta" timers, nothing else.
        let mut got = ids_matching(
            entries.iter().map(|(id, l)| (*id, l)),
            &Some("pasta".into()),
        );
        got.sort_unstable();
        assert_eq!(got, [1, 3]);
        // No label: every timer.
        let mut all = ids_matching(entries.iter().map(|(id, l)| (*id, l)), &None);
        all.sort_unstable();
        assert_eq!(all, [1, 2, 3, 4]);
        // Unknown label: none.
        assert!(
            ids_matching(entries.iter().map(|(id, l)| (*id, l)), &Some("nope".into())).is_empty()
        );
    }

    #[test]
    fn timer_snapshot_to_json_matches_wire_shape() {
        // Empty snapshot: only `running` (0); no remaining/labels keys.
        let empty = TimerSnapshot::default();
        assert_eq!(empty.to_json(), serde_json::json!({ "running": 0 }));

        // Full snapshot: remaining + labels included.
        let full = TimerSnapshot {
            running: 2,
            next_remaining_secs: Some(125),
            labels: vec!["pasta".into()],
        };
        assert_eq!(
            full.to_json(),
            serde_json::json!({ "running": 2, "next_remaining_secs": 125, "labels": ["pasta"] })
        );

        // Running but unlabeled with unknown remaining: labels/remaining omitted.
        let unlabeled = TimerSnapshot {
            running: 1,
            next_remaining_secs: None,
            labels: vec![],
        };
        assert_eq!(unlabeled.to_json(), serde_json::json!({ "running": 1 }));
    }

    #[test]
    fn announce_phrase_uses_name_or_falls_back() {
        assert_eq!(announce_phrase(Some("pasta")), "Time's up for pasta");
        // Whitespace-only labels are treated as unnamed.
        assert_eq!(announce_phrase(Some("   ")), "Time's up");
        assert_eq!(announce_phrase(None), "Time's up");
    }

    #[test]
    fn alarm_repeats_every_five_seconds_or_after_a_long_alarm() {
        assert_eq!(
            repeat_interval(Duration::from_secs(3)),
            Duration::from_secs(5)
        );
        assert_eq!(
            repeat_interval(Duration::from_secs(6)),
            Duration::from_millis(6750)
        );
        assert_eq!(pcm_duration(32_000, 16_000), Duration::from_secs(2));
    }

    #[test]
    fn decode_pcm_i16_reads_le_pairs_and_ignores_odd_tail() {
        assert_eq!(decode_pcm_i16(&[1, 0, 0, 1]), vec![1, 256]);
        assert_eq!(decode_pcm_i16(&[1, 0, 9]), vec![1]); // trailing odd byte dropped
        assert!(decode_pcm_i16(&[]).is_empty());
    }

    #[test]
    fn alarm_pcm_rings_twice_and_is_audible() {
        let (pcm, rate) = alarm_pcm();
        assert_eq!(rate, crate::audio::TARGET_SAMPLE_RATE);
        let strike = (rate * 1100 / 1000) as usize;
        let gap = (rate * 260 / 1000) as usize;
        // Two strikes, each followed by a gap.
        assert_eq!(pcm.len(), (strike + gap) * 2);
        // Loud: the bell peaks near full scale (but never clips).
        let peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(peak as f32 > 0.9 * i16::MAX as f32, "peak {peak}");
        assert!(peak < i16::MAX as u16);
        // Rung twice: the gap before the second strike is silent, and the second
        // strike then sounds — so there is audible energy in both halves.
        let half = pcm.len() / 2;
        assert!(pcm[..half].iter().any(|&s| s.abs() > i16::MAX / 8));
        assert!(pcm[half..].iter().any(|&s| s.abs() > i16::MAX / 8));
    }
}
