//! Detection gating: turns the stream of per-block wake-word scores into discrete
//! "fire" decisions (Plan.MD §3, Phase 2 hardening; WakeWordDetection.md §4.1).
//!
//! openWakeWord emits a confidence per audio block, and a single frame over the
//! threshold is a poor trigger — it fires on brief spikes and can fire many times
//! across one utterance. Following VACA, we (1) require the *moving average* of the
//! last `capacity` block scores to clear the threshold, and (2) enforce a cooldown
//! so a single "hey jarvis" produces exactly one detection.
//!
//! The gate is a pure state machine with the clock injected (`observe(.., now)`),
//! so the smoothing + debounce logic is unit-tested without a live audio pipeline.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Rolling-average + cooldown gate over per-block wake-word scores.
pub struct DetectionGate {
    window: VecDeque<f32>,
    capacity: usize,
    cooldown: Duration,
    last_fire: Option<Instant>,
}

impl DetectionGate {
    /// `capacity` scores are averaged; `cooldown` is the minimum gap between fires.
    pub fn new(capacity: usize, cooldown: Duration) -> Self {
        Self {
            window: VecDeque::with_capacity(capacity.max(1)),
            capacity: capacity.max(1),
            cooldown,
            last_fire: None,
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

    /// Record a new block `score` and decide whether the wake word fires now.
    ///
    /// Fires (returns the smoothed average) only when the window is full, its
    /// average clears `threshold`, and at least `cooldown` has elapsed since the
    /// previous fire. `now` is injected so the debounce is testable.
    pub fn observe(&mut self, score: f32, threshold: f32, now: Instant) -> Option<f32> {
        if self.window.len() == self.capacity {
            self.window.pop_front();
        }
        self.window.push_back(score);

        if self.window.len() < self.capacity {
            return None;
        }
        let avg = self.avg();
        if avg < threshold {
            return None;
        }
        let cooled = self
            .last_fire
            .is_none_or(|t| now.duration_since(t) >= self.cooldown);
        if !cooled {
            return None;
        }
        self.last_fire = Some(now);
        Some(avg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CD: Duration = Duration::from_millis(1500);

    #[test]
    fn does_not_fire_until_window_is_full() {
        let mut g = DetectionGate::new(3, CD);
        let t = Instant::now();
        // Two high scores: window not yet full → no fire even though each is high.
        assert!(g.observe(0.9, 0.5, t).is_none());
        assert!(g.observe(0.9, 0.5, t).is_none());
        // Third fills the window and the average clears the threshold.
        assert!(g.observe(0.9, 0.5, t).is_some());
    }

    #[test]
    fn averages_smooth_out_a_single_spike() {
        let mut g = DetectionGate::new(3, CD);
        let t = Instant::now();
        // One spike surrounded by low scores averages to 0.30 < 0.50 → no fire.
        assert!(g.observe(0.0, 0.5, t).is_none());
        assert!(g.observe(0.9, 0.5, t).is_none());
        assert!(g.observe(0.0, 0.5, t).is_none());
    }

    #[test]
    fn cooldown_blocks_a_second_immediate_fire() {
        let mut g = DetectionGate::new(1, CD);
        let t0 = Instant::now();
        assert!(g.observe(0.9, 0.5, t0).is_some());
        // Still within the cooldown window → suppressed.
        assert!(g.observe(0.9, 0.5, t0 + Duration::from_millis(500)).is_none());
        assert!(g.observe(0.9, 0.5, t0 + Duration::from_millis(1499)).is_none());
        // Past the cooldown → fires again.
        assert!(g.observe(0.9, 0.5, t0 + Duration::from_millis(1500)).is_some());
    }

    #[test]
    fn respects_a_raised_active_threshold() {
        let mut g = DetectionGate::new(2, CD);
        let t = Instant::now();
        // Average 0.6 is above the idle 0.5 bar but below a raised 0.7 → no fire.
        assert!(g.observe(0.6, 0.7, t).is_none());
        assert!(g.observe(0.6, 0.7, t).is_none());
        // Louder scores clear the raised bar.
        assert!(g.observe(0.8, 0.7, t).is_some());
    }
}
