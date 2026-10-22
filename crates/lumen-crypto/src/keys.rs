//! Key separation.
//!
//! One Noise handshake produces one shared secret. Four independent keys and
//! a session id are derived from it with HKDF-SHA256. Each key belongs to one
//! channel and one direction, so the two directions never share a
//! (key, nonce) stream.

use hkdf::Hkdf;
use sha2::Sha256;

/// Domain separator for control records sent by the host.
pub const CONTROL_H2C_INFO: &[u8] = b"lumen-control-h2c-v1";
/// Domain separator for control records sent by the client.
pub const CONTROL_C2H_INFO: &[u8] = b"lumen-control-c2h-v1";
/// Domain separator for media sent by the host.
pub const MEDIA_H2C_INFO: &[u8] = b"lumen-media-h2c-v1";
/// Domain separator for media sent by the client.
pub const MEDIA_C2H_INFO: &[u8] = b"lumen-media-c2h-v1";
/// Domain separator for the audio sent by the host.
pub const AUDIO_H2C_INFO: &[u8] = b"lumen-audio-h2c-v1";
/// Domain separator for the audio sent by the client.
pub const AUDIO_C2H_INFO: &[u8] = b"lumen-audio-c2h-v1";
/// Domain separator for the session identifier.
pub const SESSION_ID_INFO: &[u8] = b"lumen-session-id-v1";

/// The session keys and identifier derived from a handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionKeys {
    /// Identifier of this session, bound into every nonce.
    pub session_id: u32,
    /// Key for control records the host encrypts.
    pub control_h2c: [u8; 32],
    /// Key for control records the client encrypts.
    pub control_c2h: [u8; 32],
    /// Key for media sent by the host (video and audio).
    pub media_h2c: [u8; 32],
    /// Key for media sent by the client (input).
    pub media_c2h: [u8; 32],
    /// Key for the audio sent by the host.
    pub audio_h2c: [u8; 32],
    /// Key for the audio sent by the client.
    pub audio_c2h: [u8; 32],
}

impl SessionKeys {
    /// Derive the session keys and identifier from the handshake key material.
    #[must_use]
    pub fn derive(shared_secret: &[u8]) -> Self {
        let id = derive_key(shared_secret, SESSION_ID_INFO);
        Self {
            session_id: u32::from_le_bytes([id[0], id[1], id[2], id[3]]),
            control_h2c: derive_key(shared_secret, CONTROL_H2C_INFO),
            control_c2h: derive_key(shared_secret, CONTROL_C2H_INFO),
            media_h2c: derive_key(shared_secret, MEDIA_H2C_INFO),
            media_c2h: derive_key(shared_secret, MEDIA_C2H_INFO),
            audio_h2c: derive_key(shared_secret, AUDIO_H2C_INFO),
            audio_c2h: derive_key(shared_secret, AUDIO_C2H_INFO),
        }
    }
}

/// Derive one 32-byte key from a secret and a domain separator.
#[must_use]
pub fn derive_key(shared_secret: &[u8], info: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(None, shared_secret);
    let mut out = [0u8; 32];
    hk.expand(info, &mut out)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_channel_and_direction_has_its_own_key() {
        let k = SessionKeys::derive(b"secret");
        let all = [
            k.control_h2c,
            k.control_c2h,
            k.media_h2c,
            k.media_c2h,
            k.audio_h2c,
            k.audio_c2h,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn derivation_is_deterministic() {
        let a = SessionKeys::derive(b"secret");
        let b = SessionKeys::derive(b"secret");
        assert_eq!(a, b);
    }

    #[test]
    fn different_secrets_give_different_keys_and_ids() {
        let a = SessionKeys::derive(b"secret-a");
        let b = SessionKeys::derive(b"secret-b");
        assert_ne!(a.control_h2c, b.control_h2c);
        assert_ne!(a.media_c2h, b.media_c2h);
        assert_ne!(a.session_id, b.session_id);
    }
}
