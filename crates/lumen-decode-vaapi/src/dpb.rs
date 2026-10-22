//! Reference-picture state for H.264 decode: `frame_num` and picture order count tracking, the
//! short-term reference set with sliding-window marking, and the list 0 order for P slices.
//!
//! Pure: no VA calls, so each rule is testable without a GPU. A picture changes this state only
//! in [`RefStore::commit`], after it decoded, so a failed picture leaves the store unchanged.

use crate::bitstream::{SliceHeader, Sps};

/// A short-term reference picture.
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
    /// `FrameNumOffset` (type 2), carried to the next picture.
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
    refs: Vec<RefPic>,
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
            refs: Vec::new(),
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

    /// The current short-term reference pictures, in insertion order.
    #[must_use]
    pub fn refs(&self) -> &[RefPic] {
        &self.refs
    }

    /// Check `header` against the stored state and derive its order counts. Changes nothing.
    ///
    /// # Errors
    /// Describes a `frame_num` gap, or an order-count type this decoder cannot derive for the
    /// picture.
    pub fn plan(&self, header: &SliceHeader) -> Result<PicturePlan, &'static str> {
        if !header.idr && header.frame_num != (self.prev_ref_frame_num + 1) % self.max_frame_num {
            return Err(
                "frame_num gap: a reference picture is missing (gaps_in_frame_num is not supported)",
            );
        }
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
            1 if header.idr => {
                // 8.2.1.2 with FrameNumOffset 0 and absFrameNum 0: the expected count is 0.
                let top = i64::from(header.delta_pic_order_cnt[0]);
                let bottom =
                    top + self.offset_top_to_bottom + i64::from(header.delta_pic_order_cnt[1]);
                (top, bottom, 0, 0)
            }
            1 => return Err("pic_order_cnt_type 1 is supported only for IDR pictures"),
            _ => {
                // 8.2.1.3: counts follow frame_num, with the offset advancing on each wrap.
                let offset = if header.idr {
                    0
                } else if self.prev_frame_num > header.frame_num {
                    self.prev_frame_num_offset + i64::from(self.max_frame_num)
                } else {
                    self.prev_frame_num_offset
                };
                let temp = if header.idr {
                    0
                } else {
                    2 * (offset + i64::from(header.frame_num)) - i64::from(!header.reference)
                };
                (temp, temp, 0, offset)
            }
        };
        Ok(PicturePlan {
            top_poc: poc_i32(top)?,
            bottom_poc: poc_i32(bottom)?,
            poc_msb,
            frame_num_offset,
        })
    }

    /// List 0 for a P slice: the short-term references in descending `FrameNumWrap` (8.2.4.2.1),
    /// truncated to `active` entries.
    #[must_use]
    pub fn p_list(&self, frame_num: u32, active: usize) -> Vec<RefPic> {
        let mut list = self.refs.clone();
        list.sort_by_key(|r| std::cmp::Reverse(self.frame_num_wrap(r.frame_num, frame_num)));
        list.truncate(active);
        list
    }

    /// The lowest pool slot in `0..pool` that no reference picture uses. `None` when every slot
    /// is a reference.
    #[must_use]
    pub fn free_slot(&self, pool: usize) -> Option<usize> {
        (0..pool).find(|slot| self.refs.iter().all(|r| r.slot != *slot))
    }

    /// Record a picture that decoded into `slot`: mark it as a reference (8.2.5), then advance
    /// the order-count state. Call only after the picture decoded.
    pub fn commit(&mut self, header: &SliceHeader, plan: &PicturePlan, slot: usize) {
        if header.idr {
            self.refs.clear();
        }
        if header.reference {
            if !header.idr {
                self.slide(header.frame_num);
            }
            self.refs.push(RefPic {
                slot,
                frame_num: header.frame_num,
                top_poc: plan.top_poc,
                bottom_poc: plan.bottom_poc,
            });
            self.prev_ref_frame_num = header.frame_num;
            self.prev_ref_poc_msb = plan.poc_msb;
            self.prev_ref_poc_lsb = i64::from(header.pic_order_cnt_lsb);
        }
        self.prev_frame_num = header.frame_num;
        self.prev_frame_num_offset = plan.frame_num_offset;
    }

    /// Sliding-window marking (8.2.5.3): while the store is full, drop the short-term reference
    /// with the smallest `FrameNumWrap`.
    fn slide(&mut self, current: u32) {
        while self.refs.len() >= self.max_refs {
            let oldest = self
                .refs
                .iter()
                .enumerate()
                .min_by_key(|(_, r)| self.frame_num_wrap(r.frame_num, current))
                .map(|(index, _)| index);
            let Some(index) = oldest else {
                break;
            };
            self.refs.remove(index);
        }
    }

    /// `FrameNumWrap` (8.2.4.1): a reference from before the last wrap of `frame_num` counts as
    /// negative, so it sorts before the current picture's frame_num.
    fn frame_num_wrap(&self, frame_num: u32, current: u32) -> i64 {
        let value = i64::from(frame_num);
        if frame_num > current {
            value - i64::from(self.max_frame_num)
        } else {
            value
        }
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
        assert_eq!(store.free_slot(2), Some(1));
        push(&mut store, &header(1, false, true, 0), 1);
        assert_eq!(frame_nums(&store), vec![1]);
        assert_eq!(store.refs()[0].slot, 1);
        assert_eq!(
            store.free_slot(2),
            Some(0),
            "the old reference's slot is free again"
        );
    }

    #[test]
    fn free_slot_skips_every_live_reference() {
        let mut store = RefStore::new(&sps(2, 4, 2, 0));
        push(&mut store, &header(0, true, true, 0), 0);
        push(&mut store, &header(1, false, true, 0), 1);
        assert_eq!(store.free_slot(3), Some(2));
        assert_eq!(store.free_slot(2), None);
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
        assert_eq!(
            store
                .p_list(1, 2)
                .iter()
                .map(|r| r.frame_num)
                .collect::<Vec<_>>(),
            vec![0, 15]
        );
        assert_eq!(
            store.p_list(1, 1).len(),
            1,
            "list is truncated to the active count"
        );
    }

    #[test]
    fn p_list_is_empty_without_references() {
        let store = RefStore::new(&sps(1, 4, 2, 0));
        assert_eq!(store.p_list(1, 1), Vec::new());
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

    #[test]
    fn poc_type_1_is_limited_to_idr() {
        let mut store = RefStore::new(&sps(1, 4, 1, 0));
        push(&mut store, &header(0, true, true, 0), 0);
        assert!(store.plan(&header(1, false, true, 0)).is_err());
    }
}
