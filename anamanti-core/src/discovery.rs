//! mDNS / Zeroconf **advertisement** of the Anamanti Core's Wyoming service
//! (Plan.MD §0 "mDNS / Zeroconf"; architecture.md §5). The Echo Show browses
//! `_wyoming._tcp` (see the device's `rust/src/wyoming/discovery.rs`) and dials
//! whatever this advertises — so no IP is ever hardcoded on either side.
//!
//! ## Two backends
//!
//! - **macOS (production):** register through Apple's system daemon
//!   `mDNSResponder` via `DNSServiceRegister` (the `astro-dnssd` wrapper). The OS
//!   daemon — not this process — owns the actual advertisement: the address
//!   records, multicast-group membership, sleep/wake recovery, interface-change
//!   refresh, and name-collision probing. This process merely holds an open handle
//!   to it. That removes an entire class of "discovery goes stale until the Core is
//!   restarted" bugs the hand-rolled responder hit: a second in-process pure-Rust
//!   responder had to pin a single IPv4 (otherwise it leaked the Mac's IPv6
//!   link-locals to the device), had to manually re-advertise on interface changes
//!   within `mdns-sd`'s ~30 s interface-poll window, and could not reliably rejoin
//!   multicast groups after the Mac slept — so an overnight sleep/wake left a zombie
//!   record that answered nothing until a restart. `mDNSResponder` handles all of
//!   that itself.
//! - **Other platforms (dev/CI):** the previous pure-Rust `mdns-sd` advertiser,
//!   kept so the crate still builds and self-advertises off macOS.
//!
//! Both advertise the identical `_wyoming._tcp` DNS-SD record with the same TXT
//! records, so the device's browser is unchanged. One consequence of letting
//! `mDNSResponder` own address advertisement (instead of pinning one IPv4) is that
//! resolves now return *every* host address, including IPv6 link-locals; the device
//! picks the best reachable one at resolve time — see `endpoint_from` /
//! `choose_address` in the device's `wyoming/discovery.rs`.

/// The Wyoming service type; the trailing `.local.` is required by the mDNS
/// browse API the device uses. (The macOS registration API takes the bare
/// `_wyoming._tcp` and supplies the `.local.` domain itself.)
pub const WYOMING_SERVICE_TYPE: &str = "_wyoming._tcp.local.";

/// The TXT records advertised for this Core, used by the device to (a) filter
/// Cores from raw Whisper/Piper Wyoming servers (`role=core`), (b) show a friendly
/// label (`name`), and (c) pin a stable selection key (`instance_id`). Shared by
/// both backends so the advertised record is byte-for-byte identical.
fn txt_records<'a>(instance_name: &'a str, instance_id: &'a str) -> [(&'a str, &'a str); 4] {
    [
        ("version", "1"),
        ("role", "core"),
        ("name", instance_name),
        ("instance_id", instance_id),
    ]
}

#[cfg(not(target_os = "macos"))]
pub use fallback::MdnsAdvertiser;
#[cfg(target_os = "macos")]
pub use macos::MdnsAdvertiser;

// ---------------------------------------------------------------------------
// macOS (production): register via Apple's system mDNSResponder.
// ---------------------------------------------------------------------------
#[cfg(target_os = "macos")]
mod macos {
    use super::txt_records;
    use anyhow::{anyhow, Result};
    use astro_dnssd::{DNSServiceBuilder, RegisteredDnsService};
    use std::collections::HashMap;

    /// The DNS-SD registration type. Unlike the browse side, the registration API
    /// takes the bare service type without the trailing `.local.` domain (it adds
    /// `.local.` itself).
    const REGTYPE: &str = "_wyoming._tcp";

    /// Owns the live Bonjour registration. Dropping it deregisters the service:
    /// `astro-dnssd` signals its background poll thread, which deallocates the
    /// `DNSServiceRef` and tells `mDNSResponder` to withdraw the record — so the
    /// service disappears cleanly when the process exits.
    pub struct MdnsAdvertiser {
        _service: RegisteredDnsService,
    }

    impl MdnsAdvertiser {
        /// Advertise `_wyoming._tcp` on `port` under `instance_name`, tagged with
        /// the stable `instance_id` the device pins its selection to.
        ///
        /// Registration is delegated to the OS `mDNSResponder`; we deliberately do
        /// **not** pin an address. The daemon answers queries with the host's
        /// current addresses and keeps them fresh across sleep/wake, DHCP renewals,
        /// and interface changes — the exact churn the old single-IPv4 pin +
        /// interface-watcher tried (and sometimes failed) to track by hand.
        ///
        /// `register()` blocks briefly waiting for `mDNSResponder` to confirm the
        /// registration (or report a name collision), then returns; the handle keeps
        /// an internal thread pumping `DNSServiceProcessResult` for its lifetime.
        pub fn advertise(instance_name: &str, instance_id: &str, port: u16) -> Result<Self> {
            let txt: HashMap<String, String> = txt_records(instance_name, instance_id)
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();

            let service = DNSServiceBuilder::new(REGTYPE, port)
                .with_name(instance_name)
                .with_txt_record(txt)
                .register()
                .map_err(|e| {
                    anyhow!("registering {REGTYPE} with macOS mDNSResponder (Bonjour): {e}")
                })?;

            log::info!(
                "advertising `{instance_name}` (instance_id=`{instance_id}`) on port {port} \
                 via macOS Bonjour/mDNSResponder"
            );

            Ok(Self { _service: service })
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn txt_records_carry_core_role_and_identity() {
            let txt = txt_records("Test Mac", "test-mac");
            assert!(txt.contains(&("role", "core")));
            assert!(txt.contains(&("name", "Test Mac")));
            assert!(txt.contains(&("instance_id", "test-mac")));
            assert!(txt.contains(&("version", "1")));
        }

        #[test]
        fn regtype_has_no_local_suffix() {
            // The registration API supplies the `.local.` domain itself; passing a
            // `.local.`-suffixed type would double it.
            assert_eq!(REGTYPE, "_wyoming._tcp");
            assert!(!REGTYPE.ends_with(".local."));
        }
    }
}

// ---------------------------------------------------------------------------
// Other platforms (dev/CI): pure-Rust `mdns-sd` responder.
//
// Retained verbatim from the previous implementation. It pins a single routable
// IPv4 (see `build_advertisement`) and re-advertises on interface changes via an
// IP watcher, because a userspace responder has to do that address bookkeeping
// itself — the fragility that motivated moving production to mDNSResponder above.
// ---------------------------------------------------------------------------
#[cfg(not(target_os = "macos"))]
mod fallback {
    use super::{txt_records, WYOMING_SERVICE_TYPE};
    use anyhow::{Context, Result};
    use mdns_sd::{DaemonEvent, ServiceDaemon, ServiceInfo};
    use std::net::Ipv4Addr;
    use std::thread::JoinHandle;

    /// Owns the running mDNS daemon; unregisters/shuts down on drop so the service
    /// disappears cleanly when the process exits.
    pub struct MdnsAdvertiser {
        daemon: ServiceDaemon,
        fullname: String,
        /// Background thread that re-registers the service when the host's active
        /// LAN address changes. Joined on drop.
        watcher: Option<JoinHandle<()>>,
    }

    /// Build the `ServiceInfo` advertised for this Core, including the TXT records
    /// the device uses to filter/label/pin it. Split out so the TXT records are
    /// unit-testable without registering a live daemon.
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
        let props = txt_records(instance_name, instance_id);
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
        /// host's IPv6 link-local (`fe80::…`) addresses — and the device's discovery
        /// might pick an unreachable one. Pinning the single reachable IPv4 makes
        /// discovery deterministic.
        ///
        /// **Note:** `mdns-sd` does **not** probe for or auto-rename on fullname
        /// collisions, so two Cores MUST be launched with distinct `service_name`
        /// values; the device disambiguates by the `instance_id` TXT record.
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
        /// and re-registers the service with a freshly-resolved primary IPv4 whenever
        /// it differs from what we last advertised.
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
            let _ = self.daemon.shutdown();
            if let Some(watcher) = self.watcher.take() {
                let _ = watcher.join();
            }
        }
    }

    /// Build the advertisement for a resolved primary IPv4: a pinned single-IPv4
    /// `ServiceInfo` when one is known, or the `addr_auto` fallback (all interfaces)
    /// when no routable LAN IPv4 could be determined.
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
    /// the last (now-stale) record rather than falling back to `addr_auto`.
    fn should_readvertise(current: Option<Ipv4Addr>, detected: Option<Ipv4Addr>) -> bool {
        detected.is_some() && detected != current
    }

    /// Best-effort discovery of the primary routable LAN IPv4 address. Opens a UDP
    /// socket "connected" to a public address (no packets are sent) and reads back
    /// the local address the OS routing table would use. Returns `None` for
    /// loopback/link-local so the caller can fall back to auto-detection.
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

            assert!(should_readvertise(Some(a), Some(b)));
            assert!(should_readvertise(None, Some(a)));

            assert!(!should_readvertise(Some(a), Some(a)));
            assert!(!should_readvertise(Some(a), None));
            assert!(!should_readvertise(None, None));
        }
    }
}
