//! On-device timer manager (Phase 2 device actions).
//!
//! A tool the LLM calls on the Mac emits an `ambient-timer` frame during the voice
//! turn; the Wyoming client surfaces it as [`TurnUpdate::Timer`](crate::wyoming::TurnUpdate)
//! and the network bridge ([`crate::engine::net`]) applies it here. Each running
//! timer is a task spawned on the **long-lived** network runtime, so it keeps
//! counting after the turn's short-lived socket closes — and even if the Mac
//! disconnects. Rust owns the countdown and the alarm sound (via the shared
//! `cpal`/`oboe` [`PlaybackSink`], no second audio path); Flutter renders the
//! countdown from the `TimerStarted` event's deadline.
//!
//! When a timer fires it rings a two-strike bell locally, then — if the Mac is
//! reachable — asks the orchestrator (over a fresh Wyoming socket, same mDNS path a
//! turn uses) to synthesize "Time's up for {name}" with Piper and streams that audio
//! back into the same `PlaybackSink`, so the voice plays right after the bell. When
//! the Mac is offline the timer is bell-only.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

/// One running timer's cancellation handle + label (for label-scoped cancel).
struct TimerEntry {
    label: Option<String>,
    abort: AbortHandle,
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
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                sink,
                playback,
                handle,
                cache,
                discovery_timeout,
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
        }
    }

    fn start(&self, label: Option<String>, duration_secs: u64) {
        let secs = duration_secs.clamp(1, MAX_TIMER_SECS);
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let inner = self.inner.clone();
        let task_label = label.clone();
        let task = self.inner.handle.spawn(async move {
            let label_str = task_label.as_deref().unwrap_or("");
            let _ = inner
                .sink
                .add(WakeWordEvent::timer_started(id, label_str, secs as u32));
            tokio::time::sleep(Duration::from_secs(secs)).await;
            // Sound the alarm through the same cpal layer that plays TTS: ring the
            // bell twice, then voice the announcement in the assistant's Piper voice.
            if let Some(pb) = &inner.playback {
                let (chime, rate) = alarm_pcm();
                pb.submit_pcm(&chime, rate);
                // The voice is appended to the same playback ring, so it plays right
                // after the two bell rings. Ask the orchestrator to synthesize it;
                // best-effort — if the Mac is unreachable the timer is bell-only.
                let phrase = announce_phrase(task_label.as_deref());
                if let Err(e) =
                    speak_via_orchestrator(&inner.cache, inner.discovery_timeout, pb, &phrase).await
                {
                    log::info!("timer voice announcement skipped ({phrase:?}): {e:#}");
                }
            }
            let _ = inner.sink.add(WakeWordEvent::timer_finished(id, label_str));
            inner.timers.lock().unwrap().remove(&id);
        });
        self.inner.timers.lock().unwrap().insert(
            id,
            TimerEntry {
                label,
                abort: task.abort_handle(),
            },
        );
    }

    fn cancel(&self, label: Option<String>) {
        let mut timers = self.inner.timers.lock().unwrap();
        let ids = ids_matching(timers.iter().map(|(id, e)| (*id, &e.label)), &label);
        for id in ids {
            if let Some(entry) = timers.remove(&id) {
                entry.abort.abort();
                let _ = self.inner.sink.add(WakeWordEvent::timer_cancelled(id));
            }
        }
    }
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
fn alarm_pcm() -> (Vec<i16>, u32) {
    const RATE: u32 = crate::audio::TARGET_SAMPLE_RATE;
    const STRIKES: u32 = 2;
    const STRIKE_MS: u32 = 1100; // strike + ring-out
    const GAP_MS: u32 = 260; // silence between the two rings
    const BASE_FREQ: f32 = 660.0;
    // (frequency multiplier, relative amplitude, decay rate in 1/s). Inharmonic
    // ratios + faster decay on the higher partials = a struck-bell timbre.
    const PARTIALS: [(f32, f32, f32); 4] =
        [(1.0, 1.0, 2.8), (2.0, 0.6, 3.8), (2.76, 0.4, 5.0), (5.4, 0.25, 7.0)];

    let strike_len = (RATE * STRIKE_MS / 1000) as usize;
    let gap_len = (RATE * GAP_MS / 1000) as usize;
    let attack = (RATE as f32 * 0.004).max(1.0) as usize; // ~4 ms, avoids an onset click
    let mut out = Vec::with_capacity((strike_len + gap_len) * STRIKES as usize);
    for _ in 0..STRIKES {
        for n in 0..strike_len {
            let t = n as f32 / RATE as f32;
            let mut s = 0.0f32;
            for (mult, amp, decay) in PARTIALS {
                s += amp * (2.0 * std::f32::consts::PI * BASE_FREQ * mult * t).sin() * (-t * decay).exp();
            }
            let atk = (n as f32 / attack as f32).clamp(0.0, 1.0); // soft attack
            let s = (s * 0.22 * atk).clamp(-1.0, 1.0);
            out.push((s * i16::MAX as f32) as i16);
        }
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

/// Ask the orchestrator to synthesize `text` (Piper) and play the returned audio on
/// `playback`. Opens a fresh Wyoming socket to the mDNS-resolved host, sends one
/// `ambient-speak` frame, then streams the `audio-start`/`audio-chunk`…/`audio-stop`
/// reply into the same [`PlaybackSink`] the alarm uses. Errors (offline, no host,
/// dropped socket) are returned so the caller can degrade to bell-only.
async fn speak_via_orchestrator(
    cache: &EndpointCache,
    discovery_timeout: Duration,
    playback: &PlaybackSink,
    text: &str,
) -> anyhow::Result<()> {
    let endpoint = resolve(cache, discovery_timeout).await?;
    let stream = TcpStream::connect(endpoint.socket_addr()).await?;
    stream.set_nodelay(true).ok();
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    protocol::write_event(&mut write_half, &WyomingEvent::speak(text)).await?;

    let mut rate = crate::audio::TARGET_SAMPLE_RATE;
    while let Some(ev) = protocol::read_event(&mut reader).await? {
        match ev.event_type.as_str() {
            types::AUDIO_START => {
                if let Some((r, _width, _channels)) = protocol::audio_format(&ev.data) {
                    rate = r;
                }
            }
            types::AUDIO_CHUNK => {
                if let Some(payload) = &ev.payload {
                    let pcm = decode_pcm_i16(payload);
                    if !pcm.is_empty() {
                        playback.submit_pcm(&pcm, rate);
                    }
                }
            }
            types::AUDIO_STOP => break,
            _ => {} // ignore any other frames on this out-of-band socket
        }
    }
    Ok(())
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
        let mut got = ids_matching(entries.iter().map(|(id, l)| (*id, l)), &Some("pasta".into()));
        got.sort_unstable();
        assert_eq!(got, [1, 3]);
        // No label: every timer.
        let mut all = ids_matching(entries.iter().map(|(id, l)| (*id, l)), &None);
        all.sort_unstable();
        assert_eq!(all, [1, 2, 3, 4]);
        // Unknown label: none.
        assert!(ids_matching(entries.iter().map(|(id, l)| (*id, l)), &Some("nope".into())).is_empty());
    }

    #[test]
    fn announce_phrase_uses_name_or_falls_back() {
        assert_eq!(announce_phrase(Some("pasta")), "Time's up for pasta");
        // Whitespace-only labels are treated as unnamed.
        assert_eq!(announce_phrase(Some("   ")), "Time's up");
        assert_eq!(announce_phrase(None), "Time's up");
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
        // Non-silent: the struck bell reaches a meaningful amplitude.
        assert!(pcm.iter().any(|&s| s.abs() > i16::MAX / 8));
        // Rung twice: the gap before the second strike is silent, and the second
        // strike then sounds — so there is audible energy in both halves.
        let half = pcm.len() / 2;
        assert!(pcm[..half].iter().any(|&s| s.abs() > i16::MAX / 8));
        assert!(pcm[half..].iter().any(|&s| s.abs() > i16::MAX / 8));
    }
}
