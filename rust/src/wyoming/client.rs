//! The `tokio` Wyoming connection + turn driver (Plan.MD §3, Phase 3;
//! architecture.md §4).
//!
//! [`WyomingConnection`] is the "connection-state struct over
//! `tokio::net::TcpStream`" the plan calls for: it owns the split read/write
//! halves and knows the PCM format so callers send audio without re-passing the
//! rate/width/channels each time. It is generic over the underlying IO so the
//! turn driver can be tested against an in-memory duplex pipe with a mock server
//! — no real network required.
//!
//! [`run_turn`] is the async realization of the pure [`Session`] state machine:
//! it feeds the machine [`ControlInput`]s from real events (socket opened,
//! `transcript` received, idle timeout) and executes the [`Action`]s it returns
//! (send `audio-start`, stream chunks, send `audio-stop`, close). The PCM to
//! stream arrives on an mpsc channel fed by the engine's capture loop, so the
//! `!Send` `cpal` stream stays on its own thread while the network runs on the
//! tokio runtime.

use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufRead, AsyncWrite, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::discovery::WyomingEndpoint;
use super::protocol::{self, WyomingEvent};
use super::state::{Action, ControlInput, Session, SessionState};

/// PCM format streamed to the Wyoming STT server: 16 kHz mono 16-bit, matching
/// the capture/resample pipeline (`audio::TARGET_SAMPLE_RATE`).
#[derive(Debug, Clone, Copy)]
pub struct AudioFormat {
    pub rate: u32,
    pub width_bytes: u16,
    pub channels: u16,
}

impl Default for AudioFormat {
    fn default() -> Self {
        Self {
            rate: crate::audio::TARGET_SAMPLE_RATE,
            width_bytes: 2, // i16
            channels: crate::audio::TARGET_CHANNELS,
        }
    }
}

/// A live Wyoming connection: buffered reader + writer halves plus the PCM format
/// for outgoing audio frames. Generic over the IO so it works with a real
/// `TcpStream` in production and a duplex pipe in tests.
pub struct WyomingConnection<R, W> {
    reader: R,
    writer: W,
    format: AudioFormat,
    timestamp_ms: u64,
}

impl WyomingConnection<BufReader<tokio::net::tcp::OwnedReadHalf>, tokio::net::tcp::OwnedWriteHalf> {
    /// Dial the resolved Wyoming endpoint and wrap the socket. `TCP_NODELAY` is
    /// set so the tiny newline-delimited control frames flush immediately rather
    /// than waiting on Nagle's algorithm.
    pub async fn connect(endpoint: &WyomingEndpoint, format: AudioFormat) -> Result<Self> {
        let stream = TcpStream::connect(endpoint.socket_addr())
            .await
            .with_context(|| format!("dialing Wyoming host {endpoint}"))?;
        stream.set_nodelay(true).ok();
        let (read_half, write_half) = stream.into_split();
        Ok(Self::from_halves(
            BufReader::new(read_half),
            write_half,
            format,
        ))
    }
}

impl<R, W> WyomingConnection<R, W>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Wrap already-split IO halves (used by tests with `tokio::io::duplex`).
    pub fn from_halves(reader: R, writer: W, format: AudioFormat) -> Self {
        Self {
            reader,
            writer,
            format,
            timestamp_ms: 0,
        }
    }

    /// Send the `audio-start` header that opens the outbound audio stream.
    pub async fn send_audio_start(&mut self) -> Result<()> {
        let ev = WyomingEvent::audio_start(
            self.format.rate,
            self.format.width_bytes,
            self.format.channels,
            self.timestamp_ms,
        );
        protocol::write_event(&mut self.writer, &ev)
            .await
            .context("sending audio-start")
    }

    /// Send one `audio-chunk` carrying `samples` as little-endian `i16` PCM. The
    /// stream timestamp advances by the chunk's duration so the server can align
    /// frames.
    pub async fn send_chunk(&mut self, samples: &[i16]) -> Result<()> {
        let mut pcm = Vec::with_capacity(samples.len() * 2);
        for &s in samples {
            pcm.extend_from_slice(&s.to_le_bytes());
        }
        let ev = WyomingEvent::audio_chunk(
            self.format.rate,
            self.format.width_bytes,
            self.format.channels,
            self.timestamp_ms,
            pcm,
        );
        // Advance the running timestamp by this chunk's wall-clock duration (ms).
        let frames = samples.len() as u64 / self.format.channels.max(1) as u64;
        self.timestamp_ms += frames * 1000 / self.format.rate.max(1) as u64;
        protocol::write_event(&mut self.writer, &ev)
            .await
            .context("sending audio-chunk")
    }

    /// Send the `audio-stop` footer that ends the outbound audio stream.
    pub async fn send_audio_stop(&mut self) -> Result<()> {
        let ev = WyomingEvent::audio_stop(self.timestamp_ms);
        protocol::write_event(&mut self.writer, &ev)
            .await
            .context("sending audio-stop")
    }

    /// Read the next event from the server, or `None` on a clean close.
    pub async fn read_event(&mut self) -> Result<Option<WyomingEvent>> {
        protocol::read_event(&mut self.reader)
            .await
            .context("reading Wyoming event")
    }
}

/// Updates surfaced from a turn as it progresses. The engine translates these
/// into FRB events for the UI (transcript render, connection status).
#[derive(Debug, Clone, PartialEq)]
pub enum TurnUpdate {
    /// The socket opened and `audio-start` was sent; now streaming PCM.
    Streaming,
    /// A (final) transcript arrived from the STT server.
    Transcript(String),
    /// The turn ended cleanly and the client is back to idle.
    Finished,
}

/// How long to wait with no server events before defensively abandoning a turn.
pub const DEFAULT_TURN_TIMEOUT: Duration = Duration::from_secs(15);

/// Drive one voice turn over an already-connected [`WyomingConnection`], applying
/// the pure [`Session`] state machine. Streams PCM pulled from `pcm_rx` up to the
/// server and reads `transcript` events down, reporting progress via `on_update`.
///
/// The caller has already opened the socket, so the machine starts by consuming a
/// synthetic `WakeWord`→`Connected` pair; from there real IO drives it. Returns
/// when the turn reaches [`SessionState::Idle`] again.
pub async fn run_turn<R, W>(
    conn: &mut WyomingConnection<R, W>,
    mut pcm_rx: mpsc::Receiver<Vec<i16>>,
    mut on_update: impl FnMut(TurnUpdate),
    timeout: Duration,
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut session = Session::new();
    // The engine only calls this after a wake word fired and the socket opened.
    session.on_input(ControlInput::WakeWord); // → OpenConnection (already done)
    run_actions(
        conn,
        session.on_input(ControlInput::Connected),
        &mut on_update,
    )
    .await?;
    on_update(TurnUpdate::Streaming);

    let mut deadline = Instant::now() + timeout;

    while session.state().is_active() {
        tokio::select! {
            biased;

            // Idle watchdog: no server activity for `timeout` → abandon the turn.
            _ = tokio::time::sleep_until(deadline) => {
                let actions = session.on_input(ControlInput::Timeout);
                run_actions(conn, actions, &mut on_update).await?;
                deadline = Instant::now() + timeout;
            }

            // Downstream: transcript / VAD events from the STT server.
            event = conn.read_event() => {
                deadline = Instant::now() + timeout;
                match event? {
                    Some(ev) => handle_server_event(&mut session, conn, ev, &mut on_update).await?,
                    None => {
                        // Peer closed the socket.
                        let actions = session.on_input(ControlInput::Closed);
                        run_actions(conn, actions, &mut on_update).await?;
                    }
                }
            }

            // Upstream: a captured PCM chunk to forward — only while STREAMING.
            chunk = pcm_rx.recv() => {
                match chunk {
                    Some(samples) if session.is_streaming() => {
                        conn.send_chunk(&samples).await?;
                    }
                    Some(_) => { /* draining/closing: drop late chunks */ }
                    None => {
                        // Capture side hung up: stop the turn gracefully.
                        let actions = session.on_input(ControlInput::Stop);
                        run_actions(conn, actions, &mut on_update).await?;
                    }
                }
            }
        }

        // Once CLOSING has drained its `audio-stop`, finish the teardown.
        if session.state() == SessionState::Closing {
            let actions = session.on_input(ControlInput::Closed);
            run_actions(conn, actions, &mut on_update).await?;
        }
    }

    on_update(TurnUpdate::Finished);
    Ok(())
}

/// Handle one inbound server event, mapping `transcript` (server-side VAD's
/// end-of-speech signal) into an `EndOfSpeech` transition.
async fn handle_server_event<R, W>(
    session: &mut Session,
    conn: &mut WyomingConnection<R, W>,
    event: WyomingEvent,
    on_update: &mut impl FnMut(TurnUpdate),
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    if let Some(text) = event.transcript_text() {
        on_update(TurnUpdate::Transcript(text.to_string()));
        let actions = session.on_input(ControlInput::EndOfSpeech);
        run_actions(conn, actions, on_update).await?;
    }
    // Other events (voice-started, info, etc.) don't change the Phase-3 turn.
    Ok(())
}

/// Execute the side effects a transition asked for.
async fn run_actions<R, W>(
    conn: &mut WyomingConnection<R, W>,
    actions: Vec<Action>,
    _on_update: &mut impl FnMut(TurnUpdate),
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    for action in actions {
        match action {
            // The connection is opened by the caller before `run_turn`; the
            // machine still emits these for symmetry, so they're no-ops here.
            Action::OpenConnection => {}
            Action::Close => {}
            Action::SendAudioStart => conn.send_audio_start().await?,
            Action::SendAudioStop => conn.send_audio_stop().await?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wyoming::protocol::{read_event, types};
    use serde_json::json;
    use tokio::io::BufReader as TokioBufReader;

    #[tokio::test]
    async fn connection_sends_well_formed_audio_frames() {
        // Client writes into one end of a duplex; the "server" reads the frames.
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = tokio::io::split(client_io);
        let mut conn =
            WyomingConnection::from_halves(TokioBufReader::new(cr), cw, AudioFormat::default());

        conn.send_audio_start().await.unwrap();
        conn.send_chunk(&[1, -1, 2, -2]).await.unwrap();
        conn.send_audio_stop().await.unwrap();
        drop(conn); // close writer so reads hit EOF

        let (sr, _sw) = tokio::io::split(server_io);
        let mut server = TokioBufReader::new(sr);

        let start = read_event(&mut server).await.unwrap().unwrap();
        assert_eq!(start.event_type, types::AUDIO_START);

        let chunk = read_event(&mut server).await.unwrap().unwrap();
        assert_eq!(chunk.event_type, types::AUDIO_CHUNK);
        // 4 samples × 2 bytes little-endian.
        assert_eq!(
            chunk.payload.as_deref(),
            Some(
                [1i16, -1, 2, -2]
                    .iter()
                    .flat_map(|s| s.to_le_bytes())
                    .collect::<Vec<_>>()
                    .as_slice()
            )
        );

        let stop = read_event(&mut server).await.unwrap().unwrap();
        assert_eq!(stop.event_type, types::AUDIO_STOP);
    }

    #[tokio::test]
    async fn run_turn_streams_then_finishes_on_transcript() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = tokio::io::split(client_io);
        let mut conn =
            WyomingConnection::from_halves(TokioBufReader::new(cr), cw, AudioFormat::default());

        // Mock STT server: read audio-start, read a chunk, then reply with a
        // transcript (server-side VAD end-of-speech), then read audio-stop.
        let server = tokio::spawn(async move {
            let (sr, sw) = tokio::io::split(server_io);
            let mut reader = TokioBufReader::new(sr);
            let mut writer = sw;

            let start = read_event(&mut reader).await.unwrap().unwrap();
            assert_eq!(start.event_type, types::AUDIO_START);
            let chunk = read_event(&mut reader).await.unwrap().unwrap();
            assert_eq!(chunk.event_type, types::AUDIO_CHUNK);

            let transcript =
                WyomingEvent::with_data(types::TRANSCRIPT, json!({ "text": "hello world" }));
            protocol::write_event(&mut writer, &transcript)
                .await
                .unwrap();

            let stop = read_event(&mut reader).await.unwrap().unwrap();
            assert_eq!(stop.event_type, types::AUDIO_STOP);
        });

        let (pcm_tx, pcm_rx) = mpsc::channel::<Vec<i16>>(8);
        pcm_tx.send(vec![7i16; 160]).await.unwrap();
        // Keep `pcm_tx` alive for the whole turn (as the real capture loop does):
        // the turn must end on the server's transcript, not on the channel
        // closing. An empty-but-open channel simply pends in the select.

        let mut updates = Vec::new();
        run_turn(
            &mut conn,
            pcm_rx,
            |u| updates.push(u),
            Duration::from_secs(5),
        )
        .await
        .unwrap();

        drop(pcm_tx);
        server.await.unwrap();

        assert!(updates.contains(&TurnUpdate::Streaming));
        assert!(updates.contains(&TurnUpdate::Transcript("hello world".to_string())));
        assert_eq!(updates.last(), Some(&TurnUpdate::Finished));
    }

    #[tokio::test]
    async fn connect_dials_a_real_tcp_endpoint() {
        use crate::wyoming::discovery::WyomingEndpoint;
        use std::net::IpAddr;
        use tokio::net::TcpListener;

        // Bind a loopback listener and point a WyomingEndpoint at it to exercise
        // the production `TcpStream` dial path (not just the duplex pipe).
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (sr, _sw) = stream.into_split();
            let mut reader = TokioBufReader::new(sr);
            let start = read_event(&mut reader).await.unwrap().unwrap();
            assert_eq!(start.event_type, types::AUDIO_START);
        });

        let endpoint = WyomingEndpoint {
            address: IpAddr::from([127, 0, 0, 1]),
            port: addr.port(),
            hostname: "localhost.".to_string(),
        };
        let mut conn = WyomingConnection::connect(&endpoint, AudioFormat::default())
            .await
            .expect("dial loopback");
        conn.send_audio_start().await.unwrap();

        server.await.unwrap();
    }

    #[tokio::test]
    async fn run_turn_times_out_when_server_is_silent() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = tokio::io::split(client_io);
        let mut conn =
            WyomingConnection::from_halves(TokioBufReader::new(cr), cw, AudioFormat::default());

        // Server reads audio-start but never replies with a transcript. It must
        // stay alive so the client's reads block (rather than hitting EOF), so
        // the *timeout* path is what ends the turn.
        let server = tokio::spawn(async move {
            let (sr, _sw) = tokio::io::split(server_io);
            let mut reader = TokioBufReader::new(sr);
            // Drain whatever the client sends until it closes.
            while read_event(&mut reader).await.transpose().is_some() {}
        });

        let (_pcm_tx, pcm_rx) = mpsc::channel::<Vec<i16>>(8);

        let mut updates = Vec::new();
        run_turn(
            &mut conn,
            pcm_rx,
            |u| updates.push(u),
            Duration::from_millis(50),
        )
        .await
        .unwrap();

        assert_eq!(updates.last(), Some(&TurnUpdate::Finished));
        drop(conn);
        let _ = server.await;
    }
}
