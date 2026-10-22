//! Crypto error taxonomy.

/// Errors produced by the crypto layer.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    /// The Noise handshake failed.
    #[error("handshake failed: {0}")]
    Handshake(#[source] snow::error::Error),
    /// The AEAD operation failed.
    #[error("aead failure: {0}")]
    Aead(&'static str),
    /// The pairing code was rejected.
    #[error("pairing rejected: {0}")]
    Pairing(&'static str),
    /// Key material was not available.
    #[error("missing key material: {0}")]
    MissingKey(&'static str),
    /// A stored identity was malformed.
    #[error("invalid identity: {0}")]
    InvalidIdentity(&'static str),
}
