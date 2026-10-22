//! The signature of a manifest.
//!
//! The manifest is signed with Ed25519: the signature covers the manifest bytes exactly as they
//! were fetched, so a mirror cannot change one byte.

use ed25519_dalek::{Signature, Signer, Verifier, VerifyingKey};

use crate::Error;

/// Checks `signature` over `body` with the pinned `key`.
///
/// # Errors
/// Returns an error when the key is not a public key or the signature does not verify.
pub fn verify(key: &[u8; 32], body: &[u8], signature: &[u8; 64]) -> Result<(), Error> {
    let verifying = VerifyingKey::from_bytes(key)
        .map_err(|e| Error::Signature(format!("the pinned key is not a public key: {e}")))?;
    let signature = Signature::from_bytes(signature);
    verifying
        .verify(body, &signature)
        .map_err(|e| Error::Signature(e.to_string()))
}

/// Signs `body` with `key`. Only the publisher has the private key; the tests use it.
#[must_use]
pub fn sign(key: &ed25519_dalek::SigningKey, body: &[u8]) -> [u8; 64] {
    key.sign(body).to_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn key_pair() -> (SigningKey, [u8; 32]) {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let public = key.verifying_key().to_bytes();
        (key, public)
    }

    #[test]
    fn a_signed_manifest_verifies() {
        let (key, public) = key_pair();
        let body = b"{\"channel\":\"stable\"}";
        let signature = sign(&key, body);
        assert!(verify(&public, body, &signature).is_ok());
    }

    #[test]
    fn a_changed_manifest_does_not_verify() {
        let (key, public) = key_pair();
        let body = b"{\"channel\":\"stable\"}";
        let signature = sign(&key, body);
        let other = b"{\"channel\":\"nightly\"}";
        assert!(verify(&public, other, &signature).is_err());
    }

    #[test]
    fn another_key_does_not_verify() {
        let (key, _) = key_pair();
        let other = SigningKey::from_bytes(&[9u8; 32]);
        let body = b"manifest";
        let signature = sign(&key, body);
        assert!(verify(&other.verifying_key().to_bytes(), body, &signature).is_err());
    }
}
