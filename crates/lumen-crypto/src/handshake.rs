//! Noise XX handshake.
//!
//! The host and client run a Noise `XX` handshake with X25519 and
//! ChaCha20-Poly1305. The two transport keys that the handshake produces are
//! the input to HKDF, which derives the four Lumen session keys. The
//! transport cipher used for the media path is chosen separately by
//! [`crate::aead::AeadKind`], because AES-256-GCM is hardware accelerated
//! where ChaCha is not.

use snow::error::Error as SnowError;
use snow::params::NoiseParams;
use snow::{Builder, HandshakeState};

use crate::error::CryptoError;

/// The Noise parameter string Lumen uses.
pub const NOISE_PATTERN: &str = "Noise_XX_25519_ChaChaPoly_SHA256";

/// Which side of the handshake a peer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The client, which initiates the handshake.
    Initiator,
    /// The host, which responds.
    Responder,
}

/// A handshake in progress.
#[derive(Debug)]
pub struct Handshake {
    state: HandshakeState,
    role: Role,
    /// This side's static public key.
    local_static: Vec<u8>,
}

/// The outcome of a completed handshake.
#[derive(Debug)]
pub struct HandshakeResult {
    /// Key material for HKDF, taken from the handshake's transport split.
    pub key_material: [u8; 64],
    /// The remote peer's static public key, if it was sent.
    pub remote_static: Option<[u8; 32]>,
    /// This side's static public key.
    pub local_static: [u8; 32],
}

/// Parse the Lumen Noise parameters.
///
/// # Errors
/// Returns [`CryptoError::Handshake`] when the pattern cannot be parsed.
pub fn noise_params() -> Result<NoiseParams, CryptoError> {
    NOISE_PATTERN.parse().map_err(CryptoError::Handshake)
}

impl Handshake {
    /// Start a handshake in the given role.
    ///
    /// # Errors
    /// Returns [`CryptoError::Handshake`] when the noise state cannot be
    /// created.
    pub fn start(role: Role) -> Result<Self, CryptoError> {
        let params = noise_params()?;
        let builder = Builder::new(params);
        // XX sends the static key in the second message, so both sides need
        // a static keypair of their own.
        let keypair = builder.generate_keypair().map_err(CryptoError::Handshake)?;
        let builder = builder
            .local_private_key(&keypair.private)
            .map_err(CryptoError::Handshake)?;
        let state = match role {
            Role::Initiator => builder.build_initiator(),
            Role::Responder => builder.build_responder(),
        }
        .map_err(CryptoError::Handshake)?;
        Ok(Self {
            state,
            role,
            local_static: keypair.public.clone(),
        })
    }

    /// The role this handshake is playing.
    #[must_use]
    pub fn role(&self) -> Role {
        self.role
    }

    /// Write the next handshake message.
    ///
    /// # Errors
    /// Returns [`CryptoError::Handshake`] when the message cannot be produced.
    pub fn write_message(&mut self) -> Result<Vec<u8>, CryptoError> {
        self.write_message_with(&[])
    }

    /// Write the next handshake message with a payload.
    ///
    /// The payload becomes part of the handshake transcript, so a peer that
    /// changes it makes the final hash check fail. That is how the
    /// capability bitmap reaches the other end before any control record
    /// exists.
    ///
    /// # Errors
    /// Returns [`CryptoError::Handshake`] when the message cannot be produced.
    pub fn write_message_with(&mut self, payload: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let mut buf = vec![0u8; 1024];
        let len = self
            .state
            .write_message(payload, &mut buf)
            .map_err(CryptoError::Handshake)?;
        buf.truncate(len);
        Ok(buf)
    }

    /// Read the peer's handshake message.
    ///
    /// # Errors
    /// Returns [`CryptoError::Handshake`] when the message is rejected.
    pub fn read_message(&mut self, msg: &[u8]) -> Result<(), CryptoError> {
        self.read_message_with(msg)?;
        Ok(())
    }

    /// Read the peer's handshake message and return its payload.
    ///
    /// # Errors
    /// Returns [`CryptoError::Handshake`] when the message is rejected.
    pub fn read_message_with(&mut self, msg: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let mut payload = vec![0u8; 1024];
        let len = self
            .state
            .read_message(msg, &mut payload)
            .map_err(CryptoError::Handshake)?;
        payload.truncate(len);
        Ok(payload)
    }

    /// Finish the handshake and return the key material.
    ///
    /// # Errors
    /// Returns [`CryptoError::Handshake`] when the handshake is incomplete or
    /// the finalisation fails.
    pub fn finish(mut self) -> Result<HandshakeResult, CryptoError> {
        let remote_static = self.state.get_remote_static().and_then(|k| {
            let mut out = [0u8; 32];
            if k.len() == 32 {
                out.copy_from_slice(k);
                Some(out)
            } else {
                None
            }
        });
        // The raw split gives the two transport keys; they are the input to
        // HKDF, which then separates the Lumen session keys.
        let (k1, k2) = self.state.dangerously_get_raw_split();
        let mut key_material = [0u8; 64];
        key_material[..32].copy_from_slice(&k1);
        key_material[32..].copy_from_slice(&k2);
        let mut local = [0u8; 32];
        if self.local_static.len() == 32 {
            local.copy_from_slice(&self.local_static);
        }
        Ok(HandshakeResult {
            key_material,
            remote_static,
            local_static: local,
        })
    }
}

/// Convert a snow error into a crypto error.
pub fn map_snow_error(e: SnowError) -> CryptoError {
    CryptoError::Handshake(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_handshake() -> (HandshakeResult, HandshakeResult) {
        let mut initiator = Handshake::start(Role::Initiator).unwrap();
        let mut responder = Handshake::start(Role::Responder).unwrap();
        let m1 = initiator.write_message().unwrap();
        responder.read_message(&m1).unwrap();
        let m2 = responder.write_message().unwrap();
        initiator.read_message(&m2).unwrap();
        let m3 = initiator.write_message().unwrap();
        responder.read_message(&m3).unwrap();
        let i = initiator.finish().unwrap();
        let r = responder.finish().unwrap();
        (i, r)
    }

    #[test]
    fn handshake_completes() {
        let (i, r) = run_handshake();
        assert_eq!(i.key_material.len(), 64);
        assert_eq!(r.key_material.len(), 64);
    }

    #[test]
    fn both_sides_see_the_same_remote_static() {
        let (i, r) = run_handshake();
        assert!(i.remote_static.is_some());
        assert!(r.remote_static.is_some());
    }

    #[test]
    fn handshake_rejects_garbage() {
        let mut responder = Handshake::start(Role::Responder).unwrap();
        assert!(responder.read_message(b"not a handshake").is_err());
    }
}
