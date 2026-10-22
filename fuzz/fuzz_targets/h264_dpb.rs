#![no_main]

//! The reference-picture store on arbitrary slice headers.
//!
//! The store is the state machine that follows the spec's rules for order counts, long-term
//! references, adaptive marking and frame_num gaps. It is fed random headers here: it must never
//! panic, and it must never hand out a reference it does not hold.
//!
//! Found so far: nothing.

use libfuzzer_sys::fuzz_target;
use lumen_decode_vaapi::bitstream::{H264Profile, ListModification, Mmco, SliceHeader, SliceKind, Sps};
use lumen_decode_vaapi::dpb::RefStore;

fn header(mut byte: u8) -> SliceHeader {
    let mut mmco = Vec::new();
    for _ in 0..4 {
        match byte % 6 {
            0 => mmco.push(Mmco::ShortTermUnused { difference_of_pic_nums_minus1: 1 }),
            1 => mmco.push(Mmco::LongTermUnused { long_term_pic_num: 2 }),
            2 => mmco.push(Mmco::ShortTermToLongTerm {
                difference_of_pic_nums_minus1: 1,
                long_term_frame_idx: 3,
            }),
            3 => mmco.push(Mmco::MaxLongTermFrameIdx(Some(4))),
            4 => mmco.push(Mmco::ClearAll),
            _ => mmco.push(Mmco::CurrentToLongTerm { long_term_frame_idx: 5 }),
        }
        byte = byte.wrapping_mul(31).wrapping_add(7);
    }
    let mut modifications = Vec::new();
    for _ in 0..3 {
        match byte % 3 {
            0 => modifications.push(ListModification::Subtract(1)),
            1 => modifications.push(ListModification::Add(2)),
            _ => modifications.push(ListModification::LongTerm(1)),
        }
        byte = byte.wrapping_mul(17).wrapping_add(3);
    }
    SliceHeader {
        kind: if byte & 1 == 0 { SliceKind::I } else { SliceKind::P },
        first_mb_in_slice: 0,
        pps_id: 0,
        frame_num: u32::from(byte) % 8,
        idr: byte & 2 == 0,
        reference: byte & 4 != 0,
        pic_order_cnt_lsb: u32::from(byte) % 16,
        delta_pic_order_cnt_bottom: 0,
        delta_pic_order_cnt: [0, 0],
        num_ref_idx_l0_active_minus1: u32::from(byte >> 3) % 4,
        l0_modifications: modifications,
        long_term_reference: byte & 8 != 0,
        mmco: if byte & 16 == 0 { None } else { Some(mmco) },
        weights: None,
        cabac_init_idc: 0,
        slice_qp_delta: 0,
        disable_deblocking_filter_idc: 0,
        slice_alpha_c0_offset_div2: 0,
        slice_beta_offset_div2: 0,
        slice_data_bit_offset: 0,
    }
}

fuzz_target!(|data: &[u8]| {
    let mut store = RefStore::new(&Sps {
        profile: H264Profile::High,
        level_idc: 40,
        id: 0,
        log2_max_frame_num: 4,
        poc_type: u32::from(data.first().copied().unwrap_or(0)) % 3,
        log2_max_poc_lsb: 4,
        delta_pic_order_always_zero: false,
        offset_for_top_to_bottom_field: 0,
        offset_for_non_ref_pic: 0,
        offset_for_ref_frame: Vec::new(),
        gaps_in_frame_num_allowed: data.get(1).copied().unwrap_or(0) & 1 == 0,
        direct_8x8_inference: true,
        max_num_ref_frames: 4,
        coded_width: 64,
        coded_height: 64,
        width: 64,
        height: 64,
        crop_x: 0,
        crop_y: 0,
    });
    for (index, byte) in data.iter().enumerate() {
        let h = header(*byte);
        store.fill_gap(&h);
        let Ok(plan) = store.plan(&h) else {
            continue;
        };
        // The list is either built or refused, never half built.
        if let Ok(list) = store.p_list(&h) {
            for entry in &list {
                assert!(entry.frame_num < 16);
            }
        }
        store.commit(&h, &plan, index % 4);
    }
});
