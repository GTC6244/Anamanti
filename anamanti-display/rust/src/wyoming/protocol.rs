//! Wyoming Protocol wire format (Plan.MD §3, Phase 3; architecture.md §4 "Wire
//! format").
//!
//! The codec — event type, framing, frame-type constants, constructors, the
//! device-action decoders (`timer_command`, `recipe_command`, …) and the bounded
//! reader — lives in the shared `anamanti_protocol` crate (`../anamanti-protocol`),
//! which the Mac-side Anamanti Core uses too, so the two ends cannot drift. It is
//! re-exported here so `crate::wyoming::protocol::…` paths are unchanged.
//!
//! None of these types cross the FRB boundary: `crate::api` never names them, so the
//! generated Dart bindings are unaffected.

pub use anamanti_protocol::*;
