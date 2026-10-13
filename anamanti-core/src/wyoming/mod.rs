//! Wyoming Protocol layer for the orchestrator: the wire codec plus a small
//! connection helper reused in both directions — as a server to the Echo Show and
//! as a client to the downstream Whisper (STT) and Piper (TTS) services.

pub mod protocol;
pub mod stt;
pub mod tts;

use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, BufReader};
use tokio::net::TcpStream;

pub use protocol::{AudioFormat, WyomingEvent};

/// Budget for dialing a downstream Wyoming service (Whisper STT / Piper TTS). Both run
/// on the LAN (usually on this Mac), where a healthy connect is a few milliseconds; a
/// dead host would otherwise sit in the OS SYN-retry loop for over a minute while the
/// user waits mid-turn.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// An operation on the turn's critical path exceeded its budget (a downstream connect,
/// a TTS audio read, the ElevenLabs handshake, the LLM token stream). Carried inside an
/// [`anyhow::Error`] so the orchestrator can tell a stall from a hard failure (via
/// `err.chain().any(|c| c.is::<TimedOut>())`) and end the turn cleanly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedOut {
    /// What timed out, for the log line (e.g. `"TTS audio read"`).
    pub what: &'static str,
    /// The budget that was exceeded.
    pub after: Duration,
}

impl TimedOut {
    pub fn new(what: &'static str, after: Duration) -> Self {
        Self { what, after }
    }

    /// Whether `err` (or anything in its context chain) is a [`TimedOut`].
    pub fn is_in(err: &anyhow::Error) -> bool {
        err.chain().any(|c| c.is::<TimedOut>())
    }
}

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} timed out after {:?}", self.what, self.after)
    }
}

impl std::error::Error for TimedOut {}

/// A live Wyoming connection: a buffered reader half + a writer half. Generic over
/// the IO so production code uses a split `TcpStream` and tests use an in-memory
/// `tokio::io::duplex` pipe — the orchestrator logic never knows the difference.
pub struct Connection<R, W> {
    reader: R,
    writer: W,
}

/// Type-erased reader half: a boxed `AsyncRead` wrapped for buffered line reads.
pub type DynRead = BufReader<Box<dyn AsyncRead + Unpin + Send>>;
/// Type-erased writer half.
pub type DynWrite = Box<dyn AsyncWrite + Unpin + Send>;
/// A connection with its IO type erased, so one non-generic turn driver serves
/// both a real `TcpStream` and an in-memory duplex pipe.
pub type DynConnection = Connection<DynRead, DynWrite>;

impl DynConnection {
    /// Wrap an established (dialed or accepted) `TcpStream`. `TCP_NODELAY` keeps
    /// the tiny newline-delimited control frames off Nagle's algorithm.
    pub fn from_tcp_stream(stream: TcpStream) -> Self {
        stream.set_nodelay(true).ok();
        let (read_half, write_half) = stream.into_split();
        Self::from_io(read_half, write_half)
    }

    /// Dial a downstream Wyoming service (Whisper or Piper), giving up after
    /// [`CONNECT_TIMEOUT`].
    pub async fn connect_tcp(addr: std::net::SocketAddr) -> Result<Self> {
        Self::connect_tcp_within(addr, CONNECT_TIMEOUT).await
    }

    /// [`connect_tcp`](Self::connect_tcp) with an explicit budget. A stall surfaces as a
    /// [`TimedOut`] error.
    pub async fn connect_tcp_within(addr: std::net::SocketAddr, budget: Duration) -> Result<Self> {
        let stream = tokio::time::timeout(budget, TcpStream::connect(addr))
            .await
            .map_err(|_| TimedOut::new("Wyoming service connect", budget))
            .with_context(|| format!("dialing Wyoming service {addr}"))?
            .with_context(|| format!("dialing Wyoming service {addr}"))?;
        Ok(Self::from_tcp_stream(stream))
    }

    /// Erase any concrete reader/writer halves into a [`DynConnection`] (used by
    /// tests wiring `tokio::io::duplex` and by the TCP paths above).
    pub fn from_io<Rd, Wr>(read_half: Rd, write_half: Wr) -> Self
    where
        Rd: AsyncRead + Unpin + Send + 'static,
        Wr: AsyncWrite + Unpin + Send + 'static,
    {
        Connection::from_halves(
            BufReader::new(Box::new(read_half) as Box<dyn AsyncRead + Unpin + Send>),
            Box::new(write_half) as Box<dyn AsyncWrite + Unpin + Send>,
        )
    }
}

impl<R, W> Connection<R, W>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Wrap already-split IO halves.
    pub fn from_halves(reader: R, writer: W) -> Self {
        Self { reader, writer }
    }

    /// Send one event and flush it.
    pub async fn send(&mut self, event: &WyomingEvent) -> Result<()> {
        protocol::write_event(&mut self.writer, event)
            .await
            .with_context(|| format!("sending Wyoming event `{}`", event.event_type))
    }

    /// Read the next event, or `None` on a clean peer close.
    pub async fn read(&mut self) -> Result<Option<WyomingEvent>> {
        protocol::read_event(&mut self.reader)
            .await
            .context("reading Wyoming event")
    }

    /// Borrow the reader and writer halves independently so a caller can read and
    /// write concurrently on one connection (e.g. watching for a barge-in frame
    /// while streaming reply audio out). The two halves are distinct fields, so
    /// this hands out two disjoint mutable borrows.
    pub fn split_mut(&mut self) -> (&mut R, &mut W) {
        (&mut self.reader, &mut self.writer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_to_a_black_hole_gives_up_within_the_budget() {
        // 192.0.2.1 is TEST-NET-1 (RFC 5737): never routed, so the SYN is dropped and
        // the dial would otherwise hang in the OS retry loop. Without a default route
        // the OS may refuse immediately instead — either way it must not hang.
        let addr: std::net::SocketAddr = "192.0.2.1:10200".parse().unwrap();
        let started = std::time::Instant::now();
        let res = DynConnection::connect_tcp_within(addr, Duration::from_millis(200)).await;
        let err = match res {
            Ok(_) => panic!("TEST-NET-1 must not accept a connection"),
            Err(e) => e,
        };
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "dial hung: {err:#}"
        );
        if started.elapsed() >= Duration::from_millis(200) {
            assert!(TimedOut::is_in(&err), "expected TimedOut, got {err:#}");
        }
    }
}
