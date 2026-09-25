//! Camera-as-proximity presence detection (pure, unit-tested logic).
//!
//! The Echo Show's front camera is used as a cheap proximity sensor to brighten
//! the idle screen when someone approaches (Plan.MD §5). We deliberately do **not**
//! do face recognition or any ML: the Kotlin capture shim (`CameraBridge.kt`) hands
//! us a small packed luma (Y-plane) frame a few times a second, and this detector
//! measures **frame-to-frame motion** — the mean absolute per-pixel luma delta.
//!
//! That single signal captures both a person moving in front of the device and a
//! sudden ambient-light change (walking up, turning on a light) as "activity",
//! which is all a brighten-on-presence trigger needs, at a few thousand integer
//! subtractions per frame. A short debounce keeps the screen bright through brief
//! stillness and a release window dims it once activity stops.
//!
//! The detector is a pure state machine (like the wake-word `DetectionGate`): it
//! takes an explicit `now: Instant` so it can be exhaustively unit-tested with no
//! camera and no wall-clock sleeps.

use std::time::{Duration, Instant};

/// Default mean-absolute per-pixel luma delta (on the 0..255 scale) above which a
/// frame counts as motion. Sensor noise on a static scene typically sits well below
/// 1.0; a person moving into frame or a lighting change pushes it far higher, so a
/// threshold of a couple of levels cleanly separates "someone's here" from noise.
pub const DEFAULT_MOTION_THRESHOLD: f32 = 2.5;

/// Default time with no motion before the user is declared absent (screen dims).
/// Generous so a mostly-still person (reading, watching) keeps the screen bright as
/// long as there's periodic small movement, rather than dimming every few seconds.
pub const DEFAULT_RELEASE: Duration = Duration::from_secs(20);

/// Frame-motion presence detector. Holds the previous luma frame and the current
/// present/absent state; `observe` folds in each new frame.
pub struct PresenceDetector {
    /// The previous packed luma frame, for the next diff. Empty until the first frame.
    prev: Vec<u8>,
    /// Mean absolute per-pixel luma delta at/above which a frame counts as motion.
    motion_threshold: f32,
    /// How long without motion before flipping to absent.
    release: Duration,
    /// When motion was last seen (drives the release timeout).
    last_activity: Option<Instant>,
    /// Current presence state.
    present: bool,
}

impl PresenceDetector {
    /// Build a detector. `motion_threshold <= 0` and a zero `release` fall back to
    /// the module defaults, so the config can leave them unset (0).
    pub fn new(motion_threshold: f32, release: Duration) -> Self {
        Self {
            prev: Vec::new(),
            motion_threshold: if motion_threshold > 0.0 {
                motion_threshold
            } else {
                DEFAULT_MOTION_THRESHOLD
            },
            release: if release.is_zero() {
                DEFAULT_RELEASE
            } else {
                release
            },
            last_activity: None,
            present: false,
        }
    }

    /// Current presence state (true = someone is/was recently in front of the device).
    pub fn present(&self) -> bool {
        self.present
    }

    /// Feed one packed luma (Y-plane) frame. Returns `Some(present)` **only when the
    /// presence state changes**, so the caller emits a `Presence` event only on
    /// transitions (not once per frame).
    pub fn observe(&mut self, luma: &[u8], now: Instant) -> Option<bool> {
        let motion = self.frame_motion(luma);

        // Retain this frame as the baseline for the next diff.
        self.prev.clear();
        self.prev.extend_from_slice(luma);

        let moved = motion.is_some_and(|m| m >= self.motion_threshold);

        if moved {
            self.last_activity = Some(now);
            if !self.present {
                self.present = true;
                return Some(true);
            }
        } else if self.present {
            if let Some(t) = self.last_activity {
                if now.duration_since(t) >= self.release {
                    self.present = false;
                    return Some(false);
                }
            }
        }
        None
    }

    /// Register **externally-observed** user activity — a voice turn or a screen
    /// touch — as if it were motion at `now`. This refreshes the release deadline
    /// (so the screen won't dim for another full window) and, when the screen had
    /// already dimmed (absent), flips back to present. Returns `Some(true)` **only**
    /// on that absent→present transition, so the caller emits a single `Presence(true)`
    /// event to brighten the screen; otherwise `None`. It never dims — dimming stays
    /// the release-window's job in [`observe`], driven purely by a lack of activity.
    pub fn note_activity(&mut self, now: Instant) -> Option<bool> {
        self.last_activity = Some(now);
        if !self.present {
            self.present = true;
            return Some(true);
        }
        None
    }

    /// Mean absolute per-pixel luma delta versus the previous frame, or `None` when
    /// there's no comparable previous frame (the first frame, or a resolution change
    /// mid-stream) — in which case we can't judge motion and just seed the baseline.
    fn frame_motion(&self, luma: &[u8]) -> Option<f32> {
        if luma.is_empty() || self.prev.len() != luma.len() {
            return None;
        }
        let sum: u64 = self
            .prev
            .iter()
            .zip(luma)
            .map(|(&a, &b)| (a as i16 - b as i16).unsigned_abs() as u64)
            .sum();
        Some(sum as f32 / luma.len() as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REL: Duration = Duration::from_secs(20);

    /// A frame filled with a single luma value.
    fn flat(value: u8, len: usize) -> Vec<u8> {
        vec![value; len]
    }

    #[test]
    fn first_frame_seeds_baseline_without_firing() {
        let mut d = PresenceDetector::new(2.5, REL);
        assert_eq!(d.observe(&flat(40, 64), Instant::now()), None);
        assert!(!d.present());
    }

    #[test]
    fn a_big_luma_jump_marks_present() {
        let mut d = PresenceDetector::new(2.5, REL);
        let t = Instant::now();
        d.observe(&flat(40, 64), t); // baseline
                                     // Whole frame jumps 40 -> 90 (delta 50/pixel ≫ threshold): motion.
        assert_eq!(d.observe(&flat(90, 64), t), Some(true));
        assert!(d.present());
    }

    #[test]
    fn tiny_noise_stays_absent() {
        let mut d = PresenceDetector::new(2.5, REL);
        let t = Instant::now();
        d.observe(&flat(40, 64), t);
        // 1-level shimmer is below the 2.5 threshold → no presence.
        assert_eq!(d.observe(&flat(41, 64), t), None);
        assert!(!d.present());
    }

    #[test]
    fn stays_present_through_continued_motion_then_releases() {
        let mut d = PresenceDetector::new(2.5, REL);
        let t0 = Instant::now();
        d.observe(&flat(40, 64), t0);
        assert_eq!(d.observe(&flat(90, 64), t0), Some(true));
        // More motion a bit later: still present, no repeated event.
        let t1 = t0 + Duration::from_secs(5);
        assert_eq!(d.observe(&flat(40, 64), t1), None);
        assert!(d.present());
        // Now quiet frames. Just before the release window elapses: still present.
        let quiet = flat(40, 64);
        assert_eq!(d.observe(&quiet, t1 + Duration::from_secs(19)), None);
        assert!(d.present());
        // Past the release window since the last motion (t1): flips to absent once.
        assert_eq!(d.observe(&quiet, t1 + Duration::from_secs(20)), Some(false));
        assert!(!d.present());
        // Idempotent afterwards.
        assert_eq!(d.observe(&quiet, t1 + Duration::from_secs(25)), None);
    }

    #[test]
    fn re_triggers_after_release() {
        let mut d = PresenceDetector::new(2.5, REL);
        let t0 = Instant::now();
        d.observe(&flat(40, 64), t0);
        assert_eq!(d.observe(&flat(90, 64), t0), Some(true));
        let quiet = flat(90, 64);
        assert_eq!(d.observe(&quiet, t0 + REL), Some(false));
        // A fresh disturbance marks present again.
        assert_eq!(
            d.observe(&flat(30, 64), t0 + REL + Duration::from_secs(1)),
            Some(true)
        );
    }

    #[test]
    fn resolution_change_reseeds_without_firing() {
        let mut d = PresenceDetector::new(2.5, REL);
        let t = Instant::now();
        d.observe(&flat(40, 64), t);
        // A differently-sized frame can't be diffed; just reseeds, no event.
        assert_eq!(d.observe(&flat(200, 100), t), None);
        assert!(!d.present());
    }

    #[test]
    fn note_activity_from_absent_marks_present_once() {
        let mut d = PresenceDetector::new(2.5, REL);
        // Starts absent (no motion seen yet): external activity brightens.
        let t = Instant::now();
        assert_eq!(d.note_activity(t), Some(true));
        assert!(d.present());
        // Already present: a further activity ping refreshes the deadline silently.
        assert_eq!(d.note_activity(t + Duration::from_secs(1)), None);
        assert!(d.present());
    }

    #[test]
    fn note_activity_pushes_back_the_dim_deadline() {
        let mut d = PresenceDetector::new(2.5, REL);
        let t0 = Instant::now();
        d.observe(&flat(40, 64), t0);
        assert_eq!(d.observe(&flat(90, 64), t0), Some(true)); // present via motion
        let quiet = flat(90, 64);
        // A voice turn / touch just before the window elapses resets the countdown…
        assert_eq!(d.note_activity(t0 + Duration::from_secs(19)), None);
        // …so at the original deadline we're still present (no dim).
        assert_eq!(d.observe(&quiet, t0 + Duration::from_secs(20)), None);
        assert!(d.present());
        // Only after a full release window with no activity of any kind does it dim.
        assert_eq!(d.observe(&quiet, t0 + Duration::from_secs(39)), Some(false));
        assert!(!d.present());
    }

    #[test]
    fn zero_config_uses_defaults() {
        let d = PresenceDetector::new(0.0, Duration::ZERO);
        assert_eq!(d.motion_threshold, DEFAULT_MOTION_THRESHOLD);
        assert_eq!(d.release, DEFAULT_RELEASE);
    }
}
