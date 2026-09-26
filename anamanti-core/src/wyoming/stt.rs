//! Downstream **Wyoming STT** (Whisper/CoreML) client (Plan.MD Phase 4, bullet 1).
//!
//! One of two STT engines behind the [`Transcriber`](crate::stt::Transcriber) seam
//! (`plans/python-to-rust-whisper.md`); the other is the in-process whisper.cpp
//! engine. This is the client to an external `wyoming-faster-whisper` server: it
//! opens the stream, forwards the PCM the Echo Show is streaming in, and waits for
//! the server's `transcript`.
//!
//! **End-of-speech is decided by the Anamanti Core, not the server.**
//! `wyoming-faster-whisper` does no streaming VAD — it transcribes only after it
//! receives `audio-stop` — so the orchestrator runs an energy VAD over the incoming
//! PCM and sends `audio-stop` to finalize (`orchestrator::stream_to_transcript`).
//! The device streams continuously and runs no VAD of its own.

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

    /// Send `audio-stop` to finalize once the Core's energy VAD detected
    /// end-of-speech (this is what makes faster-whisper emit the `transcript`).
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
