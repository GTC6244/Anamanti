//! Wyoming Protocol layer for the orchestrator: the wire codec plus a small
//! connection helper reused in both directions — as a server to the Echo Show and
//! as a client to the downstream Whisper (STT) and Piper (TTS) services.

pub mod protocol;
pub mod stt;
pub mod tts;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, BufReader};
use tokio::net::TcpStream;

pub use protocol::{AudioFormat, WyomingEvent};

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

    /// Dial a downstream Wyoming service (Whisper or Piper).
    pub async fn connect_tcp(addr: std::net::SocketAddr) -> Result<Self> {
        let stream = TcpStream::connect(addr)
            .await
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
