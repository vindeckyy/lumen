//! The pinned host.
//!
//! The client remembers the host it paired with and refuses a different one. The host names its
//! Ed25519 public key in every session and signs the connection with it, so the pin is a
//! fingerprint the user can check, and a host that takes the place of the pinned one is refused
//! instead of trusted.
//!
//! The first pin is only taken from a host that proved, in the same session, the pairing code
//! the user typed (SPAKE2 bound to the connection's static keys). punktfunk pins the same way,
//! with the certificate fingerprints bound into the pairing key (MIT OR Apache-2.0, Copyright
//! (c) 2026 unom - Enrico Bühler).

use std::path::Path;

/// The pinned host of a client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostPin {
    /// No host has been pinned yet; the first one is.
    None,
    /// The pinned host's public key.
    Key([u8; 32]),
}

impl HostPin {
    /// Reads the pin from `path`. A missing file means no host has been pinned.
    ///
    /// # Errors
    /// Returns a description when the file cannot be read or does not hold a key.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let bytes = hex_decode(text.trim())
                    .ok_or_else(|| format!("{} does not hold a host key", path.display()))?;
                let key: [u8; 32] = bytes
                    .try_into()
                    .map_err(|_| format!("{} does not hold a 32 byte key", path.display()))?;
                Ok(Self::Key(key))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::None),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    /// Checks `key` against the pin and pins it when there is none.
    ///
    /// Returns the key when the host is the pinned one. `Err` names the host that was expected,
    /// so the user can see what changed.
    pub fn check(&mut self, path: &Path, key: [u8; 32]) -> Result<[u8; 32], String> {
        match self {
            Self::None => {
                *self = Self::Key(key);
                self.store(path, key)?;
                Ok(key)
            }
            Self::Key(pinned) if *pinned == key => Ok(key),
            Self::Key(pinned) => Err(format!(
                "the host is not the one this client paired with: it presents {} but {} was pinned",
                fingerprint(key),
                fingerprint(*pinned)
            )),
        }
    }

    /// Writes the pin to `path`.
    ///
    /// # Errors
    /// Returns a description when the file cannot be written.
    pub fn store(&self, path: &Path, key: [u8; 32]) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let text: String = key.iter().fold(String::new(), |mut text, byte| {
            use std::fmt::Write;
            let _ = write!(text, "{byte:02x}");
            text
        });
        std::fs::write(path, format!("{text}\n"))
            .map_err(|e| format!("cannot write {}: {e}", path.display()))
    }
}

/// The short form of a key, for the log.
#[must_use]
pub fn fingerprint(key: [u8; 32]) -> String {
    key[..8].iter().fold(String::new(), |mut text, byte| {
        use std::fmt::Write;
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// The bytes of a hex text.
fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len() / 2)
        .map(|index| u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lumen-pin-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join("host")
    }

    #[test]
    fn the_first_host_is_pinned() {
        let path = temp_path("first");
        let mut pin = HostPin::load(&path).expect("load");
        assert_eq!(pin, HostPin::None);
        assert_eq!(pin.check(&path, [7u8; 32]).expect("pin"), [7u8; 32]);
        // The pin is on disk and the same host is accepted again.
        let reloaded = HostPin::load(&path).expect("reload");
        assert_eq!(reloaded, HostPin::Key([7u8; 32]));
        let mut pin = reloaded;
        assert!(pin.check(&path, [7u8; 32]).is_ok());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_different_host_is_refused() {
        let path = temp_path("other");
        let mut pin = HostPin::load(&path).expect("load");
        pin.check(&path, [7u8; 32]).expect("pin");
        let error = pin.check(&path, [9u8; 32]).expect_err("refused");
        assert!(
            error.contains("not the one this client paired with"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
