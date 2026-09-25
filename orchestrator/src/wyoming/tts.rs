//! Downstream **Wyoming TTS** (Piper) client (Plan.MD Phase 4, bullet 4).
//!
//! Once the LLM reply text is known, the orchestrator is a Wyoming *client* to the
//! Piper service: it sends one `synthesize` request and reads back the synthesized
//! audio stream (`audio-start` → `audio-chunk`… → `audio-stop`). The orchestrator
//! relays those frames straight to the Echo Show over the device socket
//! (architecture.md §4 SPEAKING).

use anyhow::{anyhow, Result};
use serde_json::Value;

use super::protocol::{types, WyomingEvent};
use super::Connection;
use tokio::io::{AsyncBufRead, AsyncWrite};

/// One voice advertised by a downstream Piper `describe`/`info` exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceEntry {
    /// Piper voice id, e.g. `en_US-amy-medium`.
    pub name: String,
    /// Primary BCP-47-ish locale, e.g. `en_US`, if advertised.
    pub language: Option<String>,
    /// Human-friendly label for the dropdown (Piper's description, else the name).
    pub label: String,
}

/// Ask a downstream Piper connection for its advertised voice catalog: send a
/// `describe`, read frames until the `info` reply, and parse its `tts[].voices[]`.
/// The returned list is Piper's *advertised* catalog (which may include voices not
/// yet downloaded); the caller intersects it with what is on disk.
pub async fn describe_voices<R, W>(mut conn: Connection<R, W>) -> Result<Vec<VoiceEntry>>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    conn.send(&WyomingEvent::new(types::DESCRIBE)).await?;
    loop {
        match conn.read().await? {
            Some(ev) if ev.event_type == types::INFO => return Ok(parse_voice_catalog(&ev.data)),
            Some(_) => continue, // skip any stray frame before the info reply
            None => {
                return Err(anyhow!(
                    "Piper closed the connection without an info response"
                ))
            }
        }
    }
}

/// Parse the `tts[].voices[]` array from a Wyoming `info` `data` block into
/// [`VoiceEntry`]s. Tolerant of missing fields: a voice with no `name` is skipped.
pub fn parse_voice_catalog(data: &Value) -> Vec<VoiceEntry> {
    let mut out = Vec::new();
    let Some(programs) = data.get("tts").and_then(Value::as_array) else {
        return out;
    };
    for program in programs {
        let Some(voices) = program.get("voices").and_then(Value::as_array) else {
            continue;
        };
        for v in voices {
            let Some(name) = v
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let language = v
                .get("languages")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_str)
                .or_else(|| v.get("language").and_then(Value::as_str))
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let label = v
                .get("description")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(name)
                .to_string();
            out.push(VoiceEntry {
                name: name.to_string(),
                language,
                label,
            });
        }
    }
    out
}

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

    #[test]
    fn parse_voice_catalog_flattens_tts_programs() {
        let info = serde_json::json!({
            "tts": [{
                "name": "piper",
                "voices": [
                    { "name": "en_US-amy-medium", "languages": ["en_US"], "description": "amy (medium)" },
                    { "name": "de_DE-thorsten-low", "languages": ["de_DE"], "description": "thorsten (low)" },
                    { "name": "no_lang" }, // no languages/description → language None, label falls back to name
                    { "description": "nameless" } // no name → skipped
                ]
            }],
            "asr": []
        });
        let voices = parse_voice_catalog(&info);
        assert_eq!(voices.len(), 3);
        assert_eq!(voices[0].name, "en_US-amy-medium");
        assert_eq!(voices[0].language.as_deref(), Some("en_US"));
        assert_eq!(voices[0].label, "amy (medium)");
        assert_eq!(voices[2].name, "no_lang");
        assert_eq!(voices[2].language, None);
        assert_eq!(voices[2].label, "no_lang"); // label falls back to name
    }

    #[test]
    fn parse_voice_catalog_tolerates_missing_tts() {
        assert!(parse_voice_catalog(&serde_json::json!({})).is_empty());
        assert!(parse_voice_catalog(&serde_json::json!({ "tts": [] })).is_empty());
    }

    #[tokio::test]
    async fn describe_voices_sends_describe_and_parses_info() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = split(client_io);
        let conn = Connection::from_halves(BufReader::new(cr), cw);

        let server = tokio::spawn(async move {
            let (sr, sw) = split(server_io);
            let mut reader = BufReader::new(sr);
            let mut writer = sw;
            let req = read_event(&mut reader).await.unwrap().unwrap();
            assert_eq!(req.event_type, types::DESCRIBE);
            let info = WyomingEvent::with_data(
                types::INFO,
                serde_json::json!({
                    "tts": [{ "voices": [
                        { "name": "en_US-amy-medium", "languages": ["en_US"] },
                    ]}]
                }),
            );
            write_event(&mut writer, &info).await.unwrap();
        });

        let voices = describe_voices(conn).await.unwrap();
        assert_eq!(voices.len(), 1);
        assert_eq!(voices[0].name, "en_US-amy-medium");
        server.await.unwrap();
    }
}
