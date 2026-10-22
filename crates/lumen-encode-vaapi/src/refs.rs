//! The reference window of the encoder, and reference frame invalidation.
//!
//! The encoder keeps the last `capacity` pictures as short-term references, with the same
//! sliding window a decoder applies (8.2.5.3), so both sides hold the same `frame_num`s. A
//! normal P picture references the newest one. When the client reports a loss, the next P
//! picture references the newest frame the client still has instead, and every picture after
//! it is distrusted: the client inferred those as non-existing frames (8.2.5.2), so none of
//! them may anchor a later recovery. No IDR is sent. This is punktfunk's slot recovery
//! (`pf-encode-core/src/rfi.rs`, MIT OR Apache-2.0, Copyright (c) 2026 unom - Enrico Bühler)
//! applied to the encoder-owned H.264 DPB.

use crate::buffer::RefPicture;

/// One reference picture and the source frame it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    pic: RefPicture,
    /// `VideoFrame::index` of the source frame.
    index: u64,
    /// Whether the client may hold this picture. False for the pictures encoded between a
    /// recovery anchor and the recovery picture.
    trusted: bool,
}

/// The reference the next P picture uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reference {
    /// The picture.
    pub pic: RefPicture,
    /// `abs_diff_pic_num_minus1` of the list-0 modification that moves the picture to index 0,
    /// or `None` when it is the newest reference, which list 0 already starts with.
    pub modification: Option<u32>,
    /// Whether this picture recovers from a loss.
    pub recovery: bool,
}

/// The short-term references, oldest first.
#[derive(Debug)]
pub(crate) struct RefWindow {
    entries: Vec<Entry>,
    capacity: usize,
    anchor: Option<u64>,
}

impl RefWindow {
    /// An empty window of `capacity` references (`max_num_ref_frames`).
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            entries: Vec::with_capacity(capacity),
            capacity: capacity.max(1),
            anchor: None,
        }
    }

    /// The newest picture, which the next picture's `frame_num` follows.
    pub(crate) fn newest(&self) -> Option<RefPicture> {
        self.entries.last().map(|e| e.pic)
    }

    /// Every reference picture, for `ReferenceFrames`.
    pub(crate) fn pictures(&self) -> impl Iterator<Item = RefPicture> + '_ {
        self.entries.iter().map(|e| e.pic)
    }

    /// A reconstruction slot no reference uses, out of `slots`.
    pub(crate) fn free_slot(&self, slots: usize) -> usize {
        (0..slots)
            .find(|s| self.entries.iter().all(|e| e.pic.slot != *s))
            .expect("there is one more slot than references")
    }

    /// Asks the next P picture to reference the source frame `index`. Returns `false` when the
    /// window no longer holds a trusted picture of that frame; the caller then needs an IDR.
    pub(crate) fn recover_from(&mut self, index: u64) -> bool {
        let held = self.entries.iter().any(|e| e.index == index && e.trusted);
        self.anchor = held.then_some(index);
        held
    }

    /// The reference of the next P picture, whose `frame_num` is `frame_num`. Consumes a
    /// pending recovery: the anchor becomes the reference and every newer picture is distrusted.
    pub(crate) fn next_reference(&mut self, frame_num: u16) -> Option<Reference> {
        let newest = *self.entries.last()?;
        let Some(anchor) = self.anchor.take() else {
            return Some(Reference {
                pic: newest.pic,
                modification: None,
                recovery: false,
            });
        };
        let position = self
            .entries
            .iter()
            .position(|e| e.index == anchor && e.trusted)?;
        for entry in &mut self.entries[position + 1..] {
            entry.trusted = false;
        }
        let pic = self.entries[position].pic;
        // PicNum distance from the current picture; the u16 wrap is MaxFrameNum's wrap.
        let distance = u32::from(frame_num.wrapping_sub(pic.frame_num));
        Some(Reference {
            pic,
            modification: (pic != newest.pic).then(|| distance - 1),
            recovery: true,
        })
    }

    /// Adds the picture just encoded, sliding the oldest out when the window is full.
    pub(crate) fn push(&mut self, pic: RefPicture, index: u64) {
        if self.entries.len() >= self.capacity {
            self.entries.remove(0);
        }
        self.entries.push(Entry {
            pic,
            index,
            trusted: true,
        });
    }

    /// Forgets every reference, for an IDR.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.anchor = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pic(slot: usize, frame_num: u16) -> RefPicture {
        RefPicture {
            slot,
            surface: slot as u32,
            frame_num,
            poc: frame_num.wrapping_mul(2),
        }
    }

    fn window_of(frames: u16, capacity: usize) -> RefWindow {
        let mut window = RefWindow::new(capacity);
        for n in 0..frames {
            let slot = window.free_slot(capacity + 1);
            window.push(pic(slot, n), u64::from(n) + 100);
        }
        window
    }

    #[test]
    fn a_normal_picture_references_the_newest() {
        let mut window = window_of(3, 4);
        let r = window.next_reference(3).unwrap();
        assert_eq!(r.pic.frame_num, 2);
        assert_eq!(r.modification, None);
        assert!(!r.recovery);
    }

    #[test]
    fn the_window_slides_like_a_decoder() {
        let window = window_of(6, 4);
        let held: Vec<u16> = window.pictures().map(|p| p.frame_num).collect();
        assert_eq!(held, vec![2, 3, 4, 5]);
        // Frame 5 reused slot 0 when frame 0 slid out, so slot 1 (frame 1's) is free now.
        assert_eq!(window.free_slot(5), 1);
    }

    #[test]
    fn recovery_references_the_anchor_and_distrusts_what_followed() {
        let mut window = window_of(6, 4);
        // Source frame 103 is frame_num 3; frames 4 and 5 were lost.
        assert!(window.recover_from(103));
        let r = window.next_reference(6).unwrap();
        assert_eq!(r.pic.frame_num, 3);
        assert_eq!(r.modification, Some(2), "PicNum 6 - 3 = 3, minus 1");
        assert!(r.recovery);
        // Frame 104 was distrusted: it cannot anchor a later recovery.
        assert!(!window.recover_from(104));
        // Frame 103 still can.
        assert!(window.recover_from(103));
    }

    #[test]
    fn recovery_from_the_newest_needs_no_modification() {
        let mut window = window_of(3, 4);
        assert!(window.recover_from(102));
        let r = window.next_reference(3).unwrap();
        assert_eq!((r.modification, r.recovery), (None, true));
    }

    #[test]
    fn a_frame_out_of_the_window_cannot_anchor() {
        let mut window = window_of(6, 4);
        assert!(!window.recover_from(101), "frame_num 1 slid out");
        assert_eq!(window.next_reference(6).unwrap().modification, None);
    }

    #[test]
    fn the_distance_wraps_with_frame_num() {
        let mut window = RefWindow::new(4);
        window.push(pic(0, u16::MAX - 1), 1);
        window.push(pic(1, u16::MAX), 2);
        window.push(pic(2, 0), 3);
        assert!(window.recover_from(1));
        let r = window.next_reference(1).unwrap();
        assert_eq!(r.modification, Some(2), "1 - 65534 wraps to 3");
    }
}
