//! Forward error correction for media slices.
//!
//! Each FEC block carries a number of data slices and a number of parity
//! slices. The parity is Reed-Solomon over GF(2^16) (Leopard-RS, SIMD) from
//! the `reed-solomon-simd` crate, the code punktfunk uses: with `n` parity
//! slices any `n` losses inside a block are recoverable, at O(n log n) cost.
//!
//! The code is a function of the block's data-slice count and parity count,
//! so the sender pads a short last block to the full block size with zero
//! slices, exactly as the receiver does, and both sides take the parity count
//! from the block size ([`parity_for_block`]).

use std::collections::BTreeMap;

/// Number of data slices per FEC block at the default parity level.
pub const BLOCK_DATA_SLICES: usize = 8;

/// Number of parity slices per FEC block at the default parity level.
pub const BLOCK_PARITY_SLICES: usize = 2;

/// The number of parity slices the sender computes for a block of
/// `block_data` data slices. The heavy level is the only one with twelve.
#[must_use]
pub const fn parity_for_block(block_data: usize) -> usize {
    if block_data == ParityLevel::Heavy.block_data_slices() {
        ParityLevel::Heavy.parity_slices()
    } else {
        BLOCK_PARITY_SLICES
    }
}

/// A FEC block: data slices plus the parity computed over them.
#[derive(Debug, Clone)]
pub struct FecBlock {
    /// The data slices in the block. A missing slice may hold anything of the
    /// block's slice length; it is never read.
    pub data: Vec<Vec<u8>>,
    /// The parity slices, by parity index. An empty entry is a parity slice
    /// that did not arrive.
    pub parity: Vec<Vec<u8>>,
}

impl FecBlock {
    /// Build a block and compute its parity slices.
    #[must_use]
    pub fn new(data: Vec<Vec<u8>>) -> Self {
        let parity = compute_parity(&data);
        Self { data, parity }
    }

    /// Try to recover a missing data slice from the parity slices.
    ///
    /// `missing` lists the indices that are absent. Returns
    /// `Some(recovered_bytes)` when the slice could be rebuilt.
    #[must_use]
    pub fn recover(&self, index: usize, missing: &[usize]) -> Option<Vec<u8>> {
        if !missing.contains(&index) {
            return None;
        }
        self.recover_missing(missing)?
            .into_iter()
            .find_map(|(i, bytes)| (i == index).then_some(bytes))
    }

    /// Rebuild every slice in `missing` with one decode, which is what a
    /// receiver wants: the decode, not the copy, is the cost. Returns the
    /// rebuilt slices by index, or `None` when the block cannot be rebuilt.
    #[must_use]
    pub fn recover_missing(&self, missing: &[usize]) -> Option<Vec<(usize, Vec<u8>)>> {
        if missing.is_empty()
            || missing.iter().any(|&m| m >= self.data.len())
            // A repeated index is a malformed list, not a second loss.
            || missing.iter().enumerate().any(|(i, m)| missing[..i].contains(m))
        {
            return None;
        }
        // Every slice of a block must have the same length, or the length of
        // the lost slice is not known. The wire format guarantees this (every
        // slice is padded to the slice size); a block that does not keep it is
        // refused rather than rebuilt to the wrong length.
        let len = self.data.first()?.len();
        if self.data.iter().any(|d| d.len() != len) {
            return None;
        }
        let shard = even(len);
        let recovery: Vec<(usize, &Vec<u8>)> = self
            .parity
            .iter()
            .enumerate()
            .filter(|(_, p)| p.len() == shard)
            .collect();
        if recovery.len() < missing.len() {
            return None;
        }
        let original = self
            .data
            .iter()
            .enumerate()
            .filter(|(i, _)| !missing.contains(i))
            .map(|(i, d)| (i, padded(d, shard)));
        let restored =
            reed_solomon_simd::decode(self.data.len(), self.parity.len(), original, recovery)
                .ok()?;
        Some(
            restored
                .into_iter()
                .map(|(i, mut bytes)| {
                    bytes.truncate(len);
                    (i, bytes)
                })
                .collect(),
        )
    }
}

/// `len` rounded up to the even shard size the Reed-Solomon engine needs.
const fn even(len: usize) -> usize {
    len + len % 2
}

/// `slice` zero-padded to `shard` bytes; borrowed when it already is.
fn padded(slice: &[u8], shard: usize) -> std::borrow::Cow<'_, [u8]> {
    if slice.len() == shard {
        std::borrow::Cow::Borrowed(slice)
    } else {
        let mut owned = slice.to_vec();
        owned.resize(shard, 0);
        std::borrow::Cow::Owned(owned)
    }
}

/// Compute the parity slices for a set of data slices.
///
/// The number of parity slices depends on the parity level: two by default,
/// three when the level is heavy.
#[must_use]
pub fn compute_parity(data: &[Vec<u8>]) -> Vec<Vec<u8>> {
    compute_parity_with(data, BLOCK_PARITY_SLICES)
}

/// Compute `count` parity slices for a set of data slices.
///
/// Every parity slice is as long as the longest data slice, rounded up to an
/// even length; shorter data slices count as zero-padded.
#[must_use]
pub fn compute_parity_with(data: &[Vec<u8>], count: usize) -> Vec<Vec<u8>> {
    let len = data.iter().map(Vec::len).max().unwrap_or(0);
    if data.is_empty() || count == 0 || len == 0 {
        return Vec::new();
    }
    let shard = even(len);
    // The engine accepts every count up to 65535 in total, far above any block here.
    reed_solomon_simd::encode(data.len(), count, data.iter().map(|d| padded(d, shard)))
        .unwrap_or_default()
}

/// How much parity to send for a measured loss rate.
///
/// The order is the amount of parity, so a level can be compared with
/// another one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ParityLevel {
    /// No parity at all.
    None,
    /// Two parity slices per block (the default layout).
    Light,
    /// Three parity slices per block.
    Heavy,
}

impl ParityLevel {
    /// Choose a parity level from a measured loss fraction.
    ///
    /// The thresholds are the plan's: below 0.5 % no parity, below 2 % two
    /// parity slices per block, at or above 2 % three.
    #[must_use]
    pub fn from_loss(loss: f64) -> Self {
        if loss >= 0.02 {
            Self::Heavy
        } else if loss >= 0.005 {
            Self::Light
        } else {
            Self::None
        }
    }

    /// Number of parity slices per block.
    #[must_use]
    pub const fn parity_slices(self) -> usize {
        match self {
            Self::None => 0,
            Self::Light => BLOCK_PARITY_SLICES,
            Self::Heavy => 3,
        }
    }

    /// Number of data slices per block.
    ///
    /// The heavy level uses a larger block so that three parity slices stay
    /// within the 25 % overhead cap.
    #[must_use]
    pub const fn block_data_slices(self) -> usize {
        match self {
            Self::None | Self::Light => BLOCK_DATA_SLICES,
            // Three parity over twelve data slices is 25 %.
            Self::Heavy => 12,
        }
    }

    /// The overhead this level adds, as a fraction of the data it protects.
    #[must_use]
    pub fn overhead(self) -> f64 {
        let data = self.block_data_slices();
        if data == 0 {
            return 0.0;
        }
        self.parity_slices() as f64 / data as f64
    }
}

/// Estimator windows without a loss before the parity level falls.
///
/// A window is half a second, so the level falls about three seconds after the last loss.
pub const PARITY_GOOD_WINDOWS: u32 = 6;

/// Decides when the parity level falls. The level rises at once and never waits on this.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ParityHysteresis {
    good_windows: u32,
}

impl ParityHysteresis {
    /// Called once per estimator window with the level in use, the level the loss asks for and
    /// the loss of the window that just closed. Returns the level to fall to once
    /// [`PARITY_GOOD_WINDOWS`] lossless windows have passed while a lower level is wanted.
    pub fn on_window(
        &mut self,
        current: ParityLevel,
        wanted: ParityLevel,
        window_loss: f64,
    ) -> Option<ParityLevel> {
        if wanted >= current || window_loss > 0.0 {
            self.good_windows = 0;
            return None;
        }
        self.good_windows += 1;
        if self.good_windows >= PARITY_GOOD_WINDOWS {
            self.good_windows = 0;
            return Some(wanted);
        }
        None
    }
}

/// The block size a parity level uses, keyed by the level, so the receiver
/// can map a slice index to its block without being told the level.
pub type BlockSizes = BTreeMap<u16, usize>;

#[cfg(test)]
mod tests {
    use super::*;

    fn block() -> FecBlock {
        let data: Vec<Vec<u8>> = (0..8u8).map(|i| vec![i + 1; 16]).collect();
        FecBlock::new(data)
    }

    #[test]
    fn recovers_single_loss() {
        let b = block();
        assert_eq!(b.recover(3, &[3]).unwrap(), vec![4u8; 16]);
    }

    #[test]
    fn recovers_two_losses_any_pair() {
        let b = block();
        for (x, y) in [(0usize, 5usize), (0, 2), (1, 7), (3, 4), (6, 7)] {
            let want_x = vec![(x as u8) + 1; 16];
            let want_y = vec![(y as u8) + 1; 16];
            assert_eq!(b.recover(x, &[x, y]).unwrap(), want_x, "pair {x},{y}");
            assert_eq!(b.recover(y, &[x, y]).unwrap(), want_y, "pair {x},{y}");
        }
    }

    #[test]
    fn refuses_three_losses_without_a_third_parity() {
        let b = block();
        assert!(b.recover(0, &[0, 1, 2]).is_none());
    }

    #[test]
    fn recovers_three_losses_with_a_third_parity() {
        let data: Vec<Vec<u8>> = (0..8u8).map(|i| vec![i + 1; 16]).collect();
        let b = FecBlock {
            parity: compute_parity_with(&data, 3),
            data,
        };
        for (x, y, z) in [(0usize, 3usize, 7usize), (1, 2, 5), (0, 1, 2)] {
            assert_eq!(
                b.recover(x, &[x, y, z]).unwrap(),
                vec![(x as u8) + 1; 16],
                "triple {x},{y},{z}"
            );
            assert_eq!(
                b.recover(y, &[x, y, z]).unwrap(),
                vec![(y as u8) + 1; 16],
                "triple {x},{y},{z}"
            );
            assert_eq!(
                b.recover(z, &[x, y, z]).unwrap(),
                vec![(z as u8) + 1; 16],
                "triple {x},{y},{z}"
            );
        }
    }

    #[test]
    fn the_heavy_level_stays_inside_the_overhead_cap() {
        assert!(ParityLevel::Heavy.overhead() <= 0.25);
        assert!(ParityLevel::Light.overhead() <= 0.25);
        assert_eq!(ParityLevel::Heavy.overhead(), 0.25);
        assert_eq!(ParityLevel::Light.overhead(), 0.25);
    }

    #[test]
    fn parity_level_thresholds() {
        assert_eq!(ParityLevel::from_loss(0.0), ParityLevel::None);
        assert_eq!(ParityLevel::from_loss(0.004), ParityLevel::None);
        assert_eq!(ParityLevel::from_loss(0.005), ParityLevel::Light);
        assert_eq!(ParityLevel::from_loss(0.019), ParityLevel::Light);
        assert_eq!(ParityLevel::from_loss(0.02), ParityLevel::Heavy);
        assert_eq!(ParityLevel::from_loss(0.5), ParityLevel::Heavy);
    }

    #[test]
    fn rejects_unknown_index() {
        let b = block();
        assert!(b.recover(9, &[9]).is_none());
    }

    #[test]
    fn rejects_missing_index_outside_block() {
        let b = block();
        assert!(b.recover(0, &[0, 9]).is_none());
    }

    #[test]
    fn no_parity_is_computed_when_none_is_asked_for() {
        // The parity costs a pass over the whole block per slice, so a level
        // that needs none must not pay for any.
        let data: Vec<Vec<u8>> = (0..8u8).map(|i| vec![i; 64]).collect();
        assert_eq!(compute_parity_with(&data, 0).len(), 0);
        assert_eq!(compute_parity_with(&data, 1).len(), 1);
        assert_eq!(compute_parity_with(&data, 2).len(), 2);
        assert_eq!(compute_parity_with(&data, 3).len(), 3);
    }

    #[test]
    fn an_odd_slice_length_round_trips() {
        let data: Vec<Vec<u8>> = (0..4u8).map(|i| vec![i + 1; 9]).collect();
        let b = FecBlock::new(data);
        assert!(b.parity.iter().all(|p| p.len() == 10));
        assert_eq!(b.recover(2, &[0, 2]).unwrap(), vec![3u8; 9]);
    }

    #[test]
    fn a_lost_parity_slice_is_skipped_by_index() {
        let data: Vec<Vec<u8>> = (0..12u8).map(|i| vec![i.wrapping_mul(37); 32]).collect();
        let mut b = FecBlock {
            parity: compute_parity_with(&data, 3),
            data: data.clone(),
        };
        // Parity 0 is lost; parity 1 and 2 still rebuild two data losses.
        b.parity[0].clear();
        assert_eq!(b.recover(4, &[4, 9]).unwrap(), data[4]);
        assert_eq!(b.recover(9, &[4, 9]).unwrap(), data[9]);
        // Three losses need three parity slices.
        assert!(b.recover(1, &[1, 4, 9]).is_none());
    }

    #[test]
    fn parity_falls_after_six_clean_windows() {
        let mut h = ParityHysteresis::default();
        for _ in 0..5 {
            assert_eq!(
                h.on_window(ParityLevel::Light, ParityLevel::None, 0.0),
                None
            );
        }
        assert_eq!(
            h.on_window(ParityLevel::Light, ParityLevel::None, 0.0),
            Some(ParityLevel::None)
        );
    }

    #[test]
    fn a_lossy_window_restarts_the_count() {
        let mut h = ParityHysteresis::default();
        for _ in 0..4 {
            h.on_window(ParityLevel::Light, ParityLevel::None, 0.0);
        }
        assert_eq!(
            h.on_window(ParityLevel::Light, ParityLevel::None, 0.01),
            None
        );
        for _ in 0..5 {
            assert_eq!(
                h.on_window(ParityLevel::Light, ParityLevel::None, 0.0),
                None
            );
        }
        assert_eq!(
            h.on_window(ParityLevel::Light, ParityLevel::None, 0.0),
            Some(ParityLevel::None)
        );
    }

    #[test]
    fn a_level_that_is_still_wanted_never_falls() {
        let mut h = ParityHysteresis::default();
        for _ in 0..20 {
            assert_eq!(
                h.on_window(ParityLevel::Light, ParityLevel::Light, 0.0),
                None
            );
        }
    }
}
