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

    /// Send an `ambient-interrupt` (barge-in) frame so the orchestrator aborts the
    /// in-flight reply's LLM generation + TTS at once, rather than only noticing
    /// when the socket is torn down.
    pub async fn send_interrupt(&mut self) -> Result<()> {
        protocol::write_event(&mut self.writer, &WyomingEvent::interrupt())
            .await
            .context("sending interrupt")
    }

    /// Read the next event from the server, or `None` on a clean close.
    pub async fn read_event(&mut self) -> Result<Option<WyomingEvent>> {
        protocol::read_event(&mut self.reader)
            .await
            .context("reading Wyoming event")
    }
}

/// Updates surfaced from a turn as it progresses. The engine translates these
/// into FRB events for the UI (transcript render, reply text, connection status).
#[derive(Debug, Clone, PartialEq)]
pub enum TurnUpdate {
    /// The socket opened and `audio-start` was sent; now streaming PCM.
    Streaming,
    /// A (final) transcript arrived from the STT server.
    Transcript(String),
    /// One streamed LLM reply-token fragment (Phase 5). The UI appends these to
    /// render the reply token-by-token.
    ReplyToken(String),
    /// The reply's TTS audio has started arriving and is now being played back
    /// through the speakers (Phase 5 SPEAKING).
    Speaking,
    /// A device-action **timer** command relayed from the orchestrator (Phase 2):
    /// start or cancel a countdown. Handled by the engine's on-device timer manager,
    /// which owns the countdown + alarm and outlives the turn's socket.
    Timer(protocol::TimerCommand),
    /// The turn ended cleanly and the client is back to idle.
    Finished,
}

/// How long to wait with no server events before defensively abandoning a turn.
pub const DEFAULT_TURN_TIMEOUT: Duration = Duration::from_secs(15);

/// Drive one voice turn over an already-connected [`WyomingConnection`], applying
/// the pure [`Session`] state machine. Streams PCM pulled from `pcm_rx` up to the
/// server, reads `transcript` / `reply-token` / TTS-audio events down, reporting
/// progress via `on_update` and handing decoded playback PCM to `on_audio`.
///
/// `on_audio(pcm, rate)` receives each TTS `audio-chunk` as little-endian `i16`
/// samples plus the stream's sample rate; the engine forwards it to the speaker
/// playback sink. `interrupt` is the barge-in channel: a message on it (a new
/// wake word fired mid-turn) injects a `Stop`, cutting the turn short so a fresh
/// one can begin.
///
/// The caller has already opened the socket, so the machine starts by consuming a
/// synthetic `WakeWord`→`Connected` pair; from there real IO drives it. Returns
/// when the turn reaches [`SessionState::Idle`] again.
#[allow(clippy::too_many_arguments)]
pub async fn run_turn<R, W>(
    conn: &mut WyomingConnection<R, W>,
    mut pcm_rx: mpsc::Receiver<Vec<i16>>,
    mut on_update: impl FnMut(TurnUpdate),
    mut on_audio: impl FnMut(&[i16], u32),
    mut interrupt: mpsc::Receiver<()>,
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

    // Sample rate of the inbound TTS stream, learned from its `audio-start`.
    let mut playback_rate: u32 = crate::audio::TARGET_SAMPLE_RATE;
    let mut deadline = Instant::now() + timeout;

    while session.state().is_active() {
        tokio::select! {
            biased;

            // Barge-in: a new wake word (or on-device VAD) fired mid-turn → tell the
            // orchestrator to abort the in-flight reply, then stop this turn so a
            // fresh one can start. The engine has already flushed local playback.
            // Sending here is cancellation-safe: this arm is mutually exclusive with
            // the `read_event` arm, so no partially-read frame is dropped. A failed
            // send (socket already gone) is fine — we tear the turn down regardless.
            _ = interrupt.recv() => {
                let _ = conn.send_interrupt().await;
                let actions = session.on_input(ControlInput::Stop);
                run_actions(conn, actions, &mut on_update).await?;
            }

            // Idle watchdog: no server activity for `timeout` → abandon the turn.
            _ = tokio::time::sleep_until(deadline) => {
                let actions = session.on_input(ControlInput::Timeout);
                run_actions(conn, actions, &mut on_update).await?;
                deadline = Instant::now() + timeout;
            }

            // Downstream: transcript / reply-token / TTS-audio events.
            event = conn.read_event() => {
                deadline = Instant::now() + timeout;
                match event? {
                    Some(ev) => {
                        handle_server_event(
                            &mut session, conn, ev,
                            &mut playback_rate, &mut on_update, &mut on_audio,
                        ).await?
                    }
                    None => {
                        // Peer closed the socket.
                        let actions = session.on_input(ControlInput::Closed);
                        run_actions(conn, actions, &mut on_update).await?;
                    }
                }
            }

            // Upstream: a captured PCM chunk to forward — only while STREAMING.
            //
            // The `if session.is_streaming()` precondition is load-bearing, not just
            // an optimization: `conn.read_event()` above is NOT cancellation-safe
            // (it interleaves `read_line` + `read_exact`), so if this branch fired
            // and won the `select!` while a read was suspended mid-frame, that
            // partially-read event would be dropped and the next read would resume in
            // the middle of a binary PCM payload ("stream did not contain valid
            // UTF-8"). During SPEAKING the capture thread keeps pushing mic PCM here
            // (dropped anyway), so leaving the branch enabled would corrupt every TTS
            // audio-chunk read. Disabling it while not streaming keeps the socket
            // reader the only consumer during playback.
            chunk = pcm_rx.recv(), if session.is_streaming() => {
                match chunk {
                    Some(samples) => {
                        conn.send_chunk(&samples).await?;
                    }
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

/// Handle one inbound server event.
///
/// - `transcript` (server-side VAD end-of-speech) → render it and transition
///   `STREAMING → SPEAKING` (sends the mic `audio-stop`).
/// - `reply-token` (Phase 5) → surface the streamed reply fragment.
/// - TTS `audio-start` → note the playback rate and report `Speaking`.
/// - TTS `audio-chunk` → decode the little-endian `i16` payload and hand it to
///   `on_audio` for playback.
/// - TTS `audio-stop` → `PlaybackFinished`, ending the turn.
async fn handle_server_event<R, W>(
    session: &mut Session,
    conn: &mut WyomingConnection<R, W>,
    event: WyomingEvent,
    playback_rate: &mut u32,
    on_update: &mut impl FnMut(TurnUpdate),
    on_audio: &mut impl FnMut(&[i16], u32),
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    use protocol::types;

    if let Some(text) = event.transcript_text() {
        on_update(TurnUpdate::Transcript(text.to_string()));
        let actions = session.on_input(ControlInput::EndOfSpeech);
        run_actions(conn, actions, on_update).await?;
        return Ok(());
    }
    if let Some(text) = event.reply_token_text() {
        on_update(TurnUpdate::ReplyToken(text.to_string()));
        return Ok(());
    }

    match event.event_type.as_str() {
        types::AUDIO_START => {
            if let Some((rate, _width, _channels)) = protocol::audio_format(&event.data) {
                *playback_rate = rate;
            }
            on_update(TurnUpdate::Speaking);
        }
        types::AUDIO_CHUNK => {
            if let Some(payload) = &event.payload {
                let pcm = decode_pcm_i16(payload);
                if !pcm.is_empty() {
                    on_audio(&pcm, *playback_rate);
                }
            }
        }
        types::AUDIO_STOP => {
            let actions = session.on_input(ControlInput::PlaybackFinished);
            run_actions(conn, actions, on_update).await?;
        }
        // A device action (timer start/cancel) relayed from the orchestrator. It does
        // not change the turn's state machine — the engine's timer manager owns the
        // countdown/alarm — so just surface it and keep going.
        types::TIMER => {
            if let Some(cmd) = event.timer_command() {
                on_update(TurnUpdate::Timer(cmd));
            }
        }
        // voice-started / info / etc. don't change the turn.
        _ => {}
    }
    Ok(())
}

/// Decode a little-endian `i16` PCM payload into samples. A trailing odd byte
/// (never expected from a well-formed server) is ignored.
fn decode_pcm_i16(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
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
    async fn run_turn_streams_transcript_reply_and_plays_tts() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = tokio::io::split(client_io);
        let mut conn =
            WyomingConnection::from_halves(TokioBufReader::new(cr), cw, AudioFormat::default());

        // Mock orchestrator: read audio-start + a mic chunk, return a transcript,
        // read the device's audio-stop, then stream a reply token and a TTS
        // audio-start → audio-chunk → audio-stop back for playback.
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

            // Device stops its mic stream once it sees the transcript.
            let stop = read_event(&mut reader).await.unwrap().unwrap();
            assert_eq!(stop.event_type, types::AUDIO_STOP);

            // Reply token + TTS audio back to the device.
            protocol::write_event(&mut writer, &WyomingEvent::reply_token("Hi "))
                .await
                .unwrap();
            protocol::write_event(&mut writer, &WyomingEvent::reply_token("there"))
                .await
                .unwrap();
            let tts_pcm: Vec<u8> = [100i16, -100, 200, -200]
                .iter()
                .flat_map(|s| s.to_le_bytes())
                .collect();
            for ev in [
                WyomingEvent::audio_start(22_050, 2, 1, 0),
                WyomingEvent::audio_chunk(22_050, 2, 1, 0, tts_pcm),
                WyomingEvent::audio_stop(0),
            ] {
                protocol::write_event(&mut writer, &ev).await.unwrap();
            }
        });

        let (pcm_tx, pcm_rx) = mpsc::channel::<Vec<i16>>(8);
        pcm_tx.send(vec![7i16; 160]).await.unwrap();
        let (_int_tx, int_rx) = mpsc::channel::<()>(1);

        let mut updates = Vec::new();
        let mut played: Vec<i16> = Vec::new();
        let mut played_rate = 0u32;
        run_turn(
            &mut conn,
            pcm_rx,
            |u| updates.push(u),
            |pcm, rate| {
                played.extend_from_slice(pcm);
                played_rate = rate;
            },
            int_rx,
            Duration::from_secs(5),
        )
        .await
        .unwrap();

        drop(pcm_tx);
        server.await.unwrap();

        assert!(updates.contains(&TurnUpdate::Streaming));
        assert!(updates.contains(&TurnUpdate::Transcript("hello world".to_string())));
        assert!(updates.contains(&TurnUpdate::ReplyToken("Hi ".to_string())));
        assert!(updates.contains(&TurnUpdate::ReplyToken("there".to_string())));
        assert!(updates.contains(&TurnUpdate::Speaking));
        assert_eq!(updates.last(), Some(&TurnUpdate::Finished));
        assert_eq!(played, vec![100, -100, 200, -200]);
        assert_eq!(played_rate, 22_050);
    }

    #[tokio::test]
    async fn barge_in_interrupt_ends_the_turn() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = tokio::io::split(client_io);
        let mut conn =
            WyomingConnection::from_halves(TokioBufReader::new(cr), cw, AudioFormat::default());

        // Server accepts audio-start then goes quiet, so only the interrupt can end
        // the turn. It must stay alive so the client's reads block (not EOF). It
        // records every frame type it sees so we can assert the barge-in frame was
        // relayed to the orchestrator.
        let server = tokio::spawn(async move {
            let (sr, _sw) = tokio::io::split(server_io);
            let mut reader = TokioBufReader::new(sr);
            let mut seen = Vec::new();
            while let Ok(Some(ev)) = read_event(&mut reader).await {
                seen.push(ev.event_type);
            }
            seen
        });

        let (_pcm_tx, pcm_rx) = mpsc::channel::<Vec<i16>>(8);
        let (int_tx, int_rx) = mpsc::channel::<()>(1);
        // Fire the barge-in immediately.
        int_tx.send(()).await.unwrap();

        let mut updates = Vec::new();
        run_turn(
            &mut conn,
            pcm_rx,
            |u| updates.push(u),
            |_, _| {},
            int_rx,
            Duration::from_secs(30),
        )
        .await
        .unwrap();

        // The interrupt cut the turn short well before the 30 s watchdog.
        assert_eq!(updates.last(), Some(&TurnUpdate::Finished));
        drop(conn);
        // The device relayed an `ambient-interrupt` frame so the orchestrator can
        // abort the reply gracefully (not just learn of it via the socket drop).
        let seen = server.await.unwrap();
        assert!(
            seen.iter().any(|t| t == types::INTERRUPT),
            "barge-in should send an interrupt frame; saw {seen:?}"
        );
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
        let (_int_tx, int_rx) = mpsc::channel::<()>(1);

        let mut updates = Vec::new();
        run_turn(
            &mut conn,
            pcm_rx,
            |u| updates.push(u),
            |_, _| {},
            int_rx,
            Duration::from_millis(50),
        )
        .await
        .unwrap();

        assert_eq!(updates.last(), Some(&TurnUpdate::Finished));
        drop(conn);
        let _ = server.await;
    }
}
