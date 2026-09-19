//! mDNS / Zeroconf discovery of the Wyoming host on the LAN
//! (Plan.MD §3, Phase 3 "mDNS discovery of `_wyoming._tcp`"; architecture.md §5).
//!
//! The Echo Show never hardcodes the Mac Mini's IP: it browses `_wyoming._tcp`,
//! resolves the first responder's host/port, and **caches** it so a later
//! reconnect can skip the browse if the network is momentarily quiet (the plan's
//! "cached fallback"; full auto-reconnect/back-off is Phase 5). The IP-based
//! discovery fallback that `agents.md` forbids is a *manual static IP* — a cache
//! of an mDNS-resolved endpoint is explicitly the intended behavior here.

use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent};

/// The Wyoming service type. The trailing `.local.` is required by the mDNS
/// browse API.
pub const WYOMING_SERVICE_TYPE: &str = "_wyoming._tcp.local.";

/// Default time to wait for the first mDNS responder before giving up.
pub const DEFAULT_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// A resolved Wyoming endpoint: an IP address, its port, and the advertised
/// hostname (kept for logging / diagnostics).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WyomingEndpoint {
    pub address: IpAddr,
    pub port: u16,
    pub hostname: String,
}

impl WyomingEndpoint {
    /// The `SocketAddr` to dial for the Wyoming TCP connection.
    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.address, self.port)
    }
}

impl std::fmt::Display for WyomingEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}:{})", self.hostname, self.address, self.port)
    }
}

/// Browse `_wyoming._tcp` and return the first resolved endpoint, or an error if
/// none respond within `timeout`. Spins up (and always tears down) a dedicated
/// mDNS daemon for the browse so we never leak the background sockets.
pub async fn discover(timeout: Duration) -> Result<WyomingEndpoint> {
    let daemon = ServiceDaemon::new().context("starting mDNS daemon")?;
    let receiver = daemon
        .browse(WYOMING_SERVICE_TYPE)
        .context("browsing _wyoming._tcp")?;

    let result = tokio::time::timeout(timeout, async {
        loop {
            match receiver.recv_async().await {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    // Prefer the first advertised address. mic-array LAN setups
                    // are IPv4, but IPv6 is accepted transparently.
                    if let Some(&address) = info.get_addresses().iter().next() {
                        return Ok(WyomingEndpoint {
                            address,
                            port: info.get_port(),
                            hostname: info.get_hostname().to_string(),
                        });
                    }
                    // Resolved without an address (rare); keep waiting.
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
            "no _wyoming._tcp service found within {:?}",
            timeout
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
pub async fn resolve(cache: &EndpointCache, timeout: Duration) -> Result<WyomingEndpoint> {
    match discover(timeout).await {
        Ok(endpoint) => {
            cache.set(endpoint.clone());
            Ok(endpoint)
        }
        Err(browse_err) => cache.get().ok_or_else(|| {
            anyhow!("mDNS discovery failed and no cached endpoint is available: {browse_err}")
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn sample(port: u16) -> WyomingEndpoint {
        WyomingEndpoint {
            address: IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)),
            port,
            hostname: "mac-mini.local.".to_string(),
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
    fn cache_round_trips_and_clears() {
        let cache = EndpointCache::new();
        assert_eq!(cache.get(), None);
        cache.set(sample(10300));
        assert_eq!(cache.get(), Some(sample(10300)));
        cache.clear();
        assert_eq!(cache.get(), None);
    }

    #[tokio::test]
    async fn resolve_falls_back_to_cache_when_browse_times_out() {
        let cache = EndpointCache::new();
        cache.set(sample(10300));
        // A 0 s timeout guarantees the browse finds nothing, exercising the
        // cached-fallback path without needing a real mDNS responder.
        let resolved = resolve(&cache, Duration::from_millis(0))
            .await
            .expect("cache fallback");
        assert_eq!(resolved, sample(10300));
    }

    #[tokio::test]
    async fn resolve_errors_when_browse_fails_and_cache_empty() {
        let cache = EndpointCache::new();
        let err = resolve(&cache, Duration::from_millis(0)).await.unwrap_err();
        assert!(err.to_string().contains("no cached endpoint"));
    }

    #[test]
    fn display_is_human_readable() {
        assert_eq!(
            sample(10300).to_string(),
            "mac-mini.local. (192.168.1.50:10300)"
        );
    }
}
