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

/// A registry of live persistent push channels (one per device connection), each an
/// mpsc sender into the connection's write pump in [`crate::server`] plus per-channel
/// metadata `M`. Shared by the notify channel ([`NotificationService`]) and the
/// ambient weather push ([`crate::weather::WeatherService`]).
///
/// The registry never touches a socket: pushing is a channel send, and a channel whose
/// receiver is gone (the device disconnected) is pruned on the next
/// [`broadcast`](Self::broadcast).
pub struct ChannelRegistry<M> {
    /// conn_id → that connection's write-pump sender + metadata.
    conns: Mutex<HashMap<u64, (mpsc::UnboundedSender<WyomingEvent>, M)>>,
    /// Monotonic connection-handle allocator.
    next_conn: AtomicU64,
}

impl<M> Default for ChannelRegistry<M> {
    fn default() -> Self {
        Self {
            conns: Mutex::new(HashMap::new()),
            next_conn: AtomicU64::new(0),
        }
    }
}

impl<M> ChannelRegistry<M> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a newly-opened channel carrying `meta`. `initial` (if any) is queued
    /// before the channel becomes visible to [`broadcast`](Self::broadcast), so it is
    /// always the first frame the device sees. Returns the connection handle (for
    /// [`deregister`](Self::deregister)) and the receiver the connection task drains.
    pub fn register(
        &self,
        meta: M,
        initial: Option<WyomingEvent>,
    ) -> (u64, mpsc::UnboundedReceiver<WyomingEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        if let Some(ev) = initial {
            let _ = tx.send(ev);
        }
        let conn_id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        self.conns.lock().unwrap().insert(conn_id, (tx, meta));
        (conn_id, rx)
    }

    /// Drop a channel (its task is ending / the device closed).
    pub fn deregister(&self, conn_id: u64) {
        self.conns.lock().unwrap().remove(&conn_id);
    }

    /// How many channels are currently registered.
    pub fn len(&self) -> usize {
        self.conns.lock().unwrap().len()
    }

    /// Whether no channel is registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Send `event` to every channel, pruning the dead ones. Returns how many channels
    /// it was delivered to.
    pub fn broadcast(&self, event: &WyomingEvent) -> usize {
        self.broadcast_with(|_| event.clone())
    }

    /// Send each channel the event `f` builds from that channel's metadata (a
    /// per-device push, e.g. each display's own music widget style), pruning the dead
    /// ones. Returns how many channels it was delivered to.
    pub fn broadcast_with(&self, f: impl Fn(&M) -> WyomingEvent) -> usize {
        let mut conns = self.conns.lock().unwrap();
        let mut delivered = 0usize;
        conns.retain(|_, (tx, meta)| match tx.send(f(meta)) {
            Ok(()) => {
                delivered += 1;
                true
            }
            Err(_) => false, // receiver gone — prune this channel
        });
        delivered
    }

    /// Map every registered channel's metadata through `f` (unordered).
    pub fn list<T>(&self, f: impl Fn(&M) -> T) -> Vec<T> {
        self.conns
            .lock()
            .unwrap()
            .values()
            .map(|(_, meta)| f(meta))
            .collect()
    }
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
/// live channels) and the config page (which enqueues test notifications). Each
/// channel remembers the identity the device announced in its `anamanti-hello` frame,
/// so the config page can list which displays are connected.
#[derive(Default)]
pub struct NotificationService {
    channels: ChannelRegistry<ConnectedDevice>,
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
        self.channels.register(
            ConnectedDevice {
                device_id: device_id.to_string(),
                name: name.to_string(),
            },
            None,
        )
    }

    /// Drop a channel from the registry (its task is ending / the device closed).
    pub fn deregister(&self, conn_id: u64) {
        self.channels.deregister(conn_id);
    }

    /// How many notify channels are currently connected.
    pub fn connected(&self) -> usize {
        self.channels.len()
    }

    /// The identities (device_id + name) of every connected display, de-duplicated by
    /// `device_id` and sorted by name then id — for the config page's device list. A
    /// device holds one notify channel, so this is the canonical "who's connected" view.
    pub fn connected_devices(&self) -> Vec<ConnectedDevice> {
        let mut seen = std::collections::BTreeMap::new();
        for dev in self.channels.list(ConnectedDevice::clone) {
            seen.entry(dev.device_id.clone()).or_insert(dev);
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
        self.channels.broadcast(&note.to_event())
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
    fn registry_queues_the_initial_frame_first_and_lists_metadata() {
        let reg: ChannelRegistry<&'static str> = ChannelRegistry::new();
        let (_id, mut rx) = reg.register("kitchen", Some(note().to_event()));
        assert_eq!(reg.broadcast(&WyomingEvent::audio_stop(0)), 1);
        assert_eq!(rx.try_recv().unwrap().event_type, types::ANAMANTI_NOTIFY);
        assert_eq!(rx.try_recv().unwrap().event_type, types::AUDIO_STOP);
        assert_eq!(reg.list(|m| m.to_string()), vec!["kitchen".to_string()]);
        drop(rx);
        assert_eq!(reg.broadcast(&WyomingEvent::audio_stop(0)), 0);
        assert!(reg.is_empty(), "dead channel pruned on broadcast");
    }

    #[test]
    fn ids_are_unique() {
        let svc = NotificationService::new();
        let a = svc.new_id();
        let b = svc.new_id();
        assert_ne!(a, b);
    }
}
