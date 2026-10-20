//! Camera-as-lux-sensor: decide when the room's lights are off (pure, unit-tested).
//!
//! The Echo Show has no ambient-light sensor exposed to apps, but the front camera
//! already streams small luma frames for presence sensing (`presence.rs`). A camera
//! frame's brightness alone is useless as a light meter, because **auto-exposure
//! normalizes it** — a dim room and a bright room both come out near mid-grey. The
//! light level is instead recovered from what the AE had to do to get there: the
//! exposure time `t`, the sensor sensitivity (ISO `S`) and the lens f-number `N`,
//! reported per frame in the Camera2 `CaptureResult`. The standard reflected-light
//! exposure equation gives
//!
//! ```text
//!   EV100 = log2(N² / t) − log2(S / 100)        (exposure value at ISO 100)
//!   E     ≈ 2.5 · 2^EV100   lux                 (incident-light calibration C≈250)
//! ```
//!
//! scaled by how far the frame's linearized mean luma sits from the 18 % mid-grey
//! the AE aims for (it can't reach it once it saturates in the dark). The result is
//! an **estimate** — good to a small factor, which is plenty to tell "lights on"
//! (tens to hundreds of lux) from "lights off" (≈ 0–5 lux).
//!
//! When the HAL doesn't report exposure metadata (some LEGACY camera HALs), the
//! estimate falls back to a nominal exposure and the frame brightness alone. That
//! still works for the one decision we need: in the dark the AE saturates and the
//! frame itself goes black, while in a lit room the AE holds it near mid-grey.
//!
//! [`AmbientLightDetector`] smooths the estimate and applies hysteresis plus dwell
//! times so the decision is stable — important because the screen itself lights the
//! room a little: dimming it must not read as "the lights went off" (or on) again.
//! Like `PresenceDetector`, it takes an explicit `now` so tests need no clock.
//!
//! [`CoverDetector`] separately flags a **covered lens** (privacy shutter or tape):
//! frames that stay near-black *and* near-uniform. A covered lens is otherwise
//! indistinguishable from a dark room by light level alone, and the display lets the
//! user choose what a covered camera should mean (Settings → Display → Night). A
//! pitch-black room can also read as "covered" — the same caveat as for any light
//! sensor in total darkness.

use std::time::{Duration, Instant};

/// Default illuminance (lux) below which the room counts as dark. A lamp-lit living
/// room sits at 50–300 lux and a "lights off" room (streetlight/TV glow) under ~5.
pub const DEFAULT_DARK_LUX: f32 = 8.0;

/// The room must read this many times brighter than the dark threshold to count as
/// lit again. The gap absorbs estimate noise and the screen's own glow.
pub const LIGHT_HYSTERESIS: f32 = 3.0;

/// How long the room must stay below the threshold before it's declared dark —
/// long enough to ignore a hand over the camera or someone walking past a lamp.
pub const DARK_DWELL: Duration = Duration::from_secs(10);

/// How long the room must stay above the light threshold before it's declared lit.
/// Short, so switching the lights on restores full brightness promptly.
pub const LIGHT_DWELL: Duration = Duration::from_secs(2);

/// Smoothing weight of each new sample in the log-lux moving average (at ~5 fps this
/// is a ~1.5 s time constant): it rides out sensor noise and AE hunting.
const SMOOTHING_ALPHA: f32 = 0.15;

/// Lens f-number to assume when the HAL doesn't report one (typical webcam lens).
const DEFAULT_APERTURE: f32 = 2.0;

/// Nominal exposure assumed when the HAL reports no exposure metadata (1/30 s at
/// ISO 300). Calibrated so a frame the AE managed to bring to mid-grey reads as a
/// lit room (~100 lux) — without metadata, "the AE coped" is all we know — and only
/// a frame that stays dark after the AE saturates reads as lights-off.
const FALLBACK_EXPOSURE_S: f32 = 1.0 / 30.0;
const FALLBACK_ISO: f32 = 300.0;

/// Linear-light value of the 18 % mid-grey the auto-exposure aims for.
const MID_GREY: f32 = 0.18;

/// Lower bound on the estimate (log domain can't take 0).
const MIN_LUX: f32 = 0.01;

/// A frame counts as "covered" when its mean luma (0..255) is below this…
pub const COVERED_MAX_MEAN: f32 = 12.0;

/// …and its per-pixel luma standard deviation is below this: a shutter or tape gives
/// an almost perfectly flat black frame (only sensor noise), while even a dim room
/// has some structure (a window, a lamp, the screen's glow on a wall).
pub const COVERED_MAX_STDDEV: f32 = 6.0;

/// Mean and standard deviation of a luma frame (0..255 scale); `None` when empty.
pub fn frame_stats(luma: &[u8]) -> Option<(f32, f32)> {
    if luma.is_empty() {
        return None;
    }
    let n = luma.len() as f64;
    let (sum, sq) = luma.iter().fold((0u64, 0u64), |(s, q), &v| {
        let v = v as u64;
        (s + v, q + v * v)
    });
    let mean = sum as f64 / n;
    let var = (sq as f64 / n - mean * mean).max(0.0);
    Some((mean as f32, var.sqrt() as f32))
}

/// Whether a single frame looks like a covered lens (near-black and near-uniform).
pub fn frame_looks_covered(luma: &[u8]) -> bool {
    frame_stats(luma).is_some_and(|(mean, sd)| mean < COVERED_MAX_MEAN && sd < COVERED_MAX_STDDEV)
}

/// A boolean that only changes after the wanted value has held for a dwell time
/// (separate dwells for turning on and off). Shared by the dark and covered decisions.
struct Debounced {
    state: bool,
    pending_since: Option<Instant>,
    to_true: Duration,
    to_false: Duration,
}

impl Debounced {
    fn new(to_true: Duration, to_false: Duration) -> Self {
        Self {
            state: false,
            pending_since: None,
            to_true,
            to_false,
        }
    }

    /// Feed the currently wanted value; returns `Some(new_state)` only on a flip.
    fn update(&mut self, want: bool, now: Instant) -> Option<bool> {
        if want == self.state {
            self.pending_since = None;
            return None;
        }
        let since = *self.pending_since.get_or_insert(now);
        let dwell = if want { self.to_true } else { self.to_false };
        if now.duration_since(since) >= dwell {
            self.state = want;
            self.pending_since = None;
            return Some(want);
        }
        None
    }
}

/// Debounced "is the lens covered?" decision over a stream of frames. Starts
/// uncovered; flips to covered after [`DARK_DWELL`] of covered-looking frames and
/// back after [`LIGHT_DWELL`] of normal ones.
pub struct CoverDetector {
    covered: Debounced,
}

impl Default for CoverDetector {
    fn default() -> Self {
        Self {
            covered: Debounced::new(DARK_DWELL, LIGHT_DWELL),
        }
    }
}

impl CoverDetector {
    /// Whether the lens is currently judged covered.
    pub fn covered(&self) -> bool {
        self.covered.state
    }

    /// Fold in one frame. Returns `Some(covered)` **only when the decision flips**.
    pub fn observe(&mut self, luma: &[u8], now: Instant) -> Option<bool> {
        self.covered.update(frame_looks_covered(luma), now)
    }
}

/// Estimate scene illuminance in lux from one luma frame plus the capture metadata
/// that produced it. `exposure_ns <= 0` / `iso <= 0` / `aperture <= 0` mean "not
/// reported" and fall back to nominal values. Returns `None` for an empty frame.
pub fn estimate_lux(luma: &[u8], exposure_ns: i64, iso: i32, aperture: f32) -> Option<f32> {
    if luma.is_empty() {
        return None;
    }
    // Mean luma, linearized from the sensor's gamma-encoded output (γ ≈ 2.2).
    let sum: u64 = luma.iter().map(|&v| v as u64).sum();
    let mean = sum as f32 / luma.len() as f32 / 255.0;
    let linear = mean.powf(2.2);

    let (t, s) = if exposure_ns > 0 && iso > 0 {
        (exposure_ns as f32 / 1e9, iso as f32)
    } else {
        (FALLBACK_EXPOSURE_S, FALLBACK_ISO)
    };
    let n = if aperture > 0.0 {
        aperture
    } else {
        DEFAULT_APERTURE
    };

    // E ≈ 2.5 · N² / (t · S/100), scaled by the frame's distance from mid-grey.
    let lux = 2.5 * (n * n) / (t * s / 100.0) * (linear / MID_GREY);
    Some(lux.max(MIN_LUX))
}

/// Hysteretic "are the lights off?" decision over a stream of lux estimates.
pub struct AmbientLightDetector {
    /// Below this (smoothed) the room is a dark candidate.
    dark_lux: f32,
    /// Above this (smoothed) a dark room is a lit candidate.
    light_lux: f32,
    /// Smoothed estimate in log2(lux); `None` until the first sample.
    log_lux: Option<f32>,
    /// The debounced dark/lit decision.
    dark: Debounced,
}

impl AmbientLightDetector {
    /// Build a detector. `dark_lux <= 0` uses [`DEFAULT_DARK_LUX`]. Starts "lit", so
    /// an unavailable or still-settling camera never dims the screen.
    pub fn new(dark_lux: f32) -> Self {
        let dark_lux = if dark_lux > 0.0 {
            dark_lux
        } else {
            DEFAULT_DARK_LUX
        };
        Self {
            dark_lux,
            light_lux: dark_lux * LIGHT_HYSTERESIS,
            log_lux: None,
            dark: Debounced::new(DARK_DWELL, LIGHT_DWELL),
        }
    }

    /// Whether the room is currently judged dark.
    pub fn dark(&self) -> bool {
        self.dark.state
    }

    /// The smoothed illuminance estimate in lux (`0` before the first sample).
    pub fn lux(&self) -> f32 {
        self.log_lux.map_or(0.0, f32::exp2)
    }

    /// Fold in one lux estimate. Returns `Some(dark)` **only when the decision flips**.
    pub fn observe(&mut self, lux: f32, now: Instant) -> Option<bool> {
        let sample = lux.max(MIN_LUX).log2();
        let smoothed = match self.log_lux {
            Some(prev) => prev + SMOOTHING_ALPHA * (sample - prev),
            None => sample,
        };
        self.log_lux = Some(smoothed);
        let level = smoothed.exp2();

        // Hysteresis: once dark, stay dark until clearly above the light threshold.
        let want_dark = if self.dark.state {
            level <= self.light_lux
        } else {
            level < self.dark_lux
        };
        self.dark.update(want_dark, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(value: u8) -> Vec<u8> {
        vec![value; 64]
    }

    /// Feed `lux` at 5 fps for `secs`, starting at `t`; returns any transitions.
    fn run(d: &mut AmbientLightDetector, lux: f32, t: &mut Instant, secs: u64) -> Vec<bool> {
        let mut out = Vec::new();
        for _ in 0..secs * 5 {
            *t += Duration::from_millis(200);
            out.extend(d.observe(lux, *t));
        }
        out
    }

    #[test]
    fn empty_frame_has_no_estimate() {
        assert_eq!(estimate_lux(&[], 1, 1, 1.0), None);
    }

    #[test]
    fn mid_grey_indoor_exposure_reads_like_a_lit_room() {
        // 1/30 s at ISO 200, f/2, AE hit mid-grey (luma ≈ 118) → ~150 lux.
        let lux = estimate_lux(&frame(118), 33_333_333, 200, 2.0).unwrap();
        assert!((80.0..300.0).contains(&lux), "lit room read {lux} lux");
    }

    #[test]
    fn saturated_ae_in_the_dark_reads_near_zero() {
        // AE maxed (1/5 s, ISO 3200) and the frame is still nearly black.
        let lux = estimate_lux(&frame(12), 200_000_000, 3200, 2.0).unwrap();
        assert!(lux < 1.0, "dark room read {lux} lux");
    }

    #[test]
    fn auto_exposure_normalized_frames_differ_by_exposure() {
        // Same mid-grey frame; the AE needed 32× more exposure·gain in the dim room.
        let bright = estimate_lux(&frame(118), 10_000_000, 100, 2.0).unwrap();
        let dim = estimate_lux(&frame(118), 80_000_000, 400, 2.0).unwrap();
        assert!((bright / dim - 32.0).abs() < 0.5);
    }

    #[test]
    fn missing_metadata_falls_back_to_frame_brightness() {
        let lit = estimate_lux(&frame(150), 0, 0, 0.0).unwrap();
        let black = estimate_lux(&frame(5), 0, 0, 0.0).unwrap();
        assert!(
            lit > DEFAULT_DARK_LUX * LIGHT_HYSTERESIS,
            "lit fallback {lit}"
        );
        assert!(black < DEFAULT_DARK_LUX, "black fallback {black}");
    }

    #[test]
    fn starts_lit_and_needs_the_dark_dwell() {
        let mut d = AmbientLightDetector::new(8.0);
        assert!(!d.dark());
        let mut t = Instant::now();
        // Comfortably dark, but not yet for the full dwell (smoothing included).
        assert!(run(&mut d, 0.5, &mut t, 5).is_empty());
        assert!(!d.dark());
        // Past the dwell: flips to dark exactly once.
        assert_eq!(run(&mut d, 0.5, &mut t, 10), vec![true]);
        assert!(d.dark());
        assert!(d.lux() < 1.0);
    }

    #[test]
    fn lights_on_restores_quickly() {
        let mut d = AmbientLightDetector::new(8.0);
        let mut t = Instant::now();
        run(&mut d, 0.5, &mut t, 20);
        assert!(d.dark());
        assert_eq!(run(&mut d, 200.0, &mut t, 4), vec![false]);
        assert!(!d.dark());
    }

    #[test]
    fn hysteresis_ignores_the_screens_own_glow() {
        let mut d = AmbientLightDetector::new(8.0);
        let mut t = Instant::now();
        run(&mut d, 0.5, &mut t, 20);
        assert!(d.dark());
        // A dimly lit room just above the dark threshold but under 3× it stays dark.
        assert!(run(&mut d, 15.0, &mut t, 30).is_empty());
        assert!(d.dark());
    }

    #[test]
    fn a_brief_shadow_does_not_count_as_lights_off() {
        let mut d = AmbientLightDetector::new(8.0);
        let mut t = Instant::now();
        run(&mut d, 200.0, &mut t, 5);
        // A hand over the camera for 4 s, then the lit room again.
        assert!(run(&mut d, 0.1, &mut t, 4).is_empty());
        assert!(run(&mut d, 200.0, &mut t, 20).is_empty());
        assert!(!d.dark());
    }

    /// A frame with values alternating `a`, `b` (mean (a+b)/2, stddev |a-b|/2).
    fn checker(a: u8, b: u8) -> Vec<u8> {
        (0..64).map(|i| if i % 2 == 0 { a } else { b }).collect()
    }

    #[test]
    fn frame_stats_mean_and_stddev() {
        let (mean, sd) = frame_stats(&checker(10, 30)).unwrap();
        assert!((mean - 20.0).abs() < 1e-3);
        assert!((sd - 10.0).abs() < 1e-3);
        assert_eq!(frame_stats(&[]), None);
    }

    #[test]
    fn covered_frames_are_dark_and_flat() {
        assert!(frame_looks_covered(&frame(2))); // shutter: flat black
        assert!(frame_looks_covered(&checker(0, 8))); // flat black + sensor noise
        assert!(!frame_looks_covered(&frame(118))); // normal exposure
        assert!(!frame_looks_covered(&checker(0, 40))); // dark room with a lit window
        assert!(!frame_looks_covered(&[]));
    }

    #[test]
    fn cover_detector_debounces_both_ways() {
        let mut d = CoverDetector::default();
        let mut t = Instant::now();
        let mut feed = |d: &mut CoverDetector, f: &[u8], secs: u64| {
            let mut out = Vec::new();
            for _ in 0..secs * 5 {
                t += Duration::from_millis(200);
                out.extend(d.observe(f, t));
            }
            out
        };
        // A hand briefly over the lens isn't "covered".
        assert!(feed(&mut d, &frame(1), 5).is_empty());
        assert!(feed(&mut d, &frame(118), 1).is_empty());
        // Held covered past the dwell → one flip.
        assert_eq!(feed(&mut d, &frame(1), 12), vec![true]);
        assert!(d.covered());
        // Uncovered → back within ~2 s.
        assert_eq!(feed(&mut d, &frame(118), 3), vec![false]);
        assert!(!d.covered());
    }

    #[test]
    fn zero_threshold_uses_default() {
        let d = AmbientLightDetector::new(0.0);
        assert_eq!(d.dark_lux, DEFAULT_DARK_LUX);
        assert_eq!(d.light_lux, DEFAULT_DARK_LUX * LIGHT_HYSTERESIS);
    }
}
