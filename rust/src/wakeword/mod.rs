//! On-device wake-word detection (Plan.MD §3, Phase 2; architecture.md §2.1).
//!
//! Fully offline: no audio leaves the device until a wake word fires (privacy —
//! architecture.md §6). Inference runs on the engine's low-overhead consumer
//! thread using `tract-onnx`, keeping the memory footprint small enough for the
//! Echo Show's ~1 GB budget.

pub mod detector;

pub use detector::{WakeWordDetector, WakeWordModelPaths};
