//! H.264 VA-API parameter buffers and NV12 copy helpers. Pure: no driver calls.

use lumen_ffi_va::sys;

use crate::config::{EncoderConfig, H264_LEVEL_IDC, H264Profile, RateControl};

/// Slice type values from H.264 Table 7-6 (`slice_type` modulo 5).
const SLICE_TYPE_P: u8 = 0;
const SLICE_TYPE_I: u8 = 2;

/// Sunshine sets `idr_interval = INT_MAX`; the same value disables periodic IDRs.
const NO_PERIODIC_IDR: u32 = i32::MAX as u32;

/// Initial QP for CBR; the rate controller moves it from here.
const CBR_INITIAL_QP: u8 = 26;

/// A reconstructed picture that the next P picture may reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RefPicture {
    /// Index of the reconstruction surface holding this picture.
    pub slot: usize,
    /// VA surface ID of that reconstruction surface.
    pub surface: u32,
    /// `frame_num` of this picture.
    pub frame_num: u16,
    /// Picture order count (lsb) of this picture.
    pub poc: u16,
}

/// Where the next picture sits in the stream. Plain data so it is testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Picture {
    /// Whether this is an IDR picture.
    pub idr: bool,
    /// Reconstruction slot the picture is written to.
    pub slot: usize,
    /// `frame_num` of the picture.
    pub frame_num: u16,
    /// Picture order count (lsb) of the picture.
    pub poc: u16,
    /// `idr_pic_id` for IDR pictures.
    pub idr_pic_id: u16,
}

impl Picture {
    /// Plan the next picture. With no B-frames, POC follows decode order.
    pub(crate) fn plan(last: Option<RefPicture>, force_idr: bool, idr_count: u16) -> Self {
        match last {
            Some(prev) if !force_idr => Self {
                idr: false,
                slot: 1 - prev.slot,
                frame_num: prev.frame_num.wrapping_add(1),
                poc: prev.poc.wrapping_add(2),
                idr_pic_id: idr_count,
            },
            _ => Self {
                idr: true,
                slot: 0,
                frame_num: 0,
                poc: 0,
                idr_pic_id: idr_count,
            },
        }
    }
}

/// `VAPictureH264` marking an unused reference entry.
fn invalid_pic() -> sys::VAPictureH264 {
    sys::VAPictureH264 {
        picture_id: sys::VA_INVALID_SURFACE,
        flags: sys::VA_PICTURE_H264_INVALID,
        ..Default::default()
    }
}

fn ref_entry(r: RefPicture) -> sys::VAPictureH264 {
    sys::VAPictureH264 {
        picture_id: r.surface,
        frame_idx: u32::from(r.frame_num),
        TopFieldOrderCnt: i32::from(r.poc),
        BottomFieldOrderCnt: i32::from(r.poc),
        ..Default::default()
    }
}

/// Initial QP written to the picture parameters.
#[must_use]
pub(crate) const fn init_qp(rc: RateControl) -> u8 {
    match rc {
        RateControl::Cqp { qp } => qp,
        RateControl::Cbr { .. } => CBR_INITIAL_QP,
    }
}

/// Macroblock grid `(width, height)`, rounded up to whole 16x16 macroblocks.
#[must_use]
pub(crate) const fn macroblocks(cfg: &EncoderConfig) -> (u32, u32) {
    (cfg.width.div_ceil(16), cfg.height.div_ceil(16))
}

/// Cropping `(right, bottom)` in 4:2:0 crop units (2 luma samples each) that maps the
/// macroblock grid back to the exact frame size. Only valid with `frame_mbs_only_flag`.
#[must_use]
pub(crate) const fn crop(cfg: &EncoderConfig) -> (u32, u32) {
    let (mbs_w, mbs_h) = macroblocks(cfg);
    ((mbs_w * 16 - cfg.width) / 2, (mbs_h * 16 - cfg.height) / 2)
}

/// `VAEncSequenceParameterBufferH264`: no B-frames (`ip_period = 1`), one
/// reference frame, infinite GOP, POC type 0, cropping to the exact frame size.
#[must_use]
pub(crate) fn h264_sequence(cfg: &EncoderConfig) -> sys::VAEncSequenceParameterBufferH264 {
    let (mbs_w, mbs_h) = macroblocks(cfg);
    let (crop_right, crop_bottom) = crop(cfg);

    let mut bits = sys::_VAEncSequenceParameterBufferH264__bindgen_ty_1__bindgen_ty_1::default();
    bits.set_chroma_format_idc(1);
    bits.set_frame_mbs_only_flag(1);
    bits.set_direct_8x8_inference_flag(1);
    bits.set_log2_max_frame_num_minus4(12);
    bits.set_pic_order_cnt_type(2);

    // Timing: time_scale / (2 * num_units_in_tick) == fps. Restriction flag tells
    // decoders that no picture is output later than decode order (no reordering).
    let mut vui = sys::_VAEncSequenceParameterBufferH264__bindgen_ty_2__bindgen_ty_1::default();
    vui.set_timing_info_present_flag(1);
    vui.set_fixed_frame_rate_flag(1);
    vui.set_bitstream_restriction_flag(1);

    sys::VAEncSequenceParameterBufferH264 {
        level_idc: H264_LEVEL_IDC,
        intra_period: NO_PERIODIC_IDR,
        intra_idr_period: NO_PERIODIC_IDR,
        ip_period: 1,
        bits_per_second: match cfg.rate_control {
            RateControl::Cbr { bits_per_second } => bits_per_second,
            RateControl::Cqp { .. } => 0,
        },
        max_num_ref_frames: 1,
        picture_width_in_mbs: mbs_w as u16,
        picture_height_in_mbs: mbs_h as u16,
        seq_fields: sys::_VAEncSequenceParameterBufferH264__bindgen_ty_1 { bits },
        frame_cropping_flag: u8::from(crop_right != 0 || crop_bottom != 0),
        frame_crop_right_offset: crop_right,
        frame_crop_bottom_offset: crop_bottom,
        vui_parameters_present_flag: 1,
        vui_fields: sys::_VAEncSequenceParameterBufferH264__bindgen_ty_2 { bits: vui },
        num_units_in_tick: 1,
        time_scale: cfg.fps.saturating_mul(2),
        ..Default::default()
    }
}

/// `pic_fields` bitfield for one picture.
#[must_use]
pub(crate) fn pic_bits(
    profile: H264Profile,
    idr: bool,
) -> sys::_VAEncPictureParameterBufferH264__bindgen_ty_1__bindgen_ty_1 {
    let mut bits = sys::_VAEncPictureParameterBufferH264__bindgen_ty_1__bindgen_ty_1::default();
    bits.set_idr_pic_flag(u32::from(idr));
    bits.set_reference_pic_flag(1);
    bits.set_entropy_coding_mode_flag(u32::from(profile.cabac()));
    bits.set_transform_8x8_mode_flag(u32::from(profile.transform_8x8()));
    bits.set_deblocking_filter_control_present_flag(1);
    bits
}

/// `VAEncPictureParameterBufferH264` for one picture. `coded_buf` receives the
/// bitstream, `curr` is the reconstruction surface, `reference` the one P reference.
#[must_use]
pub(crate) fn h264_picture(
    profile: H264Profile,
    rc: RateControl,
    pic: &Picture,
    curr: u32,
    reference: Option<RefPicture>,
    coded_buf: u32,
) -> sys::VAEncPictureParameterBufferH264 {
    let mut refs = [invalid_pic(); 16];
    if let Some(r) = reference {
        refs[0] = ref_entry(r);
    }
    sys::VAEncPictureParameterBufferH264 {
        CurrPic: sys::VAPictureH264 {
            picture_id: curr,
            frame_idx: u32::from(pic.frame_num),
            TopFieldOrderCnt: i32::from(pic.poc),
            BottomFieldOrderCnt: i32::from(pic.poc),
            ..Default::default()
        },
        ReferenceFrames: refs,
        coded_buf,
        frame_num: pic.frame_num,
        pic_init_qp: init_qp(rc),
        pic_fields: sys::_VAEncPictureParameterBufferH264__bindgen_ty_1 {
            bits: pic_bits(profile, pic.idr),
        },
        ..Default::default()
    }
}

/// `VAEncSliceParameterBufferH264`: one slice covering the whole picture.
#[must_use]
pub(crate) fn h264_slice(
    cfg: &EncoderConfig,
    pic: &Picture,
    reference: Option<RefPicture>,
) -> sys::VAEncSliceParameterBufferH264 {
    let mut ref_list = [invalid_pic(); 32];
    if let Some(r) = reference {
        ref_list[0] = ref_entry(r);
    }
    sys::VAEncSliceParameterBufferH264 {
        num_macroblocks: cfg.width.div_ceil(16) * cfg.height.div_ceil(16),
        macroblock_info: sys::VA_INVALID_ID,
        slice_type: if pic.idr { SLICE_TYPE_I } else { SLICE_TYPE_P },
        idr_pic_id: pic.idr_pic_id,
        pic_order_cnt_lsb: pic.poc,
        num_ref_idx_active_override_flag: u8::from(!pic.idr),
        RefPicList0: ref_list,
        ..Default::default()
    }
}

/// Misc-parameter buffer with a `VAEncMiscParameterRateControl` payload.
/// libva reads a u32 type followed by the payload, which this `repr(C)` pair matches.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct MiscRateControl {
    ty: sys::VAEncMiscParameterType,
    rc: sys::VAEncMiscParameterRateControl,
}

impl MiscRateControl {
    pub(crate) fn cbr(bits_per_second: u32) -> Self {
        Self {
            ty: sys::VAEncMiscParameterTypeRateControl,
            rc: sys::VAEncMiscParameterRateControl {
                bits_per_second,
                target_percentage: 100,
                window_size: 1000,
                initial_qp: u32::from(CBR_INITIAL_QP),
                min_qp: 10,
                max_qp: 51,
                ..Default::default()
            },
        }
    }
}

/// Misc-parameter buffer with a `VAEncMiscParameterFrameRate` payload.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct MiscFrameRate {
    ty: sys::VAEncMiscParameterType,
    fr: sys::VAEncMiscParameterFrameRate,
}

impl MiscFrameRate {
    pub(crate) fn new(fps: u32) -> Self {
        Self {
            ty: sys::VAEncMiscParameterTypeFrameRate,
            fr: sys::VAEncMiscParameterFrameRate {
                framerate: fps,
                ..Default::default()
            },
        }
    }
}

/// EXPERIMENT: `VAEncMiscParameterQualityLevel` payload.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct MiscQuality {
    ty: sys::VAEncMiscParameterType,
    q: sys::_VAEncMiscParameterBufferQualityLevel,
}

impl MiscQuality {
    pub(crate) fn new(level: u32) -> Self {
        Self {
            ty: sys::VAEncMiscParameterTypeQualityLevel,
            q: sys::_VAEncMiscParameterBufferQualityLevel {
                quality_level: level,
                ..Default::default()
            },
        }
    }
}

/// `(offset, pitch)` of the Y and UV planes of an NV12 image, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Nv12Planes {
    /// `(offset, pitch)` of the luma plane.
    pub y: (usize, usize),
    /// `(offset, pitch)` of the interleaved chroma plane.
    pub uv: (usize, usize),
}

impl Nv12Planes {
    /// Tightly packed `width` x `height` NV12.
    #[must_use]
    pub(crate) const fn packed(width: usize, height: usize) -> Self {
        Self {
            y: (0, width),
            uv: (width * height, width),
        }
    }
}

/// Copy one NV12 image between two layouts, row by row. Each row is bounds
/// checked, so a short source or destination yields an error, never a panic.
pub(crate) fn copy_nv12(
    src: &[u8],
    src_planes: Nv12Planes,
    dst: &mut [u8],
    dst_planes: Nv12Planes,
    width: usize,
    height: usize,
) -> Result<(), String> {
    copy_plane(src, src_planes.y, dst, dst_planes.y, width, height)?;
    copy_plane(src, src_planes.uv, dst, dst_planes.uv, width, height / 2)
}

fn copy_plane(
    src: &[u8],
    (s_off, s_pitch): (usize, usize),
    dst: &mut [u8],
    (d_off, d_pitch): (usize, usize),
    row_bytes: usize,
    rows: usize,
) -> Result<(), String> {
    for row in 0..rows {
        let s = src
            .get(s_off + row * s_pitch..)
            .and_then(|r| r.get(..row_bytes))
            .ok_or("source frame is shorter than its declared NV12 layout")?;
        let d = dst
            .get_mut(d_off + row * d_pitch..)
            .and_then(|r| r.get_mut(..row_bytes))
            .ok_or("destination image is shorter than the NV12 layout")?;
        d.copy_from_slice(s);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(w: u32, h: u32) -> EncoderConfig {
        EncoderConfig::new(w, h, 60)
    }

    #[test]
    fn first_picture_is_idr_then_alternates_slots() {
        let p0 = Picture::plan(None, false, 0);
        assert!(p0.idr && p0.frame_num == 0 && p0.slot == 0);
        let last = RefPicture {
            slot: p0.slot,
            surface: 1,
            frame_num: p0.frame_num,
            poc: p0.poc,
        };
        let p1 = Picture::plan(Some(last), false, 1);
        assert!(!p1.idr && p1.frame_num == 1 && p1.slot == 1 && p1.poc == 2);
        let last = RefPicture {
            slot: p1.slot,
            surface: 2,
            frame_num: p1.frame_num,
            poc: p1.poc,
        };
        let p2 = Picture::plan(Some(last), false, 1);
        assert_eq!((p2.slot, p2.frame_num, p2.poc), (0, 2, 4));
    }

    #[test]
    fn force_idr_resets_frame_num_and_poc() {
        let last = RefPicture {
            slot: 1,
            surface: 2,
            frame_num: 77,
            poc: 154,
        };
        let p = Picture::plan(Some(last), true, 5);
        assert!(p.idr && p.frame_num == 0 && p.poc == 0 && p.idr_pic_id == 5);
    }

    #[test]
    fn sequence_crops_1080p_and_sets_no_b_frames() {
        let s = h264_sequence(&cfg(1920, 1080));
        assert_eq!((s.picture_width_in_mbs, s.picture_height_in_mbs), (120, 68));
        assert_eq!(s.frame_cropping_flag, 1);
        assert_eq!(s.frame_crop_bottom_offset, 4);
        assert_eq!(s.frame_crop_right_offset, 0);
        assert_eq!(s.ip_period, 1);
        assert_eq!(s.max_num_ref_frames, 1);
        assert_eq!(s.intra_idr_period, NO_PERIODIC_IDR);
        assert_eq!(s.bits_per_second, 0);
    }

    #[test]
    fn sequence_no_crop_when_mb_aligned() {
        let s = h264_sequence(&cfg(1280, 720));
        assert_eq!(s.frame_cropping_flag, 0);
        assert_eq!(s.frame_crop_bottom_offset, 0);
        assert_eq!(s.time_scale, 120);
    }

    #[test]
    fn idr_picture_sets_idr_flag_and_invalid_refs() {
        let p = Picture::plan(None, false, 0);
        let pic = h264_picture(H264Profile::High, RateControl::default(), &p, 10, None, 20);
        let bits = pic_bits(H264Profile::High, true);
        assert_eq!(bits.idr_pic_flag(), 1);
        assert_eq!(bits.entropy_coding_mode_flag(), 1);
        assert_eq!(bits.transform_8x8_mode_flag(), 1);
        assert!(
            pic.ReferenceFrames
                .iter()
                .all(|r| r.picture_id == sys::VA_INVALID_SURFACE)
        );
        assert_eq!(pic.coded_buf, 20);
        assert_eq!(pic.pic_init_qp, 22);
    }

    #[test]
    fn baseline_profile_uses_cavlc_and_no_8x8() {
        let bits = pic_bits(H264Profile::ConstrainedBaseline, false);
        assert_eq!(bits.idr_pic_flag(), 0);
        assert_eq!(bits.entropy_coding_mode_flag(), 0);
        assert_eq!(bits.transform_8x8_mode_flag(), 0);
    }

    #[test]
    fn p_picture_references_previous_surface() {
        let last = RefPicture {
            slot: 0,
            surface: 10,
            frame_num: 0,
            poc: 0,
        };
        let p = Picture::plan(Some(last), false, 0);
        let pic = h264_picture(
            H264Profile::Main,
            RateControl::default(),
            &p,
            11,
            Some(last),
            20,
        );
        assert_eq!(pic.ReferenceFrames[0].picture_id, 10);
        assert_eq!(pic.ReferenceFrames[1].picture_id, sys::VA_INVALID_SURFACE);
        assert_eq!(pic_bits(H264Profile::Main, false).idr_pic_flag(), 0);
        let s = h264_slice(&cfg(1280, 720), &p, Some(last));
        assert_eq!(s.slice_type, SLICE_TYPE_P);
        assert_eq!(s.RefPicList0[0].picture_id, 10);
        assert_eq!(s.num_macroblocks, 80 * 45);
    }

    #[test]
    fn copy_nv12_honours_pitches() {
        let src: Vec<u8> = (0..12).collect(); // 4x2 NV12, tightly packed
        let mut dst = vec![0xEE_u8; 32];
        let dst_planes = Nv12Planes {
            y: (0, 8),
            uv: (16, 8),
        };
        copy_nv12(&src, Nv12Planes::packed(4, 2), &mut dst, dst_planes, 4, 2).unwrap();
        assert_eq!(&dst[0..4], &[0, 1, 2, 3]);
        assert_eq!(&dst[8..12], &[4, 5, 6, 7]);
        assert_eq!(&dst[16..20], &[8, 9, 10, 11]);
        assert_eq!(dst[4], 0xEE, "padding between pitched rows stays untouched");
    }

    #[test]
    fn copy_nv12_rejects_short_source() {
        let mut dst = vec![0_u8; 64];
        let r = copy_nv12(
            &[0; 4],
            Nv12Planes::packed(4, 2),
            &mut dst,
            Nv12Planes::packed(4, 2),
            4,
            2,
        );
        assert!(r.is_err());
    }
}
