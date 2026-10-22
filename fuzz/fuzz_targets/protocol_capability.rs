#![no_main]

use libfuzzer_sys::fuzz_target;
use lumen_protocol::{Capabilities, ProtocolError, SUPPORTED_VERSIONS, VersionHandshake};

fn u64_at(data: &[u8], at: usize) -> u64 {
    let mut out = [0u8; 8];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = data.get(at + i).copied().unwrap_or(0);
    }
    u64::from_le_bytes(out)
}

/// Checks `negotiate` against its contract for any peers and supported set.
fn check_negotiate(local: &VersionHandshake, remote: &VersionHandshake, supported: &[u8]) {
    match VersionHandshake::negotiate(local, remote, supported) {
        Ok((version, caps)) => {
            assert!(supported.contains(&version));
            assert_eq!(version, local.version.min(remote.version));
            assert_eq!(caps, local.capabilities.intersect(remote.capabilities));
            assert!(!caps.is_empty());
            assert!(local.capabilities.contains_all(caps));
            assert!(remote.capabilities.contains_all(caps));
        }
        Err(ProtocolError::Negotiation(_)) => {
            // Failure only when a version is unsupported or the capability sets are disjoint.
            let versions_ok =
                supported.contains(&local.version) && supported.contains(&remote.version);
            assert!(!versions_ok || local.capabilities.intersect(remote.capabilities).is_empty());
        }
        Err(other) => panic!("negotiate returned a non-negotiation error: {other:?}"),
    }
}

fuzz_target!(|data: &[u8]| {
    let bits = u64_at(data, 0);
    let known = Capabilities::all().bits();
    match Capabilities::from_bits_strict(bits) {
        Ok(caps) => assert_eq!(caps.bits(), bits),
        Err(ProtocolError::UnknownCapabilities(got)) => {
            assert_eq!(got, bits);
            assert_ne!(bits & !known, 0);
        }
        Err(other) => panic!("from_bits_strict returned {other:?}"),
    }

    // Layout: local caps @8, local version @16, remote version @17, remote caps @18, supported set @26.
    let local = VersionHandshake {
        version: data.get(16).copied().unwrap_or(0),
        capabilities: Capabilities::from_bits_truncate(u64_at(data, 8)),
    };
    let remote = VersionHandshake {
        version: data.get(17).copied().unwrap_or(0),
        capabilities: Capabilities::from_bits_truncate(u64_at(data, 18)),
    };
    check_negotiate(&local, &remote, data.get(26..).unwrap_or(&[]));
    check_negotiate(&local, &remote, SUPPORTED_VERSIONS);
});
