//! Host discovery over mDNS.
//!
//! A host advertises itself, so a client finds it without being told an address. The
//! advertisement carries the control port and the host's identity fingerprint, so a client can
//! tell one host from another before it pairs.
//!
//! The idea is punktfunk's `punktfunk-core/src/discovery.rs` (MIT OR Apache-2.0, Copyright (c)
//! 2026 unom - Enrico Bühler).

use std::collections::HashMap;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

/// The service type Lumen advertises.
pub const SERVICE_TYPE: &str = "_lumen._tcp.local.";

/// The TXT key that carries the host's identity fingerprint.
pub const FINGERPRINT_KEY: &str = "fp";

/// The TXT key that carries the host's name.
pub const NAME_KEY: &str = "name";

/// A host that answered the query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundHost {
    /// The instance name, the host's name.
    pub name: String,
    /// The address the host answered on.
    pub address: std::net::IpAddr,
    /// The control port.
    pub port: u16,
    /// The host's identity fingerprint, when it advertised one.
    pub fingerprint: Option<String>,
}

/// The advertisement of one host. Keep it alive for as long as the host is.
pub struct Advertiser {
    /// The daemon holds the registration; dropping it withdraws the advertisement.
    _daemon: ServiceDaemon,
}

impl std::fmt::Debug for Advertiser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Advertiser").finish_non_exhaustive()
    }
}

impl Advertiser {
    /// Advertises the host on `port` with `fingerprint` and `name`.
    ///
    /// # Errors
    /// Returns a description when the daemon or the registration fails.
    pub fn advertise(
        port: u16,
        name: &str,
        fingerprint: &str,
        address: std::net::IpAddr,
    ) -> Result<Self, String> {
        let daemon = ServiceDaemon::new().map_err(|e| format!("mDNS daemon: {e}"))?;
        let properties: HashMap<String, String> = HashMap::from([
            (FINGERPRINT_KEY.to_owned(), fingerprint.to_owned()),
            (NAME_KEY.to_owned(), name.to_owned()),
        ]);
        // The host name must end with `.local.`, which is what the resolver expects.
        let host_name = if name.ends_with(".local.") {
            name.to_owned()
        } else {
            format!("{name}.local.")
        };
        let info = ServiceInfo::new(SERVICE_TYPE, name, &host_name, address, port, properties)
            .map_err(|e| format!("mDNS service info: {e}"))?;
        daemon
            .register(info)
            .map_err(|e| format!("mDNS registration: {e}"))?;
        Ok(Self { _daemon: daemon })
    }
}

/// The hosts that answered within `timeout`.
///
/// # Errors
/// Returns a description when the daemon or the query fails.
pub fn browse(timeout: Duration) -> Result<Vec<FoundHost>, String> {
    let daemon = ServiceDaemon::new().map_err(|e| format!("mDNS daemon: {e}"))?;
    let receiver = daemon
        .browse(SERVICE_TYPE)
        .map_err(|e| format!("mDNS browse: {e}"))?;
    let mut found: Vec<FoundHost> = Vec::new();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match receiver.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let properties = info.get_properties();
                // The value alone: a property's `Display` is `key=value`.
                let fingerprint = properties
                    .get_property_val_str(FINGERPRINT_KEY)
                    .map(str::to_owned);
                let name = properties
                    .get_property_val_str(NAME_KEY)
                    .map_or_else(|| info.get_fullname().to_owned(), str::to_owned);
                for address in info.get_addresses() {
                    let host = FoundHost {
                        name: name.clone(),
                        address: address.to_ip_addr(),
                        port: info.get_port(),
                        fingerprint: fingerprint.clone(),
                    };
                    if !found.contains(&host) {
                        found.push(host);
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    // The daemon runs a thread of its own; a caller that browses again must not collect them.
    let _ = daemon.shutdown();
    Ok(found)
}
