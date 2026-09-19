//! mDNS / Zeroconf **advertisement** of the orchestrator's Wyoming service
//! (Plan.MD §0 "mDNS / Zeroconf"; architecture.md §5). The Echo Show browses
//! `_wyoming._tcp` (see the device's `rust/src/wyoming/discovery.rs`) and dials
//! whatever this daemon advertises — so no IP is ever hardcoded on either side.

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceInfo};

/// The Wyoming service type; the trailing `.local.` is required by mDNS.
pub const WYOMING_SERVICE_TYPE: &str = "_wyoming._tcp.local.";

/// Owns the running mDNS daemon; unregisters/shuts down on drop so the service
/// disappears cleanly when the process exits.
pub struct MdnsAdvertiser {
    daemon: ServiceDaemon,
    fullname: String,
}

impl MdnsAdvertiser {
    /// Advertise `_wyoming._tcp` on `port` under `instance_name`.
    ///
    /// We advertise **only the routable LAN IPv4 address**, not every interface
    /// address. `enable_addr_auto()` announces all interfaces — including the
    /// Mac's many IPv6 link-local (`fe80::…`) addresses — and the device's
    /// discovery picks an arbitrary entry from an unordered set, so it usually
    /// grabs an unreachable link-local address and can never connect. Pinning the
    /// single reachable IPv4 makes discovery deterministic.
    pub fn advertise(instance_name: &str, port: u16) -> Result<Self> {
        let daemon = ServiceDaemon::new().context("starting mDNS daemon")?;
        let host_label = sanitize_label(instance_name);
        let host_name = format!("{host_label}.local.");

        let props = &[("version", "1"), ("role", "orchestrator")][..];
        let service = match primary_ipv4() {
            Some(ip) => {
                log::info!("advertising Wyoming service at LAN IPv4 {ip}");
                ServiceInfo::new(
                    WYOMING_SERVICE_TYPE,
                    instance_name,
                    &host_name,
                    ip.to_string().as_str(),
                    port,
                    props,
                )
                .context("building Wyoming ServiceInfo")?
            }
            None => {
                log::warn!(
                    "could not determine a routable LAN IPv4; \
                     falling back to auto address detection (all interfaces)"
                );
                ServiceInfo::new(
                    WYOMING_SERVICE_TYPE,
                    instance_name,
                    &host_name,
                    "",
                    port,
                    props,
                )
                .context("building Wyoming ServiceInfo")?
                .enable_addr_auto()
            }
        };

        let fullname = service.get_fullname().to_string();
        daemon
            .register(service)
            .context("registering _wyoming._tcp with mDNS")?;
        log::info!("advertising `{fullname}` on port {port} via mDNS");

        Ok(Self { daemon, fullname })
    }
}

impl Drop for MdnsAdvertiser {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
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
        "ambient-orchestrator".to_string()
    } else {
        trimmed.to_lowercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_instance_names_into_dns_labels() {
        assert_eq!(
            sanitize_label("Ambient Orchestrator"),
            "ambient-orchestrator"
        );
        assert_eq!(sanitize_label("Mac Mini!"), "mac-mini");
        assert_eq!(sanitize_label("***"), "ambient-orchestrator");
    }
}
