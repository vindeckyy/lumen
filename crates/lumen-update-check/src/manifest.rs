//! The manifest of a release.
//!
//! The schema is small on purpose: a version, a channel, a serial that only goes up, and where
//! the file is. A manifest that does not parse is refused, not guessed at.

use crate::Error;

/// What a release manifest says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// The version of the build, as a string.
    pub version: String,
    /// The channel the build belongs to, for example `stable`.
    pub channel: String,
    /// A number that only goes up. A manifest with a smaller serial is a replay.
    pub serial: u64,
    /// Where the build is.
    pub url: String,
    /// The size of the build in bytes, when the publisher knows it.
    pub size: Option<u64>,
}

/// Parses a manifest. The format is one `key=value` per line, which keeps it readable and
/// keeps a JSON parser out of the host.
///
/// # Errors
/// Returns an error when a key is missing, a value is empty or the serial is not a number.
pub fn parse(body: &[u8]) -> Result<Manifest, Error> {
    let text = std::str::from_utf8(body)
        .map_err(|e| Error::Manifest(format!("the manifest is not text: {e}")))?;
    let mut version = None;
    let mut channel = None;
    let mut serial = None;
    let mut url = None;
    let mut size = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(Error::Manifest(format!("the line {line:?} is not a pair")));
        };
        let value = value.trim();
        if value.is_empty() {
            return Err(Error::Manifest(format!("{key} has no value")));
        }
        match key.trim() {
            "version" => version = Some(value.to_owned()),
            "channel" => channel = Some(value.to_owned()),
            "serial" => {
                serial = Some(value.parse::<u64>().map_err(|_| {
                    Error::Manifest(format!("the serial {value:?} is not a number"))
                })?);
            }
            "url" => url = Some(value.to_owned()),
            "size" => {
                size =
                    Some(value.parse::<u64>().map_err(|_| {
                        Error::Manifest(format!("the size {value:?} is not a number"))
                    })?);
            }
            _ => {}
        }
    }
    Ok(Manifest {
        version: version.ok_or_else(|| Error::Manifest("the manifest has no version".into()))?,
        channel: channel.ok_or_else(|| Error::Manifest("the manifest has no channel".into()))?,
        serial: serial.ok_or_else(|| Error::Manifest("the manifest has no serial".into()))?,
        url: url.ok_or_else(|| Error::Manifest("the manifest has no url".into()))?,
        size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_manifest_parses() {
        let manifest = parse(
            b"version=1.2.3\nchannel=stable\nserial=17\nurl=https://example.invalid/lumen-1.2.3\nsize=42\n",
        )
        .expect("parses");
        assert_eq!(manifest.version, "1.2.3");
        assert_eq!(manifest.channel, "stable");
        assert_eq!(manifest.serial, 17);
        assert_eq!(manifest.size, Some(42));
    }

    #[test]
    fn a_missing_key_is_refused() {
        assert!(parse(b"channel=stable\nserial=1\nurl=x\n").is_err());
        assert!(parse(b"version=1\nserial=1\nurl=x\n").is_err());
        assert!(parse(b"version=1\nchannel=stable\nurl=x\n").is_err());
        assert!(parse(b"version=1\nchannel=stable\nserial=1\n").is_err());
    }

    #[test]
    fn a_bad_value_is_refused() {
        assert!(parse(b"version=1\nchannel=stable\nserial=many\nurl=x\n").is_err());
        assert!(parse(b"version=\nchannel=stable\nserial=1\nurl=x\n").is_err());
    }
}
