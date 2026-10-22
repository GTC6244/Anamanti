//! Persistent proactive-notification channel (Approach A, visual-only phase;
//! `role=notify`): reads `anamanti-notify` push frames so the orchestrator can reach the
//! display unprompted. The connection, registration and reconnect logic is the shared
//! [`super::push_channel`] driver (see there for the transport details); this module
//! only decodes each frame into a [`Notification`].
//!
//! The same socket also carries the device's timer list *up* to the orchestrator: the
//! caller passes an [`Outbound`] (the engine's `anamanti-timers` report), which the
//! driver sends after the hello and again on every timer change.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use super::discovery::EndpointCache;
use super::protocol::{types, WyomingEvent};
use super::push_channel::{self, Channel, Outbound};

/// A proactive notification decoded from an `anamanti-notify` frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    /// Stable id (for dedup / ack in a later phase).
    pub id: String,
    /// `"info"` | `"reminder"` | `"alert"` (defaults to `"info"` if absent).
    pub priority: String,
    /// Short headline.
    pub title: String,
    /// Body text.
    pub body: String,
}

impl Notification {
    /// Decode an `anamanti-notify` frame, or `None` if `ev` is a different frame.
    pub fn from_event(ev: &WyomingEvent) -> Option<Self> {
        if ev.event_type != types::ANAMANTI_NOTIFY {
            return None;
        }
        let field = |key: &str| {
            ev.data
                .get(key)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let priority = field("priority");
        Some(Self {
            id: field("id"),
            priority: if priority.is_empty() {
                "info".to_string()
            } else {
                priority
            },
            title: field("title"),
            body: field("body"),
        })
    }
}

/// Run the persistent notify channel until `running` is cleared or the consumer goes
/// away (see [`push_channel::run`]). Each decoded notification is handed to
/// `on_notification`. `report`, when given, is kept current up the same socket (see
/// [`Outbound`]).
#[allow(clippy::too_many_arguments)]
pub async fn run<F>(
    cache: &EndpointCache,
    discovery_timeout: Duration,
    orchestrator_key: Option<String>,
    device_id: String,
    device_name: String,
    running: Arc<AtomicBool>,
    report: Option<Outbound>,
    on_notification: F,
) where
    F: FnMut(Notification) -> bool,
{
    // `instance_id` is left empty — the orchestrator only needs the role + device id +
    // friendly name.
    let channel = Channel {
        label: "notify",
        hello: WyomingEvent::hello(device_id, "", device_name),
        outbound: report,
    };
    push_channel::run(
        cache,
        discovery_timeout,
        orchestrator_key,
        channel,
        running,
        Notification::from_event,
        on_notification,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_notify_frame() {
        let ev = WyomingEvent::notify("7-0", "alert", "Doorbell", "Someone is at the door");
        let note = Notification::from_event(&ev).expect("decodes");
        assert_eq!(
            note,
            Notification {
                id: "7-0".into(),
                priority: "alert".into(),
                title: "Doorbell".into(),
                body: "Someone is at the door".into(),
            }
        );
    }

    #[test]
    fn missing_priority_defaults_to_info() {
        let ev = WyomingEvent::with_data(types::ANAMANTI_NOTIFY, json!({ "title": "Hi" }));
        let note = Notification::from_event(&ev).expect("decodes");
        assert_eq!(note.priority, "info");
        assert_eq!(note.title, "Hi");
        assert!(note.body.is_empty());
    }

    #[test]
    fn ignores_non_notify_frames() {
        assert_eq!(Notification::from_event(&WyomingEvent::interrupt()), None);
    }
}
