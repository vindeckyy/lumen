//! H.264 packed headers: SPS, PPS and slice headers.
//!
//! The driver needs the SPS and PPS, and each slice header, as packed headers. This module
//! writes them in the layout FFmpeg's VA-API H.264 encoder submits: SPS then PPS in one
//! sequence header, and a slice header per picture. The VA buffers in `buffer.rs` must agree
//! with these bytes, because the driver encodes slices against those values.

use lumen_ffi_va::sys;

use crate::{
    buffer::{Picture, crop, init_qp, macroblocks},
    config::{EncoderConfig, H264_LEVEL_IDC, H264Profile, RateControl},
};

/// `log2_max_frame_num_minus4`: 16-bit `frame_num`, the width the VA picture buffer assumes.
const LOG2_MAX_FRAME_NUM_MINUS4: u32 = 12;

/// `nal_ref_idc = 3` (reference picture) in the NAL header.
const NAL_REF_IDC_HIGHEST: u8 = 3 << 5;
const NAL_TYPE_SLICE: u8 = 1;
const NAL_TYPE_IDR_SLICE: u8 = 5;
const NAL_TYPE_SPS: u8 = 7;
const NAL_TYPE_PPS: u8 = 8;

/// Bytes of start code plus NAL header that precede the RBSP in a packed buffer.
const PREFIX_BITS: usize = (4 + 1) * 8;

/// One packed header: the VA type, the bytes of the data buffer and the exact bit count.
#[derive(Debug)]
pub(crate) struct PackedHeader {
    /// `VAEncPackedHeaderSequence`, `...Picture` or `...Slice`.
    pub ty: sys::VAEncPackedHeaderType,
    /// Start code, NAL header, then the RBSP with emulation-prevention bytes inserted.
    pub bytes: Vec<u8>,
    /// Exact bit count of the header; a slice header need not end on a byte boundary.
    pub bit_length: u32,
}

/// MSB-first bit writer for RBSP syntax elements.
#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    fn bit(&mut self, b: bool) {
        if self.bits / 8 == self.bytes.len() {
            self.bytes.push(0);
        }
        if b {
            self.bytes[self.bits / 8] |= 0x80 >> (self.bits % 8);
        }
        self.bits += 1;
    }

    /// `u(n)`: the low `n` bits of `value`, most significant first.
    fn bits(&mut self, value: u32, n: u32) {
        for i in (0..n).rev() {
            self.bit((value >> i) & 1 == 1);
        }
    }

    /// `ue(v)`: unsigned Exp-Golomb.
    fn ue(&mut self, v: u32) {
        let x = u64::from(v) + 1;
        let len = 64 - x.leading_zeros();
        for _ in 1..len {
            self.bit(false);
        }
        for i in (0..len).rev() {
            self.bit((x >> i) & 1 == 1);
        }
    }

    /// `se(v)`: signed Exp-Golomb, mapping positive `k` to `2k - 1` and negative `k` to `-2k`.
    fn se(&mut self, v: i32) {
        let code = if v > 0 {
            2 * i64::from(v) - 1
        } else {
            -2 * i64::from(v)
        };
        self.ue(code as u32);
    }

    /// Appends `rbsp_trailing_bits()` and returns the byte-aligned RBSP.
    fn finish_aligned(mut self) -> Vec<u8> {
        self.bit(true);
        while !self.bits.is_multiple_of(8) {
            self.bit(false);
        }
        self.bytes
    }
}

/// Inserts `0x03` after each pair of zero bytes that is followed by a byte of 0..=3
/// (`emulation_prevention_three_byte`, H.264 7.4.1). Returns the escaped bytes and the
/// number of bytes inserted.
fn escape(rbsp: &[u8]) -> (Vec<u8>, usize) {
    let mut out = Vec::with_capacity(rbsp.len() + 8);
    let mut inserted = 0;
    let mut zeros = 0_u32;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            inserted += 1;
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    (out, inserted)
}

/// Builds a packed header from an RBSP of `rbsp_bits` bits (the bytes may hold padding).
fn packed(
    ty: sys::VAEncPackedHeaderType,
    nal_type: u8,
    rbsp: &[u8],
    rbsp_bits: usize,
) -> PackedHeader {
    let (escaped, inserted) = escape(rbsp);
    let mut bytes = vec![0, 0, 0, 1, NAL_REF_IDC_HIGHEST | nal_type];
    bytes.extend(escaped);
    PackedHeader {
        ty,
        bytes,
        bit_length: (PREFIX_BITS + rbsp_bits + 8 * inserted) as u32,
    }
}

/// `seq_parameter_set_rbsp()` matching `h264_sequence` in `buffer.rs`.
#[must_use]
pub(crate) fn sps(cfg: &EncoderConfig, profile: H264Profile) -> PackedHeader {
    let (profile_idc, constraint_flags) = match profile {
        H264Profile::High => (100, 0),
        H264Profile::Main => (77, 0),
        // constraint_set0_flag and constraint_set1_flag make it Constrained Baseline.
        H264Profile::ConstrainedBaseline => (66, 0xC0),
    };
    let mut w = BitWriter::default();
    w.bits(profile_idc, 8);
    w.bits(constraint_flags, 8);
    w.bits(u32::from(H264_LEVEL_IDC), 8);
    w.ue(0); // seq_parameter_set_id
    if profile == H264Profile::High {
        w.ue(1); // chroma_format_idc: 4:2:0
        w.ue(0); // bit_depth_luma_minus8
        w.ue(0); // bit_depth_chroma_minus8
        w.bit(false); // qpprime_y_zero_transform_bypass_flag
        w.bit(false); // seq_scaling_matrix_present_flag
    }
    w.ue(LOG2_MAX_FRAME_NUM_MINUS4);
    w.ue(2); // pic_order_cnt_type: POC derived from frame_num, valid without B-frames
    w.ue(crate::buffer::ref_frames(cfg)); // max_num_ref_frames
    // A recovery after a loss skips the frame_nums the client never got; with the flag the
    // decoder infers them (8.2.5.2) instead of treating the gap as an error.
    w.bit(true); // gaps_in_frame_num_value_allowed_flag
    let (mbs_w, mbs_h) = macroblocks(cfg);
    w.ue(mbs_w - 1);
    w.ue(mbs_h - 1); // pic_height_in_map_units_minus1 (frame_mbs_only_flag is set)
    w.bit(true); // frame_mbs_only_flag
    w.bit(true); // direct_8x8_inference_flag
    let (crop_right, crop_bottom) = crop(cfg);
    let cropped = crop_right != 0 || crop_bottom != 0;
    w.bit(cropped);
    if cropped {
        w.ue(0); // frame_crop_left_offset
        w.ue(crop_right);
        w.ue(0); // frame_crop_top_offset
        w.ue(crop_bottom);
    }
    w.bit(true); // vui_parameters_present_flag
    write_vui(&mut w, cfg.fps, crate::buffer::ref_frames(cfg));
    let rbsp = w.finish_aligned();
    let bits = rbsp.len() * 8;
    packed(sys::VAEncPackedHeaderSequence, NAL_TYPE_SPS, &rbsp, bits)
}

/// `vui_parameters()`: timing for the frame rate, and a bitstream restriction that
/// promises no reordering, so decoders output on arrival.
fn write_vui(w: &mut BitWriter, fps: u32, ref_frames: u32) {
    w.bit(false); // aspect_ratio_info_present_flag
    w.bit(false); // overscan_info_present_flag
    w.bit(false); // video_signal_type_present_flag
    w.bit(false); // chroma_loc_info_present_flag
    w.bit(true); // timing_info_present_flag
    w.bits(1, 32); // num_units_in_tick
    w.bits(fps.saturating_mul(2), 32); // time_scale
    w.bit(true); // fixed_frame_rate_flag
    w.bit(false); // nal_hrd_parameters_present_flag
    w.bit(false); // vcl_hrd_parameters_present_flag
    w.bit(false); // pic_struct_present_flag
    w.bit(true); // bitstream_restriction_flag
    w.bit(true); // motion_vectors_over_pic_boundaries_flag
    w.ue(0); // max_bytes_per_pic_denom
    w.ue(0); // max_bits_per_mb_denom
    w.ue(16); // log2_max_mv_length_horizontal
    w.ue(16); // log2_max_mv_length_vertical
    w.ue(0); // max_num_reorder_frames
    w.ue(ref_frames); // max_dec_frame_buffering: at least max_num_ref_frames
}

/// `pic_parameter_set_rbsp()` matching `pic_bits` and `h264_picture` in `buffer.rs`.
#[must_use]
pub(crate) fn pps(profile: H264Profile, rc: RateControl) -> PackedHeader {
    let mut w = BitWriter::default();
    w.ue(0); // pic_parameter_set_id
    w.ue(0); // seq_parameter_set_id
    w.bit(profile.cabac()); // entropy_coding_mode_flag
    w.bit(false); // bottom_field_pic_order_in_frame_present_flag
    w.ue(0); // num_slice_groups_minus1
    w.ue(0); // num_ref_idx_l0_default_active_minus1
    w.ue(0); // num_ref_idx_l1_default_active_minus1
    w.bit(false); // weighted_pred_flag
    w.bits(0, 2); // weighted_bipred_idc
    w.se(i32::from(init_qp(rc)) - 26); // pic_init_qp_minus26
    w.se(0); // pic_init_qs_minus26
    w.se(0); // chroma_qp_index_offset
    w.bit(true); // deblocking_filter_control_present_flag
    w.bit(false); // constrained_intra_pred_flag
    w.bit(false); // redundant_pic_cnt_present_flag
    if profile == H264Profile::High {
        w.bit(profile.transform_8x8()); // transform_8x8_mode_flag
        w.bit(false); // pic_scaling_matrix_present_flag
        w.se(0); // second_chroma_qp_index_offset
    }
    let rbsp = w.finish_aligned();
    let bits = rbsp.len() * 8;
    packed(sys::VAEncPackedHeaderSequence, NAL_TYPE_PPS, &rbsp, bits)
}

/// SPS followed by PPS in one packed header of type `VAEncPackedHeaderSequence`. This is
/// the layout FFmpeg's VA-API H.264 encoder submits, and the one the driver consumes.
#[must_use]
pub(crate) fn sequence_headers(cfg: &EncoderConfig, profile: H264Profile) -> PackedHeader {
    let s = sps(cfg, profile);
    let p = pps(profile, cfg.rate_control);
    let mut bytes = s.bytes;
    bytes.extend(p.bytes);
    PackedHeader {
        ty: sys::VAEncPackedHeaderSequence,
        bytes,
        bit_length: s.bit_length + p.bit_length,
    }
}

/// `slice_header()` for one picture, matching the single-slice layout in `buffer.rs`.
/// It carries no slice data and no trailing bits.
#[must_use]
pub(crate) fn slice(profile: H264Profile, pic: &Picture) -> PackedHeader {
    let mut w = BitWriter::default();
    w.ue(0); // first_mb_in_slice
    w.ue(if pic.idr { 7 } else { 5 }); // slice_type: I or P, all slices of the picture
    w.ue(0); // pic_parameter_set_id
    w.bits(u32::from(pic.frame_num), 16); // frame_num
    if pic.idr {
        w.ue(u32::from(pic.idr_pic_id));
    }
    // POC type 2 carries no pic_order_cnt_lsb in the slice header.
    if !pic.idr {
        w.bit(true); // num_ref_idx_active_override_flag
        w.ue(0); // num_ref_idx_l0_active_minus1
        w.bit(pic.modification.is_some()); // ref_pic_list_modification_flag_l0
        if let Some(abs_diff_pic_num_minus1) = pic.modification {
            w.ue(0); // modification_of_pic_nums_idc: subtract from the predicted PicNum
            w.ue(abs_diff_pic_num_minus1);
            w.ue(3); // end of the modifications
        }
    }
    if pic.idr {
        w.bit(false); // no_output_of_prior_pics_flag
        w.bit(false); // long_term_reference_flag
    } else {
        w.bit(false); // adaptive_ref_pic_marking_mode_flag
    }
    if profile.cabac() && !pic.idr {
        w.ue(0); // cabac_init_idc
    }
    w.se(0); // slice_qp_delta: the picture QP is pic_init_qp
    w.ue(0); // disable_deblocking_filter_idc
    w.se(0); // slice_alpha_c0_offset_div2
    w.se(0); // slice_beta_offset_div2
    let bits = w.bits;
    let rbsp = w.bytes;
    let nal_type = if pic.idr {
        NAL_TYPE_IDR_SLICE
    } else {
        NAL_TYPE_SLICE
    };
    packed(sys::VAEncPackedHeaderSlice, nal_type, &rbsp, bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb_codes() {
        let mut w = BitWriter::default();
        w.ue(0); // 1
        w.ue(1); // 010
        w.ue(2); // 011
        w.se(1); // codeNum 1 -> 010
        w.se(-1); // codeNum 2 -> 011
        // 1 010 011 010 011, then trailing 1 and zero pad: 1010 0110 1001 1100
        assert_eq!(w.finish_aligned(), vec![0b1010_0110, 0b1001_1100]);
    }

    #[test]
    fn escape_inserts_after_two_zeros() {
        assert_eq!(escape(&[0, 0, 1]), (vec![0, 0, 3, 1], 1));
        assert_eq!(escape(&[0, 0, 0, 1]), (vec![0, 0, 3, 0, 1], 1));
        assert_eq!(escape(&[0, 0, 4]), (vec![0, 0, 4], 0));
    }

    #[test]
    fn packed_nal_has_start_code_and_type() {
        let p = packed(sys::VAEncPackedHeaderPicture, NAL_TYPE_PPS, &[0xAB], 8);
        assert_eq!(p.bytes, vec![0, 0, 0, 1, 0x68, 0xAB]);
        assert_eq!(p.bit_length, 6 * 8);
    }

    #[test]
    fn sps_high_header_fields() {
        let cfg = EncoderConfig::new(1920, 1080, 60);
        let p = sps(&cfg, H264Profile::High);
        // start code, NAL header 0x67 (ref_idc 3, SPS), profile_idc 100, constraints 0, level 5.2
        assert_eq!(&p.bytes[..8], &[0, 0, 0, 1, 0x67, 100, 0, 52]);
        assert_eq!(p.ty, sys::VAEncPackedHeaderSequence);
        assert_eq!(p.bit_length as usize, p.bytes.len() * 8);
    }

    #[test]
    fn constrained_baseline_sets_constraint_flags() {
        let cfg = EncoderConfig::new(1280, 720, 60);
        let p = sps(&cfg, H264Profile::ConstrainedBaseline);
        assert_eq!(&p.bytes[4..7], &[0x67, 66, 0xC0]);
    }

    #[test]
    fn pps_high_carries_transform_flag_and_init_qp() {
        let high = pps(H264Profile::High, RateControl::Cqp { qp: 22 });
        let main = pps(H264Profile::Main, RateControl::Cqp { qp: 22 });
        assert!(
            high.bytes.len() >= main.bytes.len(),
            "High adds the extension fields"
        );
        assert_ne!(high.bytes, main.bytes);
        assert_eq!(high.bytes[4], 0x68);
        assert_eq!(high.ty, sys::VAEncPackedHeaderSequence);
    }

    #[test]
    fn sequence_header_is_sps_then_pps() {
        let cfg = EncoderConfig::new(1280, 720, 60);
        let seq = sequence_headers(&cfg, H264Profile::High);
        let s = sps(&cfg, H264Profile::High);
        let p = pps(H264Profile::High, cfg.rate_control);
        let split = s.bytes.len();
        assert_eq!(seq.bytes.len(), split + p.bytes.len());
        assert_eq!(&seq.bytes[split..split + 5], &[0, 0, 0, 1, 0x68]);
        assert_eq!(seq.bit_length, s.bit_length + p.bit_length);
    }

    #[test]
    fn slice_header_nal_type_follows_picture() {
        let idr = Picture::plan(None, 0, false, 0);
        let s = slice(H264Profile::High, &idr);
        assert_eq!(s.bytes[4], 0x65, "IDR slice: ref_idc 3, type 5");
        assert_eq!(s.ty, sys::VAEncPackedHeaderSlice);
        let last = crate::buffer::RefPicture {
            slot: 0,
            surface: 1,
            frame_num: 0,
            poc: 0,
        };
        let p = Picture::plan(Some(last), 1, false, 0);
        assert_eq!(
            slice(H264Profile::High, &p).bytes[4],
            0x61,
            "non-IDR slice: ref_idc 3, type 1"
        );
    }
}
