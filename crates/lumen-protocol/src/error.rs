//! Protocol error taxonomy.

/// Errors produced while parsing or validating protocol data.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    /// The packet did not start with the expected magic bytes.
    #[error("bad magic bytes")]
    BadMagic,
    /// The buffer was shorter than the fixed header size.
    #[error("short header: need 42 bytes")]
    ShortHeader,
    /// The protocol version is not supported by this build.
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u8),
    /// The message kind byte is not a known kind.
    #[error("unknown message kind {0}")]
    BadKind(u8),
    /// The declared payload length does not match the received bytes.
    #[error("length mismatch: header says {0}, got {1}")]
    LengthMismatch(usize, usize),
    /// A slice index was out of range for its access unit.
    #[error("slice index {0} out of range for {1} slices")]
    BadSliceIndex(u16, u16),
    /// A capability bit was set that this build does not know.
    #[error("unknown capability bits {0:#x}")]
    UnknownCapabilities(u64),
    /// The version handshake failed.
    #[error("version negotiation failed: {0}")]
    Negotiation(&'static str),
}

impl From<ProtocolError> for std::io::Error {
    fn from(e: ProtocolError) -> Self {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    }
}
