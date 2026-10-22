//! Input events on the client-to-host channel.
//!
//! `docs/protocol.md` section 8 is the normative description. Each event is one UDP datagram on the
//! input socket, so a lost video packet cannot delay it:
//!
//! ```text
//! datagram  = sequence (u64 LE, clear, AEAD associated data) || ciphertext || tag (16 bytes)
//! plaintext = kind (u8) || client_ts_us (u64 LE) || sequence (u64 LE) || fields
//! ```
//!
//! `client_ts_us` and the repeated `sequence` form the 16-byte client stamp. The sender's sequence
//! is the [`PacketCipher`] counter, so the nonce and the clear header agree by construction.

use lumen_crypto::{CryptoError, PacketCipher};

/// Length of the clear sequence header, which is also the AEAD associated data.
pub const SEQUENCE_LEN: usize = 8;
/// Length of the authentication tag that the AEAD appends.
pub const TAG_LEN: usize = 16;
/// Length of the client stamp inside the plaintext: `client_ts_us` then `sequence`.
pub const STAMP_LEN: usize = 16;
/// Largest datagram accepted on the input socket. Larger datagrams are rejected before decryption.
pub const MAX_PACKET_LEN: usize = 128;
/// Number of sequence numbers behind the newest one that the replay window still remembers.
pub const REPLAY_WINDOW: u64 = 64;

const KIND_KEY: u8 = 1;
const KIND_MOTION_RELATIVE: u8 = 2;
const KIND_MOTION_ABSOLUTE: u8 = 3;
const KIND_BUTTON: u8 = 4;
const KIND_SCROLL: u8 = 5;

/// One input action. Coordinates are client-surface pixels; every float must be finite.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputAction {
    /// Keyboard key. `keycode` is an evdev code.
    Key { keycode: u32, pressed: bool },
    /// Relative pointer motion in logical pixels.
    MotionRelative { dx: f64, dy: f64 },
    /// Absolute pointer position in client-surface pixels.
    MotionAbsolute { x: f64, y: f64 },
    /// Pointer button. `button` is an evdev code.
    Button { button: u32, pressed: bool },
    /// Scroll in logical pixels.
    Scroll { dx: f64, dy: f64 },
}

/// A decoded input event together with its stamp.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InputEnvelope {
    /// Sender's packet counter. Equal to the clear header and to the stamp.
    pub sequence: u64,
    /// `CLOCK_MONOTONIC` microseconds on the client when the event was stamped.
    pub client_ts_us: u64,
    /// The event itself.
    pub action: InputAction,
}

/// Why an input datagram or event was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InputCodecError {
    /// The bytes end before the layout does.
    #[error("input data is truncated")]
    Truncated,
    /// The datagram is longer than [`MAX_PACKET_LEN`].
    #[error("input datagram of {0} bytes exceeds the limit")]
    Oversize(usize),
    /// The kind byte is not one this build defines.
    #[error("unknown input kind {0}")]
    UnknownKind(u8),
    /// A field is out of range, or bytes follow the event.
    #[error("malformed input event: {0}")]
    Malformed(&'static str),
    /// The tag did not verify. The datagram was altered, or sealed with another key or session.
    #[error("input datagram failed authentication")]
    Authentication,
    /// Sealing failed inside the AEAD.
    #[error("input seal failed: {0}")]
    Seal(&'static str),
}

impl InputAction {
    const fn kind_byte(self) -> u8 {
        match self {
            Self::Key { .. } => KIND_KEY,
            Self::MotionRelative { .. } => KIND_MOTION_RELATIVE,
            Self::MotionAbsolute { .. } => KIND_MOTION_ABSOLUTE,
            Self::Button { .. } => KIND_BUTTON,
            Self::Scroll { .. } => KIND_SCROLL,
        }
    }

    /// Rejects non-finite coordinates, so a NaN never reaches an injector.
    ///
    /// # Errors
    /// Returns [`InputCodecError::Malformed`] when a coordinate or delta is NaN or infinite.
    pub fn validate(self) -> Result<(), InputCodecError> {
        let finite = match self {
            Self::Key { .. } | Self::Button { .. } => true,
            Self::MotionRelative { dx: a, dy: b }
            | Self::MotionAbsolute { x: a, y: b }
            | Self::Scroll { dx: a, dy: b } => a.is_finite() && b.is_finite(),
        };
        if finite {
            Ok(())
        } else {
            Err(InputCodecError::Malformed("non-finite coordinate"))
        }
    }
}

/// Encodes the plaintext of one event. It does no validation and does not encrypt.
#[must_use]
pub fn encode_plaintext(sequence: u64, client_ts_us: u64, action: InputAction) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + STAMP_LEN + 16);
    out.push(action.kind_byte());
    out.extend_from_slice(&client_ts_us.to_le_bytes());
    out.extend_from_slice(&sequence.to_le_bytes());
    match action {
        InputAction::Key { keycode, pressed }
        | InputAction::Button {
            button: keycode,
            pressed,
        } => {
            out.extend_from_slice(&keycode.to_le_bytes());
            out.push(u8::from(pressed));
        }
        InputAction::MotionRelative { dx: a, dy: b }
        | InputAction::MotionAbsolute { x: a, y: b }
        | InputAction::Scroll { dx: a, dy: b } => {
            out.extend_from_slice(&a.to_le_bytes());
            out.extend_from_slice(&b.to_le_bytes());
        }
    }
    out
}

/// Decodes the plaintext of one event. Input must be exactly one event long.
///
/// # Errors
/// Returns [`InputCodecError::Truncated`] when bytes run out, [`InputCodecError::UnknownKind`] for
/// an undefined kind, and [`InputCodecError::Malformed`] for a bad flag, a non-finite float or
/// trailing bytes.
pub fn decode_plaintext(bytes: &[u8]) -> Result<InputEnvelope, InputCodecError> {
    let mut r = Reader { buf: bytes, pos: 0 };
    let kind = r.u8()?;
    if !matches!(kind, KIND_KEY..=KIND_SCROLL) {
        return Err(InputCodecError::UnknownKind(kind));
    }
    let client_ts_us = r.u64()?;
    let sequence = r.u64()?;
    let action = match kind {
        KIND_KEY => InputAction::Key {
            keycode: r.u32()?,
            pressed: r.flag()?,
        },
        KIND_BUTTON => InputAction::Button {
            button: r.u32()?,
            pressed: r.flag()?,
        },
        KIND_MOTION_RELATIVE => {
            let dx = r.finite()?;
            let dy = r.finite()?;
            InputAction::MotionRelative { dx, dy }
        }
        KIND_MOTION_ABSOLUTE => {
            let x = r.finite()?;
            let y = r.finite()?;
            InputAction::MotionAbsolute { x, y }
        }
        KIND_SCROLL => {
            let dx = r.finite()?;
            let dy = r.finite()?;
            InputAction::Scroll { dx, dy }
        }
        other => return Err(InputCodecError::UnknownKind(other)),
    };
    if r.remaining() != 0 {
        return Err(InputCodecError::Malformed("trailing bytes after event"));
    }
    Ok(InputEnvelope {
        sequence,
        client_ts_us,
        action,
    })
}

/// Seals one event into a datagram with the sender's cipher.
///
/// The sequence written to the header is `cipher.counter()`, the same value the AEAD uses for the
/// nonce. Each call advances the counter by one. An event that fails validation does not advance it.
///
/// # Errors
/// Returns [`InputCodecError::Malformed`] for a non-finite coordinate and [`InputCodecError::Seal`]
/// when the AEAD fails.
pub fn seal_packet(
    cipher: &mut PacketCipher,
    client_ts_us: u64,
    action: InputAction,
) -> Result<Vec<u8>, InputCodecError> {
    action.validate()?;
    let sequence = cipher.counter();
    let plaintext = encode_plaintext(sequence, client_ts_us, action);
    cipher
        .seal(&sequence.to_le_bytes(), &plaintext)
        .map_err(|e| match e {
            CryptoError::Aead(what) => InputCodecError::Seal(what),
            _ => InputCodecError::Seal("cipher error"),
        })
}

/// Opens one datagram. Authentication runs before any field is parsed.
///
/// The replay check is not part of this function. Call [`ReplayWindow::accept`] on the returned
/// sequence only after the datagram has been accepted, so forged packets cannot move the window.
///
/// # Errors
/// Returns [`InputCodecError::Oversize`] or [`InputCodecError::Truncated`] for bad lengths,
/// [`InputCodecError::Authentication`] when the tag fails, and the plaintext errors of
/// [`decode_plaintext`]. A stamp whose sequence differs from the header is `Malformed`.
pub fn open_packet(
    cipher: &PacketCipher,
    datagram: &[u8],
) -> Result<InputEnvelope, InputCodecError> {
    if datagram.len() > MAX_PACKET_LEN {
        return Err(InputCodecError::Oversize(datagram.len()));
    }
    if datagram.len() < SEQUENCE_LEN + TAG_LEN {
        return Err(InputCodecError::Truncated);
    }
    let (header, body) = datagram.split_at(SEQUENCE_LEN);
    let mut seq_bytes = [0u8; SEQUENCE_LEN];
    seq_bytes.copy_from_slice(header);
    let sequence = u64::from_le_bytes(seq_bytes);
    let plaintext = cipher
        .open(header, body, sequence)
        .map_err(|_| InputCodecError::Authentication)?;
    let envelope = decode_plaintext(&plaintext)?;
    if envelope.sequence != sequence {
        return Err(InputCodecError::Malformed(
            "stamp sequence differs from header",
        ));
    }
    Ok(envelope)
}

/// Sliding-window replay filter over sequence numbers.
///
/// It remembers the newest sequence and a bitmap of the [`REPLAY_WINDOW`] numbers behind it. An
/// authentic datagram is accepted once. A repeat is refused, and so is anything older than the
/// window. Reordering within the window is accepted, which matters for UDP.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReplayWindow {
    highest: Option<u64>,
    /// Bit `n` is set when sequence `highest - n` has been accepted.
    bits: u64,
}

impl ReplayWindow {
    /// Records `sequence` and returns true when it is new.
    pub fn accept(&mut self, sequence: u64) -> bool {
        let Some(highest) = self.highest else {
            self.highest = Some(sequence);
            self.bits = 1;
            return true;
        };
        if sequence > highest {
            let shift = sequence - highest;
            self.bits = if shift >= REPLAY_WINDOW {
                1
            } else {
                (self.bits << shift) | 1
            };
            self.highest = Some(sequence);
            return true;
        }
        let back = highest - sequence;
        if back >= REPLAY_WINDOW {
            return false;
        }
        let bit = 1u64 << back;
        if self.bits & bit != 0 {
            return false;
        }
        self.bits |= bit;
        true
    }
}

/// Bounds-checked reader over a plaintext body.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], InputCodecError> {
        let end = self.pos + N;
        let bytes = self
            .buf
            .get(self.pos..end)
            .ok_or(InputCodecError::Truncated)?;
        self.pos = end;
        let mut out = [0u8; N];
        out.copy_from_slice(bytes);
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, InputCodecError> {
        Ok(self.take::<1>()?[0])
    }

    fn u32(&mut self) -> Result<u32, InputCodecError> {
        Ok(u32::from_le_bytes(self.take()?))
    }

    fn u64(&mut self) -> Result<u64, InputCodecError> {
        Ok(u64::from_le_bytes(self.take()?))
    }

    fn finite(&mut self) -> Result<f64, InputCodecError> {
        let v = f64::from_le_bytes(self.take()?);
        if v.is_finite() {
            Ok(v)
        } else {
            Err(InputCodecError::Malformed("non-finite coordinate"))
        }
    }

    fn flag(&mut self) -> Result<bool, InputCodecError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(InputCodecError::Malformed("flag byte is not 0 or 1")),
        }
    }

    const fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_crypto::AeadKind;

    const KEY: [u8; 32] = [0x42; 32];
    const SESSION: u32 = 7;

    fn cipher() -> PacketCipher {
        PacketCipher::new(AeadKind::Aes256Gcm, KEY, SESSION)
    }

    fn every_action() -> Vec<InputAction> {
        vec![
            InputAction::Key {
                keycode: 30,
                pressed: true,
            },
            InputAction::Key {
                keycode: u32::MAX,
                pressed: false,
            },
            InputAction::MotionRelative { dx: -3.5, dy: 2.25 },
            InputAction::MotionAbsolute { x: 1919.0, y: 0.5 },
            InputAction::Button {
                button: 0x110,
                pressed: true,
            },
            InputAction::Scroll {
                dx: 0.0,
                dy: -120.0,
            },
        ]
    }

    #[test]
    fn plaintext_roundtrips_every_kind() {
        for action in every_action() {
            let bytes = encode_plaintext(9, 1_234_567, action);
            let env = decode_plaintext(&bytes).expect("decodes");
            assert_eq!(
                env,
                InputEnvelope {
                    sequence: 9,
                    client_ts_us: 1_234_567,
                    action
                }
            );
        }
    }

    #[test]
    fn plaintext_layout_is_kind_stamp_then_fields() {
        let bytes = encode_plaintext(
            3,
            5,
            InputAction::Key {
                keycode: 30,
                pressed: true,
            },
        );
        assert_eq!(bytes.len(), 22);
        assert_eq!(bytes[0], KIND_KEY);
        assert_eq!(&bytes[1..9], &5u64.to_le_bytes());
        assert_eq!(&bytes[9..17], &3u64.to_le_bytes());
        assert_eq!(&bytes[17..21], &30u32.to_le_bytes());
        assert_eq!(bytes[21], 1);
    }

    #[test]
    fn truncated_plaintext_is_rejected() {
        for action in every_action() {
            let bytes = encode_plaintext(1, 2, action);
            for cut in 0..bytes.len() {
                assert_eq!(
                    decode_plaintext(&bytes[..cut]),
                    Err(InputCodecError::Truncated),
                    "cut at {cut} for {action:?}"
                );
            }
        }
    }

    #[test]
    fn unknown_kind_is_rejected() {
        for kind in [0u8, KIND_SCROLL + 1, 0x7f, 0xff] {
            let mut bytes = encode_plaintext(
                1,
                2,
                InputAction::Key {
                    keycode: 30,
                    pressed: true,
                },
            );
            bytes[0] = kind;
            assert_eq!(
                decode_plaintext(&bytes),
                Err(InputCodecError::UnknownKind(kind))
            );
        }
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut bytes = encode_plaintext(
            1,
            2,
            InputAction::Button {
                button: 0x110,
                pressed: false,
            },
        );
        bytes.push(0);
        assert_eq!(
            decode_plaintext(&bytes),
            Err(InputCodecError::Malformed("trailing bytes after event"))
        );
    }

    #[test]
    fn bad_flag_byte_is_rejected() {
        let mut bytes = encode_plaintext(
            1,
            2,
            InputAction::Key {
                keycode: 30,
                pressed: true,
            },
        );
        bytes[21] = 2;
        assert_eq!(
            decode_plaintext(&bytes),
            Err(InputCodecError::Malformed("flag byte is not 0 or 1"))
        );
    }

    #[test]
    fn non_finite_coordinates_are_rejected_on_decode_and_seal() {
        let bytes = encode_plaintext(
            1,
            2,
            InputAction::MotionAbsolute {
                x: f64::NAN,
                y: 0.0,
            },
        );
        assert_eq!(
            decode_plaintext(&bytes),
            Err(InputCodecError::Malformed("non-finite coordinate"))
        );

        let mut tx = cipher();
        let action = InputAction::MotionRelative {
            dx: f64::INFINITY,
            dy: 0.0,
        };
        assert_eq!(
            seal_packet(&mut tx, 0, action),
            Err(InputCodecError::Malformed("non-finite coordinate"))
        );
        assert_eq!(tx.counter(), 0, "a rejected event must not use a sequence");
    }

    #[test]
    fn packet_roundtrips_and_sequence_follows_the_counter() {
        let mut tx = cipher();
        let rx = cipher();
        for (i, action) in every_action().into_iter().enumerate() {
            let datagram = seal_packet(&mut tx, 77, action).expect("seals");
            let env = open_packet(&rx, &datagram).expect("opens");
            assert_eq!(env.sequence, i as u64);
            assert_eq!(env.client_ts_us, 77);
            assert_eq!(env.action, action);
        }
    }

    #[test]
    fn chacha_packets_also_roundtrip() {
        let mut tx = PacketCipher::new(AeadKind::ChaCha20Poly1305, KEY, SESSION);
        let rx = PacketCipher::new(AeadKind::ChaCha20Poly1305, KEY, SESSION);
        let action = InputAction::Scroll { dx: 1.0, dy: -2.0 };
        let datagram = seal_packet(&mut tx, 5, action).expect("seals");
        assert_eq!(open_packet(&rx, &datagram).expect("opens").action, action);
    }

    #[test]
    fn truncated_datagram_is_rejected() {
        let mut tx = cipher();
        let rx = cipher();
        let datagram = seal_packet(
            &mut tx,
            1,
            InputAction::Key {
                keycode: 30,
                pressed: true,
            },
        )
        .expect("seals");
        assert_eq!(
            open_packet(&rx, &datagram[..SEQUENCE_LEN + TAG_LEN - 1]),
            Err(InputCodecError::Truncated)
        );
        assert_eq!(
            open_packet(&rx, &datagram[..datagram.len() - 1]),
            Err(InputCodecError::Authentication),
            "a datagram cut short fails the tag"
        );
    }

    #[test]
    fn oversize_datagram_is_rejected() {
        let rx = cipher();
        let big = vec![0u8; MAX_PACKET_LEN + 1];
        assert_eq!(
            open_packet(&rx, &big),
            Err(InputCodecError::Oversize(MAX_PACKET_LEN + 1))
        );
    }

    #[test]
    fn any_altered_byte_fails_authentication() {
        let mut tx = cipher();
        let rx = cipher();
        let datagram =
            seal_packet(&mut tx, 9, InputAction::MotionAbsolute { x: 3.0, y: 4.0 }).expect("seals");
        for index in 0..datagram.len() {
            let mut bad = datagram.clone();
            bad[index] ^= 0x01;
            assert_eq!(
                open_packet(&rx, &bad),
                Err(InputCodecError::Authentication),
                "flipped byte {index}"
            );
        }
    }

    #[test]
    fn other_key_or_session_fails_authentication() {
        let mut tx = cipher();
        let datagram = seal_packet(
            &mut tx,
            9,
            InputAction::Key {
                keycode: 30,
                pressed: true,
            },
        )
        .expect("seals");
        let wrong_key = PacketCipher::new(AeadKind::Aes256Gcm, [0x43; 32], SESSION);
        let wrong_session = PacketCipher::new(AeadKind::Aes256Gcm, KEY, SESSION + 1);
        assert_eq!(
            open_packet(&wrong_key, &datagram),
            Err(InputCodecError::Authentication)
        );
        assert_eq!(
            open_packet(&wrong_session, &datagram),
            Err(InputCodecError::Authentication)
        );
    }

    #[test]
    fn stamp_sequence_must_match_header() {
        let mut tx = cipher();
        let rx = cipher();
        for _ in 0..3 {
            seal_packet(
                &mut tx,
                0,
                InputAction::Key {
                    keycode: 1,
                    pressed: false,
                },
            )
            .expect("seals");
        }
        // The header carries counter 3, but the stamp inside claims sequence 99.
        let plaintext = encode_plaintext(
            99,
            0,
            InputAction::Key {
                keycode: 1,
                pressed: true,
            },
        );
        let datagram = tx.seal(&3u64.to_le_bytes(), &plaintext).expect("seals");
        assert_eq!(
            open_packet(&rx, &datagram),
            Err(InputCodecError::Malformed(
                "stamp sequence differs from header"
            ))
        );
    }

    #[test]
    fn replay_window_accepts_each_sequence_once() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(0));
        assert!(w.accept(1));
        assert!(!w.accept(1), "duplicate refused");
        assert!(!w.accept(0), "duplicate of the first refused");
    }

    #[test]
    fn replay_window_accepts_reordering_within_the_window() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(10));
        assert!(w.accept(12));
        assert!(w.accept(11), "late but unseen, inside the window");
        assert!(!w.accept(12), "already seen");
    }

    #[test]
    fn replay_window_refuses_what_falls_out_of_it() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(100));
        assert!(!w.accept(100 - REPLAY_WINDOW), "exactly one window behind");
        assert!(w.accept(100 - (REPLAY_WINDOW - 1)), "last slot inside");
    }

    #[test]
    fn replay_window_survives_a_large_jump() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(0));
        assert!(w.accept(1_000));
        assert!(w.accept(999), "one behind the new head is unseen");
        assert!(!w.accept(0), "old sequence now outside the window");
    }
}
