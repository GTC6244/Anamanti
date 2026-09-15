//! Ambient Smart Display — Mac Mini assistant orchestrator (Plan.MD Phase 4).
//!
//! The "brain": a Wyoming server the Echo Show discovers over mDNS, wiring
//! downstream Whisper (STT) → a pluggable LLM + persistent SQLite memory → Piper
//! (TTS), and streaming the synthesized reply back to the device. See
//! `architecture.md` §2.3 for the component design and `Plan.MD` Phase 4 for the
//! delivery scope.
//!
//! The library exposes every layer so both the `ambient-orchestrator` binary and
//! the integration tests drive the same code.

pub mod config;
pub mod control;
pub mod discovery;
pub mod llm;
pub mod memory;
pub mod orchestrator;
pub mod server;
pub mod settings;
pub mod wyoming;
