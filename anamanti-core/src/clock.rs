//! Wall-clock helpers shared by the persisted records (chat/prompt logs, memory
//! rows, speaker profiles, voice clips).

use std::time::{SystemTime, UNIX_EPOCH};

/// Current Unix time in whole seconds (`0` if the clock is before the epoch).
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Current Unix time in milliseconds (`0` if the clock is before the epoch).
pub fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
