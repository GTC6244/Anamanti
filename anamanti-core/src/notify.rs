//! Proactive notifications (Approach A, visual-only phase).
//!
//! The device dials a *persistent* Wyoming connection and holds it open (an
//! `ambient-hello` `role=notify` frame registers it); the orchestrator then pushes
//! `ambient-notify` frames down that connection whenever it has something to show,
//! without the device starting a voice turn. This module owns the small registry of
//! live notify channels and the enqueue API that fan-outs a [`Notification`] to
//! them.
//!
//! The socket itself is owned by the per-connection task in [`crate::server`]; the
//! registry only holds an mpsc *sender* into that task, so pushing never touches the
//! socket directly (and a dropped receiver = a disconnected device is pruned on the
//! next push). This keeps the design a clean sidecar over the existing server accept
//! loop — no reverse dialing, no device-side listener.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;

use crate::wyoming::protocol::WyomingEvent;

/// A proactive notification to display on the device. Visual-only in this phase.
#[derive(Debug, Clone)]
pub struct Notification {
    /// Stable id (for dedup / ack / dismiss in a later phase).
    pub id: String,
    /// `"info"` | `"reminder"` | `"alert"`.
    pub priority: String,
    /// Short headline.
    pub title: String,
    /// Body text.
    pub body: String,
}

impl Notification {
    /// Render this notification as the `ambient-notify` wire frame.
    pub fn to_event(&self) -> WyomingEvent {
        WyomingEvent::notify(&self.id, &self.priority, &self.title, &self.body)
    }
}

/// One live notify channel: the sender feeding its write pump plus the identity the
/// device announced in its `anamanti-hello` frame (so the config page can list which
/// displays are connected, and a future phase can target a push per device).
struct Conn {
    tx: mpsc::UnboundedSender<WyomingEvent>,
    device_id: String,
    name: String,
}

/// A connected display's identity, for the config page's "Connected devices" list.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ConnectedDevice {
    /// The stable `device_id` the display announced (MAC-derived, e.g.
    /// `anamanti-140ac5942aca`).
    pub device_id: String,
    /// The human-friendly name the display announced (may be empty).
    pub name: String,
}

/// Registry of connected notify channels. Cheap to share behind an `Arc`; construct
/// once at boot and hand a clone to both the device-facing server (which registers
/// live channels) and the config page (which enqueues test notifications).
#[derive(Default)]
pub struct NotificationService {
    /// conn_id → that connection's write-pump sender + announced identity.
    conns: Mutex<HashMap<u64, Conn>>,
    /// Monotonic connection-handle allocator.
    next_conn: AtomicU64,
    /// Monotonic per-process notification sequence (for id minting).
    seq: AtomicU64,
}

impl NotificationService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a newly-opened notify channel. Returns a connection handle (used to
    /// [`deregister`](Self::deregister) on close) and the receiver the connection
    /// task drains to write pushes out to the device.
    pub fn register(
        &self,
        device_id: &str,
        name: &str,
    ) -> (u64, mpsc::UnboundedReceiver<WyomingEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let conn_id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        self.conns.lock().unwrap().insert(
            conn_id,
            Conn {
                tx,
                device_id: device_id.to_string(),
                name: name.to_string(),
            },
        );
        (conn_id, rx)
    }

    /// Drop a channel from the registry (its task is ending / the device closed).
    pub fn deregister(&self, conn_id: u64) {
        self.conns.lock().unwrap().remove(&conn_id);
    }

    /// How many notify channels are currently connected.
    pub fn connected(&self) -> usize {
        self.conns.lock().unwrap().len()
    }

    /// The identities (device_id + name) of every connected display, de-duplicated by
    /// `device_id` and sorted by name then id — for the config page's device list. A
    /// device holds one notify channel, so this is the canonical "who's connected" view.
    pub fn connected_devices(&self) -> Vec<ConnectedDevice> {
        let conns = self.conns.lock().unwrap();
        let mut seen = std::collections::BTreeMap::new();
        for conn in conns.values() {
            seen.entry(conn.device_id.clone())
                .or_insert_with(|| ConnectedDevice {
                    device_id: conn.device_id.clone(),
                    name: conn.name.clone(),
                });
        }
        let mut out: Vec<ConnectedDevice> = seen.into_values().collect();
        out.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| a.device_id.cmp(&b.device_id))
        });
        out
    }

    /// Push `note` to every connected device. Dead channels (receiver dropped) are
    /// pruned. Returns how many channels the notification was delivered to.
    ///
    /// Phase A fans out to all connected devices; per-device targeting arrives with
    /// the multi-display work.
    pub fn notify(&self, note: &Notification) -> usize {
        let event = note.to_event();
        let mut conns = self.conns.lock().unwrap();
        let mut delivered = 0usize;
        conns.retain(|_, conn| match conn.tx.send(event.clone()) {
            Ok(()) => {
                delivered += 1;
                true
            }
            Err(_) => false, // receiver gone — prune this channel
        });
        delivered
    }

    /// Mint a unique notification id: milliseconds since the epoch plus a
    /// process-local sequence, so ids are unique even within the same millisecond.
    pub fn new_id(&self) -> String {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        format!("{ms}-{n}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wyoming::protocol::types;

    fn note() -> Notification {
        Notification {
            id: "1-0".into(),
            priority: "info".into(),
            title: "Hi".into(),
            body: "there".into(),
        }
    }

    #[test]
    fn delivers_to_registered_channels_and_prunes_dead_ones() {
        let svc = NotificationService::new();
        assert_eq!(svc.connected(), 0);
        assert_eq!(svc.notify(&note()), 0); // nobody connected

        let (id_a, mut rx_a) = svc.register("dev-a", "Kitchen");
        let (_id_b, rx_b) = svc.register("dev-b", "Bedroom");
        assert_eq!(svc.connected(), 2);

        // The connected-devices list reports both identities, sorted by name.
        assert_eq!(
            svc.connected_devices(),
            vec![
                ConnectedDevice {
                    device_id: "dev-b".into(),
                    name: "Bedroom".into()
                },
                ConnectedDevice {
                    device_id: "dev-a".into(),
                    name: "Kitchen".into()
                },
            ]
        );

        // Drop one receiver → it is pruned on the next push, and only the live one
        // receives the frame.
        drop(rx_b);
        assert_eq!(svc.notify(&note()), 1);
        assert_eq!(svc.connected(), 1);

        let got = rx_a.try_recv().expect("live channel received the push");
        assert_eq!(got.event_type, types::ANAMANTI_NOTIFY);
        assert_eq!(got.data["title"], serde_json::json!("Hi"));

        svc.deregister(id_a);
        assert_eq!(svc.connected(), 0);
        assert!(svc.connected_devices().is_empty());
    }

    #[test]
    fn ids_are_unique() {
        let svc = NotificationService::new();
        let a = svc.new_id();
        let b = svc.new_id();
        assert_ne!(a, b);
    }
}
