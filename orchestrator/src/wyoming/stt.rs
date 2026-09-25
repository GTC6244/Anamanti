//! Downstream **Wyoming STT** (Whisper/CoreML) client (Plan.MD Phase 4, bullet 1).
//!
//! The orchestrator is a Wyoming *client* to the Whisper service: it opens the
//! stream, forwards the PCM the Echo Show is streaming in, and waits for the
//! server's `transcript`. End-of-speech is **server-side VAD** — the Whisper
//! service decides when the utterance ends and emits the final `transcript`; only
//! then does the orchestrator send `audio-stop`. This mirrors the device's own
//! turn driver (`rust/src/wyoming/client.rs`) so both ends share one contract.

use anyhow::Result;

use super::protocol::{types, AudioFormat, WyomingEvent};
use super::Connection;
use tokio::io::{AsyncBufRead, AsyncWrite};

/// A single transcription request over a downstream STT connection. Tracks the
/// running stream timestamp so forwarded chunks stay monotonically aligned.
pub struct SttSession<R, W> {
    conn: Connection<R, W>,
    format: AudioFormat,
    timestamp_ms: u64,
}

impl<R, W> SttSession<R, W>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Open a transcription stream: announce the request with `transcribe`, then
    /// the `audio-start` header describing the PCM format that follows.
    pub async fn begin(mut conn: Connection<R, W>, format: AudioFormat) -> Result<Self> {
        // `transcribe` lets a multi-model server pin language/model; an empty data
        // object accepts the server default.
        conn.send(&WyomingEvent::with_data(
            types::TRANSCRIBE,
            serde_json::json!({}),
        ))
        .await?;
        conn.send(&WyomingEvent::audio_start(format, 0)).await?;
        Ok(Self {
            conn,
            format,
            timestamp_ms: 0,
        })
    }

    /// Forward one chunk of raw little-endian `i16` PCM bytes (as received from the
    /// device) to the STT server, advancing the running timestamp by its duration.
    pub async fn forward_pcm(&mut self, pcm: Vec<u8>) -> Result<()> {
        let bytes_per_frame = (self.format.width_bytes.max(1) * self.format.channels.max(1)) as u64;
        let frames = pcm.len() as u64 / bytes_per_frame.max(1);
        let ev = WyomingEvent::audio_chunk(self.format, self.timestamp_ms, pcm);
        self.timestamp_ms += frames * 1000 / self.format.rate.max(1) as u64;
        self.conn.send(&ev).await
    }

    /// Read the next event from the STT server (typically the final `transcript`).
    pub async fn read_event(&mut self) -> Result<Option<WyomingEvent>> {
        self.conn.read().await
    }

    /// Close the outbound audio stream after the server signalled end-of-speech.
    pub async fn finish(&mut self) -> Result<()> {
        self.conn
            .send(&WyomingEvent::audio_stop(self.timestamp_ms))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wyoming::protocol::read_event;
    use tokio::io::{split, BufReader};

    #[tokio::test]
    async fn begins_with_transcribe_then_audio_start_and_forwards_pcm() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_io);
        let conn = Connection::from_halves(BufReader::new(cr), cw);

        let mut session = SttSession::begin(conn, AudioFormat::PCM_16K_MONO)
            .await
            .unwrap();
        session.forward_pcm(vec![1, 0, 2, 0]).await.unwrap();
        session.finish().await.unwrap();
        drop(session);

        let (sr, _sw) = split(server_io);
        let mut server = BufReader::new(sr);
        assert_eq!(
            read_event(&mut server).await.unwrap().unwrap().event_type,
            types::TRANSCRIBE
        );
        assert_eq!(
            read_event(&mut server).await.unwrap().unwrap().event_type,
            types::AUDIO_START
        );
        let chunk = read_event(&mut server).await.unwrap().unwrap();
        assert_eq!(chunk.event_type, types::AUDIO_CHUNK);
        assert_eq!(chunk.payload.as_deref(), Some([1u8, 0, 2, 0].as_slice()));
        assert_eq!(
            read_event(&mut server).await.unwrap().unwrap().event_type,
            types::AUDIO_STOP
        );
    }
}
