//! The host's identity and the clients it trusts.
//!
//! The host owns a long-term Ed25519 identity key. A client is authorised
//! either by being in the trust store already, or by answering the pairing
//! code the host shows for that handshake attempt.
//!
//! The store is one JSON file holding the host's private key and the public
//! keys of the clients it trusts. It is written with owner-only permissions.

use std::path::{Path, PathBuf};

use lumen_crypto::{DeviceIdentity, KEY_LEN};
use serde::{Deserialize, Serialize};

/// Errors from the pairing store.
#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    /// The store could not be read or written.
    #[error("pairing store {path}: {source}")]
    Io {
        /// The file the operation failed on.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The store is not valid JSON or holds a bad key.
    #[error("pairing store {path}: {reason}")]
    Invalid {
        /// The file that is wrong.
        path: PathBuf,
        /// What is wrong with it.
        reason: String,
    },
    /// The identity key could not be used.
    #[error("identity key: {0}")]
    Identity(#[from] lumen_crypto::CryptoError),
}

/// The on-disk form of the store.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StoreFile {
    /// The host's Ed25519 private key, hex encoded.
    identity: String,
    /// The public keys of the trusted clients, hex encoded.
    clients: Vec<String>,
}

/// The host's identity and the clients it trusts.
#[derive(Debug)]
pub struct PairingStore {
    identity: DeviceIdentity,
    clients: Vec<[u8; KEY_LEN]>,
    path: PathBuf,
}

impl PairingStore {
    /// Load the store, creating it with a fresh identity when it does not exist.
    ///
    /// # Errors
    /// Returns an error when the file cannot be read, is not valid, or
    /// cannot be written.
    pub fn load(path: &Path) -> Result<Self, PairingError> {
        if path.exists() {
            let text = std::fs::read_to_string(path).map_err(|source| PairingError::Io {
                path: path.to_path_buf(),
                source,
            })?;
            let file: StoreFile =
                serde_json::from_str(&text).map_err(|e| PairingError::Invalid {
                    path: path.to_path_buf(),
                    reason: e.to_string(),
                })?;
            let identity_bytes =
                hex_decode(&file.identity).ok_or_else(|| PairingError::Invalid {
                    path: path.to_path_buf(),
                    reason: "the identity key is not hex".to_string(),
                })?;
            let identity = DeviceIdentity::from_bytes(&identity_bytes)?;
            let mut clients = Vec::with_capacity(file.clients.len());
            for client in &file.clients {
                let bytes = hex_decode(client).ok_or_else(|| PairingError::Invalid {
                    path: path.to_path_buf(),
                    reason: "a client key is not hex".to_string(),
                })?;
                let bytes: [u8; KEY_LEN] = bytes.try_into().map_err(|_| PairingError::Invalid {
                    path: path.to_path_buf(),
                    reason: "a client key has the wrong length".to_string(),
                })?;
                clients.push(bytes);
            }
            Ok(Self {
                identity,
                clients,
                path: path.to_path_buf(),
            })
        } else {
            let identity = DeviceIdentity::generate()?;
            let store = Self {
                identity,
                clients: Vec::new(),
                path: path.to_path_buf(),
            };
            store.save()?;
            Ok(store)
        }
    }

    /// The host's Ed25519 public key.
    #[must_use]
    pub fn public_bytes(&self) -> [u8; KEY_LEN] {
        self.identity.public_bytes()
    }

    /// Whether the host already trusts this client key.
    #[must_use]
    pub fn is_trusted(&self, device_key: &[u8; KEY_LEN]) -> bool {
        self.clients.contains(device_key)
    }

    /// Trust a client and write the store.
    ///
    /// # Errors
    /// Returns an error when the store cannot be written.
    pub fn trust(&mut self, device_key: [u8; KEY_LEN]) -> Result<(), PairingError> {
        if !self.clients.contains(&device_key) {
            self.clients.push(device_key);
            self.save()?;
        }
        Ok(())
    }

    /// Stop trusting a client.
    ///
    /// The client has to answer the pairing code again the next time it
    /// connects.
    ///
    /// # Errors
    /// Returns the error when the store cannot be written.
    pub fn untrust(&mut self, device_key: &[u8; KEY_LEN]) -> Result<(), PairingError> {
        let before = self.clients.len();
        self.clients.retain(|key| key != device_key);
        if self.clients.len() != before {
            self.save()?;
        }
        Ok(())
    }

    /// The number of trusted clients.
    #[must_use]
    pub fn trusted_count(&self) -> usize {
        self.clients.len()
    }

    /// The public keys of the trusted clients.
    #[must_use]
    pub fn trusted_clients(&self) -> Vec<[u8; KEY_LEN]> {
        self.clients.clone()
    }

    /// The host's identity, which signs the answers of every session.
    #[must_use]
    pub fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    /// Write the store to its file.
    fn save(&self) -> Result<(), PairingError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| PairingError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let file = StoreFile {
            identity: hex_encode(&self.identity.to_bytes()),
            clients: self
                .clients
                .iter()
                .map(|client| hex_encode(client))
                .collect(),
        };
        let text = serde_json::to_string_pretty(&file).map_err(|e| PairingError::Invalid {
            path: self.path.clone(),
            reason: e.to_string(),
        })?;
        write_owner_only(&self.path, format!("{text}\n").as_bytes())?;
        Ok(())
    }
}

/// Load or create a device identity at `path`.
///
/// The file holds the hex encoded Ed25519 private key.
///
/// # Errors
/// Returns an error when the file cannot be read or written, or when it does
/// not hold a valid key.
pub fn load_identity(path: &Path) -> Result<DeviceIdentity, PairingError> {
    if path.exists() {
        let text = std::fs::read_to_string(path).map_err(|source| PairingError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let bytes = hex_decode(text.trim()).ok_or_else(|| PairingError::Invalid {
            path: path.to_path_buf(),
            reason: "the identity key is not hex".to_string(),
        })?;
        return Ok(DeviceIdentity::from_bytes(&bytes)?);
    }
    let identity = DeviceIdentity::generate()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| PairingError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    write_owner_only(path, hex_encode(&identity.to_bytes()).as_bytes())?;
    Ok(identity)
}

/// Write a file that only its owner may read, atomically.
///
/// The contents go to a temporary file in the same directory, which is renamed over the target
/// afterwards: a crash halfway through leaves the old file, not a half-written one. The mode is
/// set before the rename, so the secret is never briefly readable by others.
pub(crate) fn write_owner_only(path: &Path, contents: &[u8]) -> Result<(), PairingError> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().map_or_else(
        || "lumen-secret".to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let temporary = directory.join(format!(".{name}.tmp.{}", std::process::id()));
    let io = |source| PairingError::Io {
        path: path.to_path_buf(),
        source,
    };
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&temporary)?;
        restrict_permissions(&temporary);
        std::io::Write::write_all(&mut file, contents)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if let Err(source) = result {
        let _ = std::fs::remove_file(&temporary);
        return Err(io(source));
    }
    Ok(())
}

/// Make a file readable and writable by its owner only.
fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Encode bytes as lower case hex.
#[must_use]
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

/// Decode lower case or upper case hex.
///
/// Returns `None` when the input is not hex or has an odd length.
#[must_use]
pub fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(text.len() / 2);
    for [high, low] in bytes.as_chunks::<2>().0 {
        let high = (*high as char).to_digit(16)?;
        let low = (*low as char).to_digit(16)?;
        out.push((high * 16 + low) as u8);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lumen-pairing-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("pairing.json")
    }

    #[test]
    fn a_new_store_has_a_fresh_identity_and_no_clients() {
        let path = temp_path("new");
        let store = PairingStore::load(&path).expect("create the store");
        assert_eq!(store.trusted_count(), 0);
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_trusted_client_is_recognised_after_a_reload() {
        let path = temp_path("trust");
        let key = [3u8; KEY_LEN];
        let mut store = PairingStore::load(&path).expect("create the store");
        assert!(!store.is_trusted(&key));
        store.trust(key).expect("trust the client");
        assert!(store.is_trusted(&key));

        let reloaded = PairingStore::load(&path).expect("reload the store");
        assert!(reloaded.is_trusted(&key));
        assert_eq!(reloaded.public_bytes(), store.public_bytes());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn hex_roundtrips() {
        let bytes = [0u8, 1, 15, 16, 255];
        let text = hex_encode(&bytes);
        assert_eq!(text, "00010f10ff");
        assert_eq!(hex_decode(&text).unwrap(), bytes);
        assert!(hex_decode("0f0").is_none());
        assert!(hex_decode("zz").is_none());
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lumen-pairing-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn a_secret_file_is_owner_only_and_complete() {
        let dir = temp_dir("secret");
        let path = dir.join("store.json");
        write_owner_only(&path, b"{\"a\": 1}\n").expect("write");
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"a\": 1}\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the file is owner-only");
        }
        // No temporary file is left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("tmp"))
            .collect();
        assert!(leftovers.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_write_replaces_the_file() {
        let dir = temp_dir("replace");
        let path = dir.join("store.json");
        write_owner_only(&path, b"first\n").expect("write");
        write_owner_only(&path, b"second\n").expect("write");
        assert_eq!(std::fs::read(&path).unwrap(), b"second\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
