//! The Core's one record-id scheme: `{prefix}{unix_millis}-{counter}`.
//!
//! Chat-log and prompt-log records, speaker profiles (`spk-…`) and voice clips all
//! mint ids this way. The millisecond timestamp keeps ids roughly time-ordered across
//! restarts; the per-minter counter disambiguates ids minted within the same
//! millisecond. Ids are unique per process, not globally.

use std::sync::atomic::{AtomicU64, Ordering};

/// Mints `{prefix}{unix_millis}-{counter}` ids. Cheap to share; lock-free.
#[derive(Debug, Default)]
pub struct IdMinter {
    counter: AtomicU64,
}

impl IdMinter {
    /// A fresh minter whose counter starts at 0.
    pub const fn new() -> Self {
        Self {
            counter: AtomicU64::new(0),
        }
    }

    /// Mint `{prefix}{unix_millis}-{counter}`.
    pub fn mint(&self, prefix: &str) -> String {
        let millis = crate::clock::now_millis();
        format!("{prefix}{millis}-{}", self.next_seq())
    }

    /// The next raw counter value (for callers that build their own stem around a
    /// timestamp they already have, e.g. a moved voice clip keeping its capture time).
    pub fn next_seq(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_prefixed() {
        let m = IdMinter::new();
        let a = m.mint("spk-");
        let b = m.mint("spk-");
        assert_ne!(a, b);
        assert!(a.starts_with("spk-"));
        let plain = m.mint("");
        let (millis, seq) = plain.split_once('-').unwrap();
        assert!(millis.parse::<u128>().is_ok());
        assert_eq!(seq, "2");
    }
}
