//! A generic, TTL-bounded cache for tool-call responses.
//!
//! Some tool calls are pure lookups whose answer is a function of their outgoing
//! arguments and is stable for a while (weather, geocoding, directions). Repeating the
//! upstream request for the same arguments within that window is wasted latency — the
//! cold vs. warm weather fetch (1261 ms → 278 ms) is exactly this. [`ToolCache`] caches
//! the **outgoing call data → response** so a repeated call inside its TTL is served
//! locally.
//!
//! It is deliberately generic in two ways:
//! - **Response type:** responses are stored as opaque [`serde_json::Value`], so any tool
//!   can share one cache regardless of its response type.
//! - **TTL:** freshness is **per entry**, chosen by the caller at `put` time from the
//!   process-wide [`ToolCacheConfig`] keyed by the tool's logical name — so weather can be
//!   cached for an hour while a different tool uses minutes, all in one shared cache. TTLs
//!   are set in the `tool_cache` config block (see [`ToolCacheConfig`]); `0` disables
//!   caching for that tool.
//!
//! The first application is weather (`weather::from_config` wraps its provider). Other
//! read-only tools can opt in at their call site.
//!
//! **Do NOT cache** mutating or side-effecting tools (timers, shopping-list writes), or
//! results that depend on hidden/per-user state not present in the key. Caching is opt-in
//! per call site precisely so this stays a deliberate choice.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

/// Default cap on distinct cached entries. Weather keys on (place, units), so a household
/// touches a handful; the bound just stops an unbounded key space (many places) from
/// growing without limit. Eviction is oldest-first (see [`ToolCache::put`]).
const DEFAULT_MAX_ENTRIES: usize = 256;

/// The built-in weather TTL (60 minutes) used when the `tool_cache` config omits it.
/// Weather changes slowly; a manual re-ask still reflects reality within the hour.
pub const DEFAULT_WEATHER_TTL_SECS: u64 = 3600;

/// A thread-safe cache of tool-call responses keyed by a string derived from the tool's
/// outgoing arguments. Each entry carries its own TTL (chosen per tool). Cheap to share
/// behind an `Arc`.
pub struct ToolCache {
    max_entries: usize,
    entries: Mutex<HashMap<String, Entry>>,
}

struct Entry {
    value: Value,
    stored: Instant,
    ttl: Duration,
}

impl Default for ToolCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolCache {
    /// An empty cache. Freshness is supplied per entry at [`Self::put`] time.
    pub fn new() -> Self {
        Self {
            max_entries: DEFAULT_MAX_ENTRIES,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Build a stable cache key from a tool name + its serializable outgoing arguments.
    /// Returns `None` if the arguments can't be serialized, in which case the caller
    /// should simply skip the cache (correctness over caching). The `\u{1}` separator
    /// can't appear in the JSON, so distinct (tool, args) never collide.
    pub fn key<A: Serialize>(tool: &str, args: &A) -> Option<String> {
        serde_json::to_string(args)
            .ok()
            .map(|args| format!("{tool}\u{1}{args}"))
    }

    /// The fresh cached value for `key`, or `None` if absent or past its own TTL. An
    /// expired entry is dropped on access so stale data never lingers.
    pub fn get(&self, key: &str) -> Option<Value> {
        let mut entries = self.entries.lock().unwrap();
        match entries.get(key) {
            Some(entry) if entry.stored.elapsed() < entry.ttl => Some(entry.value.clone()),
            Some(_) => {
                entries.remove(key);
                None
            }
            None => None,
        }
    }

    /// Store `value` under `key`, fresh for `ttl`, stamped now. A `ttl` of zero is a
    /// no-op — that's how a tool opts out of caching (config TTL `0`). If at capacity and
    /// this is a new key, the oldest entry is evicted first (not a hot path, so a linear
    /// scan is fine).
    pub fn put(&self, key: String, value: Value, ttl: Duration) {
        if ttl.is_zero() {
            return;
        }
        let mut entries = self.entries.lock().unwrap();
        if entries.len() >= self.max_entries && !entries.contains_key(&key) {
            if let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, e)| e.stored)
                .map(|(k, _)| k.clone())
            {
                entries.remove(&oldest);
            }
        }
        entries.insert(
            key,
            Entry {
                value,
                stored: Instant::now(),
                ttl,
            },
        );
    }

    /// Typed convenience over [`Self::get`]: return the fresh value deserialized to `T`,
    /// or `None` if absent, expired, or it fails to deserialize.
    pub fn get_as<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        self.get(key).and_then(|v| serde_json::from_value(v).ok())
    }

    /// Typed convenience over [`Self::put`]: serialize `value` and store it for `ttl`. A
    /// value that can't be serialized is silently not cached (the call still returns it).
    pub fn put_as<T: Serialize>(&self, key: String, value: &T, ttl: Duration) {
        if ttl.is_zero() {
            return;
        }
        if let Ok(v) = serde_json::to_value(value) {
            self.put(key, v, ttl);
        }
    }

    /// Number of entries currently held (fresh or not). For tests/metrics.
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    /// Whether the cache holds no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Per-tool cache TTLs, keyed by the tool's logical name (e.g. `"weather_lookup"`). This
/// is what makes caching **configurable by type of tool call**: the `tool_cache` block in
/// the config file supplies `{ "<tool>": <seconds> }`, overlaid on the built-in defaults.
/// A tool with no configured TTL (and no default) is **not cached**; an explicit `0`
/// disables caching for a tool that would otherwise default on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCacheConfig {
    ttl_secs: HashMap<String, u64>,
}

impl Default for ToolCacheConfig {
    fn default() -> Self {
        let mut ttl_secs = HashMap::new();
        ttl_secs.insert("weather_lookup".to_string(), DEFAULT_WEATHER_TTL_SECS);
        Self { ttl_secs }
    }
}

impl ToolCacheConfig {
    /// The TTL for `tool`, or zero (⇒ not cached) when unset.
    pub fn ttl(&self, tool: &str) -> Duration {
        Duration::from_secs(self.ttl_secs.get(tool).copied().unwrap_or(0))
    }

    /// Overlay per-tool TTLs from the config file onto these defaults (file entries win,
    /// unmentioned tools keep their default). An entry of `0` disables that tool's cache.
    pub fn overlay(&mut self, entries: HashMap<String, u64>) {
        self.ttl_secs.extend(entries);
    }
}

/// The process-wide per-tool TTL config. Set once at boot from the parsed config
/// (`cache::set_config`); reads before that (e.g. in tests) see the built-in defaults.
static CONFIG: OnceLock<ToolCacheConfig> = OnceLock::new();

/// Install the per-tool cache TTLs parsed from the config file. Call once, early in boot,
/// before any tool fetch. A second call is ignored (first wins) — the config is immutable
/// for the process lifetime.
pub fn set_config(cfg: ToolCacheConfig) {
    if CONFIG.set(cfg).is_err() {
        log::warn!("tool cache config already set; ignoring later set_config");
    }
}

/// The active per-tool TTL config (defaults until [`set_config`] runs).
pub fn config() -> &'static ToolCacheConfig {
    CONFIG.get_or_init(ToolCacheConfig::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, serde::Deserialize, PartialEq, Debug, Clone)]
    struct Resp {
        label: String,
        temp: i32,
    }

    fn resp(temp: i32) -> Resp {
        Resp {
            label: "Austin".into(),
            temp,
        }
    }

    const MIN: Duration = Duration::from_secs(60);

    #[test]
    fn hit_within_ttl_and_key_is_argument_sensitive() {
        let cache = ToolCache::new();
        let k_f = ToolCache::key("weather_lookup", &("Austin", true)).unwrap();
        let k_c = ToolCache::key("weather_lookup", &("Austin", false)).unwrap();
        assert_ne!(k_f, k_c, "different units must not share a key");

        assert!(cache.get_as::<Resp>(&k_f).is_none(), "cold miss");
        cache.put_as(k_f.clone(), &resp(72), MIN);
        assert_eq!(cache.get_as::<Resp>(&k_f), Some(resp(72)), "warm hit");
        assert!(
            cache.get_as::<Resp>(&k_c).is_none(),
            "other args still miss"
        );
    }

    #[test]
    fn expired_entries_are_dropped_on_access() {
        let cache = ToolCache::new();
        let k = ToolCache::key("t", &"x").unwrap();
        cache.put_as(k.clone(), &resp(1), Duration::from_millis(0)); // zero ttl = no-op
        assert_eq!(cache.len(), 0, "zero-ttl put is not stored");

        // A tiny but non-zero TTL that has already elapsed by read time.
        cache.put(
            k.clone(),
            serde_json::json!({"x":1}),
            Duration::from_nanos(1),
        );
        std::thread::sleep(Duration::from_millis(2));
        assert!(cache.get(&k).is_none(), "expired entry is a miss");
        assert_eq!(cache.len(), 0, "expired entry evicted on access");
    }

    #[test]
    fn capacity_evicts_the_oldest() {
        let mut cache = ToolCache::new();
        cache.max_entries = 2;
        cache.put_as(ToolCache::key("t", &1).unwrap(), &resp(1), MIN);
        cache.put_as(ToolCache::key("t", &2).unwrap(), &resp(2), MIN);
        cache.put_as(ToolCache::key("t", &3).unwrap(), &resp(3), MIN); // evicts key(1)
        assert_eq!(cache.len(), 2);
        assert!(cache.get(&ToolCache::key("t", &1).unwrap()).is_none());
        assert!(cache.get(&ToolCache::key("t", &3).unwrap()).is_some());
    }

    #[test]
    fn config_defaults_weather_to_an_hour_and_overlays_file_entries() {
        let mut cfg = ToolCacheConfig::default();
        assert_eq!(cfg.ttl("weather_lookup"), Duration::from_secs(3600));
        assert_eq!(
            cfg.ttl("directions_lookup"),
            Duration::ZERO,
            "unset ⇒ not cached"
        );

        cfg.overlay(HashMap::from([
            ("directions_lookup".to_string(), 600),
            ("weather_lookup".to_string(), 0), // explicit disable overrides the default
        ]));
        assert_eq!(cfg.ttl("directions_lookup"), Duration::from_secs(600));
        assert_eq!(cfg.ttl("weather_lookup"), Duration::ZERO);
    }
}
