//! Wyoming Protocol wire format: encode/decode + async framing over `tokio` IO
//! (Plan.MD §3, Phase 3; architecture.md §4 "Wire format").
//!
//! A Wyoming *event* is three concatenated parts on the socket:
//!
//! 1. A single **header line** of JSON terminated by `\n`. It always carries a
//!    `type` string and optionally `data_length` / `payload_length` integers.
//! 2. If `data_length` is present, exactly that many bytes of a **JSON `data`
//!    object** follow the header line (this is *not* inlined into the header —
//!    it is length-prefixed so binary-adjacent parsers never have to re-scan).
//! 3. If `payload_length` is present, exactly that many bytes of an opaque
//!    **binary payload** follow the data block (this is where raw PCM rides).
//!
//! This matches the reference `rhasspy/wyoming` implementation, so our client
//! interoperates with off-the-shelf Wyoming STT/TTS servers on the Mac (Phase 4).
//!
//! The codec here is deliberately split from the state machine (`state.rs`) and
//! the connection driver (`client.rs`): it is pure, synchronous byte-shuffling
//! plus a pair of thin async read/write helpers, so it can be unit-tested without
//! a socket or a runtime.

use std::io;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Wyoming event type names used by this client. Kept as constants so the state
/// machine and tests never trip over a typo in a string literal.
pub mod types {
    /// Marks the beginning of an audio stream (data: rate/width/channels).
    pub const AUDIO_START: &str = "audio-start";
    /// A chunk of raw PCM (data: rate/width/channels; payload: samples).
    pub const AUDIO_CHUNK: &str = "audio-chunk";
    /// Marks the end of an audio stream.
    pub const AUDIO_STOP: &str = "audio-stop";
    /// Client → STT: request transcription (data may pin a model/language).
    pub const TRANSCRIBE: &str = "transcribe";
    /// STT → client: a (final) transcript (data: `text`).
    pub const TRANSCRIPT: &str = "transcript";
    /// STT/VAD → client: server-side VAD detected the start of speech.
    pub const VOICE_STARTED: &str = "voice-started";
    /// STT/VAD → client: server-side VAD detected the end of speech.
    pub const VOICE_STOPPED: &str = "voice-stopped";
    /// Orchestrator → device (Phase 5): one streamed LLM reply-token fragment
    /// (data: `text`). This is a project-local extension on the device↔Mac hop —
    /// no off-the-shelf Wyoming server is on this hop — so the UI can render the
    /// reply token-by-token as it is generated (Plan.MD §3, Phase 5).
    pub const REPLY_TOKEN: &str = "reply-token";
}

/// A decoded Wyoming event: a `type` tag, an optional structured `data` object,
/// and an optional opaque binary `payload`.
#[derive(Debug, Clone, PartialEq)]
pub struct WyomingEvent {
    /// The event `type` (see [`types`]).
    pub event_type: String,
    /// The `data` object, or [`Value::Null`] when the event carries no data.
    pub data: Value,
    /// The binary payload (raw PCM for `audio-chunk`), if any.
    pub payload: Option<Vec<u8>>,
}

/// Just the header line of an event, before the length-prefixed blocks are read.
#[derive(Debug, Serialize, Deserialize)]
struct Header {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    data_length: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    payload_length: Option<usize>,
}

impl WyomingEvent {
    /// A data-less, payload-less event (e.g. a bare `audio-stop` when no
    /// timestamp is attached).
    pub fn new(event_type: impl Into<String>) -> Self {
        Self {
            event_type: event_type.into(),
            data: Value::Null,
            payload: None,
        }
    }

    /// An event carrying a JSON `data` object.
    pub fn with_data(event_type: impl Into<String>, data: Value) -> Self {
        Self {
            event_type: event_type.into(),
            data,
            payload: None,
        }
    }

    /// An `audio-start` event describing the PCM format that follows.
    pub fn audio_start(rate: u32, width_bytes: u16, channels: u16, timestamp_ms: u64) -> Self {
        Self::with_data(
            types::AUDIO_START,
            json!({
                "rate": rate,
                "width": width_bytes,
                "channels": channels,
                "timestamp": timestamp_ms,
            }),
        )
    }

    /// An `audio-chunk` carrying `pcm` (little-endian `i16` bytes) as its payload.
    pub fn audio_chunk(
        rate: u32,
        width_bytes: u16,
        channels: u16,
        timestamp_ms: u64,
        pcm: Vec<u8>,
    ) -> Self {
        Self {
            event_type: types::AUDIO_CHUNK.to_string(),
            data: json!({
                "rate": rate,
                "width": width_bytes,
                "channels": channels,
                "timestamp": timestamp_ms,
            }),
            payload: Some(pcm),
        }
    }

    /// An `audio-stop` event with a timestamp.
    pub fn audio_stop(timestamp_ms: u64) -> Self {
        Self::with_data(types::AUDIO_STOP, json!({ "timestamp": timestamp_ms }))
    }

    /// True if this is a `transcript` event. Server-side VAD makes this our
    /// end-of-speech signal (architecture.md §4).
    pub fn is_transcript(&self) -> bool {
        self.event_type == types::TRANSCRIPT
    }

    /// Extract the recognized text from a `transcript` event's `data.text`.
    pub fn transcript_text(&self) -> Option<&str> {
        if self.is_transcript() {
            self.data.get("text").and_then(Value::as_str)
        } else {
            None
        }
    }

    /// A `reply-token` event carrying one streamed LLM reply fragment (used by
    /// tests and the mock server; the orchestrator emits the wire form directly).
    pub fn reply_token(text: impl Into<String>) -> Self {
        Self::with_data(types::REPLY_TOKEN, json!({ "text": text.into() }))
    }

    /// Extract the fragment text from a `reply-token` event's `data.text`.
    pub fn reply_token_text(&self) -> Option<&str> {
        if self.event_type == types::REPLY_TOKEN {
            self.data.get("text").and_then(Value::as_str)
        } else {
            None
        }
    }

    /// Serialize this event to its on-the-wire bytes: header line, then the
    /// length-prefixed `data` block, then the binary payload.
    pub fn to_bytes(&self) -> Vec<u8> {
        // Only emit a `data` block when there is a non-null object to send;
        // `data_length: 0` on the wire would force the peer into an empty read.
        let data_bytes: Option<Vec<u8>> = match &self.data {
            Value::Null => None,
            other => Some(serde_json::to_vec(other).expect("JSON value is always serializable")),
        };

        let header = Header {
            event_type: self.event_type.clone(),
            data_length: data_bytes.as_ref().map(Vec::len),
            payload_length: self.payload.as_ref().map(Vec::len),
        };

        let mut out = serde_json::to_vec(&header).expect("header is always serializable");
        out.push(b'\n');
        if let Some(d) = data_bytes {
            out.extend_from_slice(&d);
        }
        if let Some(p) = &self.payload {
            out.extend_from_slice(p);
        }
        out
    }
}

/// Normalize a decoded `data` block: an absent block reads back as
/// [`Value::Null`]; a present block must be a JSON object.
fn coerce_data(data: Option<Value>) -> io::Result<Value> {
    match data {
        None => Ok(Value::Null),
        Some(v @ Value::Object(_)) => Ok(v),
        Some(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Wyoming data block was not a JSON object",
        )),
    }
}

/// Write a single event to an async sink and flush it. One event is one logical
/// socket write; flushing keeps latency low for the tiny control frames.
pub async fn write_event<W>(writer: &mut W, event: &WyomingEvent) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(&event.to_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

/// Read one event from an async buffered source. Returns `Ok(None)` on a clean
/// EOF at a frame boundary (peer closed the socket), matching the reference
/// implementation's "no more events" contract.
pub async fn read_event<R>(reader: &mut R) -> io::Result<Option<WyomingEvent>>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Ok(None); // clean EOF at a frame boundary
    }

    let header: Header = serde_json::from_str(line.trim_end_matches(['\r', '\n']))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let data = match header.data_length {
        Some(len) if len > 0 => {
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf).await?;
            Some(
                serde_json::from_slice(&buf)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
            )
        }
        _ => None,
    };

    let payload = match header.payload_length {
        Some(len) if len > 0 => {
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf).await?;
            Some(buf)
        }
        _ => None,
    };

    Ok(Some(WyomingEvent {
        event_type: header.event_type,
        data: coerce_data(data)?,
        payload,
    }))
}

/// Decode a full event from an in-memory byte slice — convenience used by tests
/// and any caller that already has the framed bytes buffered. A `&[u8]` is
/// itself an `AsyncBufRead`, so this reuses the streaming reader path verbatim;
/// the buffered and streamed decoders can never diverge.
pub async fn decode(bytes: &[u8]) -> io::Result<Option<WyomingEvent>> {
    read_event(&mut &bytes[..]).await
}

/// Convert a `data` map into a strongly-typed helper — currently only pulls the
/// PCM format for `audio-chunk`/`audio-start`, but centralizes the field names.
pub fn audio_format(data: &Value) -> Option<(u32, u16, u16)> {
    let obj: &Map<String, Value> = data.as_object()?;
    let rate = obj.get("rate")?.as_u64()? as u32;
    let width = obj.get("width")?.as_u64()? as u16;
    let channels = obj.get("channels")?.as_u64()? as u16;
    Some((rate, width, channels))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn roundtrip(event: &WyomingEvent) -> WyomingEvent {
        let bytes = event.to_bytes();
        decode(&bytes)
            .await
            .expect("decode ok")
            .expect("event present")
    }

    #[tokio::test]
    async fn header_only_event_roundtrips() {
        let ev = WyomingEvent::new(types::AUDIO_STOP);
        let back = roundtrip(&ev).await;
        assert_eq!(back, ev);
        assert_eq!(back.data, Value::Null);
        assert!(back.payload.is_none());
    }

    #[tokio::test]
    async fn data_only_event_roundtrips() {
        let ev = WyomingEvent::audio_start(16_000, 2, 1, 42);
        let back = roundtrip(&ev).await;
        assert_eq!(back, ev);
        assert_eq!(audio_format(&back.data), Some((16_000, 2, 1)));
    }

    #[tokio::test]
    async fn payload_event_roundtrips() {
        let pcm: Vec<u8> = (0..320u16).flat_map(|s| s.to_le_bytes()).collect();
        let ev = WyomingEvent::audio_chunk(16_000, 2, 1, 100, pcm.clone());
        let back = roundtrip(&ev).await;
        assert_eq!(back, ev);
        assert_eq!(back.payload.as_deref(), Some(pcm.as_slice()));
    }

    #[test]
    fn header_line_is_single_newline_terminated_json() {
        let bytes = WyomingEvent::audio_start(16_000, 2, 1, 0).to_bytes();
        let newline = bytes.iter().position(|&b| b == b'\n').expect("has newline");
        // Exactly one newline: the header terminator. The JSON `data` block that
        // follows must not contain a raw newline that could be mistaken for a
        // frame boundary.
        assert_eq!(bytes.iter().filter(|&&b| b == b'\n').count(), 1);
        let header: Header = serde_json::from_slice(&bytes[..newline]).expect("header parses");
        assert_eq!(header.event_type, types::AUDIO_START);
        assert_eq!(header.data_length, Some(bytes.len() - newline - 1));
        assert_eq!(header.payload_length, None);
    }

    #[test]
    fn transcript_text_is_extracted() {
        let ev =
            WyomingEvent::with_data(types::TRANSCRIPT, json!({ "text": "turn on the lights" }));
        assert!(ev.is_transcript());
        assert_eq!(ev.transcript_text(), Some("turn on the lights"));
    }

    #[test]
    fn non_transcript_has_no_text() {
        let ev = WyomingEvent::audio_stop(0);
        assert!(!ev.is_transcript());
        assert_eq!(ev.transcript_text(), None);
    }

    #[tokio::test]
    async fn empty_input_decodes_to_none() {
        assert_eq!(decode(&[]).await.expect("ok"), None);
    }

    #[tokio::test]
    async fn two_events_stream_back_to_back() {
        // Prove the length prefixes let the reader split a concatenated stream
        // (payload bytes never confuse the header/frame boundary).
        let mut wire = Vec::new();
        wire.extend_from_slice(&WyomingEvent::audio_start(16_000, 2, 1, 0).to_bytes());
        let pcm: Vec<u8> = vec![0x11, 0x22, 0x0a, 0x33]; // includes a 0x0a ('\n') byte
        wire.extend_from_slice(&WyomingEvent::audio_chunk(16_000, 2, 1, 1, pcm.clone()).to_bytes());

        let mut reader = &wire[..];
        let first = read_event(&mut reader).await.unwrap().unwrap();
        assert_eq!(first.event_type, types::AUDIO_START);
        let second = read_event(&mut reader).await.unwrap().unwrap();
        assert_eq!(second.event_type, types::AUDIO_CHUNK);
        assert_eq!(second.payload.as_deref(), Some(pcm.as_slice()));
        // Stream is now exhausted.
        assert_eq!(read_event(&mut reader).await.unwrap(), None);
    }
}
