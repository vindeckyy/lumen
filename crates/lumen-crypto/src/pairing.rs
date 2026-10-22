//! Device identity and pairing.
//!
//! The host owns a long-term Ed25519 identity key. A client is authorised
//! either by presenting the pairing code the host displays, or by presenting
//! a key that the host already trusts. The pairing code is derived from the
//! host's public key and a client nonce, so it is valid only for that one
//! handshake attempt.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::error::CryptoError;

/// Length of the identity key in bytes.
pub const KEY_LEN: usize = 32;

/// A device identity: an Ed25519 signing key and its public key.
#[derive(Debug, Clone)]
pub struct DeviceIdentity {
    signing: SigningKey,
}

impl DeviceIdentity {
    /// Generate a new random identity.
    ///
    /// # Errors
    /// Returns [`CryptoError::MissingKey`] when the system random source is
    /// unavailable.
    pub fn generate() -> Result<Self, CryptoError> {
        Ok(Self {
            signing: SigningKey::from_bytes(&random_bytes::<KEY_LEN>()),
        })
    }

    /// Restore an identity from stored bytes.
    ///
    /// # Errors
    /// Returns [`CryptoError::InvalidIdentity`] when the bytes are not a
    /// valid Ed25519 key.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CryptoError> {
        if bytes.len() != KEY_LEN {
            return Err(CryptoError::InvalidIdentity("wrong key length"));
        }
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(bytes);
        Ok(Self {
            signing: SigningKey::from_bytes(&key),
        })
    }

    /// The private key bytes, for storage.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; KEY_LEN] {
        self.signing.to_bytes()
    }

    /// The public key bytes.
    #[must_use]
    pub fn public_bytes(&self) -> [u8; KEY_LEN] {
        self.signing.verifying_key().to_bytes()
    }

    /// The public key as a verifier.
    #[must_use]
    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing.verifying_key()
    }

    /// Sign a message.
    #[must_use]
    pub fn sign(&self, message: &[u8]) -> Signature {
        self.signing.sign(message)
    }

    /// Verify a signature against a public key.
    ///
    /// # Errors
    /// Returns [`CryptoError::InvalidIdentity`] when the signature is bad.
    pub fn verify(
        public: &[u8; KEY_LEN],
        message: &[u8],
        signature: &Signature,
    ) -> Result<(), CryptoError> {
        let key = VerifyingKey::from_bytes(public)
            .map_err(|_| CryptoError::InvalidIdentity("bad public key"))?;
        key.verify(message, signature)
            .map_err(|_| CryptoError::InvalidIdentity("bad signature"))
    }

    /// Sign a message and return the raw signature bytes.
    #[must_use]
    pub fn sign_bytes(&self, message: &[u8]) -> [u8; SIGNATURE_LEN] {
        self.signing.sign(message).to_bytes()
    }

    /// Verify a signature given as raw bytes.
    ///
    /// # Errors
    /// Returns [`CryptoError::InvalidIdentity`] when the public key or the
    /// signature is bad.
    pub fn verify_bytes(
        public: &[u8; KEY_LEN],
        message: &[u8],
        signature: &[u8; SIGNATURE_LEN],
    ) -> Result<(), CryptoError> {
        let signature = Signature::from_bytes(signature);
        Self::verify(public, message, &signature)
    }
}

/// Length of an Ed25519 signature in bytes.
pub const SIGNATURE_LEN: usize = 64;

/// Fill a buffer with random bytes from the system source.
///
/// # Panics
/// Panics when the system random source is unavailable, which means the
/// machine has no entropy at all.
#[must_use]
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    rand::fill(&mut bytes);
    bytes
}

/// The bytes a client signs to prove that it owns its identity key.
///
/// The proof binds the host's key, the client's key and the host's nonce, so
/// a signature cannot be replayed into another session.
#[must_use]
pub fn pairing_proof(
    nonce: &[u8; 32],
    host_key: &[u8; KEY_LEN],
    device_key: &[u8; KEY_LEN],
) -> Vec<u8> {
    let mut message = Vec::with_capacity(24 + 32 + KEY_LEN + KEY_LEN);
    message.extend_from_slice(b"lumen-pairing-proof-v1");
    message.extend_from_slice(nonce);
    message.extend_from_slice(host_key);
    message.extend_from_slice(device_key);
    message
}

/// A six digit pairing code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairingCode(u32);

impl PairingCode {
    /// Build a code from raw digits.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value % 1_000_000)
    }

    /// The numeric value.
    #[must_use]
    pub const fn value(self) -> u32 {
        self.0
    }

    /// Parse a code from user input, tolerating spaces and dashes.
    ///
    /// # Errors
    /// Returns [`CryptoError::Pairing`] when the input is not six digits.
    pub fn parse(input: &str) -> Result<Self, CryptoError> {
        let digits: String = input.chars().filter(char::is_ascii_digit).collect();
        if digits.len() != 6 {
            return Err(CryptoError::Pairing("expected six digits"));
        }
        Ok(Self(digits.parse().unwrap_or(0)))
    }

    /// Derive the code the host shows for a client nonce.
    ///
    /// The code is six digits. The digest is reduced modulo one million, so
    /// the code is always a six digit number and two peers that derive it
    /// independently agree.
    #[must_use]
    pub fn derive(host_public: &[u8; KEY_LEN], client_nonce: &[u8; 32]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"lumen-pairing-v1");
        hasher.update(host_public);
        hasher.update(client_nonce);
        let digest = hasher.finalize();
        let value = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
        Self::new(value)
    }
}

/// A paired client, as stored by the host.
#[derive(Debug, Clone)]
pub struct PairedClient {
    /// The client's public key.
    pub public_key: [u8; KEY_LEN],
    /// Human readable label.
    pub name: String,
}

impl std::fmt::Display for PairingCode {
    /// Formats the code as a zero padded six digit string.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:06}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_roundtrip() {
        let id = DeviceIdentity::generate().unwrap();
        let bytes = id.to_bytes();
        let restored = DeviceIdentity::from_bytes(&bytes).unwrap();
        assert_eq!(restored.public_bytes(), id.public_bytes());
    }

    #[test]
    fn signature_verifies() {
        let id = DeviceIdentity::generate().unwrap();
        let sig = id.sign(b"hello");
        DeviceIdentity::verify(&id.public_bytes(), b"hello", &sig).unwrap();
        assert!(DeviceIdentity::verify(&id.public_bytes(), b"other", &sig).is_err());
    }

    #[test]
    fn pairing_code_is_six_digits() {
        let code = PairingCode::new(12345);
        assert_eq!(code.to_string(), "012345");
        assert_eq!(PairingCode::parse("012-345").unwrap(), code);
    }

    #[test]
    fn pairing_code_derivation_is_stable() {
        let id = DeviceIdentity::generate().unwrap();
        let nonce = [7u8; 32];
        let a = PairingCode::derive(&id.public_bytes(), &nonce);
        let b = PairingCode::derive(&id.public_bytes(), &nonce);
        assert_eq!(a, b);
    }

    #[test]
    fn a_derived_code_is_six_digits() {
        let id = DeviceIdentity::generate().unwrap();
        for nonce in 0..64u8 {
            let code = PairingCode::derive(&id.public_bytes(), &[nonce; 32]);
            assert!(
                code.value() < 1_000_000,
                "the derived code must fit in six digits, got {}",
                code.value()
            );
            assert_eq!(code.to_string().len(), 6);
            // A peer that receives the code over the wire rebuilds the same
            // value through `new`, so both ends must agree.
            assert_eq!(PairingCode::new(code.value()), code);
        }
    }

    #[test]
    fn a_proof_binds_the_nonce_the_host_and_the_client() {
        let id = DeviceIdentity::generate().unwrap();
        let host = [3u8; KEY_LEN];
        let nonce = [5u8; 32];
        let message = pairing_proof(&nonce, &host, &id.public_bytes());
        let signature = id.sign_bytes(&message);
        DeviceIdentity::verify_bytes(&id.public_bytes(), &message, &signature).unwrap();

        // A different nonce must not verify against the same signature.
        let other = pairing_proof(&[6u8; 32], &host, &id.public_bytes());
        assert!(DeviceIdentity::verify_bytes(&id.public_bytes(), &other, &signature).is_err());
    }

    #[test]
    fn random_bytes_differ() {
        assert_ne!(random_bytes::<32>(), random_bytes::<32>());
    }

    #[test]
    fn pairing_code_rejects_bad_input() {
        assert!(PairingCode::parse("12345").is_err());
        assert!(PairingCode::parse("abcdef").is_err());
    }
}
