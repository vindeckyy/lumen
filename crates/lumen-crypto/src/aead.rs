//! AEAD for media packets.
//!
//! AES-256-GCM is the default because the target hardware has AES-NI and
//! PCLMULQDQ. ChaCha20-Poly1305 is the fallback for machines without AES
//! acceleration; it is pure Rust and still fast, but not accelerated by the
//! same hardware.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use chacha20poly1305::{ChaCha20Poly1305, Nonce as ChaChaNonce};

use crate::error::CryptoError;

/// Which AEAD a session uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AeadKind {
    /// AES-256-GCM, hardware accelerated when AES-NI is present.
    Aes256Gcm,
    /// ChaCha20-Poly1305, portable.
    ChaCha20Poly1305,
}

impl AeadKind {
    /// Pick the AEAD for this machine: AES-GCM when the CPU has AES-NI.
    #[must_use]
    pub fn preferred() -> Self {
        if std::arch::is_x86_feature_detected!("aes")
            && std::arch::is_x86_feature_detected!("pclmulqdq")
        {
            Self::Aes256Gcm
        } else {
            Self::ChaCha20Poly1305
        }
    }

    /// Nonce length in bytes.
    #[must_use]
    pub const fn nonce_len(self) -> usize {
        12
    }

    /// Authentication tag length in bytes.
    #[must_use]
    pub const fn tag_len(self) -> usize {
        16
    }
}

/// A per-direction packet cipher.
#[derive(Debug, Clone)]
pub struct PacketCipher {
    kind: AeadKind,
    key: [u8; 32],
    /// Monotonic counter, never reused within a session.
    counter: u64,
    /// Session id mixed into the nonce so a new session never reuses a nonce.
    session_id: u32,
}

impl PacketCipher {
    /// Create a cipher for one direction.
    #[must_use]
    pub fn new(kind: AeadKind, key: [u8; 32], session_id: u32) -> Self {
        Self {
            kind,
            key,
            counter: 0,
            session_id,
        }
    }

    /// Number of packets sealed so far.
    #[must_use]
    pub fn counter(&self) -> u64 {
        self.counter
    }

    /// Build the nonce for the current counter value.
    ///
    /// Layout: 8 bytes of counter, then 4 bytes of session id.
    fn nonce(&self, counter: u64) -> [u8; 12] {
        let mut nonce = [0u8; 12];
        nonce[..8].copy_from_slice(&counter.to_le_bytes());
        nonce[8..].copy_from_slice(&self.session_id.to_le_bytes());
        nonce
    }

    /// Seal a packet.
    ///
    /// The header bytes are passed as associated data so that the header is
    /// authenticated but not encrypted.
    ///
    /// # Errors
    /// Returns [`CryptoError::Aead`] when encryption fails.
    pub fn seal(&mut self, header: &[u8], payload: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let counter = self.counter;
        self.counter += 1;
        let nonce = self.nonce(counter);
        let out = match self.kind {
            AeadKind::Aes256Gcm => {
                let cipher = Aes256Gcm::new_from_slice(&self.key)
                    .map_err(|_| CryptoError::Aead("aes-gcm key init"))?;
                cipher
                    .encrypt(
                        &Nonce::from(nonce),
                        Payload {
                            msg: payload,
                            aad: header,
                        },
                    )
                    .map_err(|_| CryptoError::Aead("aes-gcm encrypt"))?
            }
            AeadKind::ChaCha20Poly1305 => {
                let cipher = ChaCha20Poly1305::new_from_slice(&self.key)
                    .map_err(|_| CryptoError::Aead("chacha key init"))?;
                cipher
                    .encrypt(
                        &ChaChaNonce::from(nonce),
                        Payload {
                            msg: payload,
                            aad: header,
                        },
                    )
                    .map_err(|_| CryptoError::Aead("chacha encrypt"))?
            }
        };
        let mut packet = Vec::with_capacity(header.len() + out.len());
        packet.extend_from_slice(header);
        packet.extend_from_slice(&out);
        Ok(packet)
    }

    /// Seal a packet with an explicit counter.
    ///
    /// The caller chooses the counter, so the peer can use a value that
    /// travels in the clear. The audio path does that: the sequence number
    /// is in the audio header, which is authenticated as associated data.
    ///
    /// # Errors
    /// Returns [`CryptoError::Aead`] when encryption fails.
    pub fn seal_with(
        &self,
        header: &[u8],
        payload: &[u8],
        counter: u64,
    ) -> Result<Vec<u8>, CryptoError> {
        let nonce = self.nonce(counter);
        let out = match self.kind {
            AeadKind::Aes256Gcm => {
                let cipher = Aes256Gcm::new_from_slice(&self.key)
                    .map_err(|_| CryptoError::Aead("aes-gcm key init"))?;
                cipher
                    .encrypt(
                        &Nonce::from(nonce),
                        Payload {
                            msg: payload,
                            aad: header,
                        },
                    )
                    .map_err(|_| CryptoError::Aead("aes-gcm encrypt"))?
            }
            AeadKind::ChaCha20Poly1305 => {
                let cipher = ChaCha20Poly1305::new_from_slice(&self.key)
                    .map_err(|_| CryptoError::Aead("chacha key init"))?;
                cipher
                    .encrypt(
                        &ChaChaNonce::from(nonce),
                        Payload {
                            msg: payload,
                            aad: header,
                        },
                    )
                    .map_err(|_| CryptoError::Aead("chacha encrypt"))?
            }
        };
        let mut packet = Vec::with_capacity(header.len() + out.len());
        packet.extend_from_slice(header);
        packet.extend_from_slice(&out);
        Ok(packet)
    }

    /// Open a packet that was sealed by the peer.
    ///
    /// The caller supplies the header bytes and the ciphertext, and the
    /// nonce counter.
    ///
    /// # Errors
    /// Returns [`CryptoError::Aead`] when authentication fails.
    pub fn open(
        &self,
        header: &[u8],
        ciphertext: &[u8],
        counter: u64,
    ) -> Result<Vec<u8>, CryptoError> {
        let nonce = self.nonce(counter);
        match self.kind {
            AeadKind::Aes256Gcm => {
                let cipher = Aes256Gcm::new_from_slice(&self.key)
                    .map_err(|_| CryptoError::Aead("aes-gcm key init"))?;
                cipher
                    .decrypt(
                        &Nonce::from(nonce),
                        Payload {
                            msg: ciphertext,
                            aad: header,
                        },
                    )
                    .map_err(|_| CryptoError::Aead("aes-gcm decrypt"))
            }
            AeadKind::ChaCha20Poly1305 => {
                let cipher = ChaCha20Poly1305::new_from_slice(&self.key)
                    .map_err(|_| CryptoError::Aead("chacha key init"))?;
                cipher
                    .decrypt(
                        &ChaChaNonce::from(nonce),
                        Payload {
                            msg: ciphertext,
                            aad: header,
                        },
                    )
                    .map_err(|_| CryptoError::Aead("chacha decrypt"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_and_open_roundtrip() {
        let mut sealer = PacketCipher::new(AeadKind::Aes256Gcm, [7u8; 32], 42);
        let opener = PacketCipher::new(AeadKind::Aes256Gcm, [7u8; 32], 42);
        let header = b"header-bytes";
        let packet = sealer.seal(header, b"payload").unwrap();
        let open = opener
            .open(&packet[..header.len()], &packet[header.len()..], 0)
            .unwrap();
        assert_eq!(open, b"payload");
    }

    #[test]
    fn counter_never_repeats() {
        let mut sealer = PacketCipher::new(AeadKind::Aes256Gcm, [1u8; 32], 1);
        for _ in 0..100 {
            sealer.seal(b"h", b"p").unwrap();
        }
        assert_eq!(sealer.counter(), 100);
    }

    #[test]
    fn wrong_key_fails_authentication() {
        let mut sealer = PacketCipher::new(AeadKind::Aes256Gcm, [1u8; 32], 1);
        let opener = PacketCipher::new(AeadKind::Aes256Gcm, [2u8; 32], 1);
        let packet = sealer.seal(b"h", b"p").unwrap();
        assert!(opener.open(&packet[..1], &packet[1..], 0).is_err());
    }

    #[test]
    fn tampered_header_fails_authentication() {
        let mut sealer = PacketCipher::new(AeadKind::Aes256Gcm, [1u8; 32], 1);
        let opener = PacketCipher::new(AeadKind::Aes256Gcm, [1u8; 32], 1);
        let packet = sealer.seal(b"h", b"p").unwrap();
        let mut bad = packet.clone();
        bad[0] ^= 0xff;
        assert!(opener.open(&bad[..1], &bad[1..], 0).is_err());
    }

    #[test]
    fn chacha_path_also_works() {
        let mut sealer = PacketCipher::new(AeadKind::ChaCha20Poly1305, [3u8; 32], 5);
        let opener = PacketCipher::new(AeadKind::ChaCha20Poly1305, [3u8; 32], 5);
        let packet = sealer.seal(b"h", b"p").unwrap();
        let out = opener.open(&packet[..1], &packet[1..], 0).unwrap();
        assert_eq!(out, b"p");
    }

    #[test]
    fn session_id_changes_the_nonce() {
        let mut a = PacketCipher::new(AeadKind::Aes256Gcm, [1u8; 32], 1);
        let mut b = PacketCipher::new(AeadKind::Aes256Gcm, [1u8; 32], 2);
        let pa = a.seal(b"h", b"p").unwrap();
        let pb = b.seal(b"h", b"p").unwrap();
        assert_ne!(pa, pb);
    }
}
