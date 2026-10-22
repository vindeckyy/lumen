//! Reassembly of access units from transport slices, with FEC recovery.
//!
//! Slices of one access unit are grouped into FEC blocks of 8. A block
//! carries 2 parity slices; when up to two slices of a block are missing and
//! the parity slices arrived, the missing data is solved for algebraically.

use std::collections::HashMap;

use lumen_protocol::header::MediaHeader;
use lumen_protocol::message::MessageKind;

use crate::fec::{BLOCK_DATA_SLICES, FecBlock};

/// How many frames an assembler keeps before it evicts the oldest.
///
/// The receiver prunes lagging frames itself, so this cap only bites a direct
/// caller of the assembler. It bounds the memory a hostile peer can make the
/// assembler hold: each frame reserves `slice_count` slots, and `slice_count`
/// comes from the wire.
pub const MAX_PENDING_FRAMES: usize = 64;

/// A received slice with its metadata.
#[derive(Debug, Clone)]
pub struct ReceivedSlice {
    /// The packet header.
    pub header: MediaHeader,
    /// The payload.
    pub payload: Vec<u8>,
}

/// An access unit reassembled from slices, with the capture timestamp of its frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledAu {
    /// Host frame identifier of the access unit.
    pub frame_id: u32,
    /// Host capture timestamp of the frame, microseconds on the host clock.
    pub capture_ts_us: u64,
    /// The access unit bytes, trimmed to its true length.
    pub data: Vec<u8>,
}

/// State of one access unit being reassembled.
#[derive(Debug)]
struct FrameState {
    /// Slices received so far, indexed by slice index.
    slices: Vec<Option<Vec<u8>>>,
    /// Total number of slices expected.
    count: u16,
    /// Capture timestamp of the frame.
    capture_ts_us: u64,
    /// True length of the access unit in bytes, before slice padding.
    au_len: u32,
    /// Number of data slices per FEC block, as the sender chose it.
    block_data: usize,
}

impl FrameState {
    fn new(count: u16, capture_ts_us: u64, au_len: u32, block_data: usize) -> Self {
        Self {
            slices: vec![None; count as usize],
            count,
            capture_ts_us,
            au_len,
            block_data,
        }
    }

    /// Merge the metadata of a data slice into the frame state.
    ///
    /// A frame state can be created by a parity slice, which carries the same
    /// metadata as the data slices in the current wire format but is not
    /// required to. The data slices carry the content of the frame, so their
    /// metadata wins. The slice table only ever grows, because a frame's slice
    /// count cannot change once slices of it have been seen.
    fn adopt(&mut self, count: u16, capture_ts_us: u64, au_len: u32, block_data: usize) {
        if count > self.count {
            self.slices.resize(count as usize, None);
            self.count = count;
        }
        self.capture_ts_us = capture_ts_us;
        self.au_len = au_len;
        // The sender may change the block size between frames, never inside
        // one, so the first data slice wins.
        if self.block_data == 0 {
            self.block_data = block_data;
        }
    }

    fn missing(&self) -> Vec<u16> {
        self.slices
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_none())
            .map(|(i, _)| i as u16)
            .collect()
    }

    fn is_complete(&self) -> bool {
        // This runs on every slice, so it must not allocate.
        self.slices.iter().all(Option::is_some)
    }
}

/// Assembles access units from a stream of slices, applying FEC recovery.
#[derive(Debug)]
pub struct SliceAssembler {
    frames: HashMap<u32, FrameState>,
    /// Parity slices per frame, keyed by (block index, parity index).
    parity: HashMap<u32, HashMap<(u16, u16), Vec<u8>>>,
    /// Whether FEC recovery is enabled.
    fec_enabled: bool,
}

impl SliceAssembler {
    /// Create an assembler.
    #[must_use]
    pub fn new(fec_enabled: bool) -> Self {
        Self {
            frames: HashMap::new(),
            parity: HashMap::new(),
            fec_enabled,
        }
    }

    /// Evict the oldest frame when the assembler holds more than
    /// [`MAX_PENDING_FRAMES`].
    ///
    /// A frame id is monotonic, so the smallest id is the oldest frame.
    fn evict_if_over_capacity(&mut self) {
        while self.frames.len() > MAX_PENDING_FRAMES {
            let Some(oldest) = self.frames.keys().min().copied() else {
                return;
            };
            self.frames.remove(&oldest);
            self.parity.remove(&oldest);
        }
    }

    /// Feed a received slice.
    ///
    /// Returns the assembled access unit when the frame becomes complete.
    pub fn push(&mut self, slice: &ReceivedSlice) -> Option<AssembledAu> {
        let result = match slice.header.kind {
            MessageKind::VideoData => {
                let frame = self.frames.entry(slice.header.frame_id).or_insert_with(|| {
                    FrameState::new(
                        slice.header.slice_count,
                        slice.header.timestamp_us,
                        slice.header.au_len,
                        block_size(&slice.header),
                    )
                });
                if (slice.header.slice_index as usize) < frame.slices.len() {
                    frame.slices[slice.header.slice_index as usize] = Some(slice.payload.clone());
                }
                frame.adopt(
                    slice.header.slice_count,
                    slice.header.timestamp_us,
                    slice.header.au_len,
                    block_size(&slice.header),
                );
                self.finish_if_complete(slice.header.frame_id)
            }
            MessageKind::VideoParity => {
                if self.fec_enabled {
                    // The frame state must exist before recovery can fill it. A
                    // frame whose only data slice was lost has no `VideoData`
                    // arm to create it, so the parity arm creates it here.
                    self.frames.entry(slice.header.frame_id).or_insert_with(|| {
                        FrameState::new(
                            slice.header.slice_count,
                            slice.header.timestamp_us,
                            slice.header.au_len,
                            block_size(&slice.header),
                        )
                    });
                    self.parity
                        .entry(slice.header.frame_id)
                        .or_default()
                        .insert(
                            (slice.header.aux, slice.header.slice_index),
                            slice.payload.clone(),
                        );
                }
                None
            }
            _ => None,
        };
        self.evict_if_over_capacity();
        result
    }

    /// Try FEC recovery for a frame that is missing slices.
    ///
    /// Returns the assembled access unit when recovery succeeded.
    pub fn try_recover(&mut self, frame_id: u32) -> Option<AssembledAu> {
        if !self.fec_enabled {
            return None;
        }
        let frame = self.frames.get(&frame_id)?;
        let missing = frame.missing();
        if missing.is_empty() {
            return None;
        }
        let parity_map = self.parity.get(&frame_id)?;

        // Group the missing slices by FEC block.
        let block_data = frame.block_data.max(1);
        let mut by_block: HashMap<u16, Vec<u16>> = HashMap::new();
        for index in missing {
            by_block
                .entry(block_of_with(index, block_data))
                .or_default()
                .push(index);
        }

        let mut recovered: Vec<(u16, Vec<u8>)> = Vec::new();
        for (block_index, mut indices) in by_block {
            let parity_count = crate::fec::parity_for_block(block_data);
            if indices.len() > parity_count {
                continue;
            }
            indices.sort_unstable();
            // Indexed by parity slot: an empty slot is a parity slice that did not arrive.
            let parity: Vec<Vec<u8>> = (0..parity_count as u16)
                .map(|p| {
                    parity_map
                        .get(&(block_index, p))
                        .cloned()
                        .unwrap_or_default()
                })
                .collect();
            if parity.iter().filter(|p| !p.is_empty()).count() < indices.len() {
                continue;
            }
            let start = block_index as usize * block_data;
            // The slice length, taken from a slice that arrived. Every slice
            // of a frame is the same length on the wire, so this is the
            // length of all of them.
            let slice_len = (0..block_data)
                .map(|i| start + i)
                .filter(|&i| i < frame.count as usize)
                .find_map(|i| frame.slices[i].as_ref().map(Vec::len))
                .unwrap_or(lumen_protocol::slice::SLICE_MAX_PAYLOAD);
            let data: Vec<Vec<u8>> = (0..block_data)
                .map(|i| {
                    let slice_index = start + i;
                    if slice_index >= frame.count as usize {
                        // The block is padded past the end of the frame with
                        // zero slices, exactly as the sender padded it.
                        return vec![0u8; slice_len];
                    }
                    // A slice that has not arrived is zero-filled, exactly as
                    // the sender's copy of it was not: the recovery equations
                    // skip the missing entries, and a block whose entries all
                    // have the same length is the only one that can be
                    // rebuilt exactly.
                    frame.slices[slice_index]
                        .clone()
                        .unwrap_or_else(|| vec![0u8; slice_len])
                })
                .collect();
            let fec = FecBlock { data, parity };
            let relative: Vec<usize> = indices.iter().map(|&i| i as usize - start).collect();
            if let Some(rebuilt) = fec.recover_missing(&relative) {
                for (local, bytes) in rebuilt {
                    recovered.push(((start + local) as u16, bytes));
                }
            }
        }

        if recovered.is_empty() {
            return None;
        }
        if let Some(frame) = self.frames.get_mut(&frame_id) {
            for (index, bytes) in recovered {
                if (index as usize) < frame.slices.len() {
                    frame.slices[index as usize] = Some(bytes);
                }
            }
        }
        self.finish_if_complete(frame_id)
    }

    /// Number of frames currently being assembled.
    #[must_use]
    pub fn pending_frames(&self) -> usize {
        self.frames.len()
    }

    /// Frame ids that are missing slices.
    #[must_use]
    pub fn incomplete_frames(&self) -> Vec<u32> {
        self.frames
            .iter()
            .filter(|(_, f)| !f.is_complete())
            .map(|(id, _)| *id)
            .collect()
    }

    /// Missing slices of a frame, as `(frame_id, slice_index)` pairs.
    #[must_use]
    pub fn missing_slices(&self, frame_id: u32) -> Vec<u16> {
        self.frames
            .get(&frame_id)
            .map_or_else(Vec::new, FrameState::missing)
    }

    /// Forget a frame entirely.
    pub fn drop_frame(&mut self, frame_id: u32) {
        self.frames.remove(&frame_id);
    }

    fn finish_if_complete(&mut self, frame_id: u32) -> Option<AssembledAu> {
        let frame = self.frames.get(&frame_id)?;
        if !frame.is_complete() {
            return None;
        }
        let mut out = Vec::new();
        for slice in &frame.slices {
            out.extend_from_slice(slice.as_ref()?);
        }
        // Trim the zero padding of the final slice.
        out.truncate(frame.au_len as usize);
        let capture_ts_us = frame.capture_ts_us;
        self.frames.remove(&frame_id);
        self.parity.remove(&frame_id);
        Some(AssembledAu {
            frame_id,
            capture_ts_us,
            data: out,
        })
    }
}

/// The FEC block a slice index belongs to, at the default block size.
#[must_use]
pub fn block_of(slice_index: u16) -> u16 {
    block_of_with(slice_index, BLOCK_DATA_SLICES)
}

/// The FEC block a slice index belongs to, for a given block size.
#[must_use]
pub fn block_of_with(slice_index: u16, block_data: usize) -> u16 {
    slice_index / block_data as u16
}

/// The number of data slices per FEC block the sender used.
///
/// The sender puts the block size in the `flags` word of every slice, so the
/// receiver can map a slice index to its block without being told the parity
/// level. It travels on the parity slices too, so the block size is still
/// known when the data slices of a block are the ones that were lost. A zero
/// means the default.
fn block_size(header: &MediaHeader) -> usize {
    if header.flags == 0 {
        BLOCK_DATA_SLICES
    } else {
        usize::from(header.flags)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_protocol::header::MediaHeader;
    use lumen_protocol::message::MessageKind;

    /// Build a data slice whose payload is the uniform slice size, with the
    /// access unit length set to the unpadded length of the whole unit.
    fn slice(frame: u32, index: u16, count: u16, au_len: u32) -> ReceivedSlice {
        ReceivedSlice {
            header: MediaHeader::data_slice(
                MessageKind::VideoData,
                frame,
                index,
                count,
                1000,
                lumen_protocol::slice::SLICE_MAX_PAYLOAD as u16,
                au_len,
                1,
            ),
            payload: vec![0u8; lumen_protocol::slice::SLICE_MAX_PAYLOAD],
        }
    }

    fn parity(frame: u32, block: u16, index: u16, payload: &[u8]) -> ReceivedSlice {
        ReceivedSlice {
            header: MediaHeader::data_slice(
                MessageKind::VideoParity,
                frame,
                index,
                2,
                1000,
                payload.len() as u16,
                payload.len() as u32,
                1,
            )
            .with_aux(block),
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn assembles_out_of_order_slices() {
        let mut a = SliceAssembler::new(false);
        let au_len = 3 * lumen_protocol::slice::SLICE_MAX_PAYLOAD as u32;
        assert!(a.push(&slice(1, 1, 3, au_len)).is_none());
        assert!(a.push(&slice(1, 0, 3, au_len)).is_none());
        let out = a.push(&slice(1, 2, 3, au_len)).unwrap();
        assert_eq!(out.data.len(), au_len as usize);
    }

    #[test]
    fn incomplete_frame_is_reported() {
        let mut a = SliceAssembler::new(false);
        a.push(&slice(1, 0, 3, 3 * 1200));
        assert_eq!(a.pending_frames(), 1);
        assert_eq!(a.incomplete_frames(), vec![1]);
        assert_eq!(a.missing_slices(1), vec![1, 2]);
    }

    #[test]
    fn duplicate_slices_are_ignored() {
        let mut a = SliceAssembler::new(false);
        a.push(&slice(1, 0, 2, 2 * 1200));
        a.push(&slice(1, 0, 2, 2 * 1200));
        let out = a.push(&slice(1, 1, 2, 2 * 1200)).unwrap();
        assert_eq!(out.data.len(), 2 * 1200);
    }

    #[test]
    fn the_assembler_evicts_old_frames() {
        let mut a = SliceAssembler::new(false);
        // Each frame is left incomplete, so it stays in the map.
        for frame in 0..(MAX_PENDING_FRAMES as u32 + 20) {
            a.push(&slice(frame, 0, 2, 2 * 1200));
        }
        assert!(
            a.pending_frames() <= MAX_PENDING_FRAMES,
            "the assembler held {} frames",
            a.pending_frames()
        );
        // The newest frame is still there.
        let newest = MAX_PENDING_FRAMES as u32 + 19;
        assert_eq!(a.missing_slices(newest), vec![1]);
    }

    #[test]
    fn fec_recovers_two_missing_slices() {
        let count = 8u16;
        let size = lumen_protocol::slice::SLICE_MAX_PAYLOAD;
        let payloads: Vec<Vec<u8>> = (0..count)
            .map(|i| {
                (0..size)
                    .map(|j| (i as u8).wrapping_mul(31).wrapping_add(j as u8))
                    .collect()
            })
            .collect();
        let fec = FecBlock::new(payloads.clone());
        let au_len = (count as usize) * size;
        let mut a = SliceAssembler::new(true);
        for _ in 0..count {
            a.push(&parity(1, 0, 0, &fec.parity[0]));
            a.push(&parity(1, 0, 1, &fec.parity[1]));
        }
        for i in 0..count {
            if i == 2 || i == 5 {
                continue;
            }
            let mut s = slice(1, i, count, au_len as u32);
            s.payload = payloads[i as usize].clone();
            a.push(&s);
        }
        let mut last = slice(1, 7, count, au_len as u32);
        last.payload = payloads[7].clone();
        assert!(a.push(&last).is_none());
        let out = a.try_recover(1).unwrap();
        let expected: Vec<u8> = payloads.concat();
        assert_eq!(out.data, expected);
    }

    #[test]
    fn fec_recovers_a_loss_in_a_later_block() {
        use lumen_protocol::slice::SLICE_MAX_PAYLOAD;
        let payloads: Vec<Vec<u8>> = (0..12u8).map(|i| vec![i + 1; SLICE_MAX_PAYLOAD]).collect();
        // Block 1 holds slices 8..12. The sender pads it to a full block with zero slices.
        let mut block_data: Vec<Vec<u8>> = payloads[8..12].to_vec();
        block_data.resize(crate::fec::BLOCK_DATA_SLICES, vec![0u8; SLICE_MAX_PAYLOAD]);
        let fec = crate::fec::FecBlock::new(block_data);
        let au_len = 12 * SLICE_MAX_PAYLOAD as u32 - 77;
        let mut a = SliceAssembler::new(true);
        for i in (0..12u16).filter(|&i| i != 9) {
            let mut s = slice(1, i, 12, au_len);
            s.payload = payloads[i as usize].clone();
            assert!(a.push(&s).is_none());
        }
        for p in 0..2u16 {
            assert!(a.push(&parity(1, 1, p, &fec.parity[p as usize])).is_none());
        }
        let out = a.try_recover(1).expect("block 1 must recover slice 9");
        let expected: Vec<u8> = payloads.concat()[..au_len as usize].to_vec();
        assert_eq!(out.data, expected);
    }
}
