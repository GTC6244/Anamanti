//! Downstream **Wyoming TTS** (Piper) client (Plan.MD Phase 4, bullet 4).
//!
//! Once the LLM reply text is known, the orchestrator is a Wyoming *client* to the
//! Piper service: it sends one `synthesize` request and reads back the synthesized
//! audio stream (`audio-start` → `audio-chunk`… → `audio-stop`). The orchestrator
//! relays those frames straight to the Echo Show over the device socket
//! (architecture.md §4 SPEAKING).

use anyhow::Result;

use super::protocol::{types, WyomingEvent};
use super::Connection;
use tokio::io::{AsyncBufRead, AsyncWrite};

/// A single synthesis request over a downstream Piper connection.
pub struct TtsSession<R, W> {
    conn: Connection<R, W>,
    done: bool,
}

impl<R, W> TtsSession<R, W>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Send the `synthesize` request for `text` (optionally pinning `voice`).
    pub async fn begin(
        mut conn: Connection<R, W>,
        text: &str,
        voice: Option<&str>,
    ) -> Result<Self> {
        conn.send(&WyomingEvent::synthesize(text, voice)).await?;
        Ok(Self { conn, done: false })
    }

    /// Read the next synthesized-audio event. Yields `audio-start` and each
    /// `audio-chunk`, yields the terminating `audio-stop` once, then `None`. The
    /// caller relays every yielded event to the device unchanged.
    pub async fn next_audio(&mut self) -> Result<Option<WyomingEvent>> {
        if self.done {
            return Ok(None);
        }
        match self.conn.read().await? {
            Some(ev) => {
                if ev.event_type == types::AUDIO_STOP {
                    self.done = true;
                }
                Ok(Some(ev))
            }
            None => {
                self.done = true;
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wyoming::protocol::{read_event, write_event, AudioFormat};
    use tokio::io::{split, BufReader};

    #[tokio::test]
    async fn sends_synthesize_and_streams_audio_until_stop() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_io);
        let conn = Connection::from_halves(BufReader::new(cr), cw);

        // Mock Piper: read the synthesize request, emit start/chunk/stop.
        let server = tokio::spawn(async move {
            let (sr, sw) = split(server_io);
            let mut reader = BufReader::new(sr);
            let mut writer = sw;
            let req = read_event(&mut reader).await.unwrap().unwrap();
            assert_eq!(req.event_type, types::SYNTHESIZE);
            assert_eq!(req.data["text"], serde_json::json!("hello"));

            for ev in [
                WyomingEvent::audio_start(AudioFormat::PCM_16K_MONO, 0),
                WyomingEvent::audio_chunk(AudioFormat::PCM_16K_MONO, 0, vec![9, 0, 8, 0]),
                WyomingEvent::audio_stop(20),
            ] {
                write_event(&mut writer, &ev).await.unwrap();
            }
        });

        let mut session = TtsSession::begin(conn, "hello", None).await.unwrap();
        let mut kinds = Vec::new();
        while let Some(ev) = session.next_audio().await.unwrap() {
            kinds.push(ev.event_type);
        }
        assert_eq!(
            kinds,
            vec![
                types::AUDIO_START.to_string(),
                types::AUDIO_CHUNK.to_string(),
                types::AUDIO_STOP.to_string()
            ]
        );
        server.await.unwrap();
    }
}
