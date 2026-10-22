//! Audio wire format.
//!
//! An audio frame travels as a fixed-size [`AudioHeader`] followed by the
//! encoded payload. The header carries the capture timestamp of the video
//! frame the audio aligns to, so the client schedules playback against the
//! video clock instead of against the arrival time of the packet.
//!
//! The header is little-endian on the wire and travels inside the payload
//! of a media datagram, so it is covered by the AEAD and every field is
//! authenticated.
//!
//! The frame sizes are the Opus ones: 2.5 ms, 5 ms and 10 ms. The raw PCM
//! mode uses the same frame sizes, so one duration field describes both.

use std::fmt;

/// Size of the audio header in bytes.
pub const AUDIO_HEADER_LEN: usize = 48;

/// Magic bytes at the start of every audio header: `L`, `A`.
pub const AUDIO_MAGIC: [u8; 2] = *b"LA";

/// Version of the audio header, bumped on any wire-incompatible change.
pub const AUDIO_VERSION: u8 = 1;

/// The frame durations the audio path carries, in microseconds: 2.5 ms, 5 ms
/// and 10 ms.
pub const FRAME_DURATIONS_US: [u32; 3] = [2_500, 5_000, 10_000];

/// The largest audio payload one frame may carry, in bytes.
pub const MAX_AUDIO_PAYLOAD: u32 = 65_536;

/// The largest channel count one frame may carry.
pub const MAX_AUDIO_CHANNELS: u16 = 8;

/// The largest sample rate one frame may carry, in hertz.
pub const MAX_AUDIO_SAMPLE_RATE: u32 = 384_000;

/// The duration of one audio frame in microseconds.
///
/// `frame_ms` is in tenths of a millisecond, the unit the Opus frame sizes
/// come in: 25 is 2.5 ms, 50 is 5 ms and 100 is 10 ms. Returns `None` for
/// any other size.
#[must_use]
pub const fn frame_duration_us(frame_ms: u32) -> Option<u32> {
    match frame_ms {
        25 => Some(2_500),
        50 => Some(5_000),
        100 => Some(10_000),
        _ => None,
    }
}

/// Whether `duration_us` is one of the frame sizes the audio path carries.
#[must_use]
pub const fn is_frame_duration(duration_us: u32) -> bool {
    matches!(duration_us, 2_500 | 5_000 | 10_000)
}

/// The codec an audio frame is encoded with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AudioKind {
    /// An Opus packet.
    Opus = 1,
    /// Raw little-endian f32 samples.
    Pcm = 2,
}

impl AudioKind {
    /// Convert a wire byte into a kind.
    ///
    /// # Errors
    /// Returns [`AudioError::BadKind`] for an unknown byte.
    pub const fn from_u8(v: u8) -> Result<Self, AudioError> {
        Ok(match v {
            1 => Self::Opus,
            2 => Self::Pcm,
            other => return Err(AudioError::BadKind(other)),
        })
    }

    /// The wire byte of this kind.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Whether the payload is raw samples rather than an Opus packet.
    #[must_use]
    pub const fn is_pcm(self) -> bool {
        matches!(self, Self::Pcm)
    }
}

/// Errors of the audio wire format.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AudioError {
    /// The buffer was shorter than the fixed header size.
    #[error("short audio header: need {AUDIO_HEADER_LEN} bytes")]
    ShortHeader,
    /// The packet did not start with the expected magic bytes.
    #[error("bad magic bytes")]
    BadMagic,
    /// The audio header version is not supported by this build.
    #[error("unsupported audio header version {0}")]
    UnsupportedVersion(u8),
    /// The audio kind byte is not a known kind.
    #[error("unknown audio kind {0}")]
    BadKind(u8),
    /// The channel count is outside the range the format allows.
    #[error("audio channel count {0} is out of range")]
    BadChannelCount(u16),
    /// The sample rate is outside the range the format allows.
    #[error("audio sample rate {0} is out of range")]
    BadSampleRate(u32),
    /// The frame duration is not one of the supported frame sizes.
    #[error("audio frame duration {0} us is not a supported frame size")]
    BadFrameDuration(u32),
    /// A reserved byte was not zero.
    #[error("audio header reserved bytes are not zero")]
    NonZeroReserved,
    /// The declared payload length is larger than the format allows.
    #[error("audio payload length {0} is too large")]
    PayloadTooLarge(u32),
    /// The declared payload length does not match the received bytes.
    #[error("length mismatch: header says {0}, got {1}")]
    LengthMismatch(usize, usize),
}

impl From<AudioError> for std::io::Error {
    fn from(e: AudioError) -> Self {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    }
}

/// Audio packet header.
///
/// ```text
///  0      1      2      3      4      5      6      7
///  +------+------+------+------+------+------+------+------+
///  | 'L'  | 'A'  | ver  | kind |    channels    |   reserved  |
///  +------+------+------+------+------+------+------+------+
///  |                         sample_rate                    |
///  +------+------+------+------+------+------+------+------+
///  |                      frame_duration_us                 |
///  +------+------+------+------+------+------+------+------+
///  |                         frame_index                    |
///  +------+------+------+------+------+------+------+------+
///  |                     video_capture_ts_us                |
///  +------+------+------+------+------+------+------+------+
///  |                         payload_len                     |
///  +------+------+------+------+------+------+------+------+
///  |                           seq                           |
///  +------+------+------+------+------+------+------+------+
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioHeader {
    /// Audio header version, [`AUDIO_VERSION`].
    pub version: u8,
    /// The codec the payload is encoded with.
    pub kind: AudioKind,
    /// Number of interleaved channels in the payload.
    pub channels: u16,
    /// Sample rate of the payload, hertz.
    pub sample_rate: u32,
    /// Duration of the frame, microseconds. One of [`FRAME_DURATIONS_US`].
    pub frame_duration_us: u32,
    /// Monotonic frame counter assigned by the host.
    pub frame_index: u64,
    /// Host capture timestamp of the video frame this audio aligns to,
    /// microseconds on the host clock.
    pub video_capture_ts_us: u64,
    /// Length of the payload following the header, in bytes.
    pub payload_len: u32,
    /// The AEAD counter of the packet, in the clear.
    ///
    /// The receiver needs it to open the payload, so it cannot be inside the
    /// ciphertext. It is authenticated as associated data.
    pub seq: u64,
}

impl AudioHeader {
    /// Build the header of one audio frame.
    ///
    /// # Errors
    /// Returns [`AudioError::BadChannelCount`], [`AudioError::BadSampleRate`],
    /// [`AudioError::BadFrameDuration`] or [`AudioError::PayloadTooLarge`]
    /// when a field is outside the range the format allows.
    pub fn new(
        kind: AudioKind,
        channels: u16,
        sample_rate: u32,
        frame_duration_us: u32,
        frame_index: u64,
        video_capture_ts_us: u64,
        payload_len: u32,
    ) -> Result<Self, AudioError> {
        if channels == 0 || channels > MAX_AUDIO_CHANNELS {
            return Err(AudioError::BadChannelCount(channels));
        }
        if sample_rate == 0 || sample_rate > MAX_AUDIO_SAMPLE_RATE {
            return Err(AudioError::BadSampleRate(sample_rate));
        }
        if !is_frame_duration(frame_duration_us) {
            return Err(AudioError::BadFrameDuration(frame_duration_us));
        }
        if payload_len > MAX_AUDIO_PAYLOAD {
            return Err(AudioError::PayloadTooLarge(payload_len));
        }
        Ok(Self {
            version: AUDIO_VERSION,
            kind,
            channels,
            sample_rate,
            frame_duration_us,
            frame_index,
            video_capture_ts_us,
            payload_len,
            seq: 0,
        })
    }

    /// Serialize into a fixed `AUDIO_HEADER_LEN`-byte buffer.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; AUDIO_HEADER_LEN] {
        let mut b = [0u8; AUDIO_HEADER_LEN];
        b[0] = AUDIO_MAGIC[0];
        b[1] = AUDIO_MAGIC[1];
        b[2] = self.version;
        b[3] = self.kind.as_u8();
        b[4..6].copy_from_slice(&self.channels.to_le_bytes());
        // Bytes 6..8 are reserved and stay zero.
        b[8..12].copy_from_slice(&self.sample_rate.to_le_bytes());
        b[12..16].copy_from_slice(&self.frame_duration_us.to_le_bytes());
        b[16..24].copy_from_slice(&self.frame_index.to_le_bytes());
        b[24..32].copy_from_slice(&self.video_capture_ts_us.to_le_bytes());
        b[32..36].copy_from_slice(&self.payload_len.to_le_bytes());
        b[40..48].copy_from_slice(&self.seq.to_le_bytes());
        // Bytes 36..40 are reserved and stay zero.
        b
    }

    /// Parse a header from the first `AUDIO_HEADER_LEN` bytes of a packet.
    ///
    /// # Errors
    /// Returns [`AudioError::ShortHeader`] when the buffer is too short,
    /// [`AudioError::BadMagic`] when the magic does not match,
    /// [`AudioError::UnsupportedVersion`] for an unknown version and
    /// [`AudioError::BadKind`] for an unknown codec. A field outside the
    /// range the format allows is rejected as well.
    pub fn parse(buf: &[u8]) -> Result<Self, AudioError> {
        if buf.len() < AUDIO_HEADER_LEN {
            return Err(AudioError::ShortHeader);
        }
        if buf[0] != AUDIO_MAGIC[0] || buf[1] != AUDIO_MAGIC[1] {
            return Err(AudioError::BadMagic);
        }
        let version = buf[2];
        if version != AUDIO_VERSION {
            return Err(AudioError::UnsupportedVersion(version));
        }
        let kind = AudioKind::from_u8(buf[3])?;
        if buf[6] != 0
            || buf[7] != 0
            || buf[36] != 0
            || buf[37] != 0
            || buf[38] != 0
            || buf[39] != 0
        {
            return Err(AudioError::NonZeroReserved);
        }
        let channels = u16::from_le_bytes([buf[4], buf[5]]);
        if channels == 0 || channels > MAX_AUDIO_CHANNELS {
            return Err(AudioError::BadChannelCount(channels));
        }
        let sample_rate = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        if sample_rate == 0 || sample_rate > MAX_AUDIO_SAMPLE_RATE {
            return Err(AudioError::BadSampleRate(sample_rate));
        }
        let frame_duration_us = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
        if !is_frame_duration(frame_duration_us) {
            return Err(AudioError::BadFrameDuration(frame_duration_us));
        }
        let payload_len = u32::from_le_bytes([buf[32], buf[33], buf[34], buf[35]]);
        if payload_len > MAX_AUDIO_PAYLOAD {
            return Err(AudioError::PayloadTooLarge(payload_len));
        }
        Ok(Self {
            version,
            kind,
            channels,
            sample_rate,
            frame_duration_us,
            frame_index: u64::from_le_bytes([
                buf[16], buf[17], buf[18], buf[19], buf[20], buf[21], buf[22], buf[23],
            ]),
            video_capture_ts_us: u64::from_le_bytes([
                buf[24], buf[25], buf[26], buf[27], buf[28], buf[29], buf[30], buf[31],
            ]),
            payload_len,
            seq: u64::from_le_bytes([
                buf[40], buf[41], buf[42], buf[43], buf[44], buf[45], buf[46], buf[47],
            ]),
        })
    }

    /// Parse the header of a packet and return the payload that follows it.
    ///
    /// The payload must be exactly as long as the header declares. A packet
    /// whose last slice was padded by the transport has to be trimmed first.
    ///
    /// # Errors
    /// Returns the error of the header, or [`AudioError::LengthMismatch`]
    /// when the payload is not exactly as long as the header declares.
    pub fn split(buf: &[u8]) -> Result<(Self, &[u8]), AudioError> {
        let header = Self::parse(buf)?;
        let payload = buf.get(AUDIO_HEADER_LEN..).ok_or(AudioError::ShortHeader)?;
        let declared = usize::try_from(header.payload_len).unwrap_or(usize::MAX);
        if payload.len() != declared {
            return Err(AudioError::LengthMismatch(declared, payload.len()));
        }
        Ok((header, payload))
    }

    /// The host instant at which the frame ends, microseconds.
    #[must_use]
    pub fn frame_end_us(&self) -> u64 {
        self.video_capture_ts_us
            .saturating_add(u64::from(self.frame_duration_us))
    }
}

impl fmt::Display for AudioHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "audio frame {} ({} Hz, {} ch, {} us) aligned to video at {} us, {} bytes",
            self.frame_index,
            self.sample_rate,
            self.channels,
            self.frame_duration_us,
            self.video_capture_ts_us,
            self.payload_len
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> AudioHeader {
        AudioHeader::new(
            AudioKind::Opus,
            2,
            48_000,
            5_000,
            0x0123_4567_89ab_cdef,
            0x00ff_0102_0304_0506,
            87,
        )
        .expect("valid header")
    }

    #[test]
    fn roundtrip() {
        let h = header();
        let bytes = h.to_bytes();
        assert_eq!(bytes.len(), AUDIO_HEADER_LEN);
        assert_eq!(AudioHeader::parse(&bytes).unwrap(), h);
    }

    #[test]
    fn every_kind_round_trips() {
        for kind in [AudioKind::Opus, AudioKind::Pcm] {
            let h = AudioHeader::new(kind, 1, 48_000, 2_500, 7, 9, 3).unwrap();
            assert_eq!(AudioHeader::parse(&h.to_bytes()).unwrap(), h);
            assert_eq!(kind.is_pcm(), kind == AudioKind::Pcm);
        }
    }

    #[test]
    fn the_layout_is_pinned() {
        let h = header();
        let b = h.to_bytes();
        assert_eq!(AUDIO_HEADER_LEN, 48);
        assert_eq!(&b[0..2], b"LA");
        assert_eq!(b[2], AUDIO_VERSION);
        assert_eq!(b[3], AudioKind::Opus.as_u8());
        assert_eq!(&b[4..6], &2u16.to_le_bytes());
        assert_eq!(&b[6..8], &[0, 0], "reserved");
        assert_eq!(&b[8..12], &48_000u32.to_le_bytes());
        assert_eq!(&b[12..16], &5_000u32.to_le_bytes());
        assert_eq!(&b[16..24], &0x0123_4567_89ab_cdefu64.to_le_bytes());
        assert_eq!(&b[24..32], &0x00ff_0102_0304_0506u64.to_le_bytes());
        assert_eq!(&b[32..36], &87u32.to_le_bytes());
        assert_eq!(&b[36..40], &[0, 0, 0, 0], "reserved");
        assert_eq!(&b[40..48], &0u64.to_le_bytes(), "seq");
    }

    #[test]
    fn rejects_bad_magic() {
        let mut b = header().to_bytes();
        b[0] = b'X';
        assert_eq!(AudioHeader::parse(&b), Err(AudioError::BadMagic));
    }

    #[test]
    fn rejects_bad_version() {
        let mut b = header().to_bytes();
        b[2] = 99;
        assert_eq!(
            AudioHeader::parse(&b),
            Err(AudioError::UnsupportedVersion(99))
        );
    }

    #[test]
    fn rejects_unknown_kind() {
        let mut b = header().to_bytes();
        b[3] = 7;
        assert_eq!(AudioHeader::parse(&b), Err(AudioError::BadKind(7)));
    }

    #[test]
    fn rejects_short_buffer() {
        assert_eq!(AudioHeader::parse(&[0u8; 8]), Err(AudioError::ShortHeader));
    }

    #[test]
    fn rejects_a_zero_channel_count() {
        let mut b = header().to_bytes();
        b[4..6].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(AudioHeader::parse(&b), Err(AudioError::BadChannelCount(0)));
    }

    #[test]
    fn rejects_a_zero_sample_rate() {
        let mut b = header().to_bytes();
        b[8..12].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(AudioHeader::parse(&b), Err(AudioError::BadSampleRate(0)));
    }

    #[test]
    fn rejects_an_unsupported_frame_duration() {
        let mut b = header().to_bytes();
        b[12..16].copy_from_slice(&20_000u32.to_le_bytes());
        assert_eq!(
            AudioHeader::parse(&b),
            Err(AudioError::BadFrameDuration(20_000))
        );
    }

    #[test]
    fn rejects_non_zero_reserved_bytes() {
        let mut b = header().to_bytes();
        b[7] = 1;
        assert_eq!(AudioHeader::parse(&b), Err(AudioError::NonZeroReserved));
        let mut b = header().to_bytes();
        b[39] = 0x80;
        assert_eq!(AudioHeader::parse(&b), Err(AudioError::NonZeroReserved));
    }

    #[test]
    fn rejects_an_oversized_payload() {
        let mut b = header().to_bytes();
        b[32..36].copy_from_slice(&(MAX_AUDIO_PAYLOAD + 1).to_le_bytes());
        assert_eq!(
            AudioHeader::parse(&b),
            Err(AudioError::PayloadTooLarge(MAX_AUDIO_PAYLOAD + 1))
        );
    }

    #[test]
    fn the_constructor_rejects_the_same_fields() {
        assert_eq!(
            AudioHeader::new(AudioKind::Opus, 0, 48_000, 5_000, 0, 0, 0),
            Err(AudioError::BadChannelCount(0))
        );
        assert_eq!(
            AudioHeader::new(AudioKind::Opus, 2, 0, 5_000, 0, 0, 0),
            Err(AudioError::BadSampleRate(0))
        );
        assert_eq!(
            AudioHeader::new(AudioKind::Opus, 2, 48_000, 3_000, 0, 0, 0),
            Err(AudioError::BadFrameDuration(3_000))
        );
        assert_eq!(
            AudioHeader::new(
                AudioKind::Opus,
                2,
                48_000,
                5_000,
                0,
                0,
                MAX_AUDIO_PAYLOAD + 1
            ),
            Err(AudioError::PayloadTooLarge(MAX_AUDIO_PAYLOAD + 1))
        );
    }

    #[test]
    fn split_returns_the_payload() {
        let h = AudioHeader::new(AudioKind::Opus, 2, 48_000, 5_000, 1, 2, 3).unwrap();
        let mut packet = h.to_bytes().to_vec();
        packet.extend_from_slice(&[1, 2, 3]);
        let (parsed, payload) = AudioHeader::split(&packet).unwrap();
        assert_eq!(parsed, h);
        assert_eq!(payload, &[1, 2, 3]);
    }

    #[test]
    fn split_rejects_a_length_mismatch() {
        let h = header();
        let mut packet = h.to_bytes().to_vec();
        packet.extend_from_slice(&[1, 2]);
        assert_eq!(
            AudioHeader::split(&packet),
            Err(AudioError::LengthMismatch(87, 2))
        );
        let mut packet = h.to_bytes().to_vec();
        packet.extend_from_slice(&[0u8; 88]);
        assert_eq!(
            AudioHeader::split(&packet),
            Err(AudioError::LengthMismatch(87, 88))
        );
    }

    #[test]
    fn the_frame_duration_helper_maps_the_opus_frame_sizes() {
        assert_eq!(frame_duration_us(25), Some(2_500));
        assert_eq!(frame_duration_us(50), Some(5_000));
        assert_eq!(frame_duration_us(100), Some(10_000));
        // 5 is milliseconds, not tenths of a millisecond: it must not mean 0.5 ms.
        assert_eq!(frame_duration_us(5), None);
        assert_eq!(frame_duration_us(0), None);
        assert_eq!(frame_duration_us(1_000), None);
        assert!(is_frame_duration(2_500));
        assert!(!is_frame_duration(1_000));
    }

    #[test]
    fn the_frame_end_is_the_aligned_timestamp_plus_the_duration() {
        let h = header();
        assert_eq!(h.frame_end_us(), h.video_capture_ts_us + 5_000);
    }

    #[test]
    fn the_header_displays_its_fields() {
        let text = header().to_string();
        assert!(text.contains("48000 Hz"), "{text}");
        assert!(text.contains("2 ch"), "{text}");
        assert!(text.contains("87 bytes"), "{text}");
    }
}
