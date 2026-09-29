//! mDNS / Zeroconf **advertisement** of the orchestrator's Wyoming service
//! (Plan.MD §0 "mDNS / Zeroconf"; architecture.md §5). The Echo Show browses
//! `_wyoming._tcp` (see the device's `rust/src/wyoming/discovery.rs`) and dials
//! whatever this daemon advertises — so no IP is ever hardcoded on either side.

use anyhow::{Context, Result};
use mdns_sd::{DaemonEvent, ServiceDaemon, ServiceInfo};
use std::net::Ipv4Addr;
use std::thread::JoinHandle;

/// The Wyoming service type; the trailing `.local.` is required by mDNS.
pub const WYOMING_SERVICE_TYPE: &str = "_wyoming._tcp.local.";

/// Owns the running mDNS daemon; unregisters/shuts down on drop so the service
/// disappears cleanly when the process exits.
pub struct MdnsAdvertiser {
    daemon: ServiceDaemon,
    fullname: String,
    /// Background thread that re-registers the service when the Mac's active LAN
    /// address changes (see [`MdnsAdvertiser::spawn_ip_watcher`]). Joined on drop.
    watcher: Option<JoinHandle<()>>,
}

/// Build the `ServiceInfo` advertised for this orchestrator, including the TXT
/// records the device uses to (a) filter orchestrators from raw Whisper/Piper
/// Wyoming servers (`role=core`), (b) show a friendly label (`name`),
/// and (c) pin a stable selection key (`instance_id`). Split out so the TXT
/// records are unit-testable without registering a live daemon.
///
/// Pass `ip = ""` to enable auto address detection (caller applies
/// `.enable_addr_auto()`); otherwise a single pinned IPv4 string.
pub fn build_service_info(
    instance_name: &str,
    instance_id: &str,
    host_name: &str,
    ip: &str,
    port: u16,
) -> Result<ServiceInfo> {
    let props: Vec<(&str, &str)> = vec![
        ("version", "1"),
        ("role", "core"),
        ("name", instance_name),
        ("instance_id", instance_id),
    ];
    ServiceInfo::new(
        WYOMING_SERVICE_TYPE,
        instance_name,
        host_name,
        ip,
        port,
        &props[..],
    )
    .context("building Wyoming ServiceInfo")
}

impl MdnsAdvertiser {
    /// Advertise `_wyoming._tcp` on `port` under `instance_name`, tagged with the
    /// stable `instance_id` the device pins its selection to.
    ///
    /// We advertise **only the routable LAN IPv4 address**, not every interface
    /// address. `enable_addr_auto()` announces all interfaces — including the
    /// Mac's many IPv6 link-local (`fe80::…`) addresses — and the device's
    /// discovery picks an arbitrary entry from an unordered set, so it usually
    /// grabs an unreachable link-local address and can never connect. Pinning the
    /// single reachable IPv4 makes discovery deterministic.
    ///
    /// **Note:** `mdns-sd` does **not** probe for or auto-rename on fullname
    /// collisions, so two orchestrators MUST be launched with distinct
    /// `service_name` values — otherwise both answer under the same
    /// instance name and the device cannot tell them apart. The device
    /// disambiguates by the `instance_id` TXT record.
    pub fn advertise(instance_name: &str, instance_id: &str, port: u16) -> Result<Self> {
        let daemon = ServiceDaemon::new().context("starting mDNS daemon")?;
        let host_label = sanitize_label(instance_name);
        let host_name = format!("{host_label}.local.");

        let initial_ip = primary_ipv4();
        let service =
            build_advertisement(instance_name, instance_id, &host_name, initial_ip, port)?;

        let fullname = service.get_fullname().to_string();
        daemon
            .register(service)
            .context("registering _wyoming._tcp with mDNS")?;
        log::info!(
            "advertising `{fullname}` (instance_id=`{instance_id}`) on port {port} via mDNS"
        );

        // The advertised A record is pinned to a single routable IPv4 resolved at
        // register time. `mdns-sd` only auto-refreshes addresses for services that
        // enabled `addr_auto` (which we deliberately do not — it would leak the
        // Mac's IPv6 link-locals to the device). So if the active LAN interface or
        // its IP later changes (Wi-Fi↔Ethernet failover, DHCP renew, en0↔en1), the
        // record goes stale and the device can never connect. Watch the daemon's
        // interface-change events and re-register with the new IPv4.
        let watcher = Self::spawn_ip_watcher(
            &daemon,
            instance_name.to_string(),
            instance_id.to_string(),
            host_name,
            initial_ip,
            port,
        );

        Ok(Self {
            daemon,
            fullname,
            watcher,
        })
    }

    /// Spawn a thread that listens to the daemon's `IpAdd`/`IpDel` monitor events
    /// (emitted whenever `mdns-sd`'s periodic interface poll detects a host
    /// address change) and re-registers the service with a freshly-resolved
    /// primary IPv4 whenever it differs from what we last advertised.
    fn spawn_ip_watcher(
        daemon: &ServiceDaemon,
        instance_name: String,
        instance_id: String,
        host_name: String,
        initial_ip: Option<Ipv4Addr>,
        port: u16,
    ) -> Option<JoinHandle<()>> {
        let events = match daemon.monitor() {
            Ok(rx) => rx,
            Err(e) => {
                log::warn!(
                    "mDNS interface-change monitor unavailable; the advertised IP \
                     will not follow interface changes: {e:#}"
                );
                return None;
            }
        };
        let daemon = daemon.clone();
        let handle = std::thread::Builder::new()
            .name("mdns-ip-watcher".to_string())
            .spawn(move || {
                let mut current_ip = initial_ip;
                // `recv` returns `Err` once the daemon shuts down (on drop),
                // which cleanly ends the thread.
                while let Ok(event) = events.recv() {
                    if !matches!(event, DaemonEvent::IpAdd(_) | DaemonEvent::IpDel(_)) {
                        continue;
                    }
                    let detected = primary_ipv4();
                    if !should_readvertise(current_ip, detected) {
                        continue;
                    }
                    let service = match build_advertisement(
                        &instance_name,
                        &instance_id,
                        &host_name,
                        detected,
                        port,
                    ) {
                        Ok(s) => s,
                        Err(e) => {
                            log::warn!(
                                "could not rebuild Wyoming ServiceInfo after IP change: {e:#}"
                            );
                            continue;
                        }
                    };
                    match daemon.register(service) {
                        Ok(()) => {
                            log::info!(
                                "LAN address changed ({current_ip:?} -> {detected:?}); \
                                 re-advertised Wyoming service over mDNS"
                            );
                            current_ip = detected;
                        }
                        Err(e) => log::warn!(
                            "failed to re-register Wyoming service after IP change: {e:#}"
                        ),
                    }
                }
                log::debug!("mDNS interface-change watcher stopped");
            });
        match handle {
            Ok(h) => Some(h),
            Err(e) => {
                log::warn!("could not spawn mDNS interface-change watcher: {e}");
                None
            }
        }
    }
}

impl Drop for MdnsAdvertiser {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        // Shutting the daemon down disconnects the monitor channel, so the watcher
        // thread's `recv` returns `Err` and it exits; then join it.
        let _ = self.daemon.shutdown();
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
    }
}

/// Build the advertisement for a resolved primary IPv4: a pinned single-IPv4
/// `ServiceInfo` when one is known, or the `addr_auto` fallback (all interfaces)
/// when no routable LAN IPv4 could be determined. Shared by the initial
/// registration and the interface-change re-registration so both stay identical.
fn build_advertisement(
    instance_name: &str,
    instance_id: &str,
    host_name: &str,
    ip: Option<Ipv4Addr>,
    port: u16,
) -> Result<ServiceInfo> {
    match ip {
        Some(ip) => {
            log::info!("advertising Wyoming service at LAN IPv4 {ip}");
            build_service_info(
                instance_name,
                instance_id,
                host_name,
                ip.to_string().as_str(),
                port,
            )
        }
        None => {
            log::warn!(
                "could not determine a routable LAN IPv4; \
                 falling back to auto address detection (all interfaces)"
            );
            Ok(
                build_service_info(instance_name, instance_id, host_name, "", port)?
                    .enable_addr_auto(),
            )
        }
    }
}

/// Decide whether an interface-change event warrants re-registering. Only
/// re-advertise when we have resolved a new routable IPv4 that differs from what
/// we last advertised. When the LAN drops entirely (`detected == None`) we keep
/// the last (now-stale) record rather than falling back to `addr_auto`: nothing
/// is reachable anyway, and a fresh `IpAdd` will re-register once a link returns.
fn should_readvertise(current: Option<Ipv4Addr>, detected: Option<Ipv4Addr>) -> bool {
    detected.is_some() && detected != current
}

/// Best-effort discovery of the primary routable LAN IPv4 address. Opens a UDP
/// socket "connected" to a public address (no packets are sent) and reads back
/// the local address the OS routing table would use — the interface that reaches
/// the LAN/default gateway. Returns `None` for loopback/link-local so the caller
/// can fall back to auto-detection.
fn primary_ipv4() -> Option<std::net::Ipv4Addr> {
    use std::net::{IpAddr, UdpSocket};
    let sock = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    sock.connect(("8.8.8.8", 80)).ok()?;
    match sock.local_addr().ok()?.ip() {
        IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified() => {
            Some(v4)
        }
        _ => None,
    }
}

/// Reduce an instance name to a valid DNS label (alphanumerics + hyphen).
fn sanitize_label(name: &str) -> String {
    let label: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let trimmed = label.trim_matches('-');
    if trimmed.is_empty() {
        "anamanti-core".to_string()
    } else {
        trimmed.to_lowercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_instance_names_into_dns_labels() {
        assert_eq!(sanitize_label("Anamanti Core"), "anamanti-core");
        assert_eq!(sanitize_label("Mac Mini!"), "mac-mini");
        assert_eq!(sanitize_label("***"), "anamanti-core");
    }

    #[test]
    fn advertises_orchestrator_txt_records() {
        let info = build_service_info(
            "Test Mac",
            "test-mac",
            "test-mac.local.",
            "127.0.0.1",
            10700,
        )
        .expect("build service info");
        assert_eq!(info.get_property_val_str("role"), Some("core"));
        assert_eq!(info.get_property_val_str("name"), Some("Test Mac"));
        assert_eq!(info.get_property_val_str("instance_id"), Some("test-mac"));
        assert_eq!(info.get_property_val_str("version"), Some("1"));
        assert!(info.get_fullname().contains("Test Mac"));
    }

    #[test]
    fn readvertises_only_on_a_new_routable_ipv4() {
        let a = Ipv4Addr::new(192, 168, 1, 10);
        let b = Ipv4Addr::new(10, 0, 0, 5);

        // A new/different routable IPv4 → re-advertise.
        assert!(should_readvertise(Some(a), Some(b)));
        // First routable IPv4 after an addr_auto fallback (None) → re-advertise.
        assert!(should_readvertise(None, Some(a)));

        // Same IPv4 as before → no churn.
        assert!(!should_readvertise(Some(a), Some(a)));
        // LAN dropped entirely → keep the last record, don't fall back to addr_auto.
        assert!(!should_readvertise(Some(a), None));
        // Still no network → nothing to do.
        assert!(!should_readvertise(None, None));
    }
}
