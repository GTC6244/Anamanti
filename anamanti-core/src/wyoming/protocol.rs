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
    /// client → downstream Wyoming service: request its capabilities (no data).
    /// Sent to Piper to enumerate its advertised voice catalog.
    pub const DESCRIBE: &str = "describe";
    /// downstream Wyoming service → client: capabilities (data: `tts`/`asr` arrays;
    /// each `tts` entry carries a `voices` array of `{name, languages, ...}`).
    pub const INFO: &str = "info";
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
    pub const INTERRUPT: &str = "anamanti-interrupt";

    // ---- Phase 6: project-local settings + memory control frames ----
    //
    // These ride the same Wyoming framing on the device↔orchestrator hop only (no
    // off-the-shelf Wyoming server ever sees them). They let the on-device settings
    // screen read/change the orchestrator's runtime LLM backend + TTS voice and
    // view/delete persistent memory entries. Kept byte-identical to the device
    // crate's `types` (guarded by the round-trip tests in both crates).

    /// device → orchestrator: request the current runtime settings (no data).
    pub const DESCRIBE_SETTINGS: &str = "anamanti-describe-settings";
    /// device → orchestrator: change runtime settings (data: optional
    /// `llm_backend`, `llm_model`, and `tts_voice` — where a present `tts_voice:
    /// null` clears the voice and an absent key leaves it unchanged).
    pub const SET_SETTINGS: &str = "anamanti-set-settings";
    /// orchestrator → device: the resulting settings (data: `ok`, `message`,
    /// `llm_backend`, `llm_model`, `tts_voice`).
    pub const SETTINGS: &str = "anamanti-settings";
    /// device → orchestrator: list all stored memory entries (no data).
    pub const LIST_MEMORIES: &str = "anamanti-list-memories";
    /// orchestrator → device: the stored entries (data: `entries` array).
    pub const MEMORIES: &str = "anamanti-memories";
    /// device → orchestrator: delete one entry by id (data: `id`).
    pub const DELETE_MEMORY: &str = "anamanti-delete-memory";
    /// device → orchestrator: delete every stored entry (no data).
    pub const CLEAR_MEMORIES: &str = "anamanti-clear-memories";
    /// orchestrator → device: result of a delete/clear (data: `ok`, `count`).
    pub const MEMORY_RESULT: &str = "anamanti-memory-result";

    // ---- Speaker identification control frames (speaker_id_plan.md Phase C) ----
    //
    // Let the on-device "People" settings view enumerate identified speakers and
    // name / merge / delete them. Same device↔orchestrator-only framing.

    /// device → orchestrator: list identified speakers (no data).
    pub const LIST_SPEAKERS: &str = "anamanti-list-speakers";
    /// orchestrator → device: the speakers (data: `ok`, `speakers` array of
    /// `{id, name, labeled, samples, created_at}`).
    pub const SPEAKERS: &str = "anamanti-speakers";
    /// device → orchestrator: name a speaker (data: `id`, `name`).
    pub const NAME_SPEAKER: &str = "anamanti-name-speaker";
    /// device → orchestrator: merge `drop` into `keep` (data: `keep`, `drop`).
    pub const MERGE_SPEAKERS: &str = "anamanti-merge-speakers";
    /// device → orchestrator: delete a speaker profile (data: `id`).
    pub const DELETE_SPEAKER: &str = "anamanti-delete-speaker";
    /// orchestrator → device: result of a name/merge/delete (data: `ok`, `message`).
    pub const SPEAKER_RESULT: &str = "anamanti-speaker-result";

    /// device → orchestrator: list the selectable LLM models for the settings model
    /// dropdown (no data).
    pub const LIST_MODELS: &str = "anamanti-list-models";
    /// orchestrator → device: the selectable models (data: `ok`, `models` array of
    /// `{provider, id, label}`), scoped to the last 12 months per provider.
    pub const MODELS: &str = "anamanti-models";

    /// device → orchestrator: list the installed Piper voices for the settings TTS
    /// voice dropdown (no data).
    pub const LIST_VOICES: &str = "anamanti-list-voices";
    /// orchestrator → device: the installed voices (data: `ok`, `voices` array of
    /// `{name, language, label}`). The orchestrator intersects Piper's advertised
    /// catalog with the voices actually present on disk.
    pub const VOICES: &str = "anamanti-voices";

    /// device → orchestrator: fetch the Google Drive photo-slideshow credentials +
    /// linkage the orchestrator owns (no data). The orchestrator runs the one-time
    /// OAuth consent (config page); the device pulls the bundle and mints Drive
    /// access tokens on-device, so the APK ships credential-free.
    pub const GET_DRIVE_TOKEN: &str = "anamanti-get-drive-token";
    /// orchestrator → device: the Drive bundle (data: `ok`, `linked`, `configured`,
    /// `client_id`, `client_secret`, `refresh_token`, `folder_ids` array, `scope`).
    /// Secrets ride the device↔orchestrator LAN hop only.
    pub const DRIVE_TOKEN: &str = "anamanti-drive-token";

    // ---- Device-action frames (Phase 2: on-device timers/alarms) ----
    //
    // A tool the LLM calls on the Mac emits a **device action** that the pipeline
    // relays to the Echo Show as this project-local frame. The device owns the
    // action's state (unlimited concurrent timers, the countdown UI, and the alarm
    // sound), so it keeps working if the Mac disconnects. Same device↔orchestrator-
    // only framing; byte-identical to the device crate's `types::TIMER`.

    /// orchestrator → device: start or cancel a timer (data: `action` =
    /// `"start"`/`"cancel"`; for `start`: `duration_secs` + optional `label`; for
    /// `cancel`: optional `label`, where an absent/null label cancels all timers).
    pub const TIMER: &str = "anamanti-timer";

    /// device → orchestrator: synthesize `text` with Piper and stream the audio
    /// back on the same socket (data: `text`). Lets an on-device timer speak its
    /// "Time's up …" announcement in the real assistant voice when the Mac is
    /// reachable. Byte-identical to the device crate's `types::SPEAK`.
    pub const SPEAK: &str = "anamanti-speak";

    /// orchestrator → device: **follow-up listen**. The assistant's reply was a
    /// question, so the device should reopen the mic and start a fresh turn with no
    /// wake word once the reply audio finishes (data: `depth` = the chain depth the
    /// follow-up turn will carry). Sent *before* the turn's final `audio-stop` (the
    /// device ends its turn on the first `audio-stop`). Byte-identical to the device
    /// crate's `types::LISTEN`. See `plans/Plan.MD` (Follow-up listening).
    pub const LISTEN: &str = "anamanti-listen";

    // ---- Proactive notifications (Approach A: persistent device-dialed channel) ----
    //
    // A NEW long-lived connection the device dials and holds open so the
    // orchestrator can PUSH unsolicited notifications to the display *without* the
    // device starting a voice turn. Distinct from the per-turn voice socket, but
    // rides the same Wyoming framing and the same discovered endpoint. Byte-identical
    // to the device crate's `types` (guarded by the round-trip tests in both crates).

    /// device → orchestrator: open + register the persistent notify channel (data:
    /// `role` = "notify", `device_id`, optional `instance_id`). The orchestrator
    /// keeps this connection open and pushes `anamanti-notify` frames down it.
    pub const ANAMANTI_HELLO: &str = "anamanti-hello";
    /// orchestrator → device: a proactive notification to display (data: `id`,
    /// `priority` = "info" | "reminder" | "alert", `title`, `body`). Visual-only in
    /// this phase — no spoken output.
    pub const ANAMANTI_NOTIFY: &str = "anamanti-notify";
    /// device → orchestrator: acknowledge a notification by `id` (data: `id`,
    /// `result`). Reserved for delivery tracking; unused in the visual-only phase.
    pub const ANAMANTI_NOTIFY_ACK: &str = "anamanti-notify-ack";

    /// orchestrator → device: show or dismiss the recipe-mode screen (data: `action`
    /// = `"show"`/`"dismiss"`; for `show`, `recipe` is the structured recipe object —
    /// `title`, `summary`, `source_url`, `image_url`, `servings`, `total_time`,
    /// `ingredients[]`, `steps[]`). A device action driven by the `recipe_lookup`
    /// tool; the device owns the screen state until dismissed. Byte-identical to the
    /// device crate's `types::RECIPE`.
    pub const RECIPE: &str = "anamanti-recipe";

    /// orchestrator → device: show/refresh/dismiss the weather screen (data: `action`
    /// = `"show"` (full-screen forecast, voice-triggered) / `"current"` (ambient
    /// indicator refresh over the persistent channel) / `"dismiss"`; for `show`/`current`,
    /// `weather` is the structured report — `location_label`, `units`, `current{…}`,
    /// `daily[]`). Byte-identical to the device crate's `types::WEATHER`.
    pub const WEATHER: &str = "anamanti-weather";
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

    /// An `anamanti-interrupt` (barge-in) event: data-less, sent device →
    /// orchestrator to abort the in-flight reply.
    pub fn interrupt() -> Self {
        Self::new(types::INTERRUPT)
    }

    /// True if this is an `anamanti-interrupt` (barge-in) event.
    pub fn is_interrupt(&self) -> bool {
        self.event_type == types::INTERRUPT
    }

    /// An `anamanti-timer` **start** action (orchestrator → device): begin a timer
    /// for `duration_secs` with an optional spoken `label` ("pasta"). The device
    /// assigns the timer's id and owns its countdown/alarm.
    pub fn timer_start(duration_secs: u64, label: Option<&str>) -> Self {
        Self::with_data(
            types::TIMER,
            json!({
                "action": "start",
                "duration_secs": duration_secs,
                "label": label,
            }),
        )
    }

    /// An `anamanti-timer` **cancel** action (orchestrator → device): cancel timers
    /// matching `label`, or *all* timers when `label` is `None`.
    pub fn timer_cancel(label: Option<&str>) -> Self {
        Self::with_data(
            types::TIMER,
            json!({
                "action": "cancel",
                "label": label,
            }),
        )
    }

    /// True if this is an `anamanti-timer` device-action frame.
    pub fn is_timer(&self) -> bool {
        self.event_type == types::TIMER
    }

    /// An `anamanti-recipe` **show** action (orchestrator → device): render `recipe`
    /// (a serialized [`crate::recipe::Recipe`]) on the recipe-mode screen.
    pub fn recipe(recipe: Value) -> Self {
        Self::with_data(types::RECIPE, json!({ "action": "show", "recipe": recipe }))
    }

    /// An `anamanti-recipe` **dismiss** action (orchestrator → device): close the
    /// recipe screen and return to the idle/ambient display.
    pub fn recipe_dismiss() -> Self {
        Self::with_data(types::RECIPE, json!({ "action": "dismiss" }))
    }

    /// An `anamanti-recipe` **navigate** action (orchestrator → device): switch the
    /// recipe screen to the `target` tab (`"overview"` / `"ingredients"` / `"steps"`)
    /// by voice, mirroring a touch of the bottom tab bar.
    pub fn recipe_navigate(target: &str) -> Self {
        Self::with_data(
            types::RECIPE,
            json!({ "action": "navigate", "target": target }),
        )
    }

    /// An `anamanti-recipe` **scroll** action (orchestrator → device): scroll the
    /// active recipe pane in `direction` (`"up"` / `"down"` a page, or `"top"` /
    /// `"bottom"`) by voice.
    pub fn recipe_scroll(direction: &str) -> Self {
        Self::with_data(
            types::RECIPE,
            json!({ "action": "scroll", "direction": direction }),
        )
    }

    /// True if this is an `anamanti-recipe` device-action frame.
    pub fn is_recipe(&self) -> bool {
        self.event_type == types::RECIPE
    }

    /// An `anamanti-weather` **show** action (orchestrator → device): render `weather`
    /// (a serialized [`crate::weather::WeatherReport`]) full-screen on the weather
    /// screen. Voice-triggered by the `weather_lookup` tool.
    pub fn weather_show(weather: Value) -> Self {
        Self::with_data(
            types::WEATHER,
            json!({ "action": "show", "weather": weather }),
        )
    }

    /// An `anamanti-weather` **current** refresh (orchestrator → device): update the
    /// ambient indicator (icon + temperature beside the clock) without opening the
    /// full screen. Pushed periodically over the persistent channel.
    pub fn weather_current(weather: Value) -> Self {
        Self::with_data(
            types::WEATHER,
            json!({ "action": "current", "weather": weather }),
        )
    }

    /// An `anamanti-weather` **dismiss** action (orchestrator → device): close the
    /// full-screen weather view and return to the idle/ambient display.
    pub fn weather_dismiss() -> Self {
        Self::with_data(types::WEATHER, json!({ "action": "dismiss" }))
    }

    /// True if this is an `anamanti-weather` frame.
    pub fn is_weather(&self) -> bool {
        self.event_type == types::WEATHER
    }

    /// An `anamanti-speak` request (device → orchestrator): please synthesize `text`
    /// and stream its audio back. Mirror of the device crate's `speak` constructor.
    pub fn speak(text: impl Into<String>) -> Self {
        Self::with_data(types::SPEAK, json!({ "text": text.into() }))
    }

    /// Extract the text from an `anamanti-speak` request's `data.text`.
    pub fn speak_text(&self) -> Option<&str> {
        if self.event_type == types::SPEAK {
            self.data.get("text").and_then(Value::as_str)
        } else {
            None
        }
    }

    /// An `anamanti-listen` follow-up-listen frame (orchestrator → device): after a
    /// reply, ask the device to reopen the mic (no wake word) for a fresh turn once the
    /// reply audio drains. `depth` is the chain depth the follow-up turn will carry
    /// (bounded by `follow_up.max_chain`). `wait_secs` is how long the mic should stay
    /// open for input before the assistant sleeps — longer after a question — which the
    /// device echoes back on the follow-up `audio-start` so this orchestrator sizes that
    /// turn's no-speech window to match.
    pub fn listen(depth: u32, wait_secs: u32) -> Self {
        Self::with_data(
            types::LISTEN,
            json!({ "depth": depth, "wait_secs": wait_secs }),
        )
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

    /// An `anamanti-hello` channel-open frame (device → orchestrator): register the
    /// persistent notify channel. Mirror of the device crate's `hello` constructor.
    pub fn hello(device_id: impl Into<String>, instance_id: impl Into<String>) -> Self {
        Self::with_data(
            types::ANAMANTI_HELLO,
            json!({
                "role": "notify",
                "device_id": device_id.into(),
                "instance_id": instance_id.into(),
            }),
        )
    }

    /// An `anamanti-hello` frame opening the persistent **weather** channel (device →
    /// orchestrator): same frame as [`hello`](Self::hello) but with `role = "weather"`
    /// so the server registers it with the weather push service rather than the notify
    /// service. Byte-identical to the device crate's `hello_weather`.
    pub fn hello_weather(device_id: impl Into<String>, instance_id: impl Into<String>) -> Self {
        Self::with_data(
            types::ANAMANTI_HELLO,
            json!({
                "role": "weather",
                "device_id": device_id.into(),
                "instance_id": instance_id.into(),
            }),
        )
    }

    /// The `device_id` from an `anamanti-hello` frame's `data.device_id`.
    pub fn hello_device_id(&self) -> Option<&str> {
        if self.event_type == types::ANAMANTI_HELLO {
            self.data.get("device_id").and_then(Value::as_str)
        } else {
            None
        }
    }

    /// The `role` from an `anamanti-hello` frame's `data.role` (`"notify"` /
    /// `"weather"`); defaults to `"notify"` when absent, for back-compatibility.
    pub fn hello_role(&self) -> &str {
        if self.event_type == types::ANAMANTI_HELLO {
            self.data
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("notify")
        } else {
            "notify"
        }
    }

    /// An `anamanti-notify` push (orchestrator → device): a proactive notification to
    /// display. Byte-identical to the device crate's `notify` constructor.
    pub fn notify(
        id: impl Into<String>,
        priority: impl Into<String>,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self::with_data(
            types::ANAMANTI_NOTIFY,
            json!({
                "id": id.into(),
                "priority": priority.into(),
                "title": title.into(),
                "body": body.into(),
            }),
        )
    }

    /// An `anamanti-notify-ack` frame (device → orchestrator). Reserved for a later
    /// delivery-tracking phase; provided now so both crates share the constructor.
    pub fn notify_ack(id: impl Into<String>, result: impl Into<String>) -> Self {
        Self::with_data(
            types::ANAMANTI_NOTIFY_ACK,
            json!({ "id": id.into(), "result": result.into() }),
        )
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

/// Pull the follow-up chain depth out of an `audio-start` data block. The device
/// stamps `followup: true` + `followup_depth: <n>` on a turn it opened in response
/// to an `anamanti-listen` frame (no wake word); an ordinary wake-word turn omits
/// both and reads back as `0`. See `plans/Plan.MD` (Follow-up listening).
pub fn followup_depth(data: &Value) -> u32 {
    let obj = match data.as_object() {
        Some(o) => o,
        None => return 0,
    };
    if !obj
        .get("followup")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return 0;
    }
    obj.get("followup_depth")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32
}

/// Pull the follow-up listen window (seconds) the device echoed on an `audio-start`
/// (the `wait_secs` it received in the triggering `anamanti-listen`). `0` when absent —
/// an ordinary wake-word turn, or a device that didn't stamp it — so the caller falls
/// back to its default no-speech window. See `plans/Plan.MD` (Follow-up listening).
pub fn followup_wait_secs(data: &Value) -> u32 {
    data.as_object()
        .and_then(|o| o.get("wait_secs"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32
}

/// What the display is currently showing, as the device stamps it in the `screen`
/// block of each `audio-start`. It lets the model know what is on screen so it can
/// **drive it by voice** with the matching tool. This is a **general, extensible**
/// concept: each device screen that wants voice control reports its own context here,
/// discriminated by the `screen.kind` string on the wire. Today only the recipe screen
/// is implemented; music / weather / photo screens would add their own variants (and a
/// device-side setter + a Core-side prompt line) without touching the transport.
///
/// See `plans/architecture.md` §4 ("Display context") and `plans/RecipePlan.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayContext {
    /// The guided-recipe screen is up (`kind: "recipe"`).
    Recipe(RecipeScreen),
}

/// The recipe screen's state, as carried in a [`DisplayContext::Recipe`]. Tells the
/// model which tab is showing and whether the pane is scrolled to the top/bottom, so it
/// can drive the screen with `recipe_control` (switch tab / scroll) or `close_recipe`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecipeScreen {
    pub title: String,
    /// Active tab: `"overview"` / `"ingredients"` / `"steps"`.
    pub tab: String,
    pub at_top: bool,
    pub at_bottom: bool,
    pub ingredient_count: u32,
    pub step_count: u32,
}

/// Pull the display context out of an `audio-start` data block's `screen` object, or
/// `None` when the display reports nothing (an idle screen, or a device that doesn't
/// stamp context). Dispatches on `screen.kind`; an unknown kind (e.g. a newer device
/// reporting a screen this Core doesn't understand yet) yields `None`.
pub fn display_context(data: &Value) -> Option<DisplayContext> {
    let screen = data.as_object()?.get("screen")?.as_object()?;
    match screen.get("kind").and_then(Value::as_str)? {
        "recipe" => {
            parse_recipe_screen(screen.get("recipe")?.as_object()?).map(DisplayContext::Recipe)
        }
        _ => None,
    }
}

/// Parse the `recipe` sub-object of a `screen` block into a [`RecipeScreen`].
fn parse_recipe_screen(recipe: &Map<String, Value>) -> Option<RecipeScreen> {
    let s = |k: &str| {
        recipe
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let b = |k: &str, dflt: bool| recipe.get(k).and_then(Value::as_bool).unwrap_or(dflt);
    let n = |k: &str| recipe.get(k).and_then(Value::as_u64).unwrap_or(0) as u32;
    let tab = recipe
        .get("tab")
        .and_then(Value::as_str)
        .unwrap_or("overview")
        .to_string();
    Some(RecipeScreen {
        title: s("title"),
        tab,
        at_top: b("at_top", true),
        at_bottom: b("at_bottom", false),
        ingredient_count: n("ingredient_count"),
        step_count: n("step_count"),
    })
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
    async fn timer_start_and_cancel_roundtrip() {
        let start = WyomingEvent::timer_start(300, Some("pasta"));
        let back = roundtrip(&start).await;
        assert_eq!(back, start);
        assert!(back.is_timer());
        assert_eq!(back.data["action"], json!("start"));
        assert_eq!(back.data["duration_secs"], json!(300));
        assert_eq!(back.data["label"], json!("pasta"));

        // Cancel-all carries a null label.
        let cancel_all = WyomingEvent::timer_cancel(None);
        let back = roundtrip(&cancel_all).await;
        assert_eq!(back.data["action"], json!("cancel"));
        assert_eq!(back.data["label"], Value::Null);
    }

    #[tokio::test]
    async fn recipe_show_and_dismiss_roundtrip() {
        let recipe = json!({
            "title": "Carbonara",
            "ingredients": ["spaghetti", "eggs"],
            "steps": ["boil", "toss"],
        });
        let show = WyomingEvent::recipe(recipe.clone());
        let back = roundtrip(&show).await;
        assert_eq!(back, show);
        assert!(back.is_recipe());
        assert_eq!(back.data["action"], json!("show"));
        assert_eq!(back.data["recipe"], recipe);

        let dismiss = WyomingEvent::recipe_dismiss();
        let back = roundtrip(&dismiss).await;
        assert_eq!(back, dismiss);
        assert_eq!(back.data["action"], json!("dismiss"));
    }

    #[tokio::test]
    async fn weather_show_current_and_dismiss_roundtrip() {
        let weather = json!({
            "location_label": "Austin, Texas",
            "units": "imperial",
            "current": { "temp": 72, "weather_code": 2, "is_day": true },
            "daily": [{ "date": "2026-09-25", "high": 80, "low": 60 }],
        });
        let show = WyomingEvent::weather_show(weather.clone());
        let back = roundtrip(&show).await;
        assert_eq!(back, show);
        assert!(back.is_weather());
        assert_eq!(back.data["action"], json!("show"));
        assert_eq!(back.data["weather"], weather);

        let current = WyomingEvent::weather_current(weather.clone());
        let back = roundtrip(&current).await;
        assert_eq!(back, current);
        assert_eq!(back.data["action"], json!("current"));

        let dismiss = WyomingEvent::weather_dismiss();
        let back = roundtrip(&dismiss).await;
        assert_eq!(back, dismiss);
        assert_eq!(back.data["action"], json!("dismiss"));
    }

    #[tokio::test]
    async fn weather_hello_carries_role() {
        let hello = WyomingEvent::hello_weather("dev-1", "core-1");
        let back = roundtrip(&hello).await;
        assert_eq!(back, hello);
        assert_eq!(back.hello_role(), "weather");
        assert_eq!(back.hello_device_id(), Some("dev-1"));
        // The default notify hello reports the notify role.
        assert_eq!(WyomingEvent::hello("d", "c").hello_role(), "notify");
    }

    #[tokio::test]
    async fn recipe_navigate_and_scroll_roundtrip() {
        let nav = WyomingEvent::recipe_navigate("ingredients");
        let back = roundtrip(&nav).await;
        assert_eq!(back, nav);
        assert!(back.is_recipe());
        assert_eq!(back.data["action"], json!("navigate"));
        assert_eq!(back.data["target"], json!("ingredients"));

        let scroll = WyomingEvent::recipe_scroll("down");
        let back = roundtrip(&scroll).await;
        assert_eq!(back, scroll);
        assert_eq!(back.data["action"], json!("scroll"));
        assert_eq!(back.data["direction"], json!("down"));
    }

    #[test]
    fn display_context_reads_audio_start_screen_block_only() {
        // A plain audio-start (no `screen` block) → no display context.
        let plain = WyomingEvent::audio_start(AudioFormat::PCM_16K_MONO, 0);
        assert_eq!(display_context(&plain.data), None);

        // A device with the recipe screen up stamps the `screen` block.
        let marked = json!({
            "rate": 16_000, "width": 2, "channels": 1, "timestamp": 0,
            "screen": {
                "kind": "recipe",
                "recipe": {
                    "title": "Carbonara",
                    "tab": "steps",
                    "at_top": false,
                    "at_bottom": true,
                    "ingredient_count": 6,
                    "step_count": 4,
                },
            },
        });
        assert_eq!(
            display_context(&marked),
            Some(DisplayContext::Recipe(RecipeScreen {
                title: "Carbonara".to_string(),
                tab: "steps".to_string(),
                at_top: false,
                at_bottom: true,
                ingredient_count: 6,
                step_count: 4,
            }))
        );

        // An unknown screen kind (a screen this Core doesn't understand yet) is ignored.
        let other = json!({ "screen": { "kind": "weather" } });
        assert_eq!(display_context(&other), None);
    }

    #[tokio::test]
    async fn speak_frame_roundtrips_and_extracts_text() {
        let ev = WyomingEvent::speak("Time's up for pasta");
        let back = roundtrip(&ev).await;
        assert_eq!(back, ev);
        assert_eq!(back.event_type, types::SPEAK);
        assert_eq!(back.speak_text(), Some("Time's up for pasta"));
        // A non-speak frame yields no text.
        assert_eq!(WyomingEvent::audio_stop(0).speak_text(), None);
    }

    #[tokio::test]
    async fn listen_frame_roundtrips_and_carries_depth_and_wait() {
        let ev = WyomingEvent::listen(2, 10);
        let back = roundtrip(&ev).await;
        assert_eq!(back, ev);
        assert_eq!(back.event_type, types::LISTEN);
        assert_eq!(back.data["depth"], json!(2));
        assert_eq!(back.data["wait_secs"], json!(10));
    }

    #[test]
    fn followup_depth_reads_marked_audio_start_only() {
        // A plain wake-word audio-start has no follow-up marker → depth 0.
        let plain = WyomingEvent::audio_start(AudioFormat::PCM_16K_MONO, 0);
        assert_eq!(followup_depth(&plain.data), 0);

        // A device follow-up turn stamps `followup: true` + `followup_depth` + `wait_secs`.
        let marked = json!({
            "rate": 16_000, "width": 2, "channels": 1, "timestamp": 0,
            "followup": true, "followup_depth": 3, "wait_secs": 5,
        });
        assert_eq!(followup_depth(&marked), 3);
        assert_eq!(followup_wait_secs(&marked), 5);

        // `followup: false` (or absent) ignores any stray depth.
        let disabled = json!({ "followup": false, "followup_depth": 3 });
        assert_eq!(followup_depth(&disabled), 0);
        // No `wait_secs` on a plain turn → 0 (caller uses its default window).
        assert_eq!(followup_wait_secs(&plain.data), 0);
    }

    #[tokio::test]
    async fn notify_frames_roundtrip() {
        // hello (device → orchestrator)
        let hello = WyomingEvent::hello("echo-show-8", "Paul Family");
        let back = roundtrip(&hello).await;
        assert_eq!(back, hello);
        assert_eq!(back.event_type, types::ANAMANTI_HELLO);
        assert_eq!(back.hello_device_id(), Some("echo-show-8"));
        assert_eq!(back.data["role"], json!("notify"));
        // A non-hello frame yields no device id.
        assert_eq!(WyomingEvent::audio_stop(0).hello_device_id(), None);

        // notify (orchestrator → device)
        let note = WyomingEvent::notify("42-0", "info", "Reminder", "Meeting in 5 minutes");
        let back = roundtrip(&note).await;
        assert_eq!(back, note);
        assert_eq!(back.event_type, types::ANAMANTI_NOTIFY);
        assert_eq!(back.data["id"], json!("42-0"));
        assert_eq!(back.data["priority"], json!("info"));
        assert_eq!(back.data["title"], json!("Reminder"));
        assert_eq!(back.data["body"], json!("Meeting in 5 minutes"));

        // ack (device → orchestrator), reserved for a later phase
        let ack = WyomingEvent::notify_ack("42-0", "shown");
        let back = roundtrip(&ack).await;
        assert_eq!(back, ack);
        assert_eq!(back.event_type, types::ANAMANTI_NOTIFY_ACK);
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
