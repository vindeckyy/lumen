//! Annex-B byte stream helpers for H.264 encoder output.

/// `nal_unit_type` of a coded IDR slice.
pub const NAL_IDR_SLICE: u8 = 5;
/// `nal_unit_type` of a sequence parameter set.
pub const NAL_SPS: u8 = 7;
/// `nal_unit_type` of supplemental enhancement information.
pub const NAL_SEI: u8 = 6;

/// An SEI NAL unit with one recovery point message (D.1.8): `recovery_frame_cnt` 0 and
/// `exact_match_flag` 1, so the picture it precedes, and every one after it, decodes exactly.
/// Payload type 6, size 1, then the bits `1` (ue 0), `1`, `0`, `00`, padded, then the RBSP stop bit.
pub const RECOVERY_POINT_SEI: [u8; 8] = [0, 0, 0, 1, 0x06, 0x06, 0x01, 0xC0];

/// The access unit with a recovery point SEI before its first slice, which marks a picture
/// that recovers from a loss without being an IDR.
#[must_use]
pub fn with_recovery_point(au: &[u8]) -> Vec<u8> {
    let first_slice = au
        .windows(3)
        .enumerate()
        .filter(|(_, w)| *w == [0, 0, 1])
        .find(|(i, _)| au.get(i + 3).is_some_and(|b| matches!(b & 0x1f, 1 | 5)))
        // A four-byte start code begins one byte earlier.
        .map_or(0, |(i, _)| if i > 0 && au[i - 1] == 0 { i - 1 } else { i });
    let mut out = Vec::with_capacity(au.len() + RECOVERY_POINT_SEI.len() + 1);
    out.extend_from_slice(&au[..first_slice]);
    out.extend_from_slice(&RECOVERY_POINT_SEI);
    out.push(0x80); // rbsp_trailing_bits of the SEI
    out.extend_from_slice(&au[first_slice..]);
    out
}

/// Whether the stream begins with a 3- or 4-byte Annex-B start code.
#[must_use]
pub fn starts_with_start_code(stream: &[u8]) -> bool {
    stream.starts_with(&[0, 0, 0, 1]) || stream.starts_with(&[0, 0, 1])
}

/// `nal_unit_type` of every NAL unit in the stream, in order.
///
/// Emulation prevention guarantees `00 00 01` cannot occur inside a NAL
/// payload, so every match is a real start code.
pub fn nal_types(stream: &[u8]) -> impl Iterator<Item = u8> + '_ {
    stream
        .windows(3)
        .enumerate()
        .filter(|(_, w)| *w == [0, 0, 1])
        .filter_map(move |(i, _)| stream.get(i + 3).map(|b| b & 0x1f))
}

/// Whether any NAL unit in the stream has the given type.
#[must_use]
pub fn contains_nal(stream: &[u8], ty: u8) -> bool {
    nal_types(stream).any(|t| t == ty)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_idr_and_sps_in_four_and_three_byte_codes() {
        // SPS (0x67) with 4-byte start code, IDR (0x65) with 3-byte start code.
        let s = [0, 0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x65, 0x88];
        assert!(starts_with_start_code(&s));
        assert_eq!(nal_types(&s).collect::<Vec<_>>(), vec![7, 5]);
        assert!(contains_nal(&s, NAL_IDR_SLICE));
        assert!(contains_nal(&s, NAL_SPS));
    }

    #[test]
    fn p_slice_is_not_idr() {
        let s = [0, 0, 0, 1, 0x41, 0x9a];
        assert!(!contains_nal(&s, NAL_IDR_SLICE));
    }

    #[test]
    fn the_recovery_point_goes_before_the_first_slice() {
        let au = [0, 0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x41, 0x9a];
        let out = with_recovery_point(&au);
        assert_eq!(nal_types(&out).collect::<Vec<_>>(), vec![9, 6, 1]);
        assert_eq!(&out[6..15], &[0, 0, 0, 1, 0x06, 0x06, 0x01, 0xC0, 0x80]);
    }

    #[test]
    fn rejects_missing_start_code() {
        assert!(!starts_with_start_code(&[0x65, 0, 0, 1]));
    }
}
