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

/// Registry of connected notify channels. Cheap to share behind an `Arc`; construct
/// once at boot and hand a clone to both the device-facing server (which registers
/// live channels) and the config page (which enqueues test notifications).
#[derive(Default)]
pub struct NotificationService {
    /// conn_id → the sender feeding that connection's write pump.
    conns: Mutex<HashMap<u64, mpsc::UnboundedSender<WyomingEvent>>>,
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
    pub fn register(&self, _device_id: &str) -> (u64, mpsc::UnboundedReceiver<WyomingEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let conn_id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        self.conns.lock().unwrap().insert(conn_id, tx);
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

    /// Push `note` to every connected device. Dead channels (receiver dropped) are
    /// pruned. Returns how many channels the notification was delivered to.
    ///
    /// Phase A fans out to all connected devices; per-device targeting arrives with
    /// the multi-display work.
    pub fn notify(&self, note: &Notification) -> usize {
        let event = note.to_event();
        let mut conns = self.conns.lock().unwrap();
        let mut delivered = 0usize;
        conns.retain(|_, tx| match tx.send(event.clone()) {
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

        let (id_a, mut rx_a) = svc.register("dev-a");
        let (_id_b, rx_b) = svc.register("dev-b");
        assert_eq!(svc.connected(), 2);

        // Drop one receiver → it is pruned on the next push, and only the live one
        // receives the frame.
        drop(rx_b);
        assert_eq!(svc.notify(&note()), 1);
        assert_eq!(svc.connected(), 1);

        let got = rx_a.try_recv().expect("live channel received the push");
        assert_eq!(got.event_type, types::AMBIENT_NOTIFY);
        assert_eq!(got.data["title"], serde_json::json!("Hi"));

        svc.deregister(id_a);
        assert_eq!(svc.connected(), 0);
    }

    #[test]
    fn ids_are_unique() {
        let svc = NotificationService::new();
        let a = svc.new_id();
        let b = svc.new_id();
        assert_ne!(a, b);
    }
}
