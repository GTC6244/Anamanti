//! The shared driver behind every persistent, device-dialed push channel
//! ([`super::notify`], [`super::weather`], [`super::music`]).
//!
//! The device dials the orchestrator's Wyoming endpoint, sends its channel's
//! `anamanti-hello` frame (which carries the `role`), then holds the socket open reading
//! push frames — reconnecting with capped exponential backoff whenever the connection
//! drops or the Mac is unreachable. This is a *separate*, long-lived socket from the
//! per-turn voice connection (`client.rs`): the voice turn stays request-driven, while
//! these channels exist purely so the orchestrator can reach the display unprompted.
//!
//! The reconnect loop reuses the same mDNS discovery + orchestrator pinning the voice
//! path uses ([`resolve`]), so a pinned orchestrator only ever accepts pushes from that
//! Mac. The driver is transport-only: each channel supplies a `decode` that picks its own
//! frames out of the stream (anything else is ignored) and an `on_push` callback (the
//! FRB layer turns those into stream events for Flutter).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::BufReader;
use tokio::net::TcpStream;

use super::discovery::{resolve, EndpointCache};
use super::protocol::{self, WyomingEvent};

/// Shortest / longest wait between reconnect attempts.
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// How long a blocking read waits before it yields so the loop can re-check the
/// `running` flag (also bounds how quickly a stop is noticed).
const READ_POLL: Duration = Duration::from_secs(2);

/// One push channel's identity: a log/error `label` (`"notify"`, `"weather"`, …) and the
/// `anamanti-hello` frame that registers it with the orchestrator.
pub struct Channel {
    pub label: &'static str,
    pub hello: WyomingEvent,
}

/// Run `channel` until `running` is cleared or the consumer goes away. Every frame
/// `decode` accepts is handed to `on_push`, which returns `false` when the downstream
/// consumer (the FRB sink) has been dropped — that ends the loop. Reconnects with capped
/// exponential backoff on any drop or failure.
pub async fn run<T, D, F>(
    cache: &EndpointCache,
    discovery_timeout: Duration,
    orchestrator_key: Option<String>,
    channel: Channel,
    running: Arc<AtomicBool>,
    decode: D,
    mut on_push: F,
) where
    D: Fn(&WyomingEvent) -> Option<T>,
    F: FnMut(T) -> bool,
{
    let mut backoff = MIN_BACKOFF;
    while running.load(Ordering::SeqCst) {
        match connect_and_listen(
            cache,
            discovery_timeout,
            orchestrator_key.as_deref(),
            &channel,
            &running,
            &decode,
            &mut on_push,
        )
        .await
        {
            Ok(ListenEnd::ConsumerGone) => return,
            // Clean close (or a stop): reconnect promptly.
            Ok(ListenEnd::Closed) => backoff = MIN_BACKOFF,
            Err(e) => {
                log::debug!("{} channel: {e:#}; retrying in {backoff:?}", channel.label);
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

/// One connection lifecycle: resolve → dial → register (hello) → read pushes until the
/// socket closes, we're stopped, or the consumer goes away.
async fn connect_and_listen<T, D, F>(
    cache: &EndpointCache,
    discovery_timeout: Duration,
    orchestrator_key: Option<&str>,
    channel: &Channel,
    running: &Arc<AtomicBool>,
    decode: &D,
    on_push: &mut F,
) -> Result<ListenEnd>
where
    D: Fn(&WyomingEvent) -> Option<T>,
    F: FnMut(T) -> bool,
{
    let label = channel.label;
    let endpoint = resolve(cache, discovery_timeout, orchestrator_key)
        .await
        .with_context(|| format!("resolving orchestrator for {label} channel"))?;
    let stream = TcpStream::connect(endpoint.socket_addr())
        .await
        .with_context(|| format!("dialing {endpoint} for {label} channel"))?;
    stream.set_nodelay(true).ok();
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut writer = write_half;

    protocol::write_event(&mut writer, &channel.hello)
        .await
        .with_context(|| format!("sending anamanti-hello ({label})"))?;
    log::info!("{label} channel connected to {endpoint}");

    loop {
        if !running.load(Ordering::SeqCst) {
            return Ok(ListenEnd::Closed);
        }
        // Bound the blocking read so we periodically re-check `running`.
        match tokio::time::timeout(READ_POLL, protocol::read_event(&mut reader)).await {
            Err(_elapsed) => continue, // no frame yet — re-check running and read again
            Ok(read) => match read.with_context(|| format!("reading {label} frame"))? {
                None => return Ok(ListenEnd::Closed), // clean peer close
                Some(ev) => {
                    // Any frame this channel doesn't decode is ignored.
                    if let Some(item) = decode(&ev) {
                        if !on_push(item) {
                            return Ok(ListenEnd::ConsumerGone);
                        }
                    }
                }
            },
        }
    }
}
