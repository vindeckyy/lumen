//! Reference-picture state for H.264 decode (8.2): `frame_num` and picture order count tracking
//! for all three order-count types, short- and long-term references with sliding-window and
//! adaptive (MMCO) marking, and list 0 for P slices with its modifications.
//!
//! Pure: no VA calls, so each rule is testable without a GPU. A picture changes this state only
//! in [`RefStore::commit`], after it decoded, so a failed picture leaves the store unchanged.
//! Frames only: the decoder rejects field coding before it gets here.

use crate::bitstream::{ListModification, Mmco, SliceHeader, Sps};

/// The slot of a frame inferred for a `frame_num` gap (8.2.5.2): it holds no picture.
pub const NON_EXISTING: usize = usize::MAX;

/// A reference picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefPic {
    /// Index of the decode-pool surface that holds the picture.
    pub slot: usize,
    /// `frame_num` of the picture.
    pub frame_num: u32,
    /// `TopFieldOrderCnt` of the picture.
    pub top_poc: i32,
    /// `BottomFieldOrderCnt` of the picture.
    pub bottom_poc: i32,
    /// `LongTermFrameIdx` when the picture is a long-term reference.
    pub long_term: Option<u32>,
}

/// Order counts of one picture, derived before it decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PicturePlan {
    /// `TopFieldOrderCnt`.
    pub top_poc: i32,
    /// `BottomFieldOrderCnt`.
    pub bottom_poc: i32,
    /// `PicOrderCntMsb` (type 0), carried to the next reference picture.
    poc_msb: i64,
    /// `FrameNumOffset` (types 1 and 2), carried to the next picture.
    frame_num_offset: i64,
}

/// Reference state of one decoding session.
#[derive(Debug)]
pub struct RefStore {
    max_refs: usize,
    max_frame_num: u32,
    max_poc_lsb: i64,
    poc_type: u32,
    offset_top_to_bottom: i64,
    offset_for_non_ref_pic: i64,
    offset_for_ref_frame: Vec<i64>,
    refs: Vec<RefPic>,
    /// `MaxLongTermFrameIdx`; `None` is "no long-term frame indices".
    max_long_term_idx: Option<u32>,
    prev_ref_frame_num: u32,
    prev_ref_poc_msb: i64,
    prev_ref_poc_lsb: i64,
    prev_frame_num: u32,
    prev_frame_num_offset: i64,
}

impl RefStore {
    /// Empty store for a session built from `sps`.
    #[must_use]
    pub fn new(sps: &Sps) -> Self {
        Self {
            max_refs: sps.max_num_ref_frames.max(1) as usize,
            max_frame_num: 1 << sps.log2_max_frame_num,
            max_poc_lsb: 1i64 << sps.log2_max_poc_lsb,
            poc_type: sps.poc_type,
            offset_top_to_bottom: i64::from(sps.offset_for_top_to_bottom_field),
            offset_for_non_ref_pic: i64::from(sps.offset_for_non_ref_pic),
            offset_for_ref_frame: sps
                .offset_for_ref_frame
                .iter()
                .map(|&o| i64::from(o))
                .collect(),
            refs: Vec::new(),
            max_long_term_idx: None,
            prev_ref_frame_num: 0,
            prev_ref_poc_msb: 0,
            prev_ref_poc_lsb: 0,
            prev_frame_num: 0,
            prev_frame_num_offset: 0,
        }
    }

    /// Number of reference pictures the store holds at most (`max(max_num_ref_frames, 1)`).
    #[must_use]
    pub const fn max_refs(&self) -> usize {
        self.max_refs
    }

    /// The current reference pictures, short- and long-term, in insertion order.
    #[must_use]
    pub fn refs(&self) -> &[RefPic] {
        &self.refs
    }

    /// Check `header` against the stored state and derive its order counts. Changes nothing.
    ///
    /// # Errors
    /// Describes a `frame_num` gap or an order count out of range.
    pub fn plan(&self, header: &SliceHeader) -> Result<PicturePlan, &'static str> {
        if !header.idr && header.frame_num != (self.prev_ref_frame_num + 1) % self.max_frame_num {
            return Err(
                "frame_num gap: a reference picture is missing (gaps_in_frame_num is not supported)",
            );
        }
        // FrameNumOffset of types 1 and 2 (8.2.1.2, 8.2.1.3).
        let frame_num_offset = if header.idr {
            0
        } else if self.prev_frame_num > header.frame_num {
            self.prev_frame_num_offset + i64::from(self.max_frame_num)
        } else {
            self.prev_frame_num_offset
        };
        let (top, bottom, poc_msb, frame_num_offset) = match self.poc_type {
            0 => {
                let (prev_msb, prev_lsb) = if header.idr {
                    (0, 0)
                } else {
                    (self.prev_ref_poc_msb, self.prev_ref_poc_lsb)
                };
                let lsb = i64::from(header.pic_order_cnt_lsb);
                let half = self.max_poc_lsb / 2;
                // 8.2.1.1: the MSB moves by one wrap when the LSB jumps by more than half the range.
                let msb = if lsb < prev_lsb && prev_lsb - lsb >= half {
                    prev_msb + self.max_poc_lsb
                } else if lsb > prev_lsb && lsb - prev_lsb > half {
                    prev_msb - self.max_poc_lsb
                } else {
                    prev_msb
                };
                let top = msb + lsb;
                let bottom = top + i64::from(header.delta_pic_order_cnt_bottom);
                (top, bottom, msb, 0)
            }
            1 => {
                // 8.2.1.2: the expected count walks the offset_for_ref_frame cycle.
                let cycle = i64::try_from(self.offset_for_ref_frame.len()).unwrap_or(i64::MAX);
                let mut abs_frame_num = if cycle == 0 {
                    0
                } else {
                    frame_num_offset + i64::from(header.frame_num)
                };
                if !header.reference && abs_frame_num > 0 {
                    abs_frame_num -= 1;
                }
                let mut expected = 0;
                if abs_frame_num > 0 {
                    let per_cycle: i64 = self.offset_for_ref_frame.iter().sum();
                    let cycles = (abs_frame_num - 1) / cycle;
                    let in_cycle = ((abs_frame_num - 1) % cycle) as usize;
                    expected = cycles * per_cycle
                        + self.offset_for_ref_frame[..=in_cycle].iter().sum::<i64>();
                }
                if !header.reference {
                    expected += self.offset_for_non_ref_pic;
                }
                let top = expected + i64::from(header.delta_pic_order_cnt[0]);
                let bottom =
                    top + self.offset_top_to_bottom + i64::from(header.delta_pic_order_cnt[1]);
                (top, bottom, 0, frame_num_offset)
            }
            _ => {
                // 8.2.1.3: counts follow frame_num, with the offset advancing on each wrap.
                let temp = if header.idr {
                    0
                } else {
                    2 * (frame_num_offset + i64::from(header.frame_num))
                        - i64::from(!header.reference)
                };
                (temp, temp, 0, frame_num_offset)
            }
        };
        Ok(PicturePlan {
            top_poc: poc_i32(top)?,
            bottom_poc: poc_i32(bottom)?,
            poc_msb,
            frame_num_offset,
        })
    }

    /// List 0 for a P slice (8.2.4.2.1, 8.2.4.3): short-term references in descending `PicNum`,
    /// then long-term ones in ascending `LongTermPicNum`, then the slice's modifications, cut to
    /// the active count. A modification naming a picture the store does not hold is an error.
    ///
    /// # Errors
    /// Describes a modification that names no reference picture.
    pub fn p_list(&self, header: &SliceHeader) -> Result<Vec<RefPic>, &'static str> {
        let current = header.frame_num;
        let active = header.num_ref_idx_l0_active_minus1 as usize + 1;
        let mut list: Vec<RefPic> = self
            .refs
            .iter()
            .filter(|r| r.long_term.is_none())
            .copied()
            .collect();
        list.sort_by_key(|r| std::cmp::Reverse(self.frame_num_wrap(r.frame_num, current)));
        let mut long: Vec<RefPic> = self
            .refs
            .iter()
            .filter(|r| r.long_term.is_some())
            .copied()
            .collect();
        long.sort_by_key(|r| r.long_term);
        list.extend(long);
        list.truncate(active);

        let max_pic_num = i64::from(self.max_frame_num);
        let curr_pic_num = i64::from(current);
        let mut pred = curr_pic_num;
        for (ref_idx, modification) in header.l0_modifications.iter().enumerate() {
            let picked = match *modification {
                ListModification::Subtract(abs_diff_minus1)
                | ListModification::Add(abs_diff_minus1) => {
                    let delta = i64::from(abs_diff_minus1) + 1;
                    let mut no_wrap = if matches!(modification, ListModification::Subtract(_)) {
                        pred - delta
                    } else {
                        pred + delta
                    };
                    if no_wrap < 0 {
                        no_wrap += max_pic_num;
                    } else if no_wrap >= max_pic_num {
                        no_wrap -= max_pic_num;
                    }
                    pred = no_wrap;
                    let pic_num = if no_wrap > curr_pic_num {
                        no_wrap - max_pic_num
                    } else {
                        no_wrap
                    };
                    self.refs
                        .iter()
                        .find(|r| {
                            r.long_term.is_none()
                                && self.frame_num_wrap(r.frame_num, current) == pic_num
                        })
                        .copied()
                        .ok_or("list modification names a missing short-term reference")?
                }
                ListModification::LongTerm(long_term_pic_num) => self
                    .refs
                    .iter()
                    .find(|r| r.long_term == Some(long_term_pic_num))
                    .copied()
                    .ok_or("list modification names a missing long-term reference")?,
            };
            // Insert at ref_idx and drop the later copy of the same picture (8.2.4.3.1/2).
            list.insert(ref_idx.min(list.len()), picked);
            let mut seen = false;
            list.retain(|r| {
                if r.slot != picked.slot {
                    return true;
                }
                let keep = !seen;
                seen = true;
                keep
            });
        }
        list.truncate(active);
        Ok(list)
    }

    /// Infer the frames a `frame_num` gap skipped (8.2.5.2): each one becomes a short-term
    /// reference with no picture ([`NON_EXISTING`]), marked by the sliding window. Call before
    /// [`Self::plan`] for the picture after the gap.
    ///
    /// The inference runs whatever `gaps_in_frame_num_value_allowed_flag` says: without the flag
    /// a gap is an unintentional loss, which the standard leaves to the decoder, and inferring
    /// the frames is what lets a picture that references an older frame still decode. A
    /// picture that references an inferred frame fails, as it must. A jump of more than half of
    /// `MaxFrameNum` is a repeated or stale picture, not a gap, and `plan` refuses it.
    pub fn fill_gap(&mut self, header: &SliceHeader) {
        if header.idr {
            return;
        }
        let mut expected = (self.prev_ref_frame_num + 1) % self.max_frame_num;
        let missing = (header.frame_num + self.max_frame_num - expected) % self.max_frame_num;
        if missing > self.max_frame_num / 2 {
            return;
        }
        for _ in 0..missing {
            if expected == header.frame_num {
                return;
            }
            self.slide(expected);
            self.refs.push(RefPic {
                slot: NON_EXISTING,
                frame_num: expected,
                top_poc: 0,
                bottom_poc: 0,
                long_term: None,
            });
            if self.prev_frame_num > expected {
                self.prev_frame_num_offset += i64::from(self.max_frame_num);
            }
            self.prev_ref_frame_num = expected;
            self.prev_frame_num = expected;
            expected = (expected + 1) % self.max_frame_num;
        }
    }

    /// Record a picture that decoded into `slot`: mark references (8.2.5), then advance the
    /// order-count state. Call only after the picture decoded.
    pub fn commit(&mut self, header: &SliceHeader, plan: &PicturePlan, slot: usize) {
        let mut current = RefPic {
            slot,
            frame_num: header.frame_num,
            top_poc: plan.top_poc,
            bottom_poc: plan.bottom_poc,
            long_term: None,
        };
        let mut cleared = false;
        if header.idr {
            self.refs.clear();
            if header.long_term_reference {
                current.long_term = Some(0);
                self.max_long_term_idx = Some(0);
            } else {
                self.max_long_term_idx = None;
            }
        } else if header.reference {
            match &header.mmco {
                Some(ops) => {
                    for op in ops {
                        cleared |= self.apply(*op, header.frame_num, &mut current);
                    }
                }
                None => self.slide(header.frame_num),
            }
        }
        if cleared {
            // 8.2.1: after operation 5 the picture counts as frame_num 0 with its order counts
            // moved so the smaller one is 0.
            let temp = current.top_poc.min(current.bottom_poc);
            current.top_poc -= temp;
            current.bottom_poc -= temp;
            current.frame_num = 0;
        }
        if header.reference {
            self.refs.push(current);
            self.prev_ref_frame_num = current.frame_num;
            if cleared {
                self.prev_ref_poc_msb = 0;
                self.prev_ref_poc_lsb = i64::from(current.top_poc);
            } else {
                self.prev_ref_poc_msb = plan.poc_msb;
                self.prev_ref_poc_lsb = i64::from(header.pic_order_cnt_lsb);
            }
        }
        self.prev_frame_num = current.frame_num;
        self.prev_frame_num_offset = if cleared { 0 } else { plan.frame_num_offset };
    }

    /// Apply one adaptive marking operation (8.2.5.4). Returns `true` for operation 5.
    fn apply(&mut self, op: Mmco, frame_num: u32, current: &mut RefPic) -> bool {
        let pic_num_x = |diff: u32| i64::from(frame_num) - (i64::from(diff) + 1);
        match op {
            Mmco::ShortTermUnused {
                difference_of_pic_nums_minus1,
            } => {
                let x = pic_num_x(difference_of_pic_nums_minus1);
                let max_frame_num = self.max_frame_num;
                self.refs.retain(|r| {
                    r.long_term.is_some() || wrap(r.frame_num, frame_num, max_frame_num) != x
                });
            }
            Mmco::LongTermUnused { long_term_pic_num } => {
                self.refs.retain(|r| r.long_term != Some(long_term_pic_num));
            }
            Mmco::ShortTermToLongTerm {
                difference_of_pic_nums_minus1,
                long_term_frame_idx,
            } => {
                let x = pic_num_x(difference_of_pic_nums_minus1);
                self.refs
                    .retain(|r| r.long_term != Some(long_term_frame_idx));
                let max_frame_num = self.max_frame_num;
                if let Some(r) = self.refs.iter_mut().find(|r| {
                    r.long_term.is_none() && wrap(r.frame_num, frame_num, max_frame_num) == x
                }) {
                    r.long_term = Some(long_term_frame_idx);
                }
            }
            Mmco::MaxLongTermFrameIdx(max) => {
                self.max_long_term_idx = max;
                self.refs
                    .retain(|r| r.long_term.is_none_or(|idx| max.is_some_and(|m| idx <= m)));
            }
            Mmco::ClearAll => {
                self.refs.clear();
                self.max_long_term_idx = None;
                return true;
            }
            Mmco::CurrentToLongTerm {
                long_term_frame_idx,
            } => {
                self.refs
                    .retain(|r| r.long_term != Some(long_term_frame_idx));
                current.long_term = Some(long_term_frame_idx);
            }
        }
        false
    }

    /// Sliding-window marking (8.2.5.3): while the store is full, drop the short-term reference
    /// with the smallest `FrameNumWrap`. Long-term references stay.
    fn slide(&mut self, current: u32) {
        while self.refs.len() >= self.max_refs {
            let oldest = self
                .refs
                .iter()
                .enumerate()
                .filter(|(_, r)| r.long_term.is_none())
                .min_by_key(|(_, r)| self.frame_num_wrap(r.frame_num, current))
                .map(|(index, _)| index);
            let Some(index) = oldest else {
                break;
            };
            self.refs.remove(index);
        }
    }

    /// `FrameNumWrap` (8.2.4.1), which is `PicNum` for frames: a reference from before the last
    /// wrap of `frame_num` counts as negative, so it sorts before the current picture's frame_num.
    fn frame_num_wrap(&self, frame_num: u32, current: u32) -> i64 {
        wrap(frame_num, current, self.max_frame_num)
    }
}

fn wrap(frame_num: u32, current: u32, max_frame_num: u32) -> i64 {
    let value = i64::from(frame_num);
    if frame_num > current {
        value - i64::from(max_frame_num)
    } else {
        value
    }
}

fn poc_i32(value: i64) -> Result<i32, &'static str> {
    i32::try_from(value).map_err(|_| "picture order count out of range")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitstream::{H264Profile, SliceKind};

    fn sps(max_refs: u32, log2_frame: u32, poc_type: u32, log2_lsb: u32) -> Sps {
        Sps {
            profile: H264Profile::High,
            level_idc: 40,
            id: 0,
            log2_max_frame_num: log2_frame,
            poc_type,
            log2_max_poc_lsb: log2_lsb,
            delta_pic_order_always_zero: false,
            offset_for_top_to_bottom_field: 0,
            offset_for_non_ref_pic: 0,
            offset_for_ref_frame: Vec::new(),
            gaps_in_frame_num_allowed: false,
            direct_8x8_inference: true,
            max_num_ref_frames: max_refs,
            coded_width: 64,
            coded_height: 64,
            width: 64,
            height: 64,
            crop_x: 0,
            crop_y: 0,
        }
    }

    fn header(frame_num: u32, idr: bool, reference: bool, lsb: u32) -> SliceHeader {
        SliceHeader {
            kind: if idr { SliceKind::I } else { SliceKind::P },
            first_mb_in_slice: 0,
            pps_id: 0,
            frame_num,
            idr,
            reference,
            pic_order_cnt_lsb: lsb,
            delta_pic_order_cnt_bottom: 0,
            delta_pic_order_cnt: [0, 0],
            num_ref_idx_l0_active_minus1: 0,
            l0_modifications: Vec::new(),
            long_term_reference: false,
            mmco: None,
            weights: None,
            cabac_init_idc: 0,
            slice_qp_delta: 0,
            disable_deblocking_filter_idc: 0,
            slice_alpha_c0_offset_div2: 0,
            slice_beta_offset_div2: 0,
            slice_data_bit_offset: 0,
        }
    }

    /// Plan and commit one picture into `slot`.
    fn push(store: &mut RefStore, h: &SliceHeader, slot: usize) -> PicturePlan {
        let plan = store.plan(h).expect("plan");
        store.commit(h, &plan, slot);
        plan
    }

    fn frame_nums(store: &RefStore) -> Vec<u32> {
        store.refs().iter().map(|r| r.frame_num).collect()
    }

    #[test]
    fn sliding_window_keeps_only_the_newest_reference() {
        let mut store = RefStore::new(&sps(1, 4, 2, 0));
        push(&mut store, &header(0, true, true, 0), 0);
        push(&mut store, &header(1, false, true, 0), 1);
        assert_eq!(frame_nums(&store), vec![1]);
        assert_eq!(store.refs()[0].slot, 1);
    }

    #[test]
    fn idr_clears_every_reference() {
        let mut store = RefStore::new(&sps(2, 4, 2, 0));
        push(&mut store, &header(0, true, true, 0), 0);
        push(&mut store, &header(1, false, true, 0), 1);
        push(&mut store, &header(0, true, true, 0), 2);
        assert_eq!(store.refs().len(), 1);
        assert_eq!(store.refs()[0].slot, 2);
    }

    #[test]
    fn non_reference_pictures_are_not_stored() {
        let mut store = RefStore::new(&sps(1, 4, 2, 0));
        push(&mut store, &header(0, true, true, 0), 0);
        push(&mut store, &header(1, false, false, 0), 1);
        assert_eq!(frame_nums(&store), vec![0]);
        // The next reference picture still follows the last reference: frame_num 1.
        push(&mut store, &header(1, false, true, 0), 1);
        assert_eq!(frame_nums(&store), vec![1]);
    }

    #[test]
    fn p_list_orders_by_frame_num_wrap() {
        let mut store = RefStore::new(&sps(2, 4, 2, 0));
        push(&mut store, &header(0, true, true, 0), 0);
        // Pictures 1..=16 with frame_num = i % 16 and alternating slots: after 16 the references
        // are frame_num 15 and frame_num 0, which wrap.
        for i in 1..=16u32 {
            push(
                &mut store,
                &header(i % 16, false, true, 0),
                (i % 3) as usize,
            );
        }
        assert_eq!(frame_nums(&store), vec![15, 0]);
        // Current frame_num 1: frame 0 has wrap 0, frame 15 has wrap -1, so 0 comes first.
        assert_eq!(list_frame_nums(&store, &p(1, 2)), vec![0, 15]);
        assert_eq!(
            list_frame_nums(&store, &p(1, 1)).len(),
            1,
            "list is truncated to the active count"
        );
    }

    #[test]
    fn p_list_is_empty_without_references() {
        let store = RefStore::new(&sps(1, 4, 2, 0));
        assert_eq!(store.p_list(&p(1, 1)), Ok(Vec::new()));
    }

    #[test]
    fn frame_num_gap_is_rejected() {
        let mut store = RefStore::new(&sps(1, 4, 2, 0));
        push(&mut store, &header(0, true, true, 0), 0);
        assert!(store.plan(&header(2, false, true, 0)).is_err());
        assert!(
            store.plan(&header(0, false, true, 0)).is_err(),
            "repeat of the reference"
        );
        assert!(store.plan(&header(1, false, true, 0)).is_ok());
    }

    #[test]
    fn a_gap_is_inferred_and_an_older_reference_stays_usable() {
        let mut store = RefStore::new(&sps(4, 4, 2, 0));
        push(&mut store, &header(0, true, true, 0), 0);
        push(&mut store, &header(1, false, true, 0), 1);
        // Frames 2 and 3 were lost; frame 4 references frame 1 through a modification.
        let mut recovery = header(4, false, true, 0);
        recovery.l0_modifications = vec![ListModification::Subtract(2)];
        store.fill_gap(&recovery);
        assert!(store.plan(&recovery).is_ok());
        let list = store.p_list(&recovery).expect("frame 1 is held");
        assert_eq!((list[0].frame_num, list[0].slot), (1, 1));
        // Without the modification the newest reference is an inferred frame.
        let plain = header(4, false, true, 0);
        assert_eq!(store.p_list(&plain).unwrap()[0].slot, NON_EXISTING);
    }

    #[test]
    fn a_repeated_picture_is_not_a_gap() {
        let mut store = RefStore::new(&sps(4, 4, 2, 0));
        push(&mut store, &header(0, true, true, 0), 0);
        push(&mut store, &header(1, false, true, 0), 1);
        store.fill_gap(&header(1, false, true, 0));
        assert_eq!(store.refs().len(), 2, "nothing was inferred");
    }

    #[test]
    fn poc_type_2_counts_twice_frame_num() {
        let mut store = RefStore::new(&sps(1, 4, 2, 0));
        assert_eq!(push(&mut store, &header(0, true, true, 0), 0).top_poc, 0);
        assert_eq!(push(&mut store, &header(1, false, true, 0), 1).top_poc, 2);
        // Non-reference picture after the reference with frame_num 1: 2 * 2 - 1.
        assert_eq!(push(&mut store, &header(2, false, false, 0), 0).top_poc, 3);
        let last = push(&mut store, &header(2, false, true, 0), 0);
        assert_eq!((last.top_poc, last.bottom_poc), (4, 4));
    }

    #[test]
    fn poc_type_2_offset_advances_on_frame_num_wrap() {
        let mut store = RefStore::new(&sps(1, 4, 2, 0));
        push(&mut store, &header(0, true, true, 0), 0);
        for i in 1..16u32 {
            push(&mut store, &header(i, false, true, 0), (i % 2) as usize);
        }
        // frame_num wraps to 0 with the previous picture at 15: FrameNumOffset becomes 16.
        let wrapped = store.plan(&header(0, false, true, 0)).expect("plan");
        assert_eq!(wrapped.top_poc, 32);
    }

    #[test]
    fn poc_type_0_msb_advances_on_lsb_wrap() {
        // MaxPicOrderCntLsb 16. Lsb sequence 0, 6, 12, 2: the drop from 12 to 2 is a wrap.
        let mut store = RefStore::new(&sps(1, 4, 0, 4));
        assert_eq!(push(&mut store, &header(0, true, true, 0), 0).top_poc, 0);
        assert_eq!(push(&mut store, &header(1, false, true, 6), 1).top_poc, 6);
        assert_eq!(push(&mut store, &header(2, false, true, 12), 0).top_poc, 12);
        assert_eq!(push(&mut store, &header(3, false, true, 2), 1).top_poc, 18);
    }

    /// A P slice header with `active` list 0 entries.
    fn p(frame_num: u32, active: u32) -> SliceHeader {
        SliceHeader {
            num_ref_idx_l0_active_minus1: active - 1,
            ..header(frame_num, false, true, 0)
        }
    }

    fn list_frame_nums(store: &RefStore, h: &SliceHeader) -> Vec<u32> {
        store
            .p_list(h)
            .expect("list")
            .iter()
            .map(|r| r.frame_num)
            .collect()
    }

    /// IDR then reference P pictures 1..=n, each in its own slot.
    fn chain(max_refs: u32, n: u32) -> RefStore {
        let mut store = RefStore::new(&sps(max_refs, 4, 2, 0));
        push(&mut store, &header(0, true, true, 0), 0);
        for i in 1..=n {
            push(&mut store, &header(i, false, true, 0), i as usize);
        }
        store
    }

    #[test]
    fn poc_type_1_walks_the_offset_cycle() {
        // Cycle of two reference frames with offsets 4 and 2, non-reference offset -2.
        let mut s = sps(2, 4, 1, 0);
        s.offset_for_ref_frame = vec![4, 2];
        s.offset_for_non_ref_pic = -2;
        let mut store = RefStore::new(&s);
        assert_eq!(push(&mut store, &header(0, true, true, 0), 0).top_poc, 0);
        // absFrameNum 1: cycle 0, frame 0 of the cycle: 4.
        assert_eq!(push(&mut store, &header(1, false, true, 0), 1).top_poc, 4);
        // absFrameNum 2: 4 + 2.
        assert_eq!(push(&mut store, &header(2, false, true, 0), 2).top_poc, 6);
        // A non-reference picture with frame_num 3: absFrameNum 2, then the -2 offset.
        assert_eq!(
            store
                .plan(&header(3, false, false, 0))
                .expect("plan")
                .top_poc,
            4
        );
        // absFrameNum 3: one full cycle (6) plus the first offset (4).
        assert_eq!(push(&mut store, &header(3, false, true, 0), 0).top_poc, 10);
    }

    #[test]
    fn an_idr_can_be_a_long_term_reference() {
        let mut store = RefStore::new(&sps(2, 4, 2, 0));
        let idr = SliceHeader {
            long_term_reference: true,
            ..header(0, true, true, 0)
        };
        push(&mut store, &idr, 0);
        assert_eq!(store.refs()[0].long_term, Some(0));
        // The long-term picture survives the sliding window of later short-term ones.
        for i in 1..=4 {
            push(&mut store, &header(i, false, true, 0), i as usize);
        }
        let long: Vec<_> = store
            .refs()
            .iter()
            .filter(|r| r.long_term.is_some())
            .collect();
        assert_eq!(long.len(), 1);
        assert_eq!(store.refs().len(), 2);
        // List 0: the short-term one first, the long-term one after it.
        assert_eq!(list_frame_nums(&store, &p(5, 2)), vec![4, 0]);
    }

    #[test]
    fn mmco_marks_short_term_unused_and_long_term() {
        let mut store = chain(4, 2);
        // Picture 3: drop picture 1 (picNum 3 - 2 = 1), make picture 2 long-term index 1.
        let h = SliceHeader {
            mmco: Some(vec![
                Mmco::ShortTermUnused {
                    difference_of_pic_nums_minus1: 1,
                },
                Mmco::ShortTermToLongTerm {
                    difference_of_pic_nums_minus1: 0,
                    long_term_frame_idx: 1,
                },
            ]),
            ..header(3, false, true, 0)
        };
        push(&mut store, &h, 3);
        let mut marks: Vec<(u32, Option<u32>)> = store
            .refs()
            .iter()
            .map(|r| (r.frame_num, r.long_term))
            .collect();
        marks.sort_unstable();
        assert_eq!(marks, vec![(0, None), (2, Some(1)), (3, None)]);
        // Operation 2 removes the long-term one again; operation 4 with 0 clears every index.
        let h = SliceHeader {
            mmco: Some(vec![Mmco::LongTermUnused {
                long_term_pic_num: 1,
            }]),
            ..header(4, false, true, 0)
        };
        push(&mut store, &h, 1);
        assert!(store.refs().iter().all(|r| r.long_term.is_none()));
    }

    #[test]
    fn mmco_6_makes_the_current_picture_long_term_and_4_caps_indices() {
        let mut store = chain(4, 1);
        let h = SliceHeader {
            mmco: Some(vec![
                Mmco::MaxLongTermFrameIdx(Some(1)),
                Mmco::CurrentToLongTerm {
                    long_term_frame_idx: 1,
                },
            ]),
            ..header(2, false, true, 0)
        };
        push(&mut store, &h, 2);
        assert!(
            store
                .refs()
                .iter()
                .any(|r| r.frame_num == 2 && r.long_term == Some(1))
        );
        let h = SliceHeader {
            mmco: Some(vec![Mmco::MaxLongTermFrameIdx(Some(0))]),
            ..header(3, false, true, 0)
        };
        push(&mut store, &h, 3);
        assert!(store.refs().iter().all(|r| r.long_term.is_none()));
    }

    #[test]
    fn mmco_5_restarts_frame_num_and_order_counts() {
        let mut store = chain(4, 3);
        let h = SliceHeader {
            mmco: Some(vec![Mmco::ClearAll]),
            ..header(4, false, true, 0)
        };
        let plan = push(&mut store, &h, 4);
        assert_eq!(plan.top_poc, 8);
        // Only the current picture remains, as frame_num 0 with order count 0.
        assert_eq!(store.refs().len(), 1);
        assert_eq!((store.refs()[0].frame_num, store.refs()[0].top_poc), (0, 0));
        // The next picture follows frame_num 0 and counts from 0 again.
        assert_eq!(push(&mut store, &header(1, false, true, 0), 0).top_poc, 2);
    }

    #[test]
    fn list_modifications_reorder_short_and_long_term() {
        let mut store = chain(4, 3);
        // Default order for picture 4: 3, 2, 1.
        assert_eq!(list_frame_nums(&store, &p(4, 3)), vec![3, 2, 1]);
        // Subtract 3 (picNum 1) to the front, then add 1 (picNum 2): 1, 2, 3.
        let h = SliceHeader {
            l0_modifications: vec![ListModification::Subtract(2), ListModification::Add(0)],
            ..p(4, 3)
        };
        assert_eq!(list_frame_nums(&store, &h), vec![1, 2, 3]);
        // A long-term picture moved to the front.
        let lt = SliceHeader {
            mmco: Some(vec![Mmco::CurrentToLongTerm {
                long_term_frame_idx: 0,
            }]),
            ..header(4, false, true, 0)
        };
        push(&mut store, &lt, 4);
        let h = SliceHeader {
            l0_modifications: vec![ListModification::LongTerm(0)],
            ..p(5, 2)
        };
        assert_eq!(list_frame_nums(&store, &h), vec![4, 3]);
        // A modification naming a missing picture is an error, not a guess.
        let h = SliceHeader {
            l0_modifications: vec![ListModification::LongTerm(7)],
            ..p(5, 1)
        };
        assert!(store.p_list(&h).is_err());
    }
}
