//! Media packet header.
//!
//! The header is fixed at 42 bytes and is covered by the AEAD AAD, so every
//! field is authenticated. Layout is little-endian on the wire.

use crate::error::ProtocolError;
use crate::message::MessageKind;

/// Size of the media packet header in bytes.
pub const HEADER_LEN: usize = 42;

/// Magic bytes at the start of every media packet: `L`, `M`.
pub const MAGIC: [u8; 2] = *b"LM";

/// Current protocol version, bumped on any wire-incompatible change.
pub const VERSION: u8 = 1;

/// Media packet header.
///
/// ```text
///  0      1      2      3      4      5      6      7
///  +------+------+------+------+------+------+------+------+
///  | 'L'  | 'M'  | ver  | kind | flags |     slice_idx  |
///  +------+------+------+------+------+------+------+------+
///  |          frame_id           |      slice_count    |
///  +------+------+------+------+------+------+------+------+
///  |                        timestamp_us                |
///  +------+------+------+------+------+------+------+------+
///  |            aux            |          length        |
///  +------+------+------+------+------+------+------+------+
///  |                         au_len                     |
///  +------+------+------+------+------+------+------+------+
///  |                        session_id                    |
///  +------+------+------+------+------+------+------+------+
///  |                    seq (u64)                          |
///  +------+------+------+------+------+------+------+------+
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaHeader {
    /// Protocol version, [`VERSION`].
    pub version: u8,
    /// Message kind, see [`crate::message::MessageKind`].
    pub kind: MessageKind,
    /// Kind-specific flags.
    pub flags: u16,
    /// Index of this slice inside the access unit.
    pub slice_index: u16,
    /// Total number of slices in the access unit.
    pub slice_count: u16,
    /// Monotonic frame identifier assigned by the host.
    pub frame_id: u32,
    /// Host capture timestamp of the frame, microseconds on the host clock.
    pub timestamp_us: u64,
    /// Kind-specific auxiliary word (block index, parity index, ...).
    pub aux: u16,
    /// Length of the payload following the header.
    pub length: u16,
    /// Total length of the access unit in bytes, before slice padding.
    pub au_len: u32,
    /// Session identifier, incremented on every reconnect.
    pub session_id: u32,
    /// Per-packet sequence number. It is the AEAD nonce counter, so it never repeats
    /// within a session. A retransmission carries a new value.
    pub seq: u64,
}

impl MediaHeader {
    /// Maximum payload a single media packet can carry.
    pub const MAX_PAYLOAD: usize = u16::MAX as usize;

    /// Build a header for a data slice of an access unit.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub const fn data_slice(
        kind: MessageKind,
        frame_id: u32,
        slice_index: u16,
        slice_count: u16,
        timestamp_us: u64,
        length: u16,
        au_len: u32,
        session_id: u32,
    ) -> Self {
        Self {
            version: VERSION,
            kind,
            flags: 0,
            slice_index,
            slice_count,
            frame_id,
            timestamp_us,
            aux: 0,
            length,
            au_len,
            session_id,
            seq: 0,
        }
    }

    /// Set the auxiliary word.
    #[must_use]
    pub const fn with_aux(mut self, aux: u16) -> Self {
        self.aux = aux;
        self
    }

    /// Set the flags word.
    #[must_use]
    pub const fn with_flags(mut self, flags: u16) -> Self {
        self.flags = flags;
        self
    }

    /// Set the per-packet sequence number.
    #[must_use]
    pub const fn with_seq(mut self, seq: u64) -> Self {
        self.seq = seq;
        self
    }

    /// Serialize into a fixed `HEADER_LEN`-byte buffer.
    #[must_use]
    pub fn to_bytes(self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0] = MAGIC[0];
        b[1] = MAGIC[1];
        b[2] = self.version;
        b[3] = self.kind as u8;
        b[4..6].copy_from_slice(&self.flags.to_le_bytes());
        b[6..8].copy_from_slice(&self.slice_index.to_le_bytes());
        b[8..10].copy_from_slice(&self.slice_count.to_le_bytes());
        b[10..14].copy_from_slice(&self.frame_id.to_le_bytes());
        b[14..22].copy_from_slice(&self.timestamp_us.to_le_bytes());
        b[22..24].copy_from_slice(&self.aux.to_le_bytes());
        b[24..26].copy_from_slice(&self.length.to_le_bytes());
        b[26..30].copy_from_slice(&self.au_len.to_le_bytes());
        b[30..34].copy_from_slice(&self.session_id.to_le_bytes());
        b[34..42].copy_from_slice(&self.seq.to_le_bytes());
        b
    }

    /// Parse a header from the first `HEADER_LEN` bytes of a packet.
    ///
    /// # Errors
    /// Returns [`ProtocolError::BadMagic`] when the magic does not match,
    /// [`ProtocolError::UnsupportedVersion`] for an unknown version and
    /// [`ProtocolError::BadKind`] for an unknown message kind.
    pub fn parse(buf: &[u8]) -> Result<Self, ProtocolError> {
        if buf.len() < HEADER_LEN {
            return Err(ProtocolError::ShortHeader);
        }
        if buf[0] != MAGIC[0] || buf[1] != MAGIC[1] {
            return Err(ProtocolError::BadMagic);
        }
        let version = buf[2];
        if version != VERSION {
            return Err(ProtocolError::UnsupportedVersion(version));
        }
        let kind = MessageKind::from_u8(buf[3])?;
        Ok(Self {
            version,
            kind,
            flags: u16::from_le_bytes([buf[4], buf[5]]),
            slice_index: u16::from_le_bytes([buf[6], buf[7]]),
            slice_count: u16::from_le_bytes([buf[8], buf[9]]),
            frame_id: u32::from_le_bytes([buf[10], buf[11], buf[12], buf[13]]),
            timestamp_us: u64::from_le_bytes([
                buf[14], buf[15], buf[16], buf[17], buf[18], buf[19], buf[20], buf[21],
            ]),
            aux: u16::from_le_bytes([buf[22], buf[23]]),
            length: u16::from_le_bytes([buf[24], buf[25]]),
            au_len: u32::from_le_bytes([buf[26], buf[27], buf[28], buf[29]]),
            session_id: u32::from_le_bytes([buf[30], buf[31], buf[32], buf[33]]),
            seq: u64::from_le_bytes([
                buf[34], buf[35], buf[36], buf[37], buf[38], buf[39], buf[40], buf[41],
            ]),
        })
    }

    /// Whether this slice is the last one of its access unit.
    #[must_use]
    pub const fn is_last_slice(&self) -> bool {
        self.slice_index >= self.slice_count.saturating_sub(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let h = MediaHeader::data_slice(
            MessageKind::VideoData,
            0xdead_beef,
            3,
            7,
            0x0123_4567_89ab_cdef,
            1200,
            8400,
            42,
        )
        .with_aux(9)
        .with_flags(0x8001)
        .with_seq(0x1122_3344_5566_7788);
        let bytes = h.to_bytes();
        assert_eq!(bytes.len(), HEADER_LEN);
        assert_eq!(MediaHeader::parse(&bytes).unwrap(), h);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut b = MediaHeader::data_slice(MessageKind::VideoData, 1, 0, 1, 0, 0, 0, 0).to_bytes();
        b[0] = b'X';
        assert_eq!(MediaHeader::parse(&b), Err(ProtocolError::BadMagic));
    }

    #[test]
    fn rejects_bad_version() {
        let mut b = MediaHeader::data_slice(MessageKind::VideoData, 1, 0, 1, 0, 0, 0, 0).to_bytes();
        b[2] = 99;
        assert_eq!(
            MediaHeader::parse(&b),
            Err(ProtocolError::UnsupportedVersion(99))
        );
    }

    #[test]
    fn rejects_short_buffer() {
        assert_eq!(
            MediaHeader::parse(&[0u8; 8]),
            Err(ProtocolError::ShortHeader)
        );
    }

    #[test]
    fn last_slice_detection() {
        let h = MediaHeader::data_slice(MessageKind::VideoData, 1, 3, 5, 0, 0, 0, 0);
        assert!(!h.is_last_slice());
        let h = MediaHeader::data_slice(MessageKind::VideoData, 1, 4, 5, 0, 0, 0, 0);
        assert!(h.is_last_slice());
        // slice_index == u16::MAX must not overflow when counting up to the last slice.
        let h = MediaHeader::data_slice(MessageKind::VideoData, 1, u16::MAX, u16::MAX, 0, 0, 0, 0);
        assert!(h.is_last_slice());
        let h = MediaHeader::data_slice(MessageKind::VideoData, 1, 0, u16::MAX, 0, 0, 0, 0);
        assert!(!h.is_last_slice());
    }
}
