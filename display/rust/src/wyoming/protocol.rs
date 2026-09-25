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
    /// device → orchestrator: **barge-in**. The user started speaking (a new wake
    /// word, or on-device VAD) while the assistant was still replying, so stop
    /// generating/synthesizing this turn at once (no data). A project-local
    /// extension on the device↔Mac hop. The device flushes its own playback locally
    /// the instant this is sent; the frame tells the orchestrator to abort the
    /// in-flight LLM + TTS rather than waiting for the socket to drop.
    pub const INTERRUPT: &str = "ambient-interrupt";

    // ---- Phase 6: project-local settings + memory control frames ----
    //
    // Sent by the device settings screen to the orchestrator (device↔Mac hop only)
    // to read/change the runtime LLM backend + TTS voice and view/delete persistent
    // memory. Kept byte-identical to the orchestrator crate's `types` (guarded by
    // the round-trip tests in both crates).

    /// device → orchestrator: request the current runtime settings (no data).
    pub const DESCRIBE_SETTINGS: &str = "ambient-describe-settings";
    /// device → orchestrator: change runtime settings (data: optional
    /// `llm_backend`, `llm_model`, `tts_voice`).
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

    // Speaker identification control frames (speaker_id_plan.md Phase C). Kept
    // byte-identical to the orchestrator crate's `types` (guarded by round-trip
    // tests in both crates).

    /// device → orchestrator: list identified speakers (no data).
    pub const LIST_SPEAKERS: &str = "ambient-list-speakers";
    /// orchestrator → device: the speakers (data: `ok`, `speakers` array of
    /// `{id, name, labeled, samples, created_at}`).
    pub const SPEAKERS: &str = "ambient-speakers";
    /// device → orchestrator: name a speaker (data: `id`, `name`).
    pub const NAME_SPEAKER: &str = "ambient-name-speaker";
    /// device → orchestrator: merge `drop` into `keep` (data: `keep`, `drop`).
    pub const MERGE_SPEAKERS: &str = "ambient-merge-speakers";
    /// device → orchestrator: delete a speaker profile (data: `id`).
    pub const DELETE_SPEAKER: &str = "ambient-delete-speaker";
    /// orchestrator → device: result of a name/merge/delete (data: `ok`, `message`).
    pub const SPEAKER_RESULT: &str = "ambient-speaker-result";

    /// device → orchestrator: list the selectable LLM models for the settings model
    /// dropdown (no data).
    pub const LIST_MODELS: &str = "ambient-list-models";
    /// orchestrator → device: the selectable models (data: `ok`, `models` array of
    /// `{provider, id, label}`), scoped to the last 12 months per provider.
    pub const MODELS: &str = "ambient-models";

    /// device → orchestrator: list the installed Piper voices for the settings TTS
    /// voice dropdown (no data). Byte-identical to the orchestrator's `LIST_VOICES`.
    pub const LIST_VOICES: &str = "ambient-list-voices";
    /// orchestrator → device: the installed voices (data: `ok`, `voices` array of
    /// `{name, language, label}`). Byte-identical to the orchestrator's `VOICES`.
    pub const VOICES: &str = "ambient-voices";

    /// device → orchestrator: fetch the Google Drive photo-slideshow credentials +
    /// linkage the orchestrator owns (no data). Byte-identical to the orchestrator's
    /// `GET_DRIVE_TOKEN`.
    pub const GET_DRIVE_TOKEN: &str = "ambient-get-drive-token";
    /// orchestrator → device: the Drive bundle (data: `ok`, `linked`, `configured`,
    /// `client_id`, `client_secret`, `refresh_token`, `folder_ids` array, `scope`).
    /// Byte-identical to the orchestrator's `DRIVE_TOKEN`.
    pub const DRIVE_TOKEN: &str = "ambient-drive-token";

    // ---- Device-action frames (Phase 2: on-device timers/alarms) ----
    //
    // orchestrator → device: a tool the LLM called on the Mac emits a device action
    // relayed as this frame during the turn. The device owns the resulting state
    // (unlimited concurrent timers, the countdown UI, and the alarm sound), so it
    // keeps ticking after the turn's socket closes and even if the Mac disconnects.
    // Byte-identical to the orchestrator crate's `types::TIMER`.

    /// orchestrator → device: start or cancel a timer (data: `action` =
    /// `"start"`/`"cancel"`; for `start`: `duration_secs` + optional `label`; for
    /// `cancel`: optional `label`, where an absent/null label cancels all timers).
    pub const TIMER: &str = "ambient-timer";

    /// device → orchestrator: synthesize `text` with Piper and stream the audio back
    /// on the same socket (data: `text`). A fired on-device timer sends this to voice
    /// its "Time's up …" announcement in the real assistant voice when the Mac is
    /// reachable (bell-only when offline). Byte-identical to the orchestrator crate's
    /// `types::SPEAK`.
    pub const SPEAK: &str = "ambient-speak";

    /// orchestrator → device: **follow-up listen**. The assistant's reply was a
    /// question, so reopen the mic and start a fresh turn with no wake word once the
    /// reply audio finishes (data: `depth` = the chain depth the follow-up turn will
    /// carry). Arrives mid-turn, before the final TTS `audio-stop`. Byte-identical to
    /// the orchestrator crate's `types::LISTEN`. See `plans/Plan.MD` (Follow-up
    /// listening).
    pub const LISTEN: &str = "ambient-listen";

    // ---- Proactive notifications (Approach A: persistent device-dialed channel) ----
    //
    // A long-lived connection the device dials and holds open so the orchestrator can
    // PUSH unsolicited notifications to the display without a voice turn. Distinct
    // from the per-turn voice socket, but rides the same Wyoming framing on the
    // device↔orchestrator hop. Byte-identical to the orchestrator crate's `types`.

    /// device → orchestrator: open + register the persistent notify channel (data:
    /// `role` = "notify", `device_id`, optional `instance_id`).
    pub const AMBIENT_HELLO: &str = "ambient-hello";
    /// orchestrator → device: a proactive notification to display (data: `id`,
    /// `priority` = "info" | "reminder" | "alert", `title`, `body`).
    pub const AMBIENT_NOTIFY: &str = "ambient-notify";
    /// device → orchestrator: acknowledge a notification by `id` (data: `id`,
    /// `result`). Reserved for a later delivery-tracking phase.
    pub const AMBIENT_NOTIFY_ACK: &str = "ambient-notify-ack";

    /// orchestrator → device: show or dismiss the recipe-mode screen (data: `action`
    /// = `"show"`/`"dismiss"`; for `show`, `recipe` is the structured recipe object).
    /// A device action driven by the orchestrator's `recipe_lookup` tool; the device
    /// owns the screen state until dismissed. Byte-identical to the orchestrator
    /// crate's `types::RECIPE`.
    pub const RECIPE: &str = "ambient-recipe";
}

/// A device-action timer command decoded from an `ambient-timer` frame (Phase 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimerCommand {
    /// Start a countdown timer for `duration_secs` with an optional spoken `label`.
    Start {
        label: Option<String>,
        duration_secs: u64,
    },
    /// Cancel timers matching `label`, or *all* timers when `label` is `None`.
    Cancel { label: Option<String> },
}

/// A recipe-mode command decoded from an `ambient-recipe` frame. `Show` carries the
/// recipe object (surfaced to Flutter as a JSON string it parses into the tabs);
/// `Dismiss` closes the screen.
#[derive(Debug, Clone, PartialEq)]
pub enum RecipeCommand {
    /// Show this recipe (the raw `schema.org`-shaped object) on the recipe screen.
    Show(Value),
    /// Dismiss the recipe screen and return to the idle/ambient display.
    Dismiss,
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

    /// An `audio-start` for a **follow-up** turn — one the device auto-opened after a
    /// question reply, with no wake word. It is the ordinary PCM header plus
    /// `followup: true` + `followup_depth`, which the orchestrator reads to seed the
    /// prompt with recent history and to bound the follow-up chain. `depth == 0` emits
    /// a plain `audio-start` (an ordinary wake-word turn, no marker).
    pub fn audio_start_followup(
        rate: u32,
        width_bytes: u16,
        channels: u16,
        timestamp_ms: u64,
        depth: u32,
        wait_secs: u32,
    ) -> Self {
        let mut ev = Self::audio_start(rate, width_bytes, channels, timestamp_ms);
        if depth > 0 {
            if let Value::Object(map) = &mut ev.data {
                map.insert("followup".into(), Value::Bool(true));
                map.insert("followup_depth".into(), json!(depth));
                // Echo the listen window the orchestrator gave us so it sizes this
                // turn's no-speech VAD window to match.
                map.insert("wait_secs".into(), json!(wait_secs));
            }
        }
        ev
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

    /// An `ambient-interrupt` (barge-in) event: data-less, sent device →
    /// orchestrator to abort the in-flight reply.
    pub fn interrupt() -> Self {
        Self::new(types::INTERRUPT)
    }

    /// True if this is an `ambient-interrupt` (barge-in) event.
    pub fn is_interrupt(&self) -> bool {
        self.event_type == types::INTERRUPT
    }

    /// An `ambient-speak` request (device → orchestrator): please synthesize `text`
    /// and stream its audio back. Mirror of the orchestrator crate's `speak`.
    pub fn speak(text: impl Into<String>) -> Self {
        Self::with_data(types::SPEAK, json!({ "text": text.into() }))
    }

    /// An `ambient-listen` follow-up-listen frame (used by tests + the mock server; the
    /// orchestrator emits the wire form directly). `depth` is the chain depth the
    /// follow-up turn will carry; `wait_secs` is how long to keep the mic open for input
    /// before sleeping.
    pub fn listen(depth: u32, wait_secs: u32) -> Self {
        Self::with_data(
            types::LISTEN,
            json!({ "depth": depth, "wait_secs": wait_secs }),
        )
    }

    /// Decode an `ambient-listen` frame as `(depth, wait_secs)` — `depth` defaults to 1
    /// and `wait_secs` to 0 (caller falls back to its own window) if absent. `None` if
    /// this is not a follow-up-listen frame.
    pub fn listen_params(&self) -> Option<(u32, u32)> {
        if self.event_type == types::LISTEN {
            let depth = self.data.get("depth").and_then(Value::as_u64).unwrap_or(1) as u32;
            let wait_secs = self
                .data
                .get("wait_secs")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32;
            Some((depth, wait_secs))
        } else {
            None
        }
    }

    /// An `ambient-hello` channel-open frame (device → orchestrator): register the
    /// persistent notify channel. Byte-identical to the orchestrator's `hello`.
    pub fn hello(device_id: impl Into<String>, instance_id: impl Into<String>) -> Self {
        Self::with_data(
            types::AMBIENT_HELLO,
            json!({
                "role": "notify",
                "device_id": device_id.into(),
                "instance_id": instance_id.into(),
            }),
        )
    }

    /// An `ambient-notify` push (orchestrator → device). Byte-identical to the
    /// orchestrator's `notify` (used by tests + the mock server; the orchestrator
    /// emits the wire form directly).
    pub fn notify(
        id: impl Into<String>,
        priority: impl Into<String>,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self::with_data(
            types::AMBIENT_NOTIFY,
            json!({
                "id": id.into(),
                "priority": priority.into(),
                "title": title.into(),
                "body": body.into(),
            }),
        )
    }

    /// An `ambient-notify-ack` frame (device → orchestrator). Reserved for a later
    /// delivery-tracking phase; provided now so both crates share the constructor.
    pub fn notify_ack(id: impl Into<String>, result: impl Into<String>) -> Self {
        Self::with_data(
            types::AMBIENT_NOTIFY_ACK,
            json!({ "id": id.into(), "result": result.into() }),
        )
    }

    /// An `ambient-timer` **start** action (used by tests + the mock server; the
    /// orchestrator emits the wire form directly).
    pub fn timer_start(duration_secs: u64, label: Option<&str>) -> Self {
        Self::with_data(
            types::TIMER,
            json!({ "action": "start", "duration_secs": duration_secs, "label": label }),
        )
    }

    /// An `ambient-timer` **cancel** action (`label` `None` cancels all timers).
    pub fn timer_cancel(label: Option<&str>) -> Self {
        Self::with_data(types::TIMER, json!({ "action": "cancel", "label": label }))
    }

    /// Decode an `ambient-timer` frame into a [`TimerCommand`], or `None` if this is
    /// not a timer frame or its `action` is unrecognized. A `start` with no/invalid
    /// `duration_secs` is rejected (returns `None`).
    pub fn timer_command(&self) -> Option<TimerCommand> {
        if self.event_type != types::TIMER {
            return None;
        }
        let label = self
            .data
            .get("label")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string);
        match self.data.get("action").and_then(Value::as_str)? {
            "start" => {
                let duration_secs = self.data.get("duration_secs").and_then(Value::as_u64)?;
                Some(TimerCommand::Start {
                    label,
                    duration_secs,
                })
            }
            "cancel" => Some(TimerCommand::Cancel { label }),
            _ => None,
        }
    }

    /// An `ambient-recipe` **show** action (used by tests + the mock server; the
    /// orchestrator emits the wire form directly).
    pub fn recipe(recipe: Value) -> Self {
        Self::with_data(types::RECIPE, json!({ "action": "show", "recipe": recipe }))
    }

    /// An `ambient-recipe` **dismiss** action.
    pub fn recipe_dismiss() -> Self {
        Self::with_data(types::RECIPE, json!({ "action": "dismiss" }))
    }

    /// Decode an `ambient-recipe` frame into a [`RecipeCommand`], or `None` if this is
    /// not a recipe frame or its `action` is unrecognized. A `show` with no `recipe`
    /// object is rejected (returns `None`).
    pub fn recipe_command(&self) -> Option<RecipeCommand> {
        if self.event_type != types::RECIPE {
            return None;
        }
        match self.data.get("action").and_then(Value::as_str)? {
            "show" => self
                .data
                .get("recipe")
                .filter(|v| v.is_object())
                .cloned()
                .map(RecipeCommand::Show),
            "dismiss" => Some(RecipeCommand::Dismiss),
            _ => None,
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
    async fn timer_frame_roundtrips_and_decodes_command() {
        let start = WyomingEvent::timer_start(300, Some("pasta"));
        let back = roundtrip(&start).await;
        assert_eq!(back, start);
        assert_eq!(
            back.timer_command(),
            Some(TimerCommand::Start {
                label: Some("pasta".to_string()),
                duration_secs: 300
            })
        );

        // Cancel-all: null label decodes to `None`.
        let cancel = roundtrip(&WyomingEvent::timer_cancel(None)).await;
        assert_eq!(
            cancel.timer_command(),
            Some(TimerCommand::Cancel { label: None })
        );

        // A non-timer frame yields no command; a start missing a duration is rejected.
        assert_eq!(WyomingEvent::interrupt().timer_command(), None);
        let bad = WyomingEvent::with_data(types::TIMER, json!({ "action": "start" }));
        assert_eq!(bad.timer_command(), None);
    }

    #[tokio::test]
    async fn recipe_frame_roundtrips_and_decodes_command() {
        let recipe = json!({
            "title": "Carbonara",
            "ingredients": ["spaghetti", "eggs"],
            "steps": ["boil", "toss"],
        });
        let show = WyomingEvent::recipe(recipe.clone());
        let back = roundtrip(&show).await;
        assert_eq!(back, show);
        assert_eq!(back.recipe_command(), Some(RecipeCommand::Show(recipe)));

        let dismiss = roundtrip(&WyomingEvent::recipe_dismiss()).await;
        assert_eq!(dismiss.recipe_command(), Some(RecipeCommand::Dismiss));

        // A non-recipe frame yields nothing; a show missing the recipe object is rejected.
        assert_eq!(WyomingEvent::interrupt().recipe_command(), None);
        let bad = WyomingEvent::with_data(types::RECIPE, json!({ "action": "show" }));
        assert_eq!(bad.recipe_command(), None);
    }

    #[tokio::test]
    async fn listen_frame_roundtrips_and_decodes_params() {
        let ev = WyomingEvent::listen(2, 10);
        let back = roundtrip(&ev).await;
        assert_eq!(back, ev);
        assert_eq!(back.event_type, types::LISTEN);
        assert_eq!(back.listen_params(), Some((2, 10)));
        // A non-listen frame yields nothing; a listen missing fields defaults (1, 0).
        assert_eq!(WyomingEvent::interrupt().listen_params(), None);
        let no_field = WyomingEvent::with_data(types::LISTEN, json!({}));
        assert_eq!(no_field.listen_params(), Some((1, 0)));
    }

    #[tokio::test]
    async fn followup_audio_start_marks_depth_and_roundtrips() {
        // depth 0 → a plain audio-start with no follow-up marker.
        let plain = WyomingEvent::audio_start_followup(16_000, 2, 1, 0, 0, 0);
        assert_eq!(plain, WyomingEvent::audio_start(16_000, 2, 1, 0));
        assert!(plain.data.get("followup").is_none());

        // depth > 0 → the PCM header plus followup: true + followup_depth + wait_secs.
        let marked = WyomingEvent::audio_start_followup(16_000, 2, 1, 0, 3, 5);
        let back = roundtrip(&marked).await;
        assert_eq!(back, marked);
        assert_eq!(audio_format(&back.data), Some((16_000, 2, 1)));
        assert_eq!(
            back.data.get("followup").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            back.data.get("followup_depth").and_then(Value::as_u64),
            Some(3)
        );
        assert_eq!(back.data.get("wait_secs").and_then(Value::as_u64), Some(5));
    }

    #[tokio::test]
    async fn speak_frame_roundtrips_and_extracts_text() {
        let ev = WyomingEvent::speak("Time's up for pasta");
        let back = roundtrip(&ev).await;
        assert_eq!(back, ev);
        assert_eq!(back.event_type, types::SPEAK);
        assert_eq!(
            back.data.get("text").and_then(Value::as_str),
            Some("Time's up for pasta")
        );
    }

    #[tokio::test]
    async fn notify_frames_roundtrip() {
        let hello = WyomingEvent::hello("echo-show-8", "Paul Family");
        let back = roundtrip(&hello).await;
        assert_eq!(back, hello);
        assert_eq!(back.event_type, types::AMBIENT_HELLO);
        assert_eq!(back.data["role"], json!("notify"));
        assert_eq!(back.data["device_id"], json!("echo-show-8"));

        let note = WyomingEvent::notify("42-0", "info", "Reminder", "Meeting in 5 minutes");
        let back = roundtrip(&note).await;
        assert_eq!(back, note);
        assert_eq!(back.event_type, types::AMBIENT_NOTIFY);
        assert_eq!(back.data["title"], json!("Reminder"));
        assert_eq!(back.data["body"], json!("Meeting in 5 minutes"));

        let ack = WyomingEvent::notify_ack("42-0", "shown");
        let back = roundtrip(&ack).await;
        assert_eq!(back, ack);
        assert_eq!(back.event_type, types::AMBIENT_NOTIFY_ACK);
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
