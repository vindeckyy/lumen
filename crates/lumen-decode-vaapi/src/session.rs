//! One VA-API decode session for one SPS: context, decode-surface pool and reference store.
//! A change of SPS (size, profile or cropping) builds a new session, and with it an empty
//! reference store.

use lumen_ffi_va::{OwnedVaBuffer, OwnedVaSurface, VaContext, VaDisplay, sys};
use lumen_protocol::media::{FrameBuffer, KeepAlive, PixelFormat};

use crate::bitstream::{H264Profile, Picture, Pps, PredWeightTable, SliceHeader, SliceKind, Sps};
use crate::dpb::{NON_EXISTING, PicturePlan, RefPic, RefStore};
use crate::output::{self, Crop, Nv12Layout};

/// Minimum decode surfaces per session. The pool grows to one more than the reference count,
/// so the current picture always has a surface that no reference uses.
// shortcut: exported DMA-BUF frames alias pool slots, so a consumer must drop a frame before
// the pool wraps; upgrade to refcounted release when lumen-render holds frames.
const POOL: usize = 8;

/// `slice_type` values in the VA-API slice parameter (H.264 Table 7-6, modulo 5).
const SLICE_TYPE_P: u8 = 0;
const SLICE_TYPE_I: u8 = 2;

fn va_err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Map a supported H.264 profile to its VA-API decode profile.
fn va_profile(profile: H264Profile) -> sys::VAProfile {
    match profile {
        // VAProfileH264Baseline is deprecated in va.h; drivers expose ConstrainedBaseline for
        // baseline streams.
        H264Profile::Baseline => sys::VAProfileH264ConstrainedBaseline,
        H264Profile::Main => sys::VAProfileH264Main,
        H264Profile::High => sys::VAProfileH264High,
    }
}

/// `CurrPic.flags`: a reference picture is marked short-term once it is decoded (8.2.5.1), so
/// its entry carries the same marking the reference lists use. Non-reference pictures carry none.
fn curr_pic_flags(header: &SliceHeader) -> u32 {
    let long_term = header.long_term_reference
        || header.mmco.as_ref().is_some_and(|ops| {
            ops.iter()
                .any(|op| matches!(op, crate::bitstream::Mmco::CurrentToLongTerm { .. }))
        });
    if long_term {
        sys::VA_PICTURE_H264_LONG_TERM_REFERENCE
    } else if header.reference {
        sys::VA_PICTURE_H264_SHORT_TERM_REFERENCE
    } else {
        0
    }
}

/// Decode state for one SPS. Field order is drop order: the context before its surfaces.
#[derive(Debug)]
pub struct Session {
    context: VaContext,
    surfaces: Vec<OwnedVaSurface>,
    /// One lease per pool slot: the session holds one, every exported picture holds another.
    leases: Vec<KeepAlive>,
    refs: RefStore,
    sps: Sps,
}

impl Session {
    /// Create the config, the decode-surface pool, the context and an empty reference store for `sps`.
    ///
    /// # Errors
    /// Returns a description when the driver cannot decode this profile or allocation fails.
    pub fn new(display: &VaDisplay, sps: &Sps) -> Result<Self, String> {
        let profile = va_profile(sps.profile);
        if !display.supports(profile, sys::VAEntrypointVLD) {
            return Err(format!(
                "VA-API driver has no H.264 decode entry point for profile {profile}"
            ));
        }
        let attribs = [sys::VAConfigAttrib {
            type_: sys::VAConfigAttribRTFormat,
            value: sys::VA_RT_FORMAT_YUV420,
        }];
        // The config is only needed to create the context; the context does not keep it.
        let config = display
            .create_config(profile, sys::VAEntrypointVLD, &attribs)
            .map_err(va_err)?;
        let refs = RefStore::new(sps);
        let pool = POOL.max(refs.max_refs() + 1);
        let surfaces = OwnedVaSurface::create_batch(
            display,
            PixelFormat::Nv12,
            sps.coded_width,
            sps.coded_height,
            pool,
        )
        .map_err(va_err)?;
        let render_targets: Vec<&OwnedVaSurface> = surfaces.iter().collect();
        let context =
            VaContext::create(&config, sps.coded_width, sps.coded_height, &render_targets)
                .map_err(va_err)?;
        Ok(Self {
            context,
            leases: (0..pool).map(|_| KeepAlive::new()).collect(),
            surfaces,
            refs,
            sps: sps.clone(),
        })
    }

    /// A pool slot that no reference holds and that no exported picture still uses.
    ///
    /// A picture the compositor still reads aliases its slot, so the decoder must not write
    /// over it: the lease of the slot is held by that picture until the consumer drops it.
    fn free_slot(&self, pool: usize) -> Option<usize> {
        (0..pool).find(|slot| {
            !self.refs.refs().iter().any(|r| r.slot == *slot) && !self.leases[*slot].is_held()
        })
    }

    /// Whether this session was built for `sps`.
    #[must_use]
    pub fn matches(&self, sps: &Sps) -> bool {
        self.sps == *sps
    }

    /// Decode one picture into a free pool surface and return its output buffer.
    ///
    /// The reference store advances only after the picture decodes and syncs, so a failed
    /// picture leaves the stream state unchanged. The output copy happens after that point.
    ///
    /// # Errors
    /// Returns a description when the picture cannot be decoded: a frame_num gap, a P slice with
    /// no reference, or a failing VA call.
    pub fn decode(
        &mut self,
        picture: &Picture<'_>,
        prefer_dmabuf: bool,
    ) -> Result<FrameBuffer, String> {
        let first = &picture.slices[0].1;
        // Frames a frame_num gap skipped are inferred before this picture's lists are built.
        // They are implied by the header, so they stay even if this picture fails.
        self.refs.fill_gap(first);
        let plan = self.refs.plan(first).map_err(str::to_owned)?;
        let slot = self
            .free_slot(self.surfaces.len())
            .ok_or("no free decode surface: every pool slot is a reference or still in use")?;
        let slice_refs = self.list0_per_slice(picture)?;

        let target = &self.surfaces[slot];
        self.context.begin_picture(target).map_err(va_err)?;
        // End the picture even when submission failed, so the context stays usable.
        let submitted = submit(
            &self.context,
            &self.surfaces,
            slot,
            picture,
            &plan,
            &self.refs,
            &slice_refs,
        );
        let ended = self.context.end_picture().map_err(va_err);
        submitted?;
        ended?;
        target.sync().map_err(va_err)?;
        // The picture decoded, so it is part of the stream state whatever happens to the output copy.
        self.refs.commit(first, &plan, slot);

        let crop = Crop {
            x: self.sps.crop_x,
            y: self.sps.crop_y,
            width: self.sps.width,
            height: self.sps.height,
        };
        if prefer_dmabuf
            && let Some(buffer) = target
                .export_dmabuf()
                .ok()
                .and_then(|export| output::dmabuf_buffer(export, crop))
        {
            // The exported buffer aliases this pool slot, so it holds the slot's lease until the
            // consumer drops it.
            let lease = self.leases[slot].clone();
            return Ok(match buffer {
                FrameBuffer::Dmabuf(image) => FrameBuffer::Dmabuf(image.with_keepalive(lease)),
                cpu @ FrameBuffer::Cpu { .. } => cpu,
            });
        }
        cpu_nv12(target, crop).map(|data| FrameBuffer::Cpu { data })
    }

    /// List 0 of every slice, in slice order. Built before any VA call, so a bad reference leaves
    /// the device untouched.
    fn list0_per_slice(&self, picture: &Picture<'_>) -> Result<Vec<Vec<RefPic>>, String> {
        picture
            .slices
            .iter()
            .map(|(_, header)| match header.kind {
                SliceKind::I => Ok(Vec::new()),
                SliceKind::P => {
                    let list = self.refs.p_list(header).map_err(str::to_owned)?;
                    if list.first().is_some_and(|r| r.slot == NON_EXISTING) {
                        // The first reference is a frame inferred for a gap: the picture
                        // predicts from a frame that was lost, and decoding it would conceal.
                        Err("frame_num gap: the reference picture was lost".to_owned())
                    } else if list.is_empty() {
                        Err("P slice without a reference picture: the stream must start with an IDR"
                            .to_owned())
                    } else {
                        // An inferred frame further down the list still counts for the indices
                        // (8.2.5.2); VA gets an invalid entry there.
                        Ok(list)
                    }
                }
            })
            .collect()
    }

    /// Sync every pool surface. `decode` already syncs each picture, so this only guarantees
    /// that no VA work is in flight before the session is dropped.
    ///
    /// # Errors
    /// Returns a description when a sync fails.
    pub fn sync_all(&self) -> Result<(), String> {
        self.surfaces
            .iter()
            .try_for_each(|s| s.sync().map_err(va_err))
    }

    /// Export one pool surface as DMA-BUF, so a test can compare the decoder's image with the
    /// driver's own description of the same surface.
    #[cfg(test)]
    pub fn export_surface(&self, slot: usize) -> Result<lumen_ffi_va::DmabufExport, String> {
        self.surfaces[slot].export_dmabuf().map_err(va_err)
    }
}

/// Submit one picture: the picture parameter, the IQ matrix, then a slice parameter and a slice
/// data buffer per slice, in bitstream order.
///
/// Each buffer holds exactly one element (`size` = its bytes, `num_elements` = 1). The radeonsi
/// decoder reads `size` as the byte length, so a multi-element array of slice bytes decodes only
/// its first byte.
fn submit(
    context: &VaContext,
    surfaces: &[OwnedVaSurface],
    target: usize,
    picture: &Picture<'_>,
    plan: &PicturePlan,
    refs: &RefStore,
    slice_refs: &[Vec<RefPic>],
) -> Result<(), String> {
    let first = &picture.slices[0].1;
    let pic_param = picture_parameter(
        surfaces,
        target,
        first,
        plan,
        refs,
        &picture.sps,
        &picture.pps,
    )?;
    let mut buffers: Vec<OwnedVaBuffer> = Vec::with_capacity(2 + 2 * picture.slices.len());
    buffers.push(
        context
            .create_buffer(sys::VAPictureParameterBufferType, &pic_param)
            .map_err(va_err)?,
    );
    buffers.push(
        context
            .create_buffer(sys::VAIQMatrixBufferType, &flat_iq_matrix())
            .map_err(va_err)?,
    );
    for ((nal, header), list0) in picture.slices.iter().zip(slice_refs) {
        let params = slice_parameter(surfaces, header, nal.bytes.len(), list0)?;
        buffers.push(
            context
                .create_buffer(sys::VASliceParameterBufferType, &params)
                .map_err(va_err)?,
        );
        buffers.push(
            context
                .create_data_buffer(sys::VASliceDataBufferType, nal.bytes)
                .map_err(va_err)?,
        );
    }
    let list: Vec<&OwnedVaBuffer> = buffers.iter().collect();
    context.render_picture(&list).map_err(va_err)
}

/// A reference entry VA-API marks as unused.
fn invalid_picture() -> sys::VAPictureH264 {
    sys::VAPictureH264 {
        picture_id: sys::VA_INVALID_SURFACE,
        flags: sys::VA_PICTURE_H264_INVALID,
        ..Default::default()
    }
}

/// A reference picture as a VA-API entry: its surface, its frame_num (or `LongTermFrameIdx`),
/// order counts, and the marking the reference store gave it.
fn va_ref(surfaces: &[OwnedVaSurface], r: RefPic) -> sys::VAPictureH264 {
    if r.slot == NON_EXISTING {
        return invalid_picture();
    }
    let (frame_idx, flags) = match r.long_term {
        Some(idx) => (idx, sys::VA_PICTURE_H264_LONG_TERM_REFERENCE),
        None => (r.frame_num, sys::VA_PICTURE_H264_SHORT_TERM_REFERENCE),
    };
    sys::VAPictureH264 {
        picture_id: surfaces[r.slot].id(),
        frame_idx,
        flags,
        TopFieldOrderCnt: r.top_poc,
        BottomFieldOrderCnt: r.bottom_poc,
        ..Default::default()
    }
}

fn picture_parameter(
    surfaces: &[OwnedVaSurface],
    target: usize,
    header: &SliceHeader,
    plan: &PicturePlan,
    refs: &RefStore,
    sps: &Sps,
    pps: &Pps,
) -> Result<sys::VAPictureParameterBufferH264, String> {
    let mut p = sys::VAPictureParameterBufferH264 {
        CurrPic: sys::VAPictureH264 {
            picture_id: surfaces[target].id(),
            frame_idx: header.frame_num,
            flags: curr_pic_flags(header),
            TopFieldOrderCnt: plan.top_poc,
            BottomFieldOrderCnt: plan.bottom_poc,
            ..Default::default()
        },
        ..Default::default()
    };
    // Every entry starts invalid; the live references fill the front, in store order.
    p.ReferenceFrames = [invalid_picture(); 16];
    let real = refs.refs().iter().filter(|r| r.slot != NON_EXISTING);
    for (entry, r) in p.ReferenceFrames.iter_mut().zip(real) {
        *entry = va_ref(surfaces, *r);
    }
    p.picture_width_in_mbs_minus1 =
        u16::try_from(sps.coded_width / 16 - 1).map_err(|_| "width out of range")?;
    p.picture_height_in_mbs_minus1 =
        u16::try_from(sps.coded_height / 16 - 1).map_err(|_| "height out of range")?;
    // The SPS value, not zero: VA-API sizes the reference store from it.
    p.num_ref_frames =
        u8::try_from(sps.max_num_ref_frames).map_err(|_| "num_ref_frames out of range")?;
    p.seq_fields.value = seq_fields(sps);
    p.pic_init_qp_minus26 =
        i8::try_from(pps.pic_init_qp_minus26).map_err(|_| "pic_init_qp out of range")?;
    p.pic_init_qs_minus26 =
        i8::try_from(pps.pic_init_qs_minus26).map_err(|_| "pic_init_qs out of range")?;
    p.chroma_qp_index_offset =
        i8::try_from(pps.chroma_qp_index_offset).map_err(|_| "chroma qp offset out of range")?;
    p.second_chroma_qp_index_offset = i8::try_from(pps.second_chroma_qp_index_offset)
        .map_err(|_| "chroma qp offset out of range")?;
    p.pic_fields.value = pic_fields(pps, header);
    p.frame_num = u16::try_from(header.frame_num).map_err(|_| "frame_num out of range")?;
    Ok(p)
}

/// `seq_fields` packed in `va.h` declaration order, least significant bit first.
fn seq_fields(sps: &Sps) -> u32 {
    let level_31_or_higher = sps.level_idc >= 31;
    1 // chroma_format_idc = 1 (4:2:0), bits 0..=1
        | (u32::from(sps.gaps_in_frame_num_allowed) << 3)
        | (1 << 4) // frame_mbs_only_flag: the decoder accepts progressive streams only
        | (u32::from(sps.direct_8x8_inference) << 6)
        | (u32::from(level_31_or_higher) << 7) // MinLumaBiPredSize8x8
        | ((sps.log2_max_frame_num - 4) << 8)
        | (sps.poc_type << 12)
        | (sps.log2_max_poc_lsb.saturating_sub(4) << 14)
        | (u32::from(sps.delta_pic_order_always_zero) << 18)
}

/// `pic_fields` packed in `va.h` declaration order, least significant bit first.
fn pic_fields(pps: &Pps, header: &SliceHeader) -> u32 {
    u32::from(pps.entropy_coding_mode)
        | (u32::from(pps.weighted_pred) << 1)
        | (pps.weighted_bipred_idc << 2)
        | (u32::from(pps.transform_8x8_mode) << 4)
        // bit 5, field_pic_flag, stays 0: progressive frames only
        | (u32::from(pps.constrained_intra_pred) << 6)
        | (u32::from(pps.bottom_field_pic_order_present) << 7)
        | (u32::from(pps.deblocking_filter_control_present) << 8)
        | (u32::from(pps.redundant_pic_cnt_present) << 9)
        | (u32::from(header.reference) << 10)
}

fn slice_parameter(
    surfaces: &[OwnedVaSurface],
    header: &SliceHeader,
    size: usize,
    list0: &[RefPic],
) -> Result<sys::VASliceParameterBufferH264, String> {
    let mut p = sys::VASliceParameterBufferH264 {
        slice_data_size: u32::try_from(size).map_err(|_| "slice larger than 4 GiB")?,
        slice_data_offset: 0,
        slice_data_flag: sys::VA_SLICE_DATA_FLAG_ALL,
        slice_data_bit_offset: header.slice_data_bit_offset,
        first_mb_in_slice: u16::try_from(header.first_mb_in_slice)
            .map_err(|_| "first_mb_in_slice out of range")?,
        slice_type: match header.kind {
            SliceKind::I => SLICE_TYPE_I,
            SliceKind::P => SLICE_TYPE_P,
        },
        num_ref_idx_l0_active_minus1: u8::try_from(header.num_ref_idx_l0_active_minus1)
            .map_err(|_| "num_ref_idx_l0_active_minus1 out of range")?,
        cabac_init_idc: u8::try_from(header.cabac_init_idc)
            .map_err(|_| "cabac_init_idc out of range")?,
        slice_qp_delta: i8::try_from(header.slice_qp_delta)
            .map_err(|_| "slice_qp_delta out of range")?,
        disable_deblocking_filter_idc: u8::try_from(header.disable_deblocking_filter_idc)
            .map_err(|_| "disable_deblocking_filter_idc out of range")?,
        slice_alpha_c0_offset_div2: i8::try_from(header.slice_alpha_c0_offset_div2)
            .map_err(|_| "slice_alpha_c0_offset_div2 out of range")?,
        slice_beta_offset_div2: i8::try_from(header.slice_beta_offset_div2)
            .map_err(|_| "slice_beta_offset_div2 out of range")?,
        ..Default::default()
    };
    for entry in p.RefPicList0.iter_mut().chain(p.RefPicList1.iter_mut()) {
        *entry = invalid_picture();
    }
    for (entry, r) in p.RefPicList0.iter_mut().zip(list0) {
        *entry = va_ref(surfaces, *r);
    }
    if let Some(table) = &header.weights {
        apply_weights(&mut p, table)?;
    }
    Ok(p)
}

/// Explicit weighted prediction for list 0. The flags are set so the driver reads the arrays;
/// every entry holds either its explicit weight or the inferred default.
fn apply_weights(
    p: &mut sys::VASliceParameterBufferH264,
    table: &PredWeightTable,
) -> Result<(), String> {
    p.luma_log2_weight_denom =
        u8::try_from(table.luma_log2_denom).map_err(|_| "luma weight denominator out of range")?;
    p.chroma_log2_weight_denom = u8::try_from(table.chroma_log2_denom)
        .map_err(|_| "chroma weight denominator out of range")?;
    p.luma_weight_l0_flag = 1;
    p.chroma_weight_l0_flag = 1;
    for (i, e) in table.entries.iter().enumerate() {
        p.luma_weight_l0[i] = i16_of(e.luma_weight)?;
        p.luma_offset_l0[i] = i16_of(e.luma_offset)?;
        p.chroma_weight_l0[i] = [i16_of(e.chroma_weight[0])?, i16_of(e.chroma_weight[1])?];
        p.chroma_offset_l0[i] = [i16_of(e.chroma_offset[0])?, i16_of(e.chroma_offset[1])?];
    }
    Ok(())
}

fn i16_of(value: i32) -> Result<i16, String> {
    i16::try_from(value).map_err(|_| "weight value out of range".to_owned())
}

/// Flat scaling lists (all 16). The decoder rejects SPS and PPS scaling matrices, so the spec
/// default (flat) applies.
fn flat_iq_matrix() -> sys::VAIQMatrixBufferH264 {
    sys::VAIQMatrixBufferH264 {
        ScalingList4x4: [[16; 16]; 6],
        ScalingList8x8: [[16; 64]; 2],
        ..Default::default()
    }
}

/// Copy the visible picture out of a decoded surface as tightly packed NV12.
///
/// Reads through `vaGetImage`, which de-tiles. `vaDeriveImage` would expose the tiled surface
/// memory directly and returns the wrong samples on radeonsi.
fn cpu_nv12(target: &OwnedVaSurface, crop: Crop) -> Result<Vec<u8>, String> {
    let mut image = target.get_image().map_err(va_err)?;
    let info = image.info();
    let layout = Nv12Layout {
        y_offset: info.offsets[0] as usize,
        y_pitch: info.pitches[0] as usize,
        uv_offset: info.offsets[1] as usize,
        uv_pitch: info.pitches[1] as usize,
    };
    let mapped = image.map().map_err(va_err)?;
    output::pack_nv12(mapped.bytes(), layout, crop).map_err(str::to_owned)
}
