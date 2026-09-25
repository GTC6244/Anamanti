//! mDNS / Zeroconf discovery of the Wyoming host on the LAN
//! (Plan.MD §3, Phase 3 "mDNS discovery of `_wyoming._tcp`"; architecture.md §5).
//!
//! The Echo Show never hardcodes the Mac Mini's IP: it browses `_wyoming._tcp`,
//! resolves the first responder's host/port, and **caches** it so a later
//! reconnect can skip the browse if the network is momentarily quiet (the plan's
//! "cached fallback"; full auto-reconnect/back-off is Phase 5). The IP-based
//! discovery fallback that `agents.md` forbids is a *manual static IP* — a cache
//! of an mDNS-resolved endpoint is explicitly the intended behavior here.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

/// The Wyoming service type. The trailing `.local.` is required by the mDNS
/// browse API.
pub const WYOMING_SERVICE_TYPE: &str = "_wyoming._tcp.local.";

/// The TXT `role` value the orchestrator advertises. Off-the-shelf Whisper (STT)
/// and Piper (TTS) rhasspy servers also advertise `_wyoming._tcp`, so the device
/// filters browse results to orchestrators only — never resolving or connecting
/// to a raw STT/TTS server.
pub const CORE_ROLE: &str = "core";

/// Default time to wait for the first mDNS responder before giving up.
pub const DEFAULT_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// A resolved Wyoming (orchestrator) endpoint: an IP address, its port, the
/// advertised hostname, and the orchestrator's stable selection `key` +
/// human-friendly `name` (from the `instance_id` / `name` TXT records).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WyomingEndpoint {
    pub address: IpAddr,
    pub port: u16,
    pub hostname: String,
    /// Stable selection key (TXT `instance_id`; falls back to the mDNS instance
    /// name). The device persists this to pin a specific orchestrator.
    pub key: String,
    /// Human-friendly display label (TXT `name`; falls back to the instance name).
    pub name: String,
}

impl WyomingEndpoint {
    /// The `SocketAddr` to dial for the Wyoming TCP connection.
    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.address, self.port)
    }
}

/// Strip the trailing `.{service_type}` suffix off a resolved fullname, leaving
/// the bare instance label (used as the fallback key/name when TXT records are
/// absent). E.g. `"Mac Mini._wyoming._tcp.local."` → `"Mac Mini"`.
fn instance_name_of(fullname: &str) -> String {
    fullname
        .strip_suffix(&format!(".{WYOMING_SERVICE_TYPE}"))
        .unwrap_or(fullname)
        .to_string()
}

/// Extract the orchestrator identity (key, name) from a resolved service's TXT
/// records, or `None` if the service is not an orchestrator (missing/mismatched
/// `role` TXT — e.g. a raw Whisper/Piper server).
fn orchestrator_meta(info: &ServiceInfo) -> Option<(String, String)> {
    if info.get_property_val_str("role") != Some(CORE_ROLE) {
        return None;
    }
    let instance = instance_name_of(info.get_fullname());
    let key = info
        .get_property_val_str("instance_id")
        .map(str::to_string)
        .unwrap_or_else(|| instance.clone());
    let name = info
        .get_property_val_str("name")
        .map(str::to_string)
        .unwrap_or(instance);
    Some((key, name))
}

/// Build a [`WyomingEndpoint`] from a resolved service, returning `None` unless it
/// is an orchestrator (TXT `role=core`) with at least one address. This is
/// the single place the orchestrator role filter lives.
fn endpoint_from(info: &ServiceInfo) -> Option<WyomingEndpoint> {
    let (key, name) = orchestrator_meta(info)?;
    let &address = info.get_addresses().iter().next()?;
    Some(WyomingEndpoint {
        address,
        port: info.get_port(),
        hostname: info.get_hostname().to_string(),
        key,
        name,
    })
}

impl std::fmt::Display for WyomingEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}:{})", self.hostname, self.address, self.port)
    }
}

/// Browse `_wyoming._tcp` and return the first resolved **orchestrator**, or an
/// error if none respond within `timeout`. Raw Whisper/Piper Wyoming servers are
/// skipped (no `role=core` TXT). Spins up (and always tears down) a
/// dedicated mDNS daemon for the browse so we never leak the background sockets.
///
/// This is the "Auto (first available)" path: when no specific orchestrator is
/// selected, connect to whichever orchestrator resolves first.
pub async fn discover(timeout: Duration) -> Result<WyomingEndpoint> {
    let daemon = ServiceDaemon::new().context("starting mDNS daemon")?;
    let receiver = daemon
        .browse(WYOMING_SERVICE_TYPE)
        .context("browsing _wyoming._tcp")?;

    let result = tokio::time::timeout(timeout, async {
        loop {
            match receiver.recv_async().await {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    if let Some(ep) = endpoint_from(&info) {
                        return Ok(ep);
                    }
                    // Not an orchestrator, or resolved without an address; keep waiting.
                }
                Ok(_) => {} // search-started / other services — keep waiting
                Err(e) => return Err(anyhow!("mDNS channel closed: {e}")),
            }
        }
    })
    .await;

    // Best-effort teardown regardless of outcome; the daemon owns background
    // sockets we must not leak.
    let _ = daemon.shutdown();

    match result {
        Ok(inner) => inner,
        Err(_elapsed) => Err(anyhow!(
            "no _wyoming._tcp orchestrator found within {:?}",
            timeout
        )),
    }
}

/// Browse `_wyoming._tcp` for the **full** `timeout` window and return every
/// distinct orchestrator discovered (deduped by selection `key`). Used to
/// populate the device's orchestrator picker. An empty vec is a valid result
/// (no orchestrators on the network).
pub async fn discover_all(timeout: Duration) -> Result<Vec<WyomingEndpoint>> {
    let daemon = ServiceDaemon::new().context("starting mDNS daemon")?;
    let receiver = daemon
        .browse(WYOMING_SERVICE_TYPE)
        .context("browsing _wyoming._tcp")?;

    let mut found: Vec<WyomingEndpoint> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // Collect over the whole window; the timeout elapsing is the normal exit.
    let _ = tokio::time::timeout(timeout, async {
        loop {
            match receiver.recv_async().await {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    if let Some(ep) = endpoint_from(&info) {
                        if seen.insert(ep.key.clone()) {
                            found.push(ep);
                        }
                    }
                }
                Ok(_) => {}
                Err(_) => break, // channel closed — stop collecting
            }
        }
    })
    .await;

    let _ = daemon.shutdown();
    Ok(found)
}

/// Browse `_wyoming._tcp` and return the orchestrator whose selection `key`
/// matches `key`, or an error if it does not appear within `timeout`. Strict:
/// this never returns a *different* orchestrator, so a display pinned to one Mac
/// never silently talks to another.
pub async fn discover_preferred(timeout: Duration, key: &str) -> Result<WyomingEndpoint> {
    let daemon = ServiceDaemon::new().context("starting mDNS daemon")?;
    let receiver = daemon
        .browse(WYOMING_SERVICE_TYPE)
        .context("browsing _wyoming._tcp")?;

    let result = tokio::time::timeout(timeout, async {
        loop {
            match receiver.recv_async().await {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    if let Some(ep) = endpoint_from(&info) {
                        if ep.key == key {
                            return Ok(ep);
                        }
                    }
                    // A different orchestrator (or non-orchestrator); keep waiting.
                }
                Ok(_) => {}
                Err(e) => return Err(anyhow!("mDNS channel closed: {e}")),
            }
        }
    })
    .await;

    let _ = daemon.shutdown();

    match result {
        Ok(inner) => inner,
        Err(_elapsed) => Err(anyhow!(
            "selected orchestrator `{key}` not found on the network within {timeout:?}"
        )),
    }
}

/// Thread-safe cache of the last successfully resolved endpoint. Shared by the
/// engine so reconnects can fall back to the last-known host when a fresh browse
/// times out.
#[derive(Debug, Default)]
pub struct EndpointCache {
    inner: Mutex<Option<WyomingEndpoint>>,
}

impl EndpointCache {
    /// An empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// The currently cached endpoint, if any.
    pub fn get(&self) -> Option<WyomingEndpoint> {
        self.inner.lock().unwrap().clone()
    }

    /// Overwrite the cached endpoint.
    pub fn set(&self, endpoint: WyomingEndpoint) {
        *self.inner.lock().unwrap() = Some(endpoint);
    }

    /// Forget the cached endpoint (e.g. after repeated dial failures against it).
    pub fn clear(&self) {
        *self.inner.lock().unwrap() = None;
    }
}

/// Resolve the Wyoming endpoint, preferring a fresh mDNS browse and falling back
/// to the cache when the browse fails. On a successful browse the cache is
/// refreshed so it always holds the most recent good endpoint.
///
/// `preferred` selects the strategy:
/// - `Some(key)` — **strict**: resolve only the orchestrator with that stable
///   selection key. On browse failure fall back to the cache **only if** the
///   cached endpoint has the same key, so a display pinned to one Mac never
///   connects to a different one (nor reuses a stale cache for a different Mac).
/// - `None` — **Auto**: the first available orchestrator, with any cached
///   endpoint as the fallback (today's behavior).
pub async fn resolve(
    cache: &EndpointCache,
    timeout: Duration,
    preferred: Option<&str>,
) -> Result<WyomingEndpoint> {
    match preferred {
        Some(key) => match discover_preferred(timeout, key).await {
            Ok(endpoint) => {
                cache.set(endpoint.clone());
                Ok(endpoint)
            }
            Err(browse_err) => cache
                .get()
                .filter(|c| c.key == key)
                .ok_or_else(|| {
                    anyhow!(
                        "selected orchestrator `{key}` is unreachable and no matching cached \
                         endpoint is available: {browse_err}"
                    )
                }),
        },
        None => match discover(timeout).await {
            Ok(endpoint) => {
                cache.set(endpoint.clone());
                Ok(endpoint)
            }
            Err(browse_err) => cache.get().ok_or_else(|| {
                anyhow!("mDNS discovery failed and no cached endpoint is available: {browse_err}")
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn sample(port: u16) -> WyomingEndpoint {
        sample_keyed(port, "mac-mini")
    }

    fn sample_keyed(port: u16, key: &str) -> WyomingEndpoint {
        WyomingEndpoint {
            address: IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)),
            port,
            hostname: "mac-mini.local.".to_string(),
            key: key.to_string(),
            name: "Mac Mini".to_string(),
        }
    }

    #[test]
    fn socket_addr_pairs_address_and_port() {
        let ep = sample(10300);
        assert_eq!(
            ep.socket_addr(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)), 10300)
        );
    }

    #[test]
    fn instance_name_of_strips_service_suffix() {
        assert_eq!(
            instance_name_of("Mac Mini._wyoming._tcp.local."),
            "Mac Mini"
        );
        // Already-bare names pass through unchanged.
        assert_eq!(instance_name_of("bare-label"), "bare-label");
    }

    #[test]
    fn orchestrator_meta_reads_txt_and_filters_by_role() {
        // An orchestrator: role present → (instance_id, name) returned.
        let orch = ServiceInfo::new(
            WYOMING_SERVICE_TYPE,
            "Test Mac",
            "test-mac.local.",
            "127.0.0.1",
            10700,
            &[
                ("role", "core"),
                ("instance_id", "test-mac"),
                ("name", "Test Mac"),
            ][..],
        )
        .unwrap();
        assert_eq!(
            orchestrator_meta(&orch),
            Some(("test-mac".to_string(), "Test Mac".to_string()))
        );

        // A raw Whisper/Piper server (no orchestrator role) is filtered out.
        let stt = ServiceInfo::new(
            WYOMING_SERVICE_TYPE,
            "faster-whisper",
            "whisper.local.",
            "127.0.0.1",
            10300,
            &[("role", "asr")][..],
        )
        .unwrap();
        assert_eq!(orchestrator_meta(&stt), None);
    }

    #[test]
    fn orchestrator_meta_falls_back_to_instance_name_without_txt_id() {
        let orch = ServiceInfo::new(
            WYOMING_SERVICE_TYPE,
            "Legacy Orchestrator",
            "legacy.local.",
            "127.0.0.1",
            10700,
            &[("role", "core")][..],
        )
        .unwrap();
        assert_eq!(
            orchestrator_meta(&orch),
            Some((
                "Legacy Orchestrator".to_string(),
                "Legacy Orchestrator".to_string()
            ))
        );
    }

    #[test]
    fn cache_round_trips_and_clears() {
        let cache = EndpointCache::new();
        assert_eq!(cache.get(), None);
        cache.set(sample(10300));
        assert_eq!(cache.get(), Some(sample(10300)));
        cache.clear();
        assert_eq!(cache.get(), None);
    }

    #[tokio::test]
    async fn resolve_auto_falls_back_to_cache_when_browse_times_out() {
        let cache = EndpointCache::new();
        cache.set(sample(10300));
        // A 0 s timeout guarantees the browse finds nothing, exercising the
        // cached-fallback path without needing a real mDNS responder.
        let resolved = resolve(&cache, Duration::from_millis(0), None)
            .await
            .expect("cache fallback");
        assert_eq!(resolved, sample(10300));
    }

    #[tokio::test]
    async fn resolve_auto_errors_when_browse_fails_and_cache_empty() {
        let cache = EndpointCache::new();
        let err = resolve(&cache, Duration::from_millis(0), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no cached endpoint"));
    }

    #[tokio::test]
    async fn resolve_preferred_uses_cache_only_when_key_matches() {
        let cache = EndpointCache::new();
        cache.set(sample_keyed(10700, "mac-mini"));

        // Matching key: strict resolve falls back to the cached endpoint.
        let resolved = resolve(&cache, Duration::from_millis(0), Some("mac-mini"))
            .await
            .expect("cache fallback for the selected orchestrator");
        assert_eq!(resolved.key, "mac-mini");

        // Different key: strict resolve must NOT return the cached (wrong) Mac.
        let err = resolve(&cache, Duration::from_millis(0), Some("other-mac"))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("other-mac"),
            "error names the selected orchestrator, not the cached one: {err}"
        );
    }

    #[test]
    fn display_is_human_readable() {
        assert_eq!(
            sample(10300).to_string(),
            "mac-mini.local. (192.168.1.50:10300)"
        );
    }
}
