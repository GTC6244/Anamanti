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
    /// Advertise `_wyoming._tcp` on `port` under `instance_name`. Addresses are
    /// auto-detected (`enable_addr_auto`) so every LAN interface is announced.
    pub fn advertise(instance_name: &str, port: u16) -> Result<Self> {
        let daemon = ServiceDaemon::new().context("starting mDNS daemon")?;
        let host_label = sanitize_label(instance_name);
        let host_name = format!("{host_label}.local.");

        let service = ServiceInfo::new(
            WYOMING_SERVICE_TYPE,
            instance_name,
            &host_name,
            "", // addresses filled in by enable_addr_auto()
            port,
            &[("version", "1"), ("role", "orchestrator")][..],
        )
        .context("building Wyoming ServiceInfo")?
        .enable_addr_auto();

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
