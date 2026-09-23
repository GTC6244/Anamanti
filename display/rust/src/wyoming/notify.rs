//! Persistent proactive-notification channel (Approach A, visual-only phase).
//!
//! The device dials the orchestrator's Wyoming endpoint, sends an `ambient-hello`
//! `role=notify` frame to register, then holds the socket open reading
//! `ambient-notify` push frames — reconnecting with capped exponential backoff
//! whenever the connection drops or the Mac is unreachable. This is a *separate*,
//! long-lived socket from the per-turn voice connection (`client.rs`): the voice
//! turn stays request-driven, while this channel exists purely so the orchestrator
//! can reach the display unprompted.
//!
//! The reconnect loop reuses the same mDNS discovery + orchestrator pinning the
//! voice path uses (`discovery::resolve`), so a pinned orchestrator only ever
//! accepts pushes from that Mac. The driver is transport-only: it forwards each
//! decoded [`Notification`] to a callback (the FRB layer turns those into stream
//! events for Flutter).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::BufReader;
use tokio::net::TcpStream;

use super::discovery::{resolve, EndpointCache};
use super::protocol::{self, types, WyomingEvent};

/// Shortest / longest wait between reconnect attempts.
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// How long a blocking read waits before it yields so the loop can re-check the
/// `running` flag (also bounds how quickly a stop is noticed).
const READ_POLL: Duration = Duration::from_secs(2);

/// A proactive notification decoded from an `ambient-notify` frame.
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
    /// Decode an `ambient-notify` frame, or `None` if `ev` is a different frame.
    pub fn from_event(ev: &WyomingEvent) -> Option<Self> {
        if ev.event_type != types::AMBIENT_NOTIFY {
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
/// away. Each decoded notification is handed to `on_notification`, which returns
/// `false` when the downstream consumer (the FRB sink) has been dropped — that ends
/// the loop. Reconnects with capped exponential backoff on any drop or failure.
pub async fn run<F>(
    cache: &EndpointCache,
    discovery_timeout: Duration,
    orchestrator_key: Option<String>,
    device_id: String,
    running: Arc<AtomicBool>,
    mut on_notification: F,
) where
    F: FnMut(Notification) -> bool,
{
    let mut backoff = MIN_BACKOFF;
    while running.load(Ordering::SeqCst) {
        match connect_and_listen(
            cache,
            discovery_timeout,
            orchestrator_key.as_deref(),
            &device_id,
            &running,
            &mut on_notification,
        )
        .await
        {
            Ok(ListenEnd::ConsumerGone) => return,
            Ok(ListenEnd::Closed) => {
                // Clean close (or a stop): reconnect promptly.
                backoff = MIN_BACKOFF;
            }
            Err(e) => {
                log::debug!("notify channel: {e:#}; retrying in {backoff:?}");
                cache.clear(); // force a fresh mDNS browse next attempt
            }
        }
        // Sleep before retrying, but wake early if we're being stopped.
        let mut waited = Duration::ZERO;
        while running.load(Ordering::SeqCst) && waited < backoff {
            let step = READ_POLL.min(backoff - waited);
            tokio::time::sleep(step).await;
            waited += step;
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Why a single connection ended.
enum ListenEnd {
    /// The socket closed (peer close) or we were asked to stop.
    Closed,
    /// The downstream consumer was dropped — the whole channel should end.
    ConsumerGone,
}

/// One connection lifecycle: resolve → dial → register → read pushes until the
/// socket closes, we're stopped, or the consumer goes away.
async fn connect_and_listen<F>(
    cache: &EndpointCache,
    discovery_timeout: Duration,
    orchestrator_key: Option<&str>,
    device_id: &str,
    running: &Arc<AtomicBool>,
    on_notification: &mut F,
) -> Result<ListenEnd>
where
    F: FnMut(Notification) -> bool,
{
    let endpoint = resolve(cache, discovery_timeout, orchestrator_key)
        .await
        .context("resolving orchestrator for notify channel")?;
    let stream = TcpStream::connect(endpoint.socket_addr())
        .await
        .with_context(|| format!("dialing {endpoint} for notify channel"))?;
    stream.set_nodelay(true).ok();
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut writer = write_half;

    // Register this connection as the device's notify channel. `instance_id` is left
    // empty — the orchestrator only needs the role + device id.
    protocol::write_event(&mut writer, &WyomingEvent::hello(device_id, ""))
        .await
        .context("sending ambient-hello")?;
    log::info!("notify channel connected to {endpoint}");

    loop {
        if !running.load(Ordering::SeqCst) {
            return Ok(ListenEnd::Closed);
        }
        // Bound the blocking read so we periodically re-check `running`.
        match tokio::time::timeout(READ_POLL, protocol::read_event(&mut reader)).await {
            Err(_elapsed) => continue, // no frame yet — re-check running and read again
            Ok(read) => match read.context("reading notify frame")? {
                None => return Ok(ListenEnd::Closed), // clean peer close
                Some(ev) => {
                    if let Some(note) = Notification::from_event(&ev) {
                        if !on_notification(note) {
                            return Ok(ListenEnd::ConsumerGone);
                        }
                    }
                    // Any other frame type on this channel is ignored.
                }
            },
        }
    }
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
        let ev = WyomingEvent::with_data(types::AMBIENT_NOTIFY, json!({ "title": "Hi" }));
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
