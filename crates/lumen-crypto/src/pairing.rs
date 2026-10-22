//! Device identity and pairing.
//!
//! The host owns a long-term Ed25519 identity key. A client is authorised
//! either by proving it knows the pairing code the host displays, or by
//! presenting a key that the host already trusts. The code is random, fresh
//! for each attempt, and never sent: both ends run SPAKE2 over it, with the
//! Noise static keys of the connection as the SPAKE2 identities. A third party
//! in the middle holds two connections with different static keys, so even
//! with the right code the two ends derive different keys and the
//! confirmations disagree. punktfunk pairs the same way with its certificate
//! fingerprints (`punktfunk-core/src/quic/pake.rs`, MIT OR Apache-2.0,
//! Copyright (c) 2026 unom - Enrico Bühler).

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use spake2::{Ed25519Group, Identity, Password, Spake2};

use crate::error::CryptoError;
use crate::handshake::Role;
use crate::keys::derive_key;

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

/// The Noise static keys of one connection, client first.
///
/// Both ends of a direct connection see the same pair. A third party in the
/// middle runs one handshake with each end, with its own keys, so the two ends
/// then see different pairs. Everything that proves an identity or the pairing
/// code is bound to this pair, so it cannot be relayed into another connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelBinding {
    /// The client's Noise static public key.
    pub client_static: [u8; 32],
    /// The host's Noise static public key.
    pub host_static: [u8; 32],
}

impl ChannelBinding {
    /// The pair as one byte string, client first.
    #[must_use]
    pub fn bytes(&self) -> [u8; 64] {
        let mut out = [0u8; 64];
        out[..32].copy_from_slice(&self.client_static);
        out[32..].copy_from_slice(&self.host_static);
        out
    }
}

/// The bytes a client signs to prove that it owns its identity key.
///
/// The proof binds the host's nonce, the host's key, the client's key and the
/// connection, so a signature cannot be replayed into another session or
/// relayed by a third party in the middle.
#[must_use]
pub fn pairing_proof(
    nonce: &[u8; 32],
    host_key: &[u8; KEY_LEN],
    device_key: &[u8; KEY_LEN],
    binding: &ChannelBinding,
) -> Vec<u8> {
    let mut message = Vec::with_capacity(22 + 32 + KEY_LEN + KEY_LEN + 64);
    message.extend_from_slice(b"lumen-pairing-proof-v2");
    message.extend_from_slice(nonce);
    message.extend_from_slice(host_key);
    message.extend_from_slice(device_key);
    message.extend_from_slice(&binding.bytes());
    message
}

/// The bytes the host signs to prove that it owns its identity key.
///
/// The client checks this signature against the host key it pinned, so a host
/// that only knows the public key cannot take the pinned host's place.
#[must_use]
pub fn host_proof(
    host_key: &[u8; KEY_LEN],
    device_key: &[u8; KEY_LEN],
    binding: &ChannelBinding,
) -> Vec<u8> {
    let mut message = Vec::with_capacity(19 + KEY_LEN + KEY_LEN + 64);
    message.extend_from_slice(b"lumen-host-proof-v1");
    message.extend_from_slice(host_key);
    message.extend_from_slice(device_key);
    message.extend_from_slice(&binding.bytes());
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

    /// A fresh random code, for one pairing attempt.
    #[must_use]
    pub fn random() -> Self {
        // The modulo bias of a 64-bit draw over one million values is below 2^-44.
        let draw = u64::from_le_bytes(random_bytes::<8>()) % 1_000_000;
        Self(u32::try_from(draw).unwrap_or(0))
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
}

/// Length of a SPAKE2 message over Ed25519: one side byte and one point.
pub const PAKE_MESSAGE_LEN: usize = 33;

/// One side of the SPAKE2 exchange over the pairing code.
pub struct PairingPake {
    state: Spake2<Ed25519Group>,
    binding: ChannelBinding,
}

impl std::fmt::Debug for PairingPake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingPake")
            .field("binding", &self.binding)
            .finish_non_exhaustive()
    }
}

/// The two key confirmations of a finished exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Confirmations {
    /// What the host sends; the client checks it.
    pub host: [u8; 32],
    /// What the client sends; the host checks it.
    pub client: [u8; 32],
}

impl PairingPake {
    /// Start the exchange. Returns the state and the message to send to the peer.
    ///
    /// The client is SPAKE2 side A and the host side B; the identities are the
    /// two Noise static keys.
    #[must_use]
    pub fn start(
        role: Role,
        code: PairingCode,
        binding: &ChannelBinding,
    ) -> (Self, [u8; PAKE_MESSAGE_LEN]) {
        let password = Password::new(code.value().to_le_bytes());
        let client = Identity::new(&binding.client_static);
        let host = Identity::new(&binding.host_static);
        let (state, message) = match role {
            Role::Initiator => Spake2::<Ed25519Group>::start_a(&password, &client, &host),
            Role::Responder => Spake2::<Ed25519Group>::start_b(&password, &client, &host),
        };
        let mut out = [0u8; PAKE_MESSAGE_LEN];
        out.copy_from_slice(&message);
        (
            Self {
                state,
                binding: *binding,
            },
            out,
        )
    }

    /// Finish the exchange with the peer's message.
    ///
    /// A wrong code does not fail here: it yields a different key, and the
    /// confirmations of the two ends then disagree.
    ///
    /// # Errors
    /// Returns [`CryptoError::Pairing`] when the peer's message is malformed.
    pub fn finish(self, peer: &[u8; PAKE_MESSAGE_LEN]) -> Result<Confirmations, CryptoError> {
        let key = self
            .state
            .finish(peer)
            .map_err(|_| CryptoError::Pairing("malformed pairing exchange"))?;
        let confirm = |label: &[u8]| {
            let mut info = Vec::with_capacity(label.len() + 64);
            info.extend_from_slice(label);
            info.extend_from_slice(&self.binding.bytes());
            derive_key(&key, &info)
        };
        Ok(Confirmations {
            host: confirm(b"lumen-pair-host"),
            client: confirm(b"lumen-pair-client"),
        })
    }
}

/// Compares two confirmations in constant time.
#[must_use]
pub fn confirmation_matches(expected: &[u8; 32], got: &[u8; 32]) -> bool {
    expected
        .iter()
        .zip(got)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
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

    const DIRECT: ChannelBinding = ChannelBinding {
        client_static: [0x11; 32],
        host_static: [0x22; 32],
    };

    /// Runs both sides of the exchange, each with its own code and view of the connection.
    fn exchange(
        client: (PairingCode, &ChannelBinding),
        host: (PairingCode, &ChannelBinding),
    ) -> (Confirmations, Confirmations) {
        let (a, message_a) = PairingPake::start(Role::Initiator, client.0, client.1);
        let (b, message_b) = PairingPake::start(Role::Responder, host.0, host.1);
        (a.finish(&message_b).unwrap(), b.finish(&message_a).unwrap())
    }

    #[test]
    fn the_exchange_agrees_only_on_the_same_code_and_connection() {
        let code = PairingCode::new(4321);
        let (client, host) = exchange((code, &DIRECT), (code, &DIRECT));
        assert!(confirmation_matches(&client.client, &host.client));
        assert!(confirmation_matches(&client.host, &host.host));

        let (client, host) = exchange((PairingCode::new(0), &DIRECT), (code, &DIRECT));
        assert!(!confirmation_matches(&client.client, &host.client));
        assert!(!confirmation_matches(&client.host, &host.host));

        // A relay in the middle: the right code, but each end sees the relay's static key.
        let client_view = ChannelBinding {
            host_static: [0x33; 32],
            ..DIRECT
        };
        let host_view = ChannelBinding {
            client_static: [0x44; 32],
            ..DIRECT
        };
        let (client, host) = exchange((code, &client_view), (code, &host_view));
        assert!(!confirmation_matches(&client.client, &host.client));
        assert!(!confirmation_matches(&client.host, &host.host));
    }

    #[test]
    fn a_malformed_message_is_refused() {
        let (pake, _) = PairingPake::start(Role::Responder, PairingCode::new(1), &DIRECT);
        assert!(pake.finish(&[0u8; PAKE_MESSAGE_LEN]).is_err());
    }

    #[test]
    fn a_proof_binds_the_nonce_the_keys_and_the_connection() {
        let id = DeviceIdentity::generate().unwrap();
        let host = [3u8; KEY_LEN];
        let nonce = [5u8; 32];
        let message = pairing_proof(&nonce, &host, &id.public_bytes(), &DIRECT);
        let signature = id.sign_bytes(&message);
        DeviceIdentity::verify_bytes(&id.public_bytes(), &message, &signature).unwrap();

        let other = pairing_proof(&[6u8; 32], &host, &id.public_bytes(), &DIRECT);
        assert!(DeviceIdentity::verify_bytes(&id.public_bytes(), &other, &signature).is_err());
        let relayed = ChannelBinding {
            client_static: [0x44; 32],
            ..DIRECT
        };
        let other = pairing_proof(&nonce, &host, &id.public_bytes(), &relayed);
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
