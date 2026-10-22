//! Lumen cryptography.
//!
//! The handshake is Noise `XX` with X25519. The transport cipher is
//! AES-256-GCM where the hardware accelerates it and ChaCha20-Poly1305
//! otherwise. Four session keys are derived from the handshake with
//! HKDF-SHA256: one per direction for the control channel, and one per
//! direction for media, so no two (key, nonce) streams ever share a key.

#![forbid(unsafe_code)]

pub mod aead;
pub mod error;
pub mod handshake;
pub mod keys;
pub mod pairing;

pub use aead::{AeadKind, PacketCipher};
pub use error::CryptoError;
pub use handshake::{Handshake, HandshakeResult, Role};
pub use keys::{CONTROL_C2H_INFO, CONTROL_H2C_INFO, MEDIA_C2H_INFO, MEDIA_H2C_INFO, SessionKeys};
pub use pairing::{
    DeviceIdentity, KEY_LEN, PairedClient, PairingCode, SIGNATURE_LEN, pairing_proof, random_bytes,
};
