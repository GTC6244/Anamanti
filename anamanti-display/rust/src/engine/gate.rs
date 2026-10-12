//! Detection gating: turns the stream of per-block wake-word scores into discrete
//! "fire" decisions (Plan.MD §3, Phase 2 hardening; WakeWordDetection.md §4.1).
//!
//! openWakeWord emits a confidence per audio block, and a single frame over the
//! threshold is a poor trigger — it fires on brief spikes and can fire many times
//! across one utterance. Following VACA, we (1) smooth the last `capacity` block
//! scores and (2) enforce a cooldown so a single "hey jarvis" produces exactly one
//! detection.
//!
//! Two smoothing criteria are supported, selectable per config for on-device A/B
//! tuning (WakeWordDetection.md §4.1):
//! - **average** (default): the *mean* of the window must clear the threshold.
//!   Maximally robust against single-frame false triggers, but it dilutes a brief
//!   or faint wake word whose confidence peaks for only one block — the reason a
//!   quiet far-field "hey jarvis" is missed unless the speaker is louder.
//! - **peak**: the *max* score in the window must clear the threshold. Far more
//!   responsive to short/quiet utterances; the full-window + cooldown requirements
//!   still guard against isolated spikes firing repeatedly.
//!
//! The gate is a pure state machine with the clock injected (`observe(.., now)`),
//! so the smoothing + debounce logic is unit-tested without a live audio pipeline.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Rolling smoothing + cooldown gate over per-block wake-word scores.
pub struct DetectionGate {
    window: VecDeque<f32>,
    capacity: usize,
    cooldown: Duration,
    last_fire: Option<Instant>,
    /// When true, gate on the window's peak score rather than its average.
    fire_on_peak: bool,
}

impl DetectionGate {
    /// `capacity` scores are smoothed; `cooldown` is the minimum gap between fires.
    /// `fire_on_peak` selects the peak criterion instead of the average.
    pub fn new(capacity: usize, cooldown: Duration, fire_on_peak: bool) -> Self {
        Self {
            window: VecDeque::with_capacity(capacity.max(1)),
            capacity: capacity.max(1),
            cooldown,
            last_fire: None,
            fire_on_peak,
        }
    }

    /// Mean of the scores currently in the window (0.0 when empty). Exposed for the
    /// live tuning trace so logs show the smoothed value the gate actually tests.
    pub fn avg(&self) -> f32 {
        if self.window.is_empty() {
            return 0.0;
        }
        self.window.iter().sum::<f32>() / self.window.len() as f32
    }

    /// Max score currently in the window (0.0 when empty) — the value the gate tests
    /// in peak mode.
    pub fn peak(&self) -> f32 {
        self.window.iter().copied().fold(0.0_f32, f32::max)
    }

    /// The value the gate compares against the threshold under the active criterion.
    fn metric(&self) -> f32 {
        if self.fire_on_peak {
            self.peak()
        } else {
            self.avg()
        }
    }

    /// Switch the firing criterion (average vs peak) in place, so the
    /// audio-diagnostics screen can A/B the two **without restarting the engine**.
    /// Keeps the current window contents.
    pub fn set_fire_on_peak(&mut self, fire_on_peak: bool) {
        self.fire_on_peak = fire_on_peak;
    }

    /// Resize the smoothing window in place (live tuning), preserving the most
    /// recent scores. `capacity` is floored at 1 (a 1-block window = no smoothing).
    /// Shrinking drops the oldest scores so the gate can fire again as soon as the
    /// smaller window fills.
    pub fn set_capacity(&mut self, capacity: usize) {
        let capacity = capacity.max(1);
        self.capacity = capacity;
        while self.window.len() > capacity {
            self.window.pop_front();
        }
    }

    /// Record a new block `score` and decide whether the wake word fires now.
    ///
    /// Fires (returning the smoothed metric — average, or peak in peak mode) only
    /// when the window is full, that metric clears `threshold`, and at least
    /// `cooldown` has elapsed since the previous fire. `now` is injected so the
    /// debounce is testable.
    pub fn observe(&mut self, score: f32, threshold: f32, now: Instant) -> Option<f32> {
        if self.window.len() == self.capacity {
            self.window.pop_front();
        }
        self.window.push_back(score);

        if self.window.len() < self.capacity {
            return None;
        }
        let metric = self.metric();
        if metric < threshold {
            return None;
        }
        let cooled = self
            .last_fire
            .is_none_or(|t| now.duration_since(t) >= self.cooldown);
        if !cooled {
            return None;
        }
        self.last_fire = Some(now);
        Some(metric)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CD: Duration = Duration::from_millis(1500);

    #[test]
    fn does_not_fire_until_window_is_full() {
        let mut g = DetectionGate::new(3, CD, false);
        let t = Instant::now();
        // Two high scores: window not yet full → no fire even though each is high.
        assert!(g.observe(0.9, 0.5, t).is_none());
        assert!(g.observe(0.9, 0.5, t).is_none());
        // Third fills the window and the average clears the threshold.
        assert!(g.observe(0.9, 0.5, t).is_some());
    }

    #[test]
    fn averages_smooth_out_a_single_spike() {
        let mut g = DetectionGate::new(3, CD, false);
        let t = Instant::now();
        // One spike surrounded by low scores averages to 0.30 < 0.50 → no fire.
        assert!(g.observe(0.0, 0.5, t).is_none());
        assert!(g.observe(0.9, 0.5, t).is_none());
        assert!(g.observe(0.0, 0.5, t).is_none());
    }

    #[test]
    fn peak_mode_fires_on_a_single_spike_the_average_would_miss() {
        // Same spike-surrounded-by-silence sequence as above, but in peak mode the
        // max in the window clears the bar → fires (the responsiveness lever for a
        // brief/faint "hey jarvis"). The window must still be full first.
        let mut g = DetectionGate::new(3, CD, true);
        let t = Instant::now();
        assert!(g.observe(0.0, 0.5, t).is_none()); // window not full yet
        assert!(g.observe(0.9, 0.5, t).is_none()); // window not full yet
        assert!(g.observe(0.0, 0.5, t).is_some()); // full; peak 0.9 ≥ 0.5 → fires
    }

    #[test]
    fn peak_mode_still_respects_the_threshold() {
        let mut g = DetectionGate::new(2, CD, true);
        let t = Instant::now();
        // Peak 0.4 never clears the 0.5 bar → no fire.
        assert!(g.observe(0.4, 0.5, t).is_none());
        assert!(g.observe(0.3, 0.5, t).is_none());
    }

    #[test]
    fn cooldown_blocks_a_second_immediate_fire() {
        let mut g = DetectionGate::new(1, CD, false);
        let t0 = Instant::now();
        assert!(g.observe(0.9, 0.5, t0).is_some());
        // Still within the cooldown window → suppressed.
        assert!(g
            .observe(0.9, 0.5, t0 + Duration::from_millis(500))
            .is_none());
        assert!(g
            .observe(0.9, 0.5, t0 + Duration::from_millis(1499))
            .is_none());
        // Past the cooldown → fires again.
        assert!(g
            .observe(0.9, 0.5, t0 + Duration::from_millis(1500))
            .is_some());
    }

    #[test]
    fn set_fire_on_peak_switches_criterion_live() {
        // Start in average mode with a spike-then-silence window that averages below
        // the bar, so it does not fire.
        let mut g = DetectionGate::new(3, CD, false);
        let t = Instant::now();
        assert!(g.observe(0.0, 0.5, t).is_none());
        assert!(g.observe(0.9, 0.5, t).is_none());
        assert!(g.observe(0.0, 0.5, t).is_none()); // avg 0.30 < 0.50
                                                   // Flip to peak mode mid-stream (what the diagnostics "fire on peak" toggle
                                                   // does); the window's existing 0.9 peak now clears the bar on the next block.
        g.set_fire_on_peak(true);
        assert!(g.fire_on_peak);
        assert!(g.observe(0.0, 0.5, t).is_some());
    }

    #[test]
    fn set_capacity_shrinks_and_preserves_recent_scores() {
        // A 3-wide window primed with three high scores.
        let mut g = DetectionGate::new(3, CD, false);
        let t = Instant::now();
        assert_eq!(g.capacity, 3);
        g.observe(0.9, 0.5, t);
        g.observe(0.9, 0.5, t);
        g.observe(0.9, 0.5, t);
        // Shrink to a 1-block window: it drops the oldest scores but stays full of the
        // most recent 0.9, so the very next high block fires immediately.
        g.set_capacity(1);
        assert_eq!(g.capacity, 1);
        // Past the cooldown so the debounce doesn't mask the resize behaviour.
        assert!(g
            .observe(0.9, 0.5, t + Duration::from_millis(1500))
            .is_some());
    }

    #[test]
    fn set_capacity_floors_at_one() {
        let mut g = DetectionGate::new(2, CD, false);
        g.set_capacity(0);
        assert_eq!(g.capacity, 1);
    }

    #[test]
    fn respects_a_raised_active_threshold() {
        let mut g = DetectionGate::new(2, CD, false);
        let t = Instant::now();
        // Average 0.6 is above the idle 0.5 bar but below a raised 0.7 → no fire.
        assert!(g.observe(0.6, 0.7, t).is_none());
        assert!(g.observe(0.6, 0.7, t).is_none());
        // Louder scores clear the raised bar.
        assert!(g.observe(0.8, 0.7, t).is_some());
    }
}
