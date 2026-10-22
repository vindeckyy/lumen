//! Check whether a newer Lumen build exists.
//!
//! The check is signed: the manifest is fetched over plain HTTP and verified with an Ed25519
//! key that is pinned in the binary. The signature is what makes the check safe, so the
//! transport does not have to be: a mirror that answers with another manifest cannot forge one.
//!
//! The rules are punktfunk's `pf-update-check` (MIT OR Apache-2.0, Copyright (c) 2026 unom -
//! Enrico Bühler): the signature is checked before anything in the manifest is read, the serial
//! is a floor so an old manifest cannot be replayed, and a manifest that does not parse is
//! refused rather than guessed at.

use std::fmt;

pub mod feed;
pub mod floor;
pub mod manifest;
pub mod sig;
pub mod version;

/// Why a check failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The manifest could not be fetched.
    Fetch(String),
    /// The manifest is not the signed bytes the signature covers.
    Signature(String),
    /// The manifest does not parse.
    Manifest(String),
    /// The manifest is older than the one already seen.
    Replayed {
        /// The serial of the manifest that was refused.
        got: u64,
        /// The serial already seen.
        floor: u64,
    },
    /// The channel in the manifest is not the one that was asked for.
    Channel {
        /// The channel the manifest names.
        got: String,
        /// The channel that was asked for.
        wanted: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fetch(why) => write!(f, "cannot fetch the manifest: {why}"),
            Self::Signature(why) => write!(f, "the manifest signature is not valid: {why}"),
            Self::Manifest(why) => write!(f, "the manifest is not valid: {why}"),
            Self::Replayed { got, floor } => {
                write!(
                    f,
                    "the manifest is older than the one already seen: {got} < {floor}"
                )
            }
            Self::Channel { got, wanted } => {
                write!(f, "the manifest is for the channel {got}, not {wanted}")
            }
        }
    }
}

impl std::error::Error for Error {}

/// A manifest that was fetched, checked and parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    /// The manifest.
    pub manifest: manifest::Manifest,
    /// The serial that was seen, for the floor of the next check.
    pub serial: u64,
}

/// Fetches the manifest at `url`, checks the signature, the channel and the floor, and returns
/// the manifest.
///
/// `key` is the pinned Ed25519 public key of the publisher. `floor` is the serial of the last
/// manifest that was accepted.
///
/// # Errors
/// Returns the reason the check failed.
pub fn check(
    url: &str,
    key: &[u8; 32],
    channel: &str,
    floor: &floor::Floor,
) -> Result<Update, Error> {
    let (body, signature) = feed::fetch_signed(url)?;
    sig::verify(key, &body, &signature)?;
    let manifest = manifest::parse(&body)?;
    if manifest.channel != channel {
        return Err(Error::Channel {
            got: manifest.channel.clone(),
            wanted: channel.to_owned(),
        });
    }
    floor::check(manifest.serial, floor)?;
    let serial = manifest.serial;
    Ok(Update { manifest, serial })
}
