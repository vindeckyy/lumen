//! Annex-B H.264 parsing for the VA-API decode path.
//!
//! Supported: 4:2:0 8-bit progressive frames in Baseline, Main or High profile, no FMO, no
//! scaling matrices, I and P slices with list 0 only, with list 0 reordering, adaptive reference
//! marking (MMCO) and long-term references. B, SP and SI slices, interlace and high bit depth are
//! explicit errors, never guesses.

use std::collections::HashMap;

/// Parse result: `Err` carries a static description of why the data is out of scope or broken.
pub type Res<T> = Result<T, &'static str>;

const TRUNCATED: &str = "bitstream ended early";
const MAX_DIMENSION: u32 = 8192;
/// `max_num_ref_frames` bound of the spec.
const MAX_REF_FRAMES: u32 = 16;
/// Largest `num_ref_idx_l0_active_minus1` the spec allows for frame pictures.
const MAX_REF_IDX: u32 = 31;

/// One NAL unit of an Annex-B byte stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NalUnit<'a> {
    /// `nal_ref_idc`, 0..=3.
    pub ref_idc: u8,
    /// `nal_unit_type`, 0..=31.
    pub nal_type: u8,
    /// The whole NAL unit including its header byte. No start code, no trailing zero bytes.
    pub bytes: &'a [u8],
}

impl NalUnit<'_> {
    /// Payload after the header byte, with emulation-prevention bytes removed.
    #[must_use]
    pub fn rbsp(&self) -> Vec<u8> {
        unescape(self.bytes.get(1..).unwrap_or_default())
    }
}

/// Split an Annex-B byte stream into NAL units.
#[must_use]
pub fn nal_units(data: &[u8]) -> Vec<NalUnit<'_>> {
    // Index of the first byte after every `00 00 01` start code.
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (idx, &start) in starts.iter().enumerate() {
        // The next start code's `00 00 01` begins 3 bytes before the next start; a 4-byte
        // start code adds one more zero, which the trim below removes.
        let mut end = starts.get(idx + 1).map_or(data.len(), |next| next - 3);
        while end > start && data[end - 1] == 0 {
            end -= 1;
        }
        if end > start {
            let header = data[start];
            out.push(NalUnit {
                ref_idc: (header >> 5) & 3,
                nal_type: header & 0x1f,
                bytes: &data[start..end],
            });
        }
    }
    out
}

/// H.264 profiles this decoder accepts. Mapped to VA-API profiles by the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H264Profile {
    /// `profile_idc` 66.
    Baseline,
    /// `profile_idc` 77.
    Main,
    /// `profile_idc` 100.
    High,
}

impl H264Profile {
    /// Map a raw `profile_idc` byte. `None` for profiles this decoder does not handle.
    #[must_use]
    pub const fn from_idc(idc: u8) -> Option<Self> {
        match idc {
            66 => Some(Self::Baseline),
            77 => Some(Self::Main),
            100 => Some(Self::High),
            _ => None,
        }
    }
}

/// Sequence parameter set, reduced to the fields the decoder uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sps {
    /// Profile, restricted to the supported set.
    pub profile: H264Profile,
    /// `level_idc`.
    pub level_idc: u8,
    /// `seq_parameter_set_id`.
    pub id: u32,
    /// Bits used for `frame_num` (`log2_max_frame_num_minus4 + 4`).
    pub log2_max_frame_num: u32,
    /// `pic_order_cnt_type`.
    pub poc_type: u32,
    /// Bits used for `pic_order_cnt_lsb` (`log2_max_pic_order_cnt_lsb_minus4 + 4`), type 0 only.
    pub log2_max_poc_lsb: u32,
    /// `delta_pic_order_always_zero_flag`.
    pub delta_pic_order_always_zero: bool,
    /// `offset_for_top_to_bottom_field`.
    pub offset_for_top_to_bottom_field: i32,
    /// `offset_for_non_ref_pic`, type 1 only.
    pub offset_for_non_ref_pic: i32,
    /// `offset_for_ref_frame[]`, type 1 only: one entry per frame of the order-count cycle.
    pub offset_for_ref_frame: Vec<i32>,
    /// `gaps_in_frame_num_value_allowed_flag`.
    pub gaps_in_frame_num_allowed: bool,
    /// `direct_8x8_inference_flag`.
    pub direct_8x8_inference: bool,
    /// `max_num_ref_frames`.
    pub max_num_ref_frames: u32,
    /// Coded width, a multiple of 16. VA surfaces are allocated at this size.
    pub coded_width: u32,
    /// Coded height, a multiple of 16.
    pub coded_height: u32,
    /// Visible width after the cropping rectangle.
    pub width: u32,
    /// Visible height after the cropping rectangle.
    pub height: u32,
    /// Luma x offset of the visible picture inside the coded picture.
    pub crop_x: u32,
    /// Luma y offset of the visible picture inside the coded picture.
    pub crop_y: u32,
}

/// Picture parameter set, reduced to the fields the decoder uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pps {
    /// `pic_parameter_set_id`.
    pub id: u32,
    /// `seq_parameter_set_id` this PPS refers to.
    pub sps_id: u32,
    /// `entropy_coding_mode_flag`: true for CABAC.
    pub entropy_coding_mode: bool,
    /// `bottom_field_pic_order_in_frame_present_flag`.
    pub bottom_field_pic_order_present: bool,
    /// `num_ref_idx_l0_default_active_minus1`.
    pub num_ref_idx_l0_default_active_minus1: u32,
    /// `weighted_pred_flag`: P slices carry a `pred_weight_table`.
    pub weighted_pred: bool,
    /// `weighted_bipred_idc`.
    pub weighted_bipred_idc: u32,
    /// `pic_init_qp_minus26`.
    pub pic_init_qp_minus26: i32,
    /// `pic_init_qs_minus26`.
    pub pic_init_qs_minus26: i32,
    /// `chroma_qp_index_offset`.
    pub chroma_qp_index_offset: i32,
    /// `second_chroma_qp_index_offset` (equals the first when the extension is absent).
    pub second_chroma_qp_index_offset: i32,
    /// `deblocking_filter_control_present_flag`.
    pub deblocking_filter_control_present: bool,
    /// `constrained_intra_pred_flag`.
    pub constrained_intra_pred: bool,
    /// `redundant_pic_cnt_present_flag`.
    pub redundant_pic_cnt_present: bool,
    /// `transform_8x8_mode_flag`.
    pub transform_8x8_mode: bool,
}

/// Slice types this decoder handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceKind {
    /// Intra slice.
    I,
    /// Predicted slice, from list 0 only.
    P,
}

/// Explicit weights of one list 0 entry. Entries without explicit weights hold the inferred
/// defaults (`2^denominator` and offset 0), so the table is complete for every entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefWeight {
    /// Luma weight.
    pub luma_weight: i32,
    /// Luma offset.
    pub luma_offset: i32,
    /// Cb and Cr weights.
    pub chroma_weight: [i32; 2],
    /// Cb and Cr offsets.
    pub chroma_offset: [i32; 2],
}

/// `pred_weight_table()` of a P slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredWeightTable {
    /// `luma_log2_weight_denom`.
    pub luma_log2_denom: u32,
    /// `chroma_log2_weight_denom`.
    pub chroma_log2_denom: u32,
    /// One entry per list 0 index.
    pub entries: Vec<RefWeight>,
}

/// One `memory_management_control_operation` of `dec_ref_pic_marking()` (7.4.3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mmco {
    /// 1: mark a short-term picture unused.
    ShortTermUnused {
        /// `difference_of_pic_nums_minus1`.
        difference_of_pic_nums_minus1: u32,
    },
    /// 2: mark a long-term picture unused.
    LongTermUnused {
        /// `long_term_pic_num`.
        long_term_pic_num: u32,
    },
    /// 3: turn a short-term picture into a long-term one.
    ShortTermToLongTerm {
        /// `difference_of_pic_nums_minus1`.
        difference_of_pic_nums_minus1: u32,
        /// `long_term_frame_idx`.
        long_term_frame_idx: u32,
    },
    /// 4: set the largest long-term index; `None` means no long-term pictures.
    MaxLongTermFrameIdx(Option<u32>),
    /// 5: mark every reference unused and restart frame_num and order counts.
    ClearAll,
    /// 6: mark the current picture long-term.
    CurrentToLongTerm {
        /// `long_term_frame_idx`.
        long_term_frame_idx: u32,
    },
}

/// One `ref_pic_list_modification()` entry for list 0 (7.4.3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListModification {
    /// Idc 0: the short-term picture `abs_diff_pic_num_minus1 + 1` below the prediction.
    Subtract(u32),
    /// Idc 1: the short-term picture `abs_diff_pic_num_minus1 + 1` above the prediction.
    Add(u32),
    /// Idc 2: the long-term picture with this `long_term_pic_num`.
    LongTerm(u32),
}

/// Header of one slice, with the fields the decoder and the VA-API slice parameter need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceHeader {
    /// I or P.
    pub kind: SliceKind,
    /// `first_mb_in_slice`.
    pub first_mb_in_slice: u32,
    /// `pic_parameter_set_id`.
    pub pps_id: u32,
    /// `frame_num`.
    pub frame_num: u32,
    /// The slice belongs to an IDR picture (`nal_unit_type` 5).
    pub idr: bool,
    /// `nal_ref_idc != 0`.
    pub reference: bool,
    /// `pic_order_cnt_lsb`, 0 unless `pic_order_cnt_type` is 0.
    pub pic_order_cnt_lsb: u32,
    /// `delta_pic_order_cnt_bottom`.
    pub delta_pic_order_cnt_bottom: i32,
    /// `delta_pic_order_cnt[0..2]`, used by `pic_order_cnt_type` 1.
    pub delta_pic_order_cnt: [i32; 2],
    /// `num_ref_idx_l0_active_minus1`, 0 for I slices.
    pub num_ref_idx_l0_active_minus1: u32,
    /// List 0 modifications, in bitstream order; empty when the list keeps its initial order.
    pub l0_modifications: Vec<ListModification>,
    /// `long_term_reference_flag` of an IDR picture.
    pub long_term_reference: bool,
    /// `adaptive_ref_pic_marking_mode_flag` operations; `None` means sliding-window marking.
    pub mmco: Option<Vec<Mmco>>,
    /// `pred_weight_table()` when the PPS sets `weighted_pred_flag` on a P slice.
    pub weights: Option<PredWeightTable>,
    /// `cabac_init_idc`, 0 when CAVLC or an I slice.
    pub cabac_init_idc: u32,
    /// `slice_qp_delta`.
    pub slice_qp_delta: i32,
    /// `disable_deblocking_filter_idc`.
    pub disable_deblocking_filter_idc: u32,
    /// `slice_alpha_c0_offset_div2`.
    pub slice_alpha_c0_offset_div2: i32,
    /// `slice_beta_offset_div2`.
    pub slice_beta_offset_div2: i32,
    /// Bit offset from the NAL header byte to `slice_data()`, emulation prevention removed.
    /// Includes the 8 bits of the NAL header, as VA-API defines it.
    pub slice_data_bit_offset: u16,
}

/// One picture: its active parameter sets and its slices in bitstream order.
#[derive(Debug)]
pub struct Picture<'a> {
    /// Active sequence parameter set.
    pub sps: Sps,
    /// Active picture parameter set.
    pub pps: Pps,
    /// Slice NAL units with their parsed headers, in bitstream order.
    pub slices: Vec<(NalUnit<'a>, SliceHeader)>,
}

/// Parameter sets seen so far, keyed by id.
#[derive(Debug, Default)]
pub struct ParamSets {
    sps: HashMap<u32, Sps>,
    pps: HashMap<u32, Pps>,
}

impl ParamSets {
    /// Record an SPS or PPS NAL unit. Other NAL types are ignored.
    ///
    /// # Errors
    /// Returns a description when the parameter set is malformed or uses an unsupported feature.
    pub fn observe(&mut self, nal: &NalUnit<'_>) -> Res<()> {
        match nal.nal_type {
            7 => {
                let sps = parse_sps(&nal.rbsp())?;
                self.sps.insert(sps.id, sps);
            }
            8 => {
                let pps = parse_pps(&nal.rbsp())?;
                self.pps.insert(pps.id, pps);
            }
            _ => {}
        }
        Ok(())
    }

    /// Parse the header of one I or P slice NAL unit and resolve the parameter sets it uses.
    ///
    /// # Errors
    /// Returns a description for unsupported slice types and features, unknown parameter set
    /// ids, and truncated headers.
    pub fn slice_header(&self, nal: &NalUnit<'_>) -> Res<(SliceHeader, &Sps, &Pps)> {
        let rbsp = nal.rbsp();
        let mut r = Bits::new(&rbsp);
        let first_mb_in_slice = r.ue()?;
        let kind = slice_kind(r.ue()?)?;
        let idr = nal.nal_type == 5;
        if idr && kind != SliceKind::I {
            return Err("IDR pictures carry only I slices");
        }
        let pps_id = r.ue()?;
        let pps = self
            .pps
            .get(&pps_id)
            .ok_or("slice refers to an unknown PPS")?;
        let sps = self
            .sps
            .get(&pps.sps_id)
            .ok_or("PPS refers to an unknown SPS")?;
        let frame_num = r.bits(sps.log2_max_frame_num)?;
        if idr {
            r.ue()?; // idr_pic_id
        }
        let mut pic_order_cnt_lsb = 0;
        let mut delta_pic_order_cnt_bottom = 0;
        let mut delta_pic_order_cnt = [0, 0];
        match sps.poc_type {
            0 => {
                pic_order_cnt_lsb = r.bits(sps.log2_max_poc_lsb)?;
                if pps.bottom_field_pic_order_present {
                    delta_pic_order_cnt_bottom = r.se()?;
                }
            }
            1 if !sps.delta_pic_order_always_zero => {
                delta_pic_order_cnt[0] = r.se()?;
                if pps.bottom_field_pic_order_present {
                    delta_pic_order_cnt[1] = r.se()?;
                }
            }
            _ => {}
        }
        if pps.redundant_pic_cnt_present && r.ue()? != 0 {
            return Err("redundant slices are not supported");
        }
        let mut num_ref_idx_l0_active_minus1 = 0;
        let mut weights = None;
        let mut l0_modifications = Vec::new();
        if kind == SliceKind::P {
            num_ref_idx_l0_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
            if r.flag()? {
                // num_ref_idx_active_override_flag
                num_ref_idx_l0_active_minus1 = r.ue()?;
            }
            if num_ref_idx_l0_active_minus1 > MAX_REF_IDX {
                return Err("num_ref_idx_l0_active_minus1 out of range");
            }
            if r.flag()? {
                // ref_pic_list_modification_flag_l0
                l0_modifications = parse_list_modifications(&mut r, MAX_REF_IDX + 1)?;
            }
            if pps.weighted_pred {
                weights = Some(parse_pred_weight_table(
                    &mut r,
                    num_ref_idx_l0_active_minus1 + 1,
                )?);
            }
        }
        let reference = nal.ref_idc != 0;
        let mut long_term_reference = false;
        let mut mmco = None;
        if reference {
            if idr {
                // no_output_of_prior_pics_flag, then long_term_reference_flag.
                r.skip(1)?;
                long_term_reference = r.flag()?;
            } else if r.flag()? {
                mmco = Some(parse_mmco(&mut r)?);
            }
        }
        let cabac_init_idc = if pps.entropy_coding_mode && kind == SliceKind::P {
            r.ue()?
        } else {
            0
        };
        let slice_qp_delta = r.se()?;
        let mut disable_deblocking_filter_idc = 0;
        let mut slice_alpha_c0_offset_div2 = 0;
        let mut slice_beta_offset_div2 = 0;
        if pps.deblocking_filter_control_present {
            disable_deblocking_filter_idc = r.ue()?;
            if disable_deblocking_filter_idc != 1 {
                slice_alpha_c0_offset_div2 = r.se()?;
                slice_beta_offset_div2 = r.se()?;
            }
        }
        let slice_data_bit_offset = u16::try_from(8 + r.pos)
            .map_err(|_| "slice header longer than the VA-API bit offset field")?;
        let header = SliceHeader {
            kind,
            first_mb_in_slice,
            pps_id,
            frame_num,
            idr,
            reference,
            pic_order_cnt_lsb,
            delta_pic_order_cnt_bottom,
            delta_pic_order_cnt,
            num_ref_idx_l0_active_minus1,
            l0_modifications,
            long_term_reference,
            mmco,
            weights,
            cabac_init_idc,
            slice_qp_delta,
            disable_deblocking_filter_idc,
            slice_alpha_c0_offset_div2,
            slice_beta_offset_div2,
            slice_data_bit_offset,
        };
        Ok((header, sps, pps))
    }

    /// Feed one access unit: record its parameter sets and resolve the picture it codes.
    ///
    /// Returns `Ok(None)` when the access unit has no slices, or when its first slice does not
    /// start at macroblock 0 (the picture is incomplete). Whether every slice of a picture
    /// arrived is the transport's job; this only checks what the bytes show.
    ///
    /// # Errors
    /// Returns a description for malformed or unsupported data and for slices that disagree.
    pub fn ingest_access_unit<'a>(&mut self, data: &'a [u8]) -> Res<Option<Picture<'a>>> {
        let mut slice_nals = Vec::new();
        for nal in nal_units(data) {
            match nal.nal_type {
                1 | 5 => slice_nals.push(nal),
                7 | 8 => self.observe(&nal)?,
                _ => {}
            }
        }
        let mut slices: Vec<(NalUnit<'a>, SliceHeader)> = Vec::with_capacity(slice_nals.len());
        for nal in slice_nals {
            let (header, _, _) = self.slice_header(&nal)?;
            if slices
                .first()
                .is_some_and(|(_, first)| !same_picture(first, &header))
            {
                return Err(
                    "slices of one picture disagree on PPS, frame_num, IDR, reference flag or picture order count",
                );
            }
            slices.push((nal, header));
        }
        let Some((_, first)) = slices.first() else {
            return Ok(None);
        };
        if first.first_mb_in_slice != 0 {
            return Ok(None);
        }
        let pps = self
            .pps
            .get(&first.pps_id)
            .ok_or("slice refers to an unknown PPS")?;
        let sps = self
            .sps
            .get(&pps.sps_id)
            .ok_or("PPS refers to an unknown SPS")?;
        Ok(Some(Picture {
            sps: sps.clone(),
            pps: pps.clone(),
            slices,
        }))
    }
}

/// Whether two slice headers belong to the same picture: every field that must match across the
/// slices of one picture (7.4.3) is compared.
fn same_picture(a: &SliceHeader, b: &SliceHeader) -> bool {
    a.pps_id == b.pps_id
        && a.frame_num == b.frame_num
        && a.idr == b.idr
        && a.reference == b.reference
        && a.pic_order_cnt_lsb == b.pic_order_cnt_lsb
        && a.delta_pic_order_cnt_bottom == b.delta_pic_order_cnt_bottom
        && a.delta_pic_order_cnt == b.delta_pic_order_cnt
}

/// `ref_pic_list_modification()` entries until `modification_of_pic_nums_idc` 3, at most `limit`.
fn parse_list_modifications(r: &mut Bits<'_>, limit: u32) -> Res<Vec<ListModification>> {
    let mut out = Vec::new();
    loop {
        let entry = match r.ue()? {
            0 => ListModification::Subtract(r.ue()?),
            1 => ListModification::Add(r.ue()?),
            2 => ListModification::LongTerm(r.ue()?),
            3 => return Ok(out),
            _ => return Err("modification_of_pic_nums_idc out of range"),
        };
        if out.len() as u32 >= limit {
            return Err("too many reference list modifications");
        }
        out.push(entry);
    }
}

/// The operations of `dec_ref_pic_marking()` until operation 0. At most 66 operations fit a
/// conforming picture (one per reference plus the bounded ones); more is malformed.
fn parse_mmco(r: &mut Bits<'_>) -> Res<Vec<Mmco>> {
    const LIMIT: usize = 66;
    let mut out = Vec::new();
    loop {
        let op = match r.ue()? {
            0 => return Ok(out),
            1 => Mmco::ShortTermUnused {
                difference_of_pic_nums_minus1: r.ue()?,
            },
            2 => Mmco::LongTermUnused {
                long_term_pic_num: r.ue()?,
            },
            3 => Mmco::ShortTermToLongTerm {
                difference_of_pic_nums_minus1: r.ue()?,
                long_term_frame_idx: r.ue()?,
            },
            4 => Mmco::MaxLongTermFrameIdx(r.ue()?.checked_sub(1)),
            5 => Mmco::ClearAll,
            6 => Mmco::CurrentToLongTerm {
                long_term_frame_idx: r.ue()?,
            },
            _ => return Err("memory_management_control_operation out of range"),
        };
        if out.len() >= LIMIT {
            return Err("too many reference marking operations");
        }
        out.push(op);
    }
}

/// `slice_type` (0..=9) to the slice kinds this decoder handles.
fn slice_kind(slice_type: u32) -> Res<SliceKind> {
    match slice_type {
        0 | 5 => Ok(SliceKind::P),
        2 | 7 => Ok(SliceKind::I),
        1 | 6 => Err("B slices are not supported"),
        3 | 8 => Err("SP slices are not supported"),
        4 | 9 => Err("SI slices are not supported"),
        _ => Err("slice_type out of range"),
    }
}

/// `pred_weight_table()` for `entries` list 0 indices. Absent weights take their inferred defaults.
fn parse_pred_weight_table(r: &mut Bits<'_>, entries: u32) -> Res<PredWeightTable> {
    let luma_log2_denom = r.ue()?;
    let chroma_log2_denom = r.ue()?;
    if luma_log2_denom > 7 || chroma_log2_denom > 7 {
        return Err("weight denominator out of range");
    }
    let luma_default = 1i32 << luma_log2_denom;
    let chroma_default = 1i32 << chroma_log2_denom;
    let mut table = Vec::with_capacity(entries as usize);
    for _ in 0..entries {
        let mut weight = RefWeight {
            luma_weight: luma_default,
            luma_offset: 0,
            chroma_weight: [chroma_default; 2],
            chroma_offset: [0; 2],
        };
        if r.flag()? {
            weight.luma_weight = weight_value(r.se()?)?;
            weight.luma_offset = weight_value(r.se()?)?;
        }
        if r.flag()? {
            for (w, o) in weight
                .chroma_weight
                .iter_mut()
                .zip(weight.chroma_offset.iter_mut())
            {
                *w = weight_value(r.se()?)?;
                *o = weight_value(r.se()?)?;
            }
        }
        table.push(weight);
    }
    Ok(PredWeightTable {
        luma_log2_denom,
        chroma_log2_denom,
        entries: table,
    })
}

/// A weight or offset, which the spec limits to 8-bit signed range.
fn weight_value(v: i32) -> Res<i32> {
    if (-128..=127).contains(&v) {
        Ok(v)
    } else {
        Err("weighted prediction value out of range")
    }
}

/// Read a bit position inside an RBSP, most significant bit first.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(rbsp: &'a [u8]) -> Self {
        Self { data: rbsp, pos: 0 }
    }

    fn bit(&mut self) -> Res<u32> {
        let byte = *self.data.get(self.pos / 8).ok_or(TRUNCATED)?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Ok(u32::from(bit))
    }

    fn flag(&mut self) -> Res<bool> {
        Ok(self.bit()? == 1)
    }

    fn bits(&mut self, count: u32) -> Res<u32> {
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1) | self.bit()?;
        }
        Ok(value)
    }

    fn skip(&mut self, count: u32) -> Res<()> {
        for _ in 0..count {
            self.bit()?;
        }
        Ok(())
    }

    /// Unsigned Exp-Golomb `ue(v)`. Codes longer than 32 bits are rejected.
    fn ue(&mut self) -> Res<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err("Exp-Golomb code longer than 32 bits");
            }
        }
        Ok((1u32 << zeros) - 1 + self.bits(zeros)?)
    }

    /// Signed Exp-Golomb `se(v)`.
    fn se(&mut self) -> Res<i32> {
        let code = self.ue()?;
        let magnitude =
            i32::try_from(code.div_ceil(2)).map_err(|_| "signed Exp-Golomb out of range")?;
        Ok(if code.is_multiple_of(2) {
            -magnitude
        } else {
            magnitude
        })
    }

    /// `more_rbsp_data()`: true while bits remain before the RBSP stop bit.
    fn more_rbsp_data(&self) -> bool {
        match self.data.iter().rposition(|&b| b != 0) {
            Some(last) => {
                let stop_bit = last * 8 + 7 - self.data[last].trailing_zeros() as usize;
                self.pos < stop_bit
            }
            None => false,
        }
    }
}

/// Remove emulation prevention bytes (`00 00 03 xx` becomes `00 00 xx`).
fn unescape(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    let mut zeros = 0u32;
    for &byte in payload {
        if zeros >= 2 && byte == 0x03 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

/// Visible extent along one axis. Crop units are 2 luma samples for 4:2:0 frame pictures.
fn crop_axis(coded: u32, lo: u32, hi: u32) -> Res<(u32, u32)> {
    let lo = u64::from(lo) * 2;
    let removed = lo + u64::from(hi) * 2;
    let visible = u64::from(coded)
        .checked_sub(removed)
        .filter(|&v| v > 0)
        .ok_or("cropping leaves no picture")?;
    Ok((lo as u32, visible as u32))
}

fn parse_sps(rbsp: &[u8]) -> Res<Sps> {
    let mut r = Bits::new(rbsp);
    let profile_idc = r.bits(8)? as u8;
    let profile = H264Profile::from_idc(profile_idc)
        .ok_or("unsupported profile_idc (decoder handles 66, 77 and 100)")?;
    r.skip(8)?; // constraint_set flags and reserved bits
    let level_idc = r.bits(8)? as u8;
    let sps_id = r.ue()?;
    if profile == H264Profile::High {
        if r.ue()? != 1 {
            return Err("only 4:2:0 chroma is supported");
        }
        let bit_depth_luma = r.ue()?;
        let bit_depth_chroma = r.ue()?;
        if bit_depth_luma != 0 || bit_depth_chroma != 0 {
            return Err("only 8-bit samples are supported");
        }
        r.skip(1)?; // qpprime_y_zero_transform_bypass_flag
        if r.flag()? {
            return Err("scaling matrices are not supported");
        }
    }
    let log2_max_frame_num_minus4 = r.ue()?;
    if log2_max_frame_num_minus4 > 12 {
        return Err("log2_max_frame_num out of range");
    }
    let poc_type = r.ue()?;
    let mut log2_max_poc_lsb = 0;
    let mut delta_pic_order_always_zero = false;
    let mut offset_for_top_to_bottom_field = 0;
    let mut offset_for_non_ref_pic = 0;
    let mut offset_for_ref_frame = Vec::new();
    match poc_type {
        0 => {
            let minus4 = r.ue()?;
            if minus4 > 12 {
                return Err("log2_max_pic_order_cnt_lsb out of range");
            }
            log2_max_poc_lsb = minus4 + 4;
        }
        1 => {
            delta_pic_order_always_zero = r.flag()?;
            offset_for_non_ref_pic = r.se()?;
            offset_for_top_to_bottom_field = r.se()?;
            let cycle = r.ue()?;
            if cycle > 255 {
                return Err("num_ref_frames_in_pic_order_cnt_cycle out of range");
            }
            for _ in 0..cycle {
                offset_for_ref_frame.push(r.se()?);
            }
        }
        2 => {}
        _ => return Err("pic_order_cnt_type out of range"),
    }
    let max_num_ref_frames = r.ue()?;
    if max_num_ref_frames > MAX_REF_FRAMES {
        return Err("max_num_ref_frames out of range");
    }
    let gaps_in_frame_num_allowed = r.flag()?;
    let width_mbs = r.ue()?.saturating_add(1);
    let height_mbs = r.ue()?.saturating_add(1);
    if !r.flag()? {
        return Err("interlaced streams (frame_mbs_only_flag = 0) are not supported");
    }
    let direct_8x8_inference = r.flag()?;
    let (mut crop_left, mut crop_right, mut crop_top, mut crop_bottom) = (0, 0, 0, 0);
    if r.flag()? {
        crop_left = r.ue()?;
        crop_right = r.ue()?;
        crop_top = r.ue()?;
        crop_bottom = r.ue()?;
    }
    let coded_width = width_mbs
        .checked_mul(16)
        .filter(|&w| w <= MAX_DIMENSION)
        .ok_or("picture width out of range")?;
    let coded_height = height_mbs
        .checked_mul(16)
        .filter(|&h| h <= MAX_DIMENSION)
        .ok_or("picture height out of range")?;
    let (crop_x, width) = crop_axis(coded_width, crop_left, crop_right)?;
    let (crop_y, height) = crop_axis(coded_height, crop_top, crop_bottom)?;
    Ok(Sps {
        profile,
        level_idc,
        id: sps_id,
        log2_max_frame_num: log2_max_frame_num_minus4 + 4,
        poc_type,
        log2_max_poc_lsb,
        delta_pic_order_always_zero,
        offset_for_top_to_bottom_field,
        offset_for_non_ref_pic,
        offset_for_ref_frame,
        gaps_in_frame_num_allowed,
        direct_8x8_inference,
        max_num_ref_frames,
        coded_width,
        coded_height,
        width,
        height,
        crop_x,
        crop_y,
    })
}

fn parse_pps(rbsp: &[u8]) -> Res<Pps> {
    let mut r = Bits::new(rbsp);
    let pps_id = r.ue()?;
    let sps_id = r.ue()?;
    let entropy_coding_mode = r.flag()?;
    let bottom_field_pic_order_present = r.flag()?;
    if r.ue()? != 0 {
        return Err("slice groups (FMO) are not supported");
    }
    let num_ref_idx_l0_default_active_minus1 = r.ue()?;
    if num_ref_idx_l0_default_active_minus1 > MAX_REF_IDX {
        return Err("num_ref_idx_l0_default_active_minus1 out of range");
    }
    r.ue()?; // num_ref_idx_l1_default_active_minus1: B slices only
    let weighted_pred = r.flag()?;
    let weighted_bipred_idc = r.bits(2)?;
    let pic_init_qp_minus26 = r.se()?;
    let pic_init_qs_minus26 = r.se()?;
    let chroma_qp_index_offset = r.se()?;
    let deblocking_filter_control_present = r.flag()?;
    let constrained_intra_pred = r.flag()?;
    let redundant_pic_cnt_present = r.flag()?;
    let mut transform_8x8_mode = false;
    let mut second_chroma_qp_index_offset = chroma_qp_index_offset;
    if r.more_rbsp_data() {
        transform_8x8_mode = r.flag()?;
        if r.flag()? {
            return Err("PPS scaling matrices are not supported");
        }
        second_chroma_qp_index_offset = r.se()?;
    }
    Ok(Pps {
        id: pps_id,
        sps_id,
        entropy_coding_mode,
        bottom_field_pic_order_present,
        num_ref_idx_l0_default_active_minus1,
        weighted_pred,
        weighted_bipred_idc,
        pic_init_qp_minus26,
        pic_init_qs_minus26,
        chroma_qp_index_offset,
        second_chroma_qp_index_offset,
        deblocking_filter_control_present,
        constrained_intra_pred,
        redundant_pic_cnt_present,
        transform_8x8_mode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{FLAT_IDR_AU, FLAT_P1_AU, IDR_64X64};

    fn fixture_params() -> ParamSets {
        let mut params = ParamSets::default();
        for nal in nal_units(IDR_64X64)
            .iter()
            .filter(|n| n.nal_type == 7 || n.nal_type == 8)
        {
            params.observe(nal).expect("fixture parameter set parses");
        }
        params
    }

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let data = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 0, 1, 0x65, 4,
        ];
        let nals = nal_units(&data);
        let types: Vec<u8> = nals.iter().map(|n| n.nal_type).collect();
        assert_eq!(types, vec![7, 8, 5]);
        assert_eq!(nals[0].bytes, &[0x67, 1, 2]);
        assert_eq!(nals[1].bytes, &[0x68, 3]);
        assert_eq!(nals[2].bytes, &[0x65, 4]);
        assert_eq!(nals[1].ref_idc, 3);
    }

    #[test]
    fn exp_golomb_codes() {
        // '1' '010' '011' '00100' -> ue 0, 1, 2, 3
        let mut r = Bits::new(&[0xA6, 0x40]);
        assert_eq!(
            [
                r.ue().unwrap(),
                r.ue().unwrap(),
                r.ue().unwrap(),
                r.ue().unwrap()
            ],
            [0, 1, 2, 3]
        );
        // '010' '011' '1' -> se +1, -1, 0
        let mut r = Bits::new(&[0x4E]);
        assert_eq!(
            [r.se().unwrap(), r.se().unwrap(), r.se().unwrap()],
            [1, -1, 0]
        );
    }

    #[test]
    fn fixture_sps_reads_size_and_profile() {
        let params = fixture_params();
        let sps = params.sps.get(&0).expect("sps 0");
        assert_eq!(sps.profile, H264Profile::Baseline);
        assert_eq!((sps.width, sps.height), (64, 64));
        assert_eq!((sps.coded_width, sps.coded_height), (64, 64));
        assert_eq!((sps.crop_x, sps.crop_y), (0, 0));
        assert_eq!(sps.log2_max_frame_num, 4);
        assert_eq!(sps.poc_type, 2);
        assert_eq!(sps.max_num_ref_frames, 1);
    }

    #[test]
    fn fixture_pps_flags() {
        let params = fixture_params();
        let pps = params.pps.get(&0).expect("pps 0");
        assert!(!pps.entropy_coding_mode);
        assert!(pps.deblocking_filter_control_present);
        assert!(!pps.transform_8x8_mode);
        assert!(!pps.weighted_pred);
        assert_eq!(pps.num_ref_idx_l0_default_active_minus1, 0);
        assert_eq!(pps.pic_init_qp_minus26, -3);
    }

    #[test]
    fn fixture_idr_slice_header_bit_offset() {
        let params = fixture_params();
        let idr = nal_units(IDR_64X64)
            .into_iter()
            .find(|n| n.nal_type == 5)
            .expect("idr");
        let (header, _, _) = params.slice_header(&idr).expect("slice header");
        assert_eq!(header.kind, SliceKind::I);
        assert_eq!(header.first_mb_in_slice, 0);
        assert!(header.idr && header.reference);
        assert_eq!(header.slice_qp_delta, -3);
        assert_eq!(header.disable_deblocking_filter_idc, 1);
        assert!(header.weights.is_none());
        // 8 NAL header bits + 24 header bits; slice data starts at byte 4 of the NAL unit.
        assert_eq!(header.slice_data_bit_offset, 32);
        assert_eq!(idr.bytes[4], 0x26);
    }

    #[test]
    fn flat_p_slice_header_matches_reference_decoder() {
        // The flat CABAC stream from ffmpeg: its P slice starts at bit 39 (ffmpeg's VA trace).
        let mut params = ParamSets::default();
        let idr_nals = nal_units(FLAT_IDR_AU);
        for nal in idr_nals
            .iter()
            .filter(|n| n.nal_type == 7 || n.nal_type == 8)
        {
            params.observe(nal).expect("parameter sets parse");
        }
        let p = nal_units(FLAT_P1_AU).into_iter().next().expect("P slice");
        let (header, _, _) = params.slice_header(&p).expect("P slice header");
        assert_eq!(header.kind, SliceKind::P);
        assert_eq!(header.frame_num, 1);
        assert!(!header.idr && header.reference);
        assert_eq!(header.num_ref_idx_l0_active_minus1, 0);
        assert_eq!(header.cabac_init_idc, 0);
        assert_eq!(header.slice_qp_delta, -11);
        assert_eq!(header.slice_data_bit_offset, 39);
        let weights = header.weights.expect("weighted_pred table");
        assert_eq!(weights.entries.len(), 1);
        assert_eq!(weights.entries[0].luma_weight, 1);
        assert_eq!(weights.entries[0].chroma_weight, [1, 1]);
    }

    #[test]
    fn flat_idr_slice_header_matches_reference_decoder() {
        let mut params = ParamSets::default();
        let nals = nal_units(FLAT_IDR_AU);
        for nal in nals.iter().filter(|n| n.nal_type == 7 || n.nal_type == 8) {
            params.observe(nal).expect("parameter sets parse");
        }
        let idr = nals.iter().find(|n| n.nal_type == 5).expect("idr");
        let (header, _, _) = params.slice_header(idr).expect("IDR header");
        // ffmpeg's VA trace puts the IDR slice data at bit 36.
        assert_eq!(header.slice_data_bit_offset, 36);
    }

    #[test]
    fn unknown_profile_is_rejected() {
        // profile_idc 99 is not handled.
        let sps = [0x00, 0x00, 0x01, 0x67, 99, 0xc0, 0x0a, 0x80];
        let mut params = ParamSets::default();
        let nal = nal_units(&sps).into_iter().next().expect("nal");
        assert!(params.observe(&nal).is_err());
    }

    #[test]
    fn slice_without_parameter_sets_is_rejected() {
        let params = ParamSets::default();
        let idr = nal_units(IDR_64X64)
            .into_iter()
            .find(|n| n.nal_type == 5)
            .expect("idr");
        assert!(params.slice_header(&idr).is_err());
    }

    #[test]
    fn access_unit_without_slices_is_not_a_picture() {
        let mut params = ParamSets::default();
        assert!(
            params
                .ingest_access_unit(&IDR_64X64[..34])
                .expect("parameter sets parse")
                .is_none()
        );
        assert!(params.sps.contains_key(&0) && params.pps.contains_key(&0));
    }

    #[test]
    fn slice_without_parameter_sets_fails_in_access_unit() {
        let mut params = ParamSets::default();
        assert!(params.ingest_access_unit(&IDR_64X64[34..]).is_err());
    }

    #[test]
    fn fixture_access_unit_resolves_one_picture() {
        let mut params = ParamSets::default();
        let picture = params
            .ingest_access_unit(IDR_64X64)
            .expect("parses")
            .expect("picture");
        assert_eq!(picture.slices.len(), 1);
        assert_eq!((picture.sps.width, picture.pps.id), (64, 0));
    }

    #[test]
    fn b_slice_is_rejected() {
        // SPS and PPS, then a non-IDR slice with slice_type 1 (B): first_mb 0, slice_type 1.
        let mut au = IDR_64X64[..34].to_vec();
        au.extend_from_slice(&[0x00, 0x00, 0x01, 0x41, 0xA8]);
        let mut params = ParamSets::default();
        let error = params.ingest_access_unit(&au).expect_err("B slice");
        assert!(error.contains("B slices"), "{error}");
    }

    #[test]
    fn weight_values_are_range_checked() {
        assert_eq!(weight_value(127), Ok(127));
        assert!(weight_value(128).is_err());
        assert!(weight_value(-129).is_err());
    }
}
