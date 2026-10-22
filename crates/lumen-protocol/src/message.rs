//! Media message kinds.

use crate::error::ProtocolError;

/// Kind of a media packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MessageKind {
    /// A slice of a video access unit.
    VideoData = 1,
    /// A parity slice protecting a FEC block.
    VideoParity = 2,
    /// An audio payload.
    Audio = 3,
    /// An input event travelling client to host.
    Input = 4,
    /// A control message on the reliable control channel.
    Control = 5,
}

impl MessageKind {
    /// Convert a wire byte into a kind.
    ///
    /// # Errors
    /// Returns [`ProtocolError::BadKind`] for an unknown byte.
    pub const fn from_u8(v: u8) -> Result<Self, ProtocolError> {
        Ok(match v {
            1 => Self::VideoData,
            2 => Self::VideoParity,
            3 => Self::Audio,
            4 => Self::Input,
            5 => Self::Control,
            other => return Err(ProtocolError::BadKind(other)),
        })
    }

    /// The wire byte for this kind.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_kinds() {
        for k in [
            MessageKind::VideoData,
            MessageKind::VideoParity,
            MessageKind::Audio,
            MessageKind::Input,
            MessageKind::Control,
        ] {
            assert_eq!(MessageKind::from_u8(k.as_u8()).unwrap(), k);
        }
    }

    #[test]
    fn rejects_unknown_kind() {
        assert_eq!(MessageKind::from_u8(200), Err(ProtocolError::BadKind(200)));
    }
}
