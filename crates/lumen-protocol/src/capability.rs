//! Capability negotiation.
//!
//! Both peers exchange a 64-bit capability bitmap during the handshake. The
//! intersection of the two bitmaps is the set of features used for the
//! session. Unknown bits are ignored so that a newer peer can talk to an
//! older one.

use crate::error::ProtocolError;

bitflags::bitflags! {
    /// Feature bits exchanged during the handshake.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct Capabilities: u64 {
        /// H.264 video.
        const H264 = 1 << 0;
        /// HEVC video.
        const HEVC = 1 << 1;
        /// AV1 video.
        const AV1 = 1 << 2;
        /// 4:4:4 chroma sampling.
        const CHROMA_444 = 1 << 3;
        /// 10-bit pixel depth.
        const BIT_DEPTH_10 = 1 << 4;
        /// HDR metadata transport.
        const HDR = 1 << 5;
        /// Reed-Solomon FEC in addition to XOR parity.
        const FEC_RS = 1 << 6;
        /// Client can request an on-demand IDR frame.
        const REQUEST_IDR = 1 << 7;
        /// Client supports relative pointer mode.
        const RELATIVE_POINTER = 1 << 8;
        /// Client supports pointer constraints.
        const POINTER_CONSTRAINTS = 1 << 9;
        /// Client supports touch input.
        const TOUCH = 1 << 10;
        /// Client supports pen input.
        const PEN = 1 << 11;
        /// Client supports gamepads.
        const GAMEPAD = 1 << 12;
        /// Client supports bidirectional audio.
        const AUDIO_BIDIRECTIONAL = 1 << 13;
        /// Client supports clipboard transfer.
        const CLIPBOARD = 1 << 14;
        /// Client supports multiple monitors.
        const MULTI_MONITOR = 1 << 15;
        /// Client supports arbitrary resolution changes mid-session.
        const RESOLUTION_CHANGE = 1 << 16;
        /// Client supports the raw (uncompressed) codec, used for correctness tests.
        const RAW_CODEC = 1 << 17;
        /// Host can present a shadow (view-only) session.
        const SHADOW_MODE = 1 << 18;
        /// The peer has no AES-NI and wants ChaCha20-Poly1305 for the
        /// transport cipher.
        ///
        /// The baseline is AES-256-GCM. This bit is a request, not a
        /// capability: the peer that sets it is telling the other end that
        /// hardware acceleration is unavailable on its machine.
        const AEAD_CHACHA20 = 1 << 19;
    }
}

impl Capabilities {
    /// The intersection of two capability sets.
    #[must_use]
    pub fn intersect(self, other: Self) -> Self {
        Self::from_bits_truncate(self.bits() & other.bits())
    }

    /// Whether every bit of `other` is present in `self`.
    #[must_use]
    pub fn contains_all(self, other: Self) -> bool {
        self.bits() & other.bits() == other.bits()
    }

    /// Parse a bitmap, rejecting unknown bits.
    ///
    /// # Errors
    /// Returns [`ProtocolError::UnknownCapabilities`] when unknown bits are set.
    pub fn from_bits_strict(bits: u64) -> Result<Self, ProtocolError> {
        Self::from_bits(bits).ok_or(ProtocolError::UnknownCapabilities(bits))
    }
}

/// Version handshake payload exchanged over the control channel.
///
/// Both peers send this; the session uses the lower protocol version that
/// both understand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionHandshake {
    /// Protocol version offered by the sender.
    pub version: u8,
    /// Capability bitmap of the sender.
    pub capabilities: Capabilities,
}

impl VersionHandshake {
    /// Negotiate the session version and capabilities.
    ///
    /// # Errors
    /// Returns [`ProtocolError::Negotiation`] when the versions do not overlap
    /// or the capability intersection is empty.
    pub fn negotiate(
        local: &Self,
        remote: &Self,
        supported: &[u8],
    ) -> Result<(u8, Capabilities), ProtocolError> {
        if !supported.contains(&local.version) || !supported.contains(&remote.version) {
            return Err(ProtocolError::Negotiation("unsupported protocol version"));
        }
        let version = local.version.min(remote.version);
        if !supported.contains(&version) {
            return Err(ProtocolError::Negotiation("no common version"));
        }
        let caps = local.capabilities.intersect(remote.capabilities);
        if caps.is_empty() {
            return Err(ProtocolError::Negotiation("no common capabilities"));
        }
        Ok((version, caps))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersect_keeps_common_bits() {
        let a = Capabilities::H264 | Capabilities::HEVC | Capabilities::AUDIO_BIDIRECTIONAL;
        let b = Capabilities::H264 | Capabilities::AUDIO_BIDIRECTIONAL | Capabilities::TOUCH;
        assert_eq!(
            a.intersect(b),
            Capabilities::H264 | Capabilities::AUDIO_BIDIRECTIONAL
        );
    }

    #[test]
    fn contains_all_works() {
        let a = Capabilities::H264 | Capabilities::TOUCH;
        assert!(a.contains_all(Capabilities::H264));
        assert!(!a.contains_all(Capabilities::H264 | Capabilities::PEN));
    }

    #[test]
    fn strict_rejects_unknown_bits() {
        assert!(Capabilities::from_bits_strict(Capabilities::H264.bits()).is_ok());
        assert!(Capabilities::from_bits_strict(1 << 63).is_err());
    }

    #[test]
    fn negotiation_picks_lower_version() {
        let local = VersionHandshake {
            version: 1,
            capabilities: Capabilities::H264 | Capabilities::HEVC,
        };
        let remote = VersionHandshake {
            version: 1,
            capabilities: Capabilities::H264,
        };
        let (v, caps) = VersionHandshake::negotiate(&local, &remote, &[1]).unwrap();
        assert_eq!(v, 1);
        assert_eq!(caps, Capabilities::H264);
    }

    #[test]
    fn negotiation_rejects_disjoint_caps() {
        let local = VersionHandshake {
            version: 1,
            capabilities: Capabilities::H264,
        };
        let remote = VersionHandshake {
            version: 1,
            capabilities: Capabilities::AV1,
        };
        assert!(VersionHandshake::negotiate(&local, &remote, &[1]).is_err());
    }
}
