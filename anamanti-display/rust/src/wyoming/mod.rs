//! Wyoming Protocol client core (Plan.MD §3, Phase 3; architecture.md §4–5).
//!
//! The device side of the assistant: discover the Mac Mini's Wyoming service on
//! the LAN, open a `tokio` TCP connection, and drive a single voice turn through
//! the `IDLE → TRIGGERED → STREAMING → CLOSING → IDLE` state machine — streaming
//! captured PCM up and reading `transcript` events back. End-of-speech is decided
//! **server-side** (the STT server's VAD), so the device just streams until told.
//!
//! Layering (each layer is independently unit-testable):
//! - [`protocol`] — the newline-JSON + length-prefixed-payload wire codec.
//! - [`state`] — the pure turn state machine (no IO, no runtime).
//! - [`discovery`] — mDNS browse of `_wyoming._tcp` with a cached fallback.
//! - [`client`] — the `tokio` connection struct + async turn driver that binds
//!   the codec and state machine to a real socket.
//!
//! Phase 5 extends the turn past the transcript into a SPEAKING phase: the driver
//! also reads streamed `reply-token`s and the TTS `audio-start`/`audio-chunk`/
//! `audio-stop` frames, handing decoded PCM to the speaker playback sink, and a
//! wake word arriving mid-turn barges in. The Mac-side STT/LLM/TTS services are
//! Phase 4.

pub mod client;
pub mod control;
pub mod discovery;
pub mod notify;
pub mod protocol;
pub mod state;
pub mod weather;

pub use client::{run_turn, AudioFormat, TurnUpdate, WyomingConnection, DEFAULT_TURN_TIMEOUT};
pub use discovery::{
    discover, discover_all, discover_preferred, resolve, EndpointCache, WyomingEndpoint, CORE_ROLE,
    DEFAULT_DISCOVERY_TIMEOUT, WYOMING_SERVICE_TYPE,
};
pub use protocol::{read_event, write_event, WyomingEvent};
pub use state::{Action, ControlInput, Session, SessionState};
