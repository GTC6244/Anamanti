//! Wyoming Protocol wire format for the Mac orchestrator (Plan.MD §3 "Wire
//! format"; architecture.md §4).
//!
//! This is the byte-for-byte counterpart of the device engine's
//! `rust/src/wyoming/protocol.rs`: a Wyoming *event* is three concatenated parts
//! on the socket —
//!
//! 1. one JSON **header line** terminated by `\n` (`type`, optional
//!    `data_length` / `payload_length`),
//! 2. a length-prefixed JSON **`data` object** (when `data_length` is present),
//! 3. an opaque binary **payload** (when `payload_length` is present; raw PCM).
//!
//! The device crate cannot be shared as a library here (it is an Android
//! `cdylib`/`staticlib` that links `cpal`/`tract`/JNI), so the codec is
//! re-implemented independently but kept wire-identical, and the round-trip tests
//! below are the guardrail against drift. The orchestrator uses the same codec in
//! *both* directions: as a Wyoming **server** to the Echo Show, and as a Wyoming
//! **client** to the downstream Whisper (STT) and Piper (TTS) services.

use std::io;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Wyoming event `type` names used by the orchestrator.
pub mod types {
    /// Marks the beginning of an audio stream (data: rate/width/channels).
    pub const AUDIO_START: &str = "audio-start";
    /// A chunk of raw PCM (data: rate/width/channels; payload: samples).
    pub const AUDIO_CHUNK: &str = "audio-chunk";
    /// Marks the end of an audio stream.
    pub const AUDIO_STOP: &str = "audio-stop";
    /// client → STT: request transcription (data may pin a model/language).
    pub const TRANSCRIBE: &str = "transcribe";
    /// STT → client: a (final) transcript (data: `text`). Server-side VAD makes
    /// this the end-of-speech signal.
    pub const TRANSCRIPT: &str = "transcript";
    /// client → TTS: request synthesis of `text` (data: `text`, optional `voice`).
    pub const SYNTHESIZE: &str = "synthesize";
    /// orchestrator → device (Phase 5): one streamed LLM reply-token fragment
    /// (data: `text`), so the device renders the reply token-by-token as it is
    /// generated. A project-local extension on the device↔Mac hop (no off-the-shelf
    /// Wyoming server is on that hop).
    pub const REPLY_TOKEN: &str = "reply-token";
    /// device → orchestrator: **barge-in**. The user started speaking (a new wake
    /// word, or on-device VAD) while the assistant was still replying, so the
    /// orchestrator must abort the in-flight LLM generation + TTS synthesis for
    /// this turn at once (no data). A project-local extension on the device↔Mac
    /// hop; kept byte-identical to the device crate's `types::INTERRUPT`.
    pub const INTERRUPT: &str = "ambient-interrupt";

    // ---- Phase 6: project-local settings + memory control frames ----
    //
    // These ride the same Wyoming framing on the device↔orchestrator hop only (no
    // off-the-shelf Wyoming server ever sees them). They let the on-device settings
    // screen read/change the orchestrator's runtime LLM backend + TTS voice and
    // view/delete persistent memory entries. Kept byte-identical to the device
    // crate's `types` (guarded by the round-trip tests in both crates).

    /// device → orchestrator: request the current runtime settings (no data).
    pub const DESCRIBE_SETTINGS: &str = "ambient-describe-settings";
    /// device → orchestrator: change runtime settings (data: optional
    /// `llm_backend`, `llm_model`, and `tts_voice` — where a present `tts_voice:
    /// null` clears the voice and an absent key leaves it unchanged).
    pub const SET_SETTINGS: &str = "ambient-set-settings";
    /// orchestrator → device: the resulting settings (data: `ok`, `message`,
    /// `llm_backend`, `llm_model`, `tts_voice`).
    pub const SETTINGS: &str = "ambient-settings";
    /// device → orchestrator: list all stored memory entries (no data).
    pub const LIST_MEMORIES: &str = "ambient-list-memories";
    /// orchestrator → device: the stored entries (data: `entries` array).
    pub const MEMORIES: &str = "ambient-memories";
    /// device → orchestrator: delete one entry by id (data: `id`).
    pub const DELETE_MEMORY: &str = "ambient-delete-memory";
    /// device → orchestrator: delete every stored entry (no data).
    pub const CLEAR_MEMORIES: &str = "ambient-clear-memories";
    /// orchestrator → device: result of a delete/clear (data: `ok`, `count`).
    pub const MEMORY_RESULT: &str = "ambient-memory-result";
}

/// PCM format carried by `audio-start` / `audio-chunk` frames. The device streams
/// 16 kHz mono 16-bit and every downstream service is fed the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub rate: u32,
    pub width_bytes: u16,
    pub channels: u16,
}

impl AudioFormat {
    /// The pipeline default: 16 kHz, mono, 16-bit signed PCM.
    pub const PCM_16K_MONO: AudioFormat = AudioFormat {
        rate: 16_000,
        width_bytes: 2,
        channels: 1,
    };
}

impl Default for AudioFormat {
    fn default() -> Self {
        AudioFormat::PCM_16K_MONO
    }
}

/// A decoded Wyoming event: a `type` tag, an optional structured `data` object,
/// and an optional opaque binary `payload`.
#[derive(Debug, Clone, PartialEq)]
pub struct WyomingEvent {
    pub event_type: String,
    pub data: Value,
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
    /// A data-less, payload-less event.
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
    pub fn audio_start(fmt: AudioFormat, timestamp_ms: u64) -> Self {
        Self::with_data(
            types::AUDIO_START,
            json!({
                "rate": fmt.rate,
                "width": fmt.width_bytes,
                "channels": fmt.channels,
                "timestamp": timestamp_ms,
            }),
        )
    }

    /// An `audio-chunk` carrying little-endian `i16` PCM bytes as its payload.
    pub fn audio_chunk(fmt: AudioFormat, timestamp_ms: u64, pcm: Vec<u8>) -> Self {
        Self {
            event_type: types::AUDIO_CHUNK.to_string(),
            data: json!({
                "rate": fmt.rate,
                "width": fmt.width_bytes,
                "channels": fmt.channels,
                "timestamp": timestamp_ms,
            }),
            payload: Some(pcm),
        }
    }

    /// An `audio-stop` event with a timestamp.
    pub fn audio_stop(timestamp_ms: u64) -> Self {
        Self::with_data(types::AUDIO_STOP, json!({ "timestamp": timestamp_ms }))
    }

    /// A `transcript` event carrying recognized `text`.
    pub fn transcript(text: impl Into<String>) -> Self {
        Self::with_data(types::TRANSCRIPT, json!({ "text": text.into() }))
    }

    /// A `reply-token` event carrying one streamed LLM reply fragment, relayed to
    /// the device so it can render the reply as it is generated (Phase 5).
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

    /// An `ambient-interrupt` (barge-in) event: data-less, sent device →
    /// orchestrator to abort the in-flight reply.
    pub fn interrupt() -> Self {
        Self::new(types::INTERRUPT)
    }

    /// True if this is an `ambient-interrupt` (barge-in) event.
    pub fn is_interrupt(&self) -> bool {
        self.event_type == types::INTERRUPT
    }

    /// A `synthesize` request for the Piper TTS server. `voice` pins a named voice
    /// when the config asks for one; otherwise the server default is used.
    pub fn synthesize(text: impl Into<String>, voice: Option<&str>) -> Self {
        let mut data = Map::new();
        data.insert("text".into(), Value::String(text.into()));
        if let Some(v) = voice {
            data.insert("voice".into(), json!({ "name": v }));
        }
        Self::with_data(types::SYNTHESIZE, Value::Object(data))
    }

    /// True if this is a `transcript` event.
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

    /// Serialize to on-the-wire bytes: header line, then the length-prefixed
    /// `data` block, then the binary payload.
    pub fn to_bytes(&self) -> Vec<u8> {
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

/// Write a single event to an async sink and flush it.
pub async fn write_event<W>(writer: &mut W, event: &WyomingEvent) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(&event.to_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

/// Read one event from an async buffered source. Returns `Ok(None)` on a clean
/// EOF at a frame boundary (peer closed the socket).
pub async fn read_event<R>(reader: &mut R) -> io::Result<Option<WyomingEvent>>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Ok(None);
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

/// Decode a full event from an in-memory byte slice (used by tests).
pub async fn decode(bytes: &[u8]) -> io::Result<Option<WyomingEvent>> {
    read_event(&mut &bytes[..]).await
}

/// Pull the `(rate, width, channels)` PCM format out of an `audio-*` data block.
pub fn audio_format(data: &Value) -> Option<AudioFormat> {
    let obj: &Map<String, Value> = data.as_object()?;
    Some(AudioFormat {
        rate: obj.get("rate")?.as_u64()? as u32,
        width_bytes: obj.get("width")?.as_u64()? as u16,
        channels: obj.get("channels")?.as_u64()? as u16,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn roundtrip(event: &WyomingEvent) -> WyomingEvent {
        decode(&event.to_bytes())
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
    async fn audio_start_roundtrips_and_reports_format() {
        let ev = WyomingEvent::audio_start(AudioFormat::PCM_16K_MONO, 42);
        let back = roundtrip(&ev).await;
        assert_eq!(back, ev);
        assert_eq!(audio_format(&back.data), Some(AudioFormat::PCM_16K_MONO));
    }

    #[tokio::test]
    async fn payload_event_roundtrips() {
        let pcm: Vec<u8> = (0..320u16).flat_map(|s| s.to_le_bytes()).collect();
        let ev = WyomingEvent::audio_chunk(AudioFormat::PCM_16K_MONO, 100, pcm.clone());
        let back = roundtrip(&ev).await;
        assert_eq!(back, ev);
        assert_eq!(back.payload.as_deref(), Some(pcm.as_slice()));
    }

    #[tokio::test]
    async fn interrupt_event_roundtrips() {
        let ev = WyomingEvent::interrupt();
        let back = roundtrip(&ev).await;
        assert_eq!(back, ev);
        assert_eq!(back.event_type, types::INTERRUPT);
        assert!(back.is_interrupt());
        assert_eq!(back.data, Value::Null);
        assert!(back.payload.is_none());
    }

    #[tokio::test]
    async fn synthesize_carries_text_and_optional_voice() {
        let with_voice = WyomingEvent::synthesize("hello there", Some("en_US-amy-medium"));
        let back = roundtrip(&with_voice).await;
        assert_eq!(back.event_type, types::SYNTHESIZE);
        assert_eq!(back.data["text"], json!("hello there"));
        assert_eq!(back.data["voice"]["name"], json!("en_US-amy-medium"));

        let no_voice = WyomingEvent::synthesize("hi", None);
        assert!(no_voice.data.get("voice").is_none());
    }

    #[test]
    fn header_line_is_single_newline_terminated_json() {
        let bytes = WyomingEvent::audio_start(AudioFormat::PCM_16K_MONO, 0).to_bytes();
        let newline = bytes.iter().position(|&b| b == b'\n').expect("has newline");
        assert_eq!(bytes.iter().filter(|&&b| b == b'\n').count(), 1);
        let header: Header = serde_json::from_slice(&bytes[..newline]).expect("header parses");
        assert_eq!(header.event_type, types::AUDIO_START);
        assert_eq!(header.data_length, Some(bytes.len() - newline - 1));
        assert_eq!(header.payload_length, None);
    }

    #[test]
    fn transcript_text_is_extracted() {
        let ev = WyomingEvent::transcript("turn on the lights");
        assert!(ev.is_transcript());
        assert_eq!(ev.transcript_text(), Some("turn on the lights"));
    }

    #[tokio::test]
    async fn empty_input_decodes_to_none() {
        assert_eq!(decode(&[]).await.expect("ok"), None);
    }

    #[tokio::test]
    async fn two_events_stream_back_to_back() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&WyomingEvent::audio_start(AudioFormat::PCM_16K_MONO, 0).to_bytes());
        let pcm: Vec<u8> = vec![0x11, 0x22, 0x0a, 0x33]; // includes a 0x0a ('\n')
        wire.extend_from_slice(
            &WyomingEvent::audio_chunk(AudioFormat::PCM_16K_MONO, 1, pcm.clone()).to_bytes(),
        );

        let mut reader = &wire[..];
        let first = read_event(&mut reader).await.unwrap().unwrap();
        assert_eq!(first.event_type, types::AUDIO_START);
        let second = read_event(&mut reader).await.unwrap().unwrap();
        assert_eq!(second.event_type, types::AUDIO_CHUNK);
        assert_eq!(second.payload.as_deref(), Some(pcm.as_slice()));
        assert_eq!(read_event(&mut reader).await.unwrap(), None);
    }
}
