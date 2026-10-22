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
//!
//! A channel may also carry an optional [`Outbound`] report (the notify channel's
//! `anamanti-timers` list): the driver sends its current frame right after the hello on
//! every (re)connect and again whenever its `watch` signals a change, alongside the read
//! loop. The read future is kept alive across loop iterations (never dropped mid-frame),
//! because `protocol::read_event` is not cancellation-safe.

use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::BufReader;
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::TcpStream;
use tokio::sync::watch;

use super::discovery::{resolve, EndpointCache};
use super::protocol::{self, WyomingEvent};

/// Shortest / longest wait between reconnect attempts.
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// How long the loop waits for a frame (or an outbound change) before it re-checks the
/// `running` flag (also bounds how quickly a stop is noticed).
const READ_POLL: Duration = Duration::from_secs(2);
/// An outbound report write that can't complete in this long (peer not reading) is
/// treated as a dead connection and the channel reconnects.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// One push channel's identity: a log/error `label` (`"notify"`, `"weather"`, …), the
/// `anamanti-hello` frame that registers it with the orchestrator, and an optional
/// device → orchestrator report kept current on the socket.
pub struct Channel {
    pub label: &'static str,
    pub hello: WyomingEvent,
    pub outbound: Option<Outbound>,
}

/// A device → orchestrator report a channel keeps current: `frame` builds it fresh at
/// send time (so relative values like time-remaining are accurate), and `changes` is
/// signalled whenever it should be resent. Sent after the hello on every (re)connect
/// and on each change; changes that land while a write is in flight coalesce.
pub struct Outbound {
    pub changes: watch::Receiver<()>,
    pub frame: Box<dyn Fn() -> WyomingEvent + Send + Sync>,
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
    let mut writer = write_half;

    protocol::write_event(&mut writer, &channel.hello)
        .await
        .with_context(|| format!("sending anamanti-hello ({label})"))?;
    log::info!("{label} channel connected to {endpoint}");

    // Outbound report: mark the current value seen *before* building the frame, so a
    // change racing the build still triggers a resend.
    let mut changes = channel.outbound.as_ref().map(|o| o.changes.clone());
    if let (Some(rx), Some(out)) = (changes.as_mut(), channel.outbound.as_ref()) {
        rx.borrow_and_update();
        send_outbound(&mut writer, out, label).await?;
    }

    // One read future lives across iterations (re-armed only after it completes), so
    // the poll timeout / outbound branch never drops a half-read frame.
    let mut read = pin!(read_next(BufReader::new(read_half)));
    loop {
        if !running.load(Ordering::SeqCst) {
            return Ok(ListenEnd::Closed);
        }
        tokio::select! {
            (reader, result) = &mut read => {
                read.set(read_next(reader));
                match result.with_context(|| format!("reading {label} frame"))? {
                    None => return Ok(ListenEnd::Closed), // clean peer close
                    Some(ev) => {
                        // Any frame this channel doesn't decode is ignored.
                        if let Some(item) = decode(&ev) {
                            if !on_push(item) {
                                return Ok(ListenEnd::ConsumerGone);
                            }
                        }
                    }
                }
            }
            changed = outbound_changed(&mut changes) => {
                match (changed, channel.outbound.as_ref()) {
                    (true, Some(out)) => send_outbound(&mut writer, out, label).await?,
                    // Publisher gone: stop watching (reads carry on).
                    _ => changes = None,
                }
            }
            // No frame or change yet — re-check `running`.
            _ = tokio::time::sleep(READ_POLL) => {}
        }
    }
}

/// Read the next frame, handing the reader back so the caller can re-arm the read.
async fn read_next(
    mut reader: BufReader<OwnedReadHalf>,
) -> (BufReader<OwnedReadHalf>, Result<Option<WyomingEvent>>) {
    let result = protocol::read_event(&mut reader).await.map_err(Into::into);
    (reader, result)
}

/// Wait for the outbound report to change (`true`), or `false` once its publisher is
/// gone. Never resolves for a channel without an outbound report. Cancellation-safe.
async fn outbound_changed(changes: &mut Option<watch::Receiver<()>>) -> bool {
    match changes {
        Some(rx) => rx.changed().await.is_ok(),
        None => std::future::pending().await,
    }
}

/// Build and write the outbound report's current frame (bounded by [`WRITE_TIMEOUT`]).
async fn send_outbound(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    out: &Outbound,
    label: &str,
) -> Result<()> {
    let frame = (out.frame)();
    tokio::time::timeout(WRITE_TIMEOUT, protocol::write_event(writer, &frame))
        .await
        .with_context(|| format!("sending {} ({label}) timed out", frame.event_type))?
        .with_context(|| format!("sending {} ({label})", frame.event_type))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wyoming::discovery::WyomingEndpoint;
    use crate::wyoming::protocol::{types, ReportedTimer};
    use std::net::{IpAddr, SocketAddr};
    use std::sync::atomic::AtomicU32;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    /// A cache pre-seeded with the loopback mock; `resolve(_, 0, None)` fails the browse
    /// immediately and falls back to it, so the test never depends on real mDNS.
    fn cache_for(addr: SocketAddr) -> EndpointCache {
        let cache = EndpointCache::new();
        cache.set(WyomingEndpoint {
            address: IpAddr::from([127, 0, 0, 1]),
            port: addr.port(),
            hostname: "localhost.".into(),
            key: "test-orch".into(),
            name: "Test Orchestrator".into(),
        });
        cache
    }

    fn timers_frame(id: u32) -> WyomingEvent {
        WyomingEvent::timers_report(&[ReportedTimer {
            id,
            label: "pasta".into(),
            duration_secs: 300,
            remaining_secs: 120,
            ringing: false,
        }])
    }

    /// Drive `channel` against the mock until the first notify frame arrives, returning it.
    fn spawn_device(
        addr: SocketAddr,
        channel: Channel,
    ) -> tokio::task::JoinHandle<Option<WyomingEvent>> {
        tokio::spawn(async move {
            let cache = cache_for(addr);
            let running = Arc::new(AtomicBool::new(true));
            let mut got = None;
            run(
                &cache,
                Duration::ZERO,
                None,
                channel,
                running,
                |ev: &WyomingEvent| (ev.event_type == types::ANAMANTI_NOTIFY).then(|| ev.clone()),
                |ev| {
                    got = Some(ev);
                    false // consumer done → the channel ends
                },
            )
            .await;
            got
        })
    }

    async fn next(reader: &mut BufReader<OwnedReadHalf>) -> WyomingEvent {
        tokio::time::timeout(Duration::from_secs(5), protocol::read_event(reader))
            .await
            .expect("frame within 5s")
            .unwrap()
            .expect("socket open")
    }

    #[tokio::test]
    async fn sends_outbound_after_hello_and_on_change_without_losing_a_split_frame() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = watch::channel(());
        let current = Arc::new(AtomicU32::new(1));
        let frame_id = current.clone();
        let device = spawn_device(
            addr,
            Channel {
                label: "test",
                hello: WyomingEvent::hello("dev-1", "", "Kitchen"),
                outbound: Some(Outbound {
                    changes: rx,
                    frame: Box::new(move || timers_frame(frame_id.load(Ordering::SeqCst))),
                }),
            },
        );

        let (stream, _) = listener.accept().await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);
        // Hello first, then the current report immediately.
        assert_eq!(next(&mut reader).await.event_type, types::ANAMANTI_HELLO);
        assert_eq!(next(&mut reader).await, timers_frame(1));

        // A change resends the freshly built report.
        current.store(2, Ordering::SeqCst);
        tx.send_replace(());
        assert_eq!(next(&mut reader).await, timers_frame(2));

        // A push frame split mid-way, with an outbound change landing in between: the
        // report still goes out, and the half-read frame is not lost.
        let push = WyomingEvent::notify("n-1", "info", "Hello", "World");
        let mut bytes = Vec::new();
        protocol::write_event(&mut bytes, &push).await.unwrap();
        let (head, tail) = bytes.split_at(bytes.len() / 2);
        w.write_all(head).await.unwrap();
        w.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        current.store(3, Ordering::SeqCst);
        tx.send_replace(());
        assert_eq!(next(&mut reader).await, timers_frame(3));
        w.write_all(tail).await.unwrap();
        w.flush().await.unwrap();

        let got = tokio::time::timeout(Duration::from_secs(5), device)
            .await
            .expect("device ends")
            .unwrap();
        assert_eq!(got, Some(push));
    }

    #[tokio::test]
    async fn channel_without_outbound_only_sends_hello() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let device = spawn_device(
            addr,
            Channel {
                label: "test",
                hello: WyomingEvent::hello_weather("dev-1", "", "Kitchen"),
                outbound: None,
            },
        );

        let (stream, _) = listener.accept().await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);
        assert_eq!(next(&mut reader).await.event_type, types::ANAMANTI_HELLO);
        let push = WyomingEvent::notify("n-2", "info", "Hi", "");
        protocol::write_event(&mut w, &push).await.unwrap();

        let got = tokio::time::timeout(Duration::from_secs(5), device)
            .await
            .expect("device ends")
            .unwrap();
        assert_eq!(got, Some(push));
        // The device closed without sending anything after its hello.
        let after = tokio::time::timeout(Duration::from_secs(5), protocol::read_event(&mut reader))
            .await
            .expect("closed within 5s")
            .unwrap();
        assert_eq!(after, None);
    }
}
