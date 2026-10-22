//! Device timers, from the Core's side.
//!
//! - [`MAX_TIMER_SECS`]: the limit shared by every path that starts a timer (the LLM's
//!   `set_timer` tool in `llm::rig` and the System-1 `timer` fast path in `orchestrator`).
//! - [`DisplayTimers`]: the latest timer list each display has reported over its
//!   persistent `role=notify` channel (`anamanti-timers`). Displays own their timers;
//!   this is a read-only mirror so the Companion app can show every display's timers
//!   (`GET /timers.json` on the companion listener). A display's entry goes away when
//!   its notify channel closes, because its timers can no longer be known.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::wyoming::protocol::ReportedTimer;

/// Hard cap on a timer duration (24 h) so a mis-parsed request can't spin an
/// effectively-infinite timer on the device. The `set_timer` tool clamps to it; the
/// System-1 fast path defers a longer request to System-2.
pub const MAX_TIMER_SECS: u64 = 24 * 60 * 60;

/// One display's last report.
#[derive(Debug, Clone)]
struct DisplayReport {
    /// The notify-channel connection that sent it, so a stale connection closing
    /// after a reconnect can't wipe the newer connection's report.
    conn_id: u64,
    name: String,
    /// Core wall-clock (Unix ms) when the report arrived; `remaining_secs` is
    /// relative to it.
    reported_at_ms: u64,
    timers: Vec<ReportedTimer>,
}

/// The latest timer list from every connected display, keyed by `device_id`.
#[derive(Debug, Default)]
pub struct DisplayTimers {
    displays: Mutex<HashMap<String, DisplayReport>>,
}

impl DisplayTimers {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `timers` (the display's full list) from notify connection `conn_id`,
    /// received at Core time `now_ms`. Replaces that display's previous report.
    pub fn report(
        &self,
        conn_id: u64,
        device_id: &str,
        name: &str,
        timers: Vec<ReportedTimer>,
        now_ms: u64,
    ) {
        self.displays.lock().unwrap().insert(
            device_id.to_string(),
            DisplayReport {
                conn_id,
                name: name.to_string(),
                reported_at_ms: now_ms,
                timers,
            },
        );
    }

    /// Forget the display whose report came from `conn_id` (its notify channel
    /// closed). A no-op when a newer connection has since reported for that display.
    pub fn connection_closed(&self, conn_id: u64) {
        self.displays
            .lock()
            .unwrap()
            .retain(|_, r| r.conn_id != conn_id);
    }

    /// The `/timers.json` payload at Core time `now_ms`:
    /// `{ok, now_ms, displays:[{device_id, name, reported_at_ms, timers:[{id, label,
    /// duration_secs, remaining_secs, ringing, ends_at_ms}]}]}`. `remaining_secs` is
    /// recomputed for `now_ms` (never below 0). Displays are sorted by name, then id;
    /// timers soonest first.
    pub fn to_json(&self, now_ms: u64) -> Value {
        let displays = self.displays.lock().unwrap();
        let mut list: Vec<(&String, &DisplayReport)> = displays.iter().collect();
        list.sort_by(|a, b| a.1.name.cmp(&b.1.name).then_with(|| a.0.cmp(b.0)));
        let displays: Vec<Value> = list
            .into_iter()
            .map(|(device_id, r)| {
                let elapsed_secs = now_ms.saturating_sub(r.reported_at_ms) / 1000;
                let mut timers: Vec<&ReportedTimer> = r.timers.iter().collect();
                timers.sort_by_key(|t| (t.remaining_secs, t.id));
                let timers: Vec<Value> = timers
                    .into_iter()
                    .map(|t| {
                        let remaining = if t.ringing {
                            0
                        } else {
                            t.remaining_secs.saturating_sub(elapsed_secs)
                        };
                        json!({
                            "id": t.id,
                            "label": t.label,
                            "duration_secs": t.duration_secs,
                            "remaining_secs": remaining,
                            "ringing": t.ringing,
                            "ends_at_ms": r.reported_at_ms + t.remaining_secs * 1000,
                        })
                    })
                    .collect();
                json!({
                    "device_id": device_id,
                    "name": r.name,
                    "reported_at_ms": r.reported_at_ms,
                    "timers": timers,
                })
            })
            .collect();
        json!({ "ok": true, "now_ms": now_ms, "displays": displays })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timer(id: u32, label: &str, remaining: u64, ringing: bool) -> ReportedTimer {
        ReportedTimer {
            id,
            label: label.into(),
            duration_secs: 600,
            remaining_secs: remaining,
            ringing,
        }
    }

    #[test]
    fn empty_registry_reports_no_displays() {
        let t = DisplayTimers::new();
        assert_eq!(
            t.to_json(5),
            json!({ "ok": true, "now_ms": 5, "displays": [] })
        );
    }

    #[test]
    fn remaining_counts_down_from_the_report_and_clamps_at_zero() {
        let t = DisplayTimers::new();
        t.report(
            1,
            "dev-k",
            "Kitchen",
            vec![timer(2, "pasta", 125, false), timer(1, "", 30, false)],
            10_000,
        );
        let v = t.to_json(10_000 + 20_500); // 20 whole seconds later
        let timers = &v["displays"][0]["timers"];
        assert_eq!(v["displays"][0]["name"], "Kitchen");
        assert_eq!(v["displays"][0]["device_id"], "dev-k");
        // Soonest first.
        assert_eq!(timers[0]["id"], 1);
        assert_eq!(timers[0]["remaining_secs"], 10);
        assert_eq!(timers[0]["ends_at_ms"], 40_000);
        assert_eq!(timers[1]["label"], "pasta");
        assert_eq!(timers[1]["remaining_secs"], 105);

        let later = t.to_json(10_000 + 600_000);
        assert_eq!(later["displays"][0]["timers"][0]["remaining_secs"], 0);
    }

    #[test]
    fn ringing_timers_report_zero_remaining() {
        let t = DisplayTimers::new();
        t.report(1, "d", "Den", vec![timer(7, "tea", 0, true)], 0);
        let v = t.to_json(1_000);
        assert_eq!(v["displays"][0]["timers"][0]["ringing"], true);
        assert_eq!(v["displays"][0]["timers"][0]["remaining_secs"], 0);
    }

    #[test]
    fn a_new_report_replaces_and_an_empty_list_is_kept() {
        let t = DisplayTimers::new();
        t.report(1, "d", "Den", vec![timer(1, "a", 60, false)], 0);
        t.report(1, "d", "Den", vec![], 1_000);
        let v = t.to_json(2_000);
        assert_eq!(v["displays"].as_array().unwrap().len(), 1);
        assert_eq!(v["displays"][0]["timers"], json!([]));
    }

    #[test]
    fn closing_a_stale_connection_keeps_the_newer_report() {
        let t = DisplayTimers::new();
        t.report(1, "d", "Den", vec![timer(1, "a", 60, false)], 0);
        // The display reconnected (conn 2) before conn 1's task noticed the close.
        t.report(2, "d", "Den", vec![timer(1, "a", 50, false)], 1_000);
        t.connection_closed(1);
        assert_eq!(t.to_json(1_000)["displays"].as_array().unwrap().len(), 1);
        t.connection_closed(2);
        assert_eq!(t.to_json(1_000)["displays"], json!([]));
    }

    #[test]
    fn displays_sort_by_name() {
        let t = DisplayTimers::new();
        t.report(1, "z", "Kitchen", vec![], 0);
        t.report(2, "a", "Bedroom", vec![], 0);
        let v = t.to_json(0);
        assert_eq!(v["displays"][0]["name"], "Bedroom");
        assert_eq!(v["displays"][1]["name"], "Kitchen");
    }
}
