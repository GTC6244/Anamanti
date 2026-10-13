//! Speakable phrasing shared by tools and fast paths.
//!
//! [`duration`] is the one place durations become words. The call sites historically
//! disagreed on how to treat zero, sub-minute values and seconds next to hours, and
//! those differences are audible, so the variants are kept as explicit
//! [`DurationStyle`]s rather than unified:
//!
//! | style | used by | `0` | `45` | `3661` |
//! | --- | --- | --- | --- | --- |
//! | [`DurationStyle::Exact`] | timers (`llm::rig`, the orchestrator's System-1 timer) | "0 seconds" | "45 seconds" | "1 hour 1 minute 1 second" |
//! | [`DurationStyle::Coarse`] | recipe total time | "" | "45 seconds" | "1 hour 1 minute" |
//! | [`DurationStyle::Minutes`] | directions ETAs / traffic delay | "under a minute" | "under a minute" | "1 hour 1 minute" |

/// How [`duration`] renders a number of seconds. See the module docs for a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DurationStyle {
    /// Hours, minutes and seconds, each only when non-zero; zero is "0 seconds".
    /// Fractional seconds are truncated.
    Exact,
    /// Like [`Exact`](Self::Exact), but seconds are dropped once there are hours, and
    /// zero is the empty string (nothing worth saying). Fractional seconds are truncated.
    Coarse,
    /// Rounded to the nearest whole minute (sign ignored): "under a minute" below
    /// that, else hours and minutes.
    Minutes,
}

/// Render `secs` as a short spoken phrase in the given [`DurationStyle`], e.g.
/// "25 minutes", "1 hour 30 minutes", "45 seconds".
pub fn duration(secs: f64, style: DurationStyle) -> String {
    match style {
        DurationStyle::Minutes => {
            let total_min = (secs.abs() / 60.0).round() as u64;
            if total_min < 1 {
                return "under a minute".to_string();
            }
            hms(total_min * 60, false)
        }
        DurationStyle::Exact => {
            let s = whole_secs(secs);
            if s == 0 {
                "0 seconds".to_string()
            } else {
                hms(s, true)
            }
        }
        DurationStyle::Coarse => {
            let s = whole_secs(secs);
            if s == 0 {
                String::new()
            } else {
                hms(s, s < 3600)
            }
        }
    }
}

fn whole_secs(secs: f64) -> u64 {
    if secs.is_finite() && secs > 0.0 {
        secs as u64
    } else {
        0
    }
}

/// "N unit(s)" pieces for the non-zero hours / minutes / (optionally) seconds of a
/// positive number of seconds.
fn hms(secs: u64, with_seconds: bool) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let mut parts = Vec::new();
    if h > 0 {
        parts.push(unit(h, "hour"));
    }
    if m > 0 {
        parts.push(unit(m, "minute"));
    }
    if with_seconds && s > 0 {
        parts.push(unit(s, "second"));
    }
    parts.join(" ")
}

fn unit(n: u64, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::DurationStyle::*;
    use super::*;

    #[test]
    fn exact_matches_the_timer_humanizers() {
        assert_eq!(duration(45.0, Exact), "45 seconds");
        assert_eq!(duration(60.0, Exact), "1 minute");
        assert_eq!(duration(90.0, Exact), "1 minute 30 seconds");
        assert_eq!(duration(300.0, Exact), "5 minutes");
        assert_eq!(duration(5400.0, Exact), "1 hour 30 minutes");
        assert_eq!(duration(3661.0, Exact), "1 hour 1 minute 1 second");
        assert_eq!(duration(0.0, Exact), "0 seconds");
    }

    #[test]
    fn coarse_matches_the_recipe_humanizer() {
        assert_eq!(duration(1500.0, Coarse), "25 minutes");
        assert_eq!(duration(5400.0, Coarse), "1 hour 30 minutes");
        assert_eq!(duration(3600.0, Coarse), "1 hour");
        assert_eq!(duration(3661.0, Coarse), "1 hour 1 minute");
        assert_eq!(duration(45.0, Coarse), "45 seconds");
        assert_eq!(duration(0.0, Coarse), "");
    }

    #[test]
    fn minutes_matches_the_directions_humanizer() {
        assert_eq!(duration(20.0, Minutes), "under a minute");
        assert_eq!(duration(29.6, Minutes), "under a minute");
        assert_eq!(duration(60.0, Minutes), "1 minute");
        assert_eq!(duration(1440.0, Minutes), "24 minutes");
        assert_eq!(duration(3600.0, Minutes), "1 hour");
        assert_eq!(duration(4200.0, Minutes), "1 hour 10 minutes");
        assert_eq!(duration(-300.0, Minutes), "5 minutes");
    }
}
