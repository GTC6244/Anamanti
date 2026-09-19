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

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::task::AbortHandle;

use crate::api::engine::WakeWordEvent;
use crate::audio::playback::PlaybackSink;
use crate::frb_generated::StreamSink;
use crate::wyoming::protocol::TimerCommand;

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
    next_id: AtomicU32,
    timers: Mutex<HashMap<u32, TimerEntry>>,
}

impl TimerManager {
    /// Build a manager that emits timer events on `sink`, sounds the alarm through
    /// `playback` (when an output device exists), and spawns countdown tasks on
    /// `handle` (the network runtime).
    pub fn new(
        sink: StreamSink<WakeWordEvent>,
        playback: Option<Arc<PlaybackSink>>,
        handle: Handle,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                sink,
                playback,
                handle,
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
            // Sound the alarm through the same cpal layer that plays TTS.
            if let Some(pb) = &inner.playback {
                let (chime, rate) = alarm_pcm();
                pb.submit_pcm(&chime, rate);
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

/// Synthesize a short triple-beep alarm as mono `i16` PCM at the device rate.
/// Returns the samples plus their sample rate, suitable for
/// [`PlaybackSink::submit_pcm`]. A 10 ms fade in/out per beep avoids clicks.
fn alarm_pcm() -> (Vec<i16>, u32) {
    const RATE: u32 = crate::audio::TARGET_SAMPLE_RATE;
    const FREQ: f32 = 880.0;
    const BEEP_MS: u32 = 200;
    const GAP_MS: u32 = 120;
    const BEEPS: u32 = 3;
    let beep_len = (RATE * BEEP_MS / 1000) as usize;
    let gap_len = (RATE * GAP_MS / 1000) as usize;
    let fade = (RATE as f32 * 0.01) as usize; // 10 ms
    let mut out = Vec::with_capacity((beep_len + gap_len) * BEEPS as usize);
    for _ in 0..BEEPS {
        for n in 0..beep_len {
            let t = n as f32 / RATE as f32;
            let env = {
                let a = n.min(fade) as f32 / fade.max(1) as f32;
                let b = (beep_len - n).min(fade) as f32 / fade.max(1) as f32;
                a.min(b).clamp(0.0, 1.0)
            };
            let s = (2.0 * std::f32::consts::PI * FREQ * t).sin() * 0.35 * env;
            out.push((s * i16::MAX as f32) as i16);
        }
        out.extend(std::iter::repeat_n(0i16, gap_len));
    }
    (out, RATE)
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
    fn alarm_pcm_has_expected_length_and_is_audible() {
        let (pcm, rate) = alarm_pcm();
        assert_eq!(rate, crate::audio::TARGET_SAMPLE_RATE);
        let beep = (rate * 200 / 1000) as usize;
        let gap = (rate * 120 / 1000) as usize;
        assert_eq!(pcm.len(), (beep + gap) * 3);
        // Non-silent: the beep body reaches a meaningful amplitude.
        assert!(pcm.iter().any(|&s| s.abs() > i16::MAX / 4));
    }
}
