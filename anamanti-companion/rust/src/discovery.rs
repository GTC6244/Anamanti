//! mDNS discovery of the Anamanti Core on the home LAN (locked decision: mDNS only,
//! no hardcoded IPs, no manual-IP fallback).
//!
//! Adapted from the Display's `anamanti-display/rust/src/wyoming/discovery.rs` (a
//! different crate, so copied rather than path-depended): browse `_wyoming._tcp`,
//! keep only TXT `role=core` (raw Whisper/Piper servers advertise the same service
//! type), read `instance_id` / `name`, and pick a reachable address with the same
//! `choose_address` rules. The companion adds one TXT key: `companion_port`, the
//! Core's read-only LAN HTTP API port (absent ⇒ listener disabled / Core too old).

use std::collections::HashSet;
use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

use crate::api::companion::CoreInfo;

/// The Wyoming service type. The trailing `.local.` is required by the browse API.
pub const WYOMING_SERVICE_TYPE: &str = "_wyoming._tcp.local.";

/// The TXT `role` value the Anamanti Core advertises.
pub const CORE_ROLE: &str = "core";

/// The TXT key carrying the companion HTTP API port.
pub const COMPANION_PORT_KEY: &str = "companion_port";

/// Strip the trailing `.{service_type}` suffix off a resolved fullname, leaving the
/// bare instance label (the fallback key/name when TXT records are absent).
fn instance_name_of(fullname: &str) -> String {
    fullname
        .strip_suffix(&format!(".{WYOMING_SERVICE_TYPE}"))
        .unwrap_or(fullname)
        .to_string()
}

/// Parse the `companion_port` TXT value. Anything that is not a non-zero `u16`
/// (missing, empty, garbage, `0`) means "no companion API".
pub fn parse_companion_port(raw: Option<&str>) -> Option<u16> {
    raw?.trim().parse::<u16>().ok().filter(|p| *p != 0)
}

/// Link-local (IPv4 169.254.0.0/16 or IPv6 fe80::/10): not dialable without a scope id.
fn is_link_local(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// Choose the best reachable address: skip loopback, unspecified and link-local, then
/// prefer IPv4 (the LAN norm) over global IPv6. `None` if nothing reachable remains.
pub fn choose_address<'a>(addrs: impl IntoIterator<Item = &'a IpAddr>) -> Option<IpAddr> {
    addrs
        .into_iter()
        .copied()
        .filter(|ip| !ip.is_loopback() && !ip.is_unspecified() && !is_link_local(ip))
        .max_by_key(|ip| match ip {
            IpAddr::V4(_) => 2,
            IpAddr::V6(_) => 1,
        })
}

/// Build a [`CoreInfo`] from a resolved service, or `None` unless it is a Core
/// (TXT `role=core`) with at least one reachable address. The single place the role
/// filter lives.
pub fn core_from(info: &ServiceInfo) -> Option<CoreInfo> {
    if info.get_property_val_str("role") != Some(CORE_ROLE) {
        return None;
    }
    let address = choose_address(info.get_addresses())?;
    let instance = instance_name_of(info.get_fullname());
    let key = info
        .get_property_val_str("instance_id")
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| instance.clone());
    let name = info
        .get_property_val_str("name")
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or(instance);
    Some(CoreInfo {
        key,
        name,
        host: info.get_hostname().to_string(),
        address: address.to_string(),
        wyoming_port: info.get_port(),
        companion_port: parse_companion_port(info.get_property_val_str(COMPANION_PORT_KEY)),
    })
}

/// Browse `_wyoming._tcp` for up to `timeout` and return every distinct Core found
/// (deduped by `key`, in discovery order). When `stop_on_key` is set and that Core
/// resolves, the browse ends early. An empty vec is a valid result. The mDNS daemon
/// is always shut down so no background sockets leak.
pub async fn discover_all(timeout: Duration, stop_on_key: Option<&str>) -> Result<Vec<CoreInfo>> {
    let daemon = ServiceDaemon::new().context("starting mDNS daemon")?;
    let receiver = daemon
        .browse(WYOMING_SERVICE_TYPE)
        .context("browsing _wyoming._tcp")?;

    let mut found: Vec<CoreInfo> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let _ = tokio::time::timeout(timeout, async {
        loop {
            match receiver.recv_async().await {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    if let Some(core) = core_from(&info) {
                        let hit = stop_on_key == Some(core.key.as_str());
                        if seen.insert(core.key.clone()) {
                            found.push(core);
                        }
                        if hit {
                            break;
                        }
                    }
                }
                Ok(_) => {}
                Err(_) => break, // channel closed
            }
        }
    })
    .await;

    let _ = daemon.shutdown();
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn service(name: &str, ip: &str, txt: &[(&str, &str)]) -> ServiceInfo {
        ServiceInfo::new(
            WYOMING_SERVICE_TYPE,
            name,
            "mac-mini.local.",
            ip,
            10700,
            txt,
        )
        .unwrap()
    }

    #[test]
    fn keeps_only_role_core_and_reads_txt() {
        let core = service(
            "Paul Family",
            "192.168.1.50",
            &[
                ("role", "core"),
                ("instance_id", "paul-family"),
                ("name", "Kitchen Mac"),
                ("companion_port", "8740"),
            ],
        );
        let info = core_from(&core).expect("a core");
        assert_eq!(info.key, "paul-family");
        assert_eq!(info.name, "Kitchen Mac");
        assert_eq!(info.address, "192.168.1.50");
        assert_eq!(info.wyoming_port, 10700);
        assert_eq!(info.companion_port, Some(8740));
        assert_eq!(info.host, "mac-mini.local.");

        let whisper = service("faster-whisper", "192.168.1.50", &[("role", "asr")]);
        assert!(core_from(&whisper).is_none());
        let bare = service("piper", "192.168.1.50", &[]);
        assert!(core_from(&bare).is_none());
    }

    #[test]
    fn falls_back_to_instance_name_and_no_companion_port() {
        let core = service("Old Core", "192.168.1.9", &[("role", "core")]);
        let info = core_from(&core).unwrap();
        assert_eq!(info.key, "Old Core");
        assert_eq!(info.name, "Old Core");
        assert_eq!(info.companion_port, None);
    }

    #[test]
    fn core_with_only_unreachable_addresses_is_skipped() {
        let core = service("Loopback", "127.0.0.1", &[("role", "core")]);
        assert!(core_from(&core).is_none());
    }

    #[test]
    fn companion_port_parsing() {
        assert_eq!(parse_companion_port(Some("8740")), Some(8740));
        assert_eq!(parse_companion_port(Some(" 8741 ")), Some(8741));
        assert_eq!(parse_companion_port(Some("0")), None);
        assert_eq!(parse_companion_port(Some("")), None);
        assert_eq!(parse_companion_port(Some("http")), None);
        assert_eq!(parse_companion_port(Some("70000")), None);
        assert_eq!(parse_companion_port(None), None);
    }

    #[test]
    fn choose_address_prefers_reachable_ipv4() {
        let lan = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50));
        let ll4 = IpAddr::V4(Ipv4Addr::new(169, 254, 1, 1));
        let ll6 = IpAddr::V6("fe80::1".parse::<Ipv6Addr>().unwrap());
        let g6 = IpAddr::V6("2001:db8::1".parse::<Ipv6Addr>().unwrap());
        let lo = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let any = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
        assert_eq!(choose_address([&ll6, &g6, &lan, &ll4, &lo]), Some(lan));
        assert_eq!(choose_address([&ll6, &g6]), Some(g6));
        assert_eq!(choose_address([&ll6, &ll4, &lo, &any]), None);
        assert_eq!(choose_address(std::iter::empty::<&IpAddr>()), None);
    }

    #[test]
    fn instance_name_strips_service_suffix() {
        assert_eq!(
            instance_name_of("Mac Mini._wyoming._tcp.local."),
            "Mac Mini"
        );
        assert_eq!(instance_name_of("bare"), "bare");
    }

    #[tokio::test]
    async fn zero_window_browse_returns_empty_not_error() {
        // No responder can answer in 0 ms; the browse must still tear down cleanly.
        let found = discover_all(Duration::from_millis(0), None).await.unwrap();
        assert!(found.is_empty() || found.iter().all(|c| !c.key.is_empty()));
    }
}
