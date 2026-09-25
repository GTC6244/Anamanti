//! The always-on ambient weather push: a registry of connected weather channels plus
//! a periodic task that fetches current conditions for the household location and
//! fans them out as `anamanti-weather` `{"action":"current"}` frames — so the small
//! icon + temperature beside the idle clock stay fresh without a voice turn.
//!
//! Structurally a twin of [`crate::notify::NotificationService`]: the socket is owned
//! by the per-connection task in [`crate::server`]; this registry only holds an mpsc
//! *sender* into that task. A newly-registered channel is immediately sent the
//! last-known report so the indicator appears at once rather than after the next tick.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use serde_json::{to_value as json_to_value, Value};

use crate::directions::LiveHomeLocation;
use crate::weather::{WeatherProvider, WeatherReport};
use crate::wyoming::protocol::WyomingEvent;

/// Serialize a report to the JSON `weather` payload carried in the frame; an empty
/// object on the impossible serialize failure keeps the push infallible.
fn to_value(report: &WeatherReport) -> Value {
    json_to_value(report).unwrap_or_default()
}

/// Registry of connected weather channels + the last-known report. Cheap to share
/// behind an `Arc`; construct once at boot and hand a clone to the device-facing
/// server (which registers live channels) and the periodic push task.
#[derive(Default)]
pub struct WeatherService {
    /// conn_id → the sender feeding that connection's write pump.
    conns: Mutex<HashMap<u64, mpsc::UnboundedSender<WyomingEvent>>>,
    /// Monotonic connection-handle allocator.
    next_conn: AtomicU64,
    /// The most recent report, replayed to a channel the moment it connects.
    last: Mutex<Option<WeatherReport>>,
}

impl WeatherService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a newly-opened weather channel. Returns a connection handle (used to
    /// [`deregister`](Self::deregister) on close) and the receiver the connection task
    /// drains to write pushes out. If a report is already cached it is queued
    /// immediately so the device shows conditions without waiting for the next tick.
    pub fn register(&self, _device_id: &str) -> (u64, mpsc::UnboundedReceiver<WyomingEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        if let Some(report) = self.last.lock().unwrap().as_ref() {
            let _ = tx.send(WyomingEvent::weather_current(to_value(report)));
        }
        let conn_id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        self.conns.lock().unwrap().insert(conn_id, tx);
        (conn_id, rx)
    }

    /// Drop a channel from the registry (its task is ending / the device closed).
    pub fn deregister(&self, conn_id: u64) {
        self.conns.lock().unwrap().remove(&conn_id);
    }

    /// How many weather channels are currently connected.
    pub fn connected(&self) -> usize {
        self.conns.lock().unwrap().len()
    }

    /// Cache `report` and push it to every connected device as a `current` frame.
    /// Dead channels (receiver dropped) are pruned. Returns the delivery count.
    pub fn broadcast(&self, report: &WeatherReport) -> usize {
        *self.last.lock().unwrap() = Some(report.clone());
        let event = WyomingEvent::weather_current(to_value(report));
        let mut conns = self.conns.lock().unwrap();
        let mut delivered = 0usize;
        conns.retain(|_, tx| match tx.send(event.clone()) {
            Ok(()) => {
                delivered += 1;
                true
            }
            Err(_) => false,
        });
        delivered
    }
}

/// Spawn the periodic ambient-weather push. Every `interval` it reads the live home
/// location, fetches current conditions from `provider`, and broadcasts them to all
/// connected weather channels. No-ops on a tick when no location is set or the fetch
/// fails (the previous report stays on screen). Returns immediately; the task runs for
/// the process lifetime.
pub fn spawn_periodic(
    service: Arc<WeatherService>,
    provider: Arc<dyn WeatherProvider>,
    home_location: LiveHomeLocation,
    imperial: bool,
    interval: Duration,
) {
    tokio::spawn(async move {
        // A tiny initial delay lets the device dial its channel before the first push.
        tokio::time::sleep(Duration::from_secs(2)).await;
        loop {
            match home_location.get() {
                Some(loc) => match provider.fetch(&loc, imperial).await {
                    Ok(report) => {
                        let n = service.broadcast(&report);
                        log::debug!(
                            "weather push: {} in {} → {n} device(s)",
                            report.current.temp,
                            report.location_label
                        );
                    }
                    Err(e) => log::warn!("weather push: fetch failed: {e:#}"),
                },
                None => log::debug!("weather push: no home location set; skipping tick"),
            }
            tokio::time::sleep(interval).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::weather::{CurrentConditions, WeatherReport};
    use crate::wyoming::protocol::types;

    fn report() -> WeatherReport {
        WeatherReport {
            location_label: "Austin, Texas".into(),
            units: "imperial".into(),
            current: CurrentConditions {
                temp: 72,
                description: "partly cloudy".into(),
                ..Default::default()
            },
            daily: Vec::new(),
        }
    }

    #[test]
    fn broadcasts_and_prunes_dead_channels() {
        let svc = WeatherService::new();
        assert_eq!(svc.connected(), 0);

        let (id_a, mut rx_a) = svc.register("dev-a");
        let (_id_b, rx_b) = svc.register("dev-b");
        assert_eq!(svc.connected(), 2);

        drop(rx_b);
        assert_eq!(svc.broadcast(&report()), 1);
        assert_eq!(svc.connected(), 1);

        let got = rx_a.try_recv().expect("live channel received the push");
        assert_eq!(got.event_type, types::WEATHER);
        assert_eq!(got.data["action"], serde_json::json!("current"));

        svc.deregister(id_a);
        assert_eq!(svc.connected(), 0);
    }

    #[test]
    fn a_new_channel_gets_the_last_report_immediately() {
        let svc = WeatherService::new();
        svc.broadcast(&report()); // caches, nobody connected yet
        let (_id, mut rx) = svc.register("late");
        let got = rx
            .try_recv()
            .expect("late joiner replayed the cached report");
        assert_eq!(got.event_type, types::WEATHER);
    }
}
