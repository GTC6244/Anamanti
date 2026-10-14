//! The Wyoming Protocol wire format shared by the Anamanti Core (Mac) and the
//! Anamanti Display device engine (Plan.MD §3 "Wire format"; architecture.md §4).
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
//! This matches the reference `rhasspy/wyoming` implementation, so both sides
//! interoperate with off-the-shelf Wyoming STT/TTS servers. The Core uses the codec
//! in *both* directions: as a Wyoming **server** to the device and as a Wyoming
//! **client** to the downstream Whisper (STT) and Piper (TTS) services; the device
//! uses it as a client to the Core.
//!
//! Before this crate existed the codec was implemented twice (once per crate) and kept
//! wire-identical by convention. It now lives here once, and both crates re-export it
//! from their own `wyoming::protocol` module so existing import paths are unchanged.
//! The golden-bytes tests below pin the exact on-the-wire encoding.
//!
//! The reader is **bounded** ([`MAX_HEADER_LINE_BYTES`], [`MAX_DATA_BYTES`],
//! [`MAX_PAYLOAD_BYTES`]): a peer cannot make either side allocate an arbitrary amount
//! of memory by lying in a header.

use std::io;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Wyoming event `type` names. Kept as constants so neither side ever trips over a
/// typo in a string literal.
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
    /// STT/VAD → client: server-side VAD detected the start of speech.
    pub const VOICE_STARTED: &str = "voice-started";
    /// STT/VAD → client: server-side VAD detected the end of speech.
    pub const VOICE_STOPPED: &str = "voice-stopped";
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
    /// orchestrator must abort the in-flight LLM generation + TTS synthesis for this
    /// turn at once (no data). The device flushes its own playback locally the
    /// instant this is sent.
    pub const INTERRUPT: &str = "anamanti-interrupt";

    // ---- Phase 6: project-local settings + memory control frames ----
    //
    // These ride the same Wyoming framing on the device↔orchestrator hop only (no
    // off-the-shelf Wyoming server ever sees them). They let the on-device settings
    // screen read/change the orchestrator's runtime LLM backend + TTS voice and
    // view/delete persistent memory entries.

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

    // ---- GraphRAG (HelixDB) memory browsing control frames ----
    //
    // Let the on-device Memory screen inspect and prune the derived GraphRAG graph —
    // the `Turn` nodes and `Entity` ("keyword") nodes that back recall — separately from
    // the durable SQLite facts above. Deletes here are Helix-index-only (the chat log,
    // the source of truth, is untouched).

    /// device → orchestrator: browse stored conversation turns, newest first (data: all
    /// optional — `query` substring, `speaker_id`, `entity` name, `limit`, `offset`).
    pub const LIST_TURNS: &str = "anamanti-list-turns";
    /// orchestrator → device: the turns (data: `ok`, `turns` array of
    /// `{id, ext_id, session, ts, transcript, reply, speaker_id, entities:[{name,kind}]}`;
    /// on failure `ok:false` + `message`).
    pub const TURNS: &str = "anamanti-turns";
    /// device → orchestrator: browse stored entities / "keywords" (data: optional
    /// `query` substring, `limit`).
    pub const LIST_ENTITIES: &str = "anamanti-list-entities";
    /// orchestrator → device: the entities (data: `ok`, `entities` array of
    /// `{id, name, kind}`; on failure `ok:false` + `message`).
    pub const ENTITIES: &str = "anamanti-entities";
    /// device → orchestrator: drop one graph node by its id (data: `id`) — a `Turn`,
    /// `Entity`, or graph `Memory` node, with its incident edges + vector entry.
    pub const DELETE_GRAPH_NODE: &str = "anamanti-delete-graph-node";
    /// orchestrator → device: result of a graph browse/delete failure or a delete
    /// (data: `ok`, optional `message`).
    pub const GRAPH_RESULT: &str = "anamanti-graph-result";

    // ---- Speaker identification control frames (speaker_id_plan.md Phase C) ----
    //
    // Let the on-device "People" settings view enumerate identified speakers and
    // name / merge / delete them.

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
    /// device → orchestrator: link a voice to a **household member** (data: `id`,
    /// `member`). The Core resolves `member` against its household roster; if another
    /// voiceprint already belongs to that member, `id` is merged into it (voiceprint,
    /// memories, clips, graph history), otherwise `id` is named as the member. Answered
    /// with `SPEAKER_RESULT` (+ `speaker_id` of the resulting profile, `name`, `merged`).
    pub const LINK_SPEAKER: &str = "anamanti-link-speaker";
    /// orchestrator → device: result of a name/merge/delete (data: `ok`, `message`).
    pub const SPEAKER_RESULT: &str = "anamanti-speaker-result";

    // ---- Voice-clip playback (speaker_id_plan.md Phase C) ----
    //
    // Let the "People" screen play back the raw utterances behind a voiceprint so a
    // human can listen and put a name to an anonymous cluster. The Core keeps a small
    // rolling window of each speaker's recent clips (raw little-endian i16 mono PCM at
    // 16 kHz); the device lists them and fetches the bytes to play locally.

    /// device → orchestrator: list a speaker's stored voice clips (data: `id`).
    pub const LIST_SPEAKER_CLIPS: &str = "anamanti-list-speaker-clips";
    /// orchestrator → device: the clips (data: `ok`, `id`, `clips` array of
    /// `{name, created_ms, samples, sample_rate}`; on failure `ok:false` + `message`).
    pub const SPEAKER_CLIPS: &str = "anamanti-speaker-clips";
    /// device → orchestrator: fetch one clip's raw audio (data: `id`, `clip` = the
    /// clip's `name`).
    pub const GET_SPEAKER_CLIP: &str = "anamanti-get-speaker-clip";
    /// orchestrator → device: one clip (data: `ok`, `id`, `clip`, `sample_rate`,
    /// `samples`; **payload** = raw little-endian i16 mono PCM. On failure `ok:false` +
    /// `message` and no payload).
    pub const SPEAKER_CLIP: &str = "anamanti-speaker-clip";

    // ---- Active voice enrollment (SpeakerEnrollmentPlan.md Phase 2) ----
    //
    // Guided "train a new person" flow. Sample + verify captures ride the ordinary
    // wake-word audio turn, tagged by `screen.kind = "enroll"`, which the orchestrator
    // short-circuits before the LLM — so there is no second audio path. Commit / cancel
    // are control frames (no audio). Every capture/commit/cancel answers with
    // `ENROLL_RESULT`.

    /// device → orchestrator: commit the current enrollment session into a named,
    /// labeled profile (data: `session`, `name`).
    pub const ENROLL_FINISH: &str = "anamanti-enroll-finish";
    /// device → orchestrator: discard an in-flight enrollment session (data:
    /// `session`).
    pub const ENROLL_CANCEL: &str = "anamanti-enroll-cancel";
    /// orchestrator → device: the result of an enrollment step (data: `ok`,
    /// `phase` = `sample`|`verify`|`finish`|`cancel`, plus `accepted`, `samples`,
    /// `consistency`, `confidence`, `speaker_id`, `name`, `message` as applicable).
    pub const ENROLL_RESULT: &str = "anamanti-enroll-result";

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

    /// device → orchestrator: fetch a read-only snapshot of the Core's configured
    /// "functions" (LLM, web search, STT, TTS, directions/weather/places, Spotify,
    /// shopping list, phone messages, calendar, System-1, embeddings) for the device
    /// Settings screen. No data in the request.
    pub const GET_FUNCTIONS: &str = "anamanti-get-functions";
    /// orchestrator → device: the functions snapshot (data: `ok`, `functions` array;
    /// each entry has `category`, `name`, `mode` (`cloud`/`local`/`off`), optional
    /// `provider`/`model`/`note`, and `key_required`/`key_set` booleans). Never carries
    /// secret values — only whether each required key is set, exactly like the config
    /// page.
    pub const FUNCTIONS: &str = "anamanti-functions";

    /// device → orchestrator: link a family member's phone to AppSaid by redeeming a
    /// short pairing code shown in the AppSaid app (data: `name`, `code`). The Core
    /// claims the code against its configured AppSaid worker, learns the phone's
    /// `user_key`, and stores it as a `send_phone_message` recipient under `name`. The
    /// code rides the device↔orchestrator LAN hop only; no secret is returned.
    pub const PAIR_PHONE: &str = "anamanti-pair-phone";
    /// orchestrator → device: the pairing outcome (data: `ok`; on success `name`; on
    /// failure a speakable `message`). Never carries the claimed `user_key`.
    pub const PAIR_RESULT: &str = "anamanti-pair-result";

    // ---- Device-action frames (Phase 2: on-device timers/alarms) ----
    //
    // A tool the LLM calls on the Mac emits a **device action** that the pipeline
    // relays to the Echo Show as this project-local frame. The device owns the
    // action's state (unlimited concurrent timers, the countdown UI, and the alarm
    // sound), so it keeps working if the Mac disconnects.

    /// orchestrator → device: start or cancel a timer (data: `action` =
    /// `"start"`/`"cancel"`; for `start`: `duration_secs` + optional `label`; for
    /// `cancel`: optional `label`, where an absent/null label cancels all timers).
    pub const TIMER: &str = "anamanti-timer";

    /// device → orchestrator: synthesize `text` with Piper and stream the audio
    /// back on the same socket (data: `text`). Lets a fired on-device timer speak its
    /// "Time's up …" announcement in the real assistant voice when the Mac is
    /// reachable (bell-only when offline).
    pub const SPEAK: &str = "anamanti-speak";

    /// orchestrator → device: **follow-up listen**. The assistant's reply was a
    /// question, so the device should reopen the mic and start a fresh turn with no
    /// wake word once the reply audio finishes (data: `depth` = the chain depth the
    /// follow-up turn will carry, `wait_secs` = the listen window). Sent *before* the
    /// turn's final `audio-stop` (the device ends its turn on the first `audio-stop`).
    /// See `plans/Plan.MD` (Follow-up listening).
    pub const LISTEN: &str = "anamanti-listen";

    // ---- Proactive notifications (Approach A: persistent device-dialed channel) ----
    //
    // A long-lived connection the device dials and holds open so the orchestrator can
    // PUSH unsolicited notifications to the display *without* the device starting a
    // voice turn. Distinct from the per-turn voice socket, but rides the same Wyoming
    // framing and the same discovered endpoint.

    /// device → orchestrator: open + register a persistent channel (data: `role` =
    /// "notify" | "weather", `device_id`, optional `instance_id`, `name`). The
    /// orchestrator keeps this connection open and pushes frames down it.
    pub const ANAMANTI_HELLO: &str = "anamanti-hello";
    /// orchestrator → device: a proactive notification to display (data: `id`,
    /// `priority` = "info" | "reminder" | "alert", `title`, `body`). Visual-only.
    pub const ANAMANTI_NOTIFY: &str = "anamanti-notify";
    /// device → orchestrator: acknowledge a notification by `id` (data: `id`,
    /// `result`). Reserved for a later delivery-tracking phase.
    pub const ANAMANTI_NOTIFY_ACK: &str = "anamanti-notify-ack";

    /// orchestrator → device: show / dismiss / drive the recipe-mode screen (data:
    /// `action` = `"show"`/`"dismiss"`/`"navigate"`/`"scroll"`; for `show`, `recipe` is
    /// the structured recipe object — `title`, `summary`, `source_url`, `image_url`,
    /// `servings`, `total_time`, `ingredients[]`, `steps[]`). A device action driven by
    /// the `recipe_lookup` tool; the device owns the screen state until dismissed.
    pub const RECIPE: &str = "anamanti-recipe";

    /// orchestrator → device: show/refresh/dismiss the weather screen (data: `action`
    /// = `"show"` (full-screen forecast, voice-triggered) / `"current"` (ambient
    /// indicator refresh over the persistent channel) / `"dismiss"`; for `show`/`current`,
    /// `weather` is the structured report — `location_label`, `units`, `current{…}`,
    /// `daily[]`).
    pub const WEATHER: &str = "anamanti-weather";

    /// orchestrator → device: show/dismiss the full-screen place card (data: `action`
    /// = `"show"` / `"dismiss"`; for `show`, `place` is the structured report — `name`,
    /// `address`, `hours[]`, `open_now`, `rating`, `phone`, `website`, `photo_uri`, …).
    pub const PLACE: &str = "anamanti-place";

    /// orchestrator → device: adjust the global on-screen font scale (data: `action` =
    /// `"adjust"`, `direction` = `"increase"` / `"decrease"`). A device action driven by
    /// the `adjust_font` tool / System-1 intent when the turn's `screen.font` context says
    /// a resizable surface is on screen.
    pub const FONT: &str = "anamanti-font";

    /// orchestrator → device: the music player (data: `action` = `"now_playing"` (the
    /// `music` object carries `playing`, `track_title`, `artist`, `album`, `artwork_uri`,
    /// `position_secs`, `duration_secs`, `volume_percent`, and a `next_up[]` queue) /
    /// `"dismiss"` (nothing is playing) — both pushed over the persistent `role="music"`
    /// channel — or `"screen"` (`screen` = `"now_playing"`/`"up_next"`/`"hidden"`, a
    /// device action on the per-turn socket driven by the `music_screen` tool)).
    pub const MUSIC: &str = "anamanti-music";

    /// device → orchestrator: a music transport/volume command from the on-screen
    /// controls (data: `action` = `"pause"`/`"resume"`/`"next"`/`"previous"`/`"volume"`,
    /// with `value` the 0–100 percent for `"volume"`). Answered with [`MUSIC_RESULT`].
    pub const MUSIC_CONTROL: &str = "anamanti-music-control";

    /// orchestrator → device: the result of a [`MUSIC_CONTROL`] command (data: `ok`
    /// bool + a short `message`).
    pub const MUSIC_RESULT: &str = "anamanti-music-result";
}

// ---------------------------------------------------------------------------
// Reader bounds (A10)
// ---------------------------------------------------------------------------

/// Largest accepted header line, **including** its `\n` terminator. A real header is
/// `{"type":…,"data_length":N,"payload_length":M}` — well under 200 bytes — so 64 KiB
/// is several hundred times the need while keeping a hostile peer from streaming an
/// endless line into memory.
pub const MAX_HEADER_LINE_BYTES: usize = 64 * 1024;

/// Largest accepted JSON `data` block. The biggest legitimate blocks are list
/// responses on the device↔Core hop (`anamanti-memories` has no row limit,
/// `anamanti-turns` carries up to `limit` turns with full transcript + reply text),
/// recipe show frames (text only — images are URLs), and Piper's `info` capability
/// catalog read by the Core (every voice with its speakers and attribution; a few
/// hundred KiB). 4 MiB leaves ample headroom over all of them.
pub const MAX_DATA_BYTES: usize = 4 * 1024 * 1024;

/// Largest accepted binary payload. Live `audio-chunk`s are a few KiB; the biggest
/// payload sent is an `anamanti-speaker-clip` (one whole stored utterance of 16 kHz
/// mono `i16` PCM — a 30 s turn is ~0.96 MB). 8 MiB is ~4.4 minutes of that format.
pub const MAX_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Audio format
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Decoded device-action commands (device side)
// ---------------------------------------------------------------------------

/// A device-action timer command decoded from an `anamanti-timer` frame (Phase 2).
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

/// A recipe-mode command decoded from an `anamanti-recipe` frame. `Show` carries the
/// recipe object (surfaced to Flutter as a JSON string it parses into the tabs);
/// `Dismiss` closes the screen.
#[derive(Debug, Clone, PartialEq)]
pub enum RecipeCommand {
    /// Show this recipe (the raw `schema.org`-shaped object) on the recipe screen.
    Show(Value),
    /// Dismiss the recipe screen and return to the idle/ambient display.
    Dismiss,
    /// Switch to a tab by voice: `"overview"` / `"ingredients"` / `"steps"`.
    Navigate(String),
    /// Scroll the active pane by voice: `"up"` / `"down"` (a page) or `"top"` / `"bottom"`.
    Scroll(String),
}

/// A font-scaling command decoded from an `anamanti-font` frame. `Adjust` carries the
/// direction (`"increase"` / `"decrease"`); the device applies it to the global font scale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FontCommand {
    /// Step the global font scale in `direction` (`"increase"` / `"decrease"`).
    Adjust(String),
}

/// A weather command decoded from an `anamanti-weather` frame. `Show`/`Current` carry
/// the report object (surfaced to Flutter as a JSON string it parses); `Dismiss`
/// closes the full-screen view. `Show` opens the full screen; `Current` only refreshes
/// the ambient indicator beside the clock.
#[derive(Debug, Clone, PartialEq)]
pub enum WeatherCommand {
    /// Show this report full-screen on the weather screen (voice-triggered).
    Show(Value),
    /// Refresh the ambient indicator (icon + temperature) from this report.
    Current(Value),
    /// Dismiss the full-screen weather view and return to the idle/ambient display.
    Dismiss,
}

/// A place command decoded from an `anamanti-place` frame. `Show` carries the place
/// report object (surfaced to Flutter as a JSON string it parses into the card);
/// `Dismiss` closes the full-screen card.
#[derive(Debug, Clone, PartialEq)]
pub enum PlaceCommand {
    /// Show this place report full-screen on the place card (voice-triggered).
    Show(Value),
    /// Dismiss the full-screen place card and return to the idle/ambient display.
    Dismiss,
}

/// A music command decoded from an `anamanti-music` frame.
#[derive(Debug, Clone, PartialEq)]
pub enum MusicCommand {
    /// Render this now-playing snapshot (track + up-next queue). Persistent channel.
    NowPlaying(Value),
    /// Nothing is playing — close the music UI. Persistent channel.
    Dismiss,
    /// Voice screen command (per-turn socket): which music screen to show —
    /// `"now_playing"` / `"up_next"` / `"hidden"`.
    Screen(String),
}

// ---------------------------------------------------------------------------
// The event
// ---------------------------------------------------------------------------

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

/// The `{rate, width, channels, timestamp}` data block shared by `audio-start` and
/// `audio-chunk`.
fn pcm_data(fmt: AudioFormat, timestamp_ms: u64) -> Value {
    json!({
        "rate": fmt.rate,
        "width": fmt.width_bytes,
        "channels": fmt.channels,
        "timestamp": timestamp_ms,
    })
}

impl WyomingEvent {
    // ---- generic ----

    /// A data-less, payload-less event (e.g. a bare `audio-stop` when no timestamp is
    /// attached).
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

    // ---- audio ----

    /// An `audio-start` event describing the PCM format that follows.
    pub fn audio_start(fmt: AudioFormat, timestamp_ms: u64) -> Self {
        Self::with_data(types::AUDIO_START, pcm_data(fmt, timestamp_ms))
    }

    /// An `audio-start` for a **follow-up** turn — one the device auto-opened after a
    /// question reply, with no wake word. It is the ordinary PCM header plus
    /// `followup: true` + `followup_depth` + `wait_secs`, which the orchestrator reads
    /// (see [`followup_depth`] / [`followup_wait_secs`]) to seed the prompt with recent
    /// history, bound the follow-up chain and size the no-speech window. `depth == 0`
    /// emits a plain `audio-start` (an ordinary wake-word turn, no marker).
    pub fn audio_start_followup(
        fmt: AudioFormat,
        timestamp_ms: u64,
        depth: u32,
        wait_secs: u32,
    ) -> Self {
        let mut ev = Self::audio_start(fmt, timestamp_ms);
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
    pub fn audio_chunk(fmt: AudioFormat, timestamp_ms: u64, pcm: Vec<u8>) -> Self {
        Self {
            event_type: types::AUDIO_CHUNK.to_string(),
            data: pcm_data(fmt, timestamp_ms),
            payload: Some(pcm),
        }
    }

    /// An `audio-stop` event with a timestamp.
    pub fn audio_stop(timestamp_ms: u64) -> Self {
        Self::with_data(types::AUDIO_STOP, json!({ "timestamp": timestamp_ms }))
    }

    // ---- STT / TTS ----

    /// A `transcript` event carrying recognized `text`.
    pub fn transcript(text: impl Into<String>) -> Self {
        Self::with_data(types::TRANSCRIPT, json!({ "text": text.into() }))
    }

    /// True if this is a `transcript` event. Server-side VAD makes this the
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

    // ---- reply streaming / barge-in ----

    /// A `reply-token` event carrying one streamed LLM reply fragment, relayed to the
    /// device so it can render the reply as it is generated (Phase 5).
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

    // ---- speak / follow-up listen ----

    /// An `anamanti-speak` request (device → orchestrator): please synthesize `text`
    /// and stream its audio back.
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
    /// reply audio drains. `depth` is the chain depth the follow-up turn will carry;
    /// `wait_secs` is how long the mic should stay open for input before the assistant
    /// sleeps, which the device echoes back on the follow-up `audio-start`.
    pub fn listen(depth: u32, wait_secs: u32) -> Self {
        Self::with_data(
            types::LISTEN,
            json!({ "depth": depth, "wait_secs": wait_secs }),
        )
    }

    /// Decode an `anamanti-listen` frame as `(depth, wait_secs)` — `depth` defaults to 1
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

    // ---- persistent channels (notify / weather) ----

    /// An `anamanti-hello` channel-open frame (device → orchestrator): register the
    /// persistent notify channel. `name` is a human-friendly label for this display
    /// (may be empty).
    pub fn hello(
        device_id: impl Into<String>,
        instance_id: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self::hello_with_role("notify", device_id, instance_id, name)
    }

    /// An `anamanti-hello` frame opening the persistent **weather** channel (device →
    /// orchestrator): same frame as [`hello`](Self::hello) but with `role = "weather"`
    /// so the orchestrator registers it with the weather push service.
    pub fn hello_weather(
        device_id: impl Into<String>,
        instance_id: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self::hello_with_role("weather", device_id, instance_id, name)
    }

    /// An `anamanti-hello` frame opening the persistent **music** channel (device →
    /// orchestrator): `role = "music"`, so the orchestrator registers it with the
    /// now-playing push service.
    pub fn hello_music(
        device_id: impl Into<String>,
        instance_id: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self::hello_with_role("music", device_id, instance_id, name)
    }

    fn hello_with_role(
        role: &str,
        device_id: impl Into<String>,
        instance_id: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self::with_data(
            types::ANAMANTI_HELLO,
            json!({
                "role": role,
                "device_id": device_id.into(),
                "instance_id": instance_id.into(),
                "name": name.into(),
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

    /// The human-friendly `name` from an `anamanti-hello` frame's `data.name`
    /// (`None` when the frame is a different type or carries no name — older devices
    /// that predate the field).
    pub fn hello_name(&self) -> Option<&str> {
        if self.event_type == types::ANAMANTI_HELLO {
            self.data.get("name").and_then(Value::as_str)
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
    /// display.
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
    /// delivery-tracking phase.
    pub fn notify_ack(id: impl Into<String>, result: impl Into<String>) -> Self {
        Self::with_data(
            types::ANAMANTI_NOTIFY_ACK,
            json!({ "id": id.into(), "result": result.into() }),
        )
    }

    // ---- device actions: timer ----

    /// An `anamanti-timer` **start** action (orchestrator → device): begin a timer for
    /// `duration_secs` with an optional spoken `label` ("pasta"). The device assigns
    /// the timer's id and owns its countdown/alarm.
    pub fn timer_start(duration_secs: u64, label: Option<&str>) -> Self {
        Self::with_data(
            types::TIMER,
            json!({ "action": "start", "duration_secs": duration_secs, "label": label }),
        )
    }

    /// An `anamanti-timer` **cancel** action (orchestrator → device): cancel timers
    /// matching `label`, or *all* timers when `label` is `None`.
    pub fn timer_cancel(label: Option<&str>) -> Self {
        Self::with_data(types::TIMER, json!({ "action": "cancel", "label": label }))
    }

    /// Decode an `anamanti-timer` frame into a [`TimerCommand`], or `None` if this is
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

    // ---- device actions: recipe ----

    /// An `anamanti-recipe` **show** action (orchestrator → device): render `recipe`
    /// (a serialized recipe object) on the recipe-mode screen.
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

    /// Decode an `anamanti-recipe` frame into a [`RecipeCommand`], or `None` if this is
    /// not a recipe frame or its `action`/params are unrecognized. A `show` with no
    /// `recipe` object, a `navigate` with no `target`, or a `scroll` with no
    /// `direction` are all rejected (return `None`).
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
            "navigate" => self
                .data
                .get("target")
                .and_then(Value::as_str)
                .map(|t| RecipeCommand::Navigate(t.to_string())),
            "scroll" => self
                .data
                .get("direction")
                .and_then(Value::as_str)
                .map(|d| RecipeCommand::Scroll(d.to_string())),
            _ => None,
        }
    }

    // ---- device actions: weather ----

    /// An `anamanti-weather` **show** action (orchestrator → device): render `weather`
    /// (a serialized weather report) full-screen. Voice-triggered by the
    /// `weather_lookup` tool.
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

    /// Decode an `anamanti-weather` frame into a [`WeatherCommand`], or `None` if this
    /// is not a weather frame or its `action` is unrecognized. A `show`/`current` with
    /// no `weather` object is rejected (returns `None`).
    pub fn weather_command(&self) -> Option<WeatherCommand> {
        if self.event_type != types::WEATHER {
            return None;
        }
        let report = || self.data.get("weather").filter(|v| v.is_object()).cloned();
        match self.data.get("action").and_then(Value::as_str)? {
            "show" => report().map(WeatherCommand::Show),
            "current" => report().map(WeatherCommand::Current),
            "dismiss" => Some(WeatherCommand::Dismiss),
            _ => None,
        }
    }

    // ---- device actions: place ----

    /// An `anamanti-place` **show** action (orchestrator → device): render `place`
    /// (a serialized place report) full-screen on the place card. Voice-triggered by
    /// the `places_lookup` tool.
    pub fn place_show(place: Value) -> Self {
        Self::with_data(types::PLACE, json!({ "action": "show", "place": place }))
    }

    /// An `anamanti-place` **dismiss** action (orchestrator → device): close the
    /// full-screen place card and return to the idle/ambient display.
    pub fn place_dismiss() -> Self {
        Self::with_data(types::PLACE, json!({ "action": "dismiss" }))
    }

    /// Decode an `anamanti-place` frame into a [`PlaceCommand`], or `None` if this is not
    /// a place frame or its `action` is unrecognized. A `show` with no `place` object is
    /// rejected (returns `None`).
    pub fn place_command(&self) -> Option<PlaceCommand> {
        if self.event_type != types::PLACE {
            return None;
        }
        let report = || self.data.get("place").filter(|v| v.is_object()).cloned();
        match self.data.get("action").and_then(Value::as_str)? {
            "show" => report().map(PlaceCommand::Show),
            "dismiss" => Some(PlaceCommand::Dismiss),
            _ => None,
        }
    }

    // ---- device actions: font ----

    /// An `anamanti-font` **adjust** action (orchestrator → device): step the global
    /// on-screen font scale in `direction` (`"increase"` / `"decrease"`) by voice.
    pub fn font_adjust(direction: &str) -> Self {
        Self::with_data(
            types::FONT,
            json!({ "action": "adjust", "direction": direction }),
        )
    }

    /// Decode an `anamanti-font` frame into a [`FontCommand`], or `None` if this is not a
    /// font frame or its `action`/`direction` is unrecognized (an `adjust` with no
    /// `direction` is rejected).
    pub fn font_command(&self) -> Option<FontCommand> {
        if self.event_type != types::FONT {
            return None;
        }
        match self.data.get("action").and_then(Value::as_str)? {
            "adjust" => self
                .data
                .get("direction")
                .and_then(Value::as_str)
                .map(|d| FontCommand::Adjust(d.to_string())),
            _ => None,
        }
    }

    // ---- music ----

    /// An `anamanti-music` **now_playing** push (orchestrator → device): render `music`
    /// (a serialized now-playing snapshot). Persistent `role="music"` channel.
    pub fn music_now_playing(music: Value) -> Self {
        Self::with_data(
            types::MUSIC,
            json!({ "action": "now_playing", "music": music }),
        )
    }

    /// An `anamanti-music` **dismiss** push (orchestrator → device): nothing is playing.
    pub fn music_dismiss() -> Self {
        Self::with_data(types::MUSIC, json!({ "action": "dismiss" }))
    }

    /// An `anamanti-music` **screen** action (orchestrator → device): show the music
    /// screen `screen` ∈ `"now_playing"` / `"up_next"` / `"hidden"` by voice.
    pub fn music_screen(screen: &str) -> Self {
        Self::with_data(
            types::MUSIC,
            json!({ "action": "screen", "screen": screen }),
        )
    }

    /// Decode an `anamanti-music` frame into a [`MusicCommand`], or `None` if this is not
    /// a music frame or its `action` is unrecognized. A `now_playing` with no `music`
    /// object (or a `screen` with no `screen` field) is rejected.
    pub fn music_command(&self) -> Option<MusicCommand> {
        if self.event_type != types::MUSIC {
            return None;
        }
        let snapshot = || self.data.get("music").filter(|v| v.is_object()).cloned();
        match self.data.get("action").and_then(Value::as_str)? {
            "now_playing" => snapshot().map(MusicCommand::NowPlaying),
            "dismiss" => Some(MusicCommand::Dismiss),
            "screen" => self
                .data
                .get("screen")
                .and_then(Value::as_str)
                .map(|s| MusicCommand::Screen(s.to_string())),
            _ => None,
        }
    }

    /// An `anamanti-music-control` command (device → orchestrator): a transport/volume
    /// action from the on-screen controls. `value` is the 0–100 percent for `"volume"`.
    pub fn music_control(action: impl Into<String>, value: Option<i64>) -> Self {
        Self::with_data(
            types::MUSIC_CONTROL,
            json!({ "action": action.into(), "value": value }),
        )
    }

    /// Decode an `anamanti-music-control` frame into `(action, value)`; `None` for a
    /// frame of another type or with no `action`.
    pub fn music_control_command(&self) -> Option<(String, Option<i64>)> {
        if self.event_type != types::MUSIC_CONTROL {
            return None;
        }
        let action = self.data.get("action").and_then(Value::as_str)?.to_string();
        let value = self.data.get("value").and_then(Value::as_i64);
        Some((action, value))
    }

    /// An `anamanti-music-result` response (orchestrator → device): the outcome of a
    /// [`music_control`](Self::music_control) command.
    pub fn music_result(ok: bool, message: impl Into<String>) -> Self {
        Self::with_data(
            types::MUSIC_RESULT,
            json!({ "ok": ok, "message": message.into() }),
        )
    }

    // ---- encoding ----

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

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

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

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Reject a declared block length over `max` **before** anything is allocated.
fn check_len(what: &str, len: usize, max: usize) -> io::Result<()> {
    if len > max {
        Err(invalid(format!(
            "Wyoming {what} of {len} bytes exceeds the {max}-byte limit"
        )))
    } else {
        Ok(())
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
///
/// Bounded: a header line longer than [`MAX_HEADER_LINE_BYTES`], a `data_length`
/// over [`MAX_DATA_BYTES`] or a `payload_length` over [`MAX_PAYLOAD_BYTES`] fails
/// with [`io::ErrorKind::InvalidData`] without allocating the oversize buffer. The
/// stream is not resynchronizable after such an error; callers drop the connection.
pub async fn read_event<R>(reader: &mut R) -> io::Result<Option<WyomingEvent>>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = String::new();
    let n = (&mut *reader)
        .take(MAX_HEADER_LINE_BYTES as u64)
        .read_line(&mut line)
        .await?;
    if n == 0 {
        return Ok(None); // clean EOF at a frame boundary
    }
    if n == MAX_HEADER_LINE_BYTES && !line.ends_with('\n') {
        return Err(invalid(format!(
            "Wyoming header line exceeds the {MAX_HEADER_LINE_BYTES}-byte limit"
        )));
    }

    let header: Header = serde_json::from_str(line.trim_end_matches(['\r', '\n']))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    // Validate both declared lengths up front so an oversize payload is rejected
    // before the data block is even read.
    if let Some(len) = header.data_length {
        check_len("data block", len, MAX_DATA_BYTES)?;
    }
    if let Some(len) = header.payload_length {
        check_len("payload", len, MAX_PAYLOAD_BYTES)?;
    }

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
/// and any caller that already has the framed bytes buffered. A `&[u8]` is itself
/// an `AsyncBufRead`, so this reuses the streaming reader path verbatim; the
/// buffered and streamed decoders can never diverge.
pub async fn decode(bytes: &[u8]) -> io::Result<Option<WyomingEvent>> {
    read_event(&mut &bytes[..]).await
}

// ---------------------------------------------------------------------------
// Data-block helpers
// ---------------------------------------------------------------------------

/// Pull the `(rate, width, channels)` PCM format out of an `audio-*` data block.
pub fn audio_format(data: &Value) -> Option<AudioFormat> {
    let obj: &Map<String, Value> = data.as_object()?;
    Some(AudioFormat {
        rate: obj.get("rate")?.as_u64()? as u32,
        width_bytes: obj.get("width")?.as_u64()? as u16,
        channels: obj.get("channels")?.as_u64()? as u16,
    })
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

#[cfg(test)]
mod tests;
