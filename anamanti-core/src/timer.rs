//! Device-timer limits shared by every path that starts a timer: the LLM's
//! `set_timer` tool (`llm::rig`) and the System-1 `timer` fast path (`orchestrator`).

/// Hard cap on a timer duration (24 h) so a mis-parsed request can't spin an
/// effectively-infinite timer on the device. The `set_timer` tool clamps to it; the
/// System-1 fast path defers a longer request to System-2.
pub const MAX_TIMER_SECS: u64 = 24 * 60 * 60;
