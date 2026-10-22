//! Forward error correction for media slices.
//!
//! Each FEC block carries a number of data slices and a number of parity
//! slices. The parity is computed over GF(2^8) with the primitive polynomial
//! `x^8 + x^4 + x^3 + x^2 + 1`:
//!
//! - `parity[0]` = sum of all data slices
//! - `parity[1]` = sum of `(index + 1) * slice` over all data slices
//! - `parity[2]` = sum of `(index + 1)^2 * slice` over all data slices
//!
//! With two parity slices any two distinct losses inside a block are
//! recoverable. With three, any three distinct losses are: the three
//! equations are a Vandermonde system in `(i+1, j+1, k+1)`, whose
//! determinant is non-zero exactly when the three values differ.
//!
//! XOR-only parity cannot do this: two slices with the same index parity are
//! indistinguishable under an XOR code, and returning wrong bytes is worse
//! than reporting a loss.

use std::collections::BTreeMap;

/// Number of data slices per FEC block at the default parity level.
pub const BLOCK_DATA_SLICES: usize = 8;

/// Number of parity slices per FEC block at the default parity level.
pub const BLOCK_PARITY_SLICES: usize = 2;

/// GF(2^8) multiplication tables for the polynomial 0x11D.
struct Gf256 {
    log: [u8; 256],
    exp: [u8; 512],
}

impl Gf256 {
    const fn new() -> Self {
        let mut log = [0u8; 256];
        let mut exp = [0u8; 512];
        let mut x: u16 = 1;
        let mut i = 0usize;
        while i < 255 {
            exp[i] = x as u8;
            exp[i + 255] = x as u8;
            log[x as usize] = i as u8;
            // x = x * 2 in GF(2^8) with the reduction polynomial 0x11D.
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= 0x11D;
            }
            i += 1;
        }
        Self { log, exp }
    }

    fn mul(&self, a: u8, b: u8) -> u8 {
        if a == 0 || b == 0 {
            return 0;
        }
        self.exp[self.log[a as usize] as usize + self.log[b as usize] as usize]
    }

    fn inv(&self, a: u8) -> u8 {
        debug_assert_ne!(a, 0);
        self.exp[255 - self.log[a as usize] as usize]
    }
}

static GF: Gf256 = Gf256::new();

/// Multiply every byte of `slice` by the scalar `k` and XOR into `dst`.
fn accumulate_scaled(dst: &mut Vec<u8>, slice: &[u8], k: u8) {
    if dst.len() < slice.len() {
        dst.resize(slice.len(), 0);
    }
    if k == 0 {
        return;
    }
    if k == 1 {
        for (d, s) in dst.iter_mut().zip(slice.iter()) {
            *d ^= s;
        }
        return;
    }
    for (d, s) in dst.iter_mut().zip(slice.iter()) {
        *d ^= GF.mul(k, *s);
    }
}

/// Parity coefficient of data slice `index`: `index + 1` modulo 256.
const fn coefficient(index: usize) -> u8 {
    (index as u8).wrapping_add(1)
}

/// Parity coefficient of data slice `index` for the third parity: the square
/// of the first coefficient.
fn coefficient_squared(index: usize) -> u8 {
    let k = coefficient(index);
    GF.mul(k, k)
}

/// A FEC block: data slices plus the parity computed over them.
#[derive(Debug, Clone)]
pub struct FecBlock {
    /// The data slices in the block.
    pub data: Vec<Vec<u8>>,
    /// The parity slices computed over `data`.
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
    /// `missing` lists the indices that are absent, in ascending order.
    /// Returns `Some(recovered_bytes)` when the slice could be rebuilt.
    #[must_use]
    pub fn recover(&self, index: usize, missing: &[usize]) -> Option<Vec<u8>> {
        if index >= self.data.len()
            || !missing.contains(&index)
            || missing.iter().any(|&m| m >= self.data.len())
        {
            return None;
        }
        // Every slice of a block must have the same length, or the length of
        // the lost slice is not recoverable from the parity: the parity
        // carries the longest slice's length. The wire format guarantees this
        // (every slice is padded to the slice size), but a block that does
        // not keep it is refused rather than rebuilt to the wrong length.
        if !self.uniform_length() {
            return None;
        }
        match missing.len() {
            1 => self.recover_single(index),
            2 => {
                let a = missing[0];
                let b = missing[1];
                if a == b {
                    return None;
                }
                self.recover_pair(a, b, index)
            }
            3 => {
                let a = missing[0];
                let b = missing[1];
                let c = missing[2];
                if a == b || b == c || a == c {
                    return None;
                }
                self.recover_triple(a, b, c, index)
            }
            _ => None,
        }
    }

    /// Whether every data slice has the same length.
    fn uniform_length(&self) -> bool {
        let mut lengths = self.data.iter().map(Vec::len);
        let Some(first) = lengths.next() else {
            return true;
        };
        lengths.all(|l| l == first)
    }

    /// Recover one missing slice: `slice = parity0 - sum(intact)`.
    fn recover_single(&self, index: usize) -> Option<Vec<u8>> {
        let p0 = self.parity.first()?;
        let mut out = p0.clone();
        for (i, slice) in self.data.iter().enumerate() {
            if i == index {
                continue;
            }
            accumulate_scaled(&mut out, slice, 1);
        }
        Some(out)
    }

    /// Recover two missing slices from the two parity equations.
    fn recover_pair(&self, a: usize, b: usize, index: usize) -> Option<Vec<u8>> {
        let p0 = self.parity.first()?;
        let p1 = self.parity.get(1)?;

        // S = a + b
        let mut s = p0.clone();
        // W = (a+1)*a_slice + (b+1)*b_slice
        let mut w = p1.clone();
        for (i, slice) in self.data.iter().enumerate() {
            if i == a || i == b {
                continue;
            }
            accumulate_scaled(&mut s, slice, 1);
            accumulate_scaled(&mut w, slice, coefficient(i));
        }

        let ka = coefficient(a);
        let kb = coefficient(b);
        // determinant = ka - kb; zero only when the two indices share a coefficient mod 256
        let det = ka ^ kb;
        if det == 0 {
            return None;
        }
        // a_slice = (W - kb*S) / (ka - kb)
        let mut num = w.clone();
        accumulate_scaled(&mut num, &s, kb);
        let inv_det = GF.inv(det);
        let mut a_slice = num.clone();
        for byte in &mut a_slice {
            *byte = GF.mul(inv_det, *byte);
        }
        // b_slice = S - a_slice
        let mut b_slice = s.clone();
        for (d, x) in b_slice.iter_mut().zip(a_slice.iter()) {
            *d ^= *x;
        }

        if a == index {
            Some(a_slice)
        } else if b == index {
            Some(b_slice)
        } else {
            None
        }
    }

    /// Recover three missing slices from the three parity equations.
    ///
    /// The three equations are a Vandermonde system in
    /// `(i+1, j+1, k+1)`. It is solved by Gaussian elimination over GF(256).
    #[allow(clippy::many_single_char_names)]
    fn recover_triple(&self, a: usize, b: usize, c: usize, index: usize) -> Option<Vec<u8>> {
        let p0 = self.parity.first()?;
        let p1 = self.parity.get(1)?;
        let p2 = self.parity.get(2)?;

        // The right-hand sides with the intact slices removed.
        let mut s = p0.clone();
        let mut w = p1.clone();
        let mut z = p2.clone();
        for (i, slice) in self.data.iter().enumerate() {
            if i == a || i == b || i == c {
                continue;
            }
            accumulate_scaled(&mut s, slice, 1);
            accumulate_scaled(&mut w, slice, coefficient(i));
            accumulate_scaled(&mut z, slice, coefficient_squared(i));
        }

        let rows = [
            [1u8, 1, 1],
            [coefficient(a), coefficient(b), coefficient(c)],
            [
                coefficient_squared(a),
                coefficient_squared(b),
                coefficient_squared(c),
            ],
        ];
        let rhs = [s, w, z];
        let solution = solve_vandermonde(rows, rhs)?;

        if a == index {
            Some(solution[0].clone())
        } else if b == index {
            Some(solution[1].clone())
        } else if c == index {
            Some(solution[2].clone())
        } else {
            None
        }
    }
}

/// Solve a 3x3 system over GF(256) by Gaussian elimination.
///
/// `rows` is the coefficient matrix and `rhs` the three right-hand sides,
/// each one byte per position. Returns `None` when the matrix is singular.
fn solve_vandermonde(rows: [[u8; 3]; 3], rhs: [Vec<u8>; 3]) -> Option<[Vec<u8>; 3]> {
    let mut matrix = rows;
    let mut values = rhs;

    // Forward elimination.
    for col in 0..3 {
        // Find a pivot in this column.
        let pivot = (col..3).find(|&r| matrix[r][col] != 0)?;
        matrix.swap(col, pivot);
        values.swap(col, pivot);

        let inv = GF.inv(matrix[col][col]);
        for entry in &mut matrix[col][col..] {
            *entry = GF.mul(inv, *entry);
        }
        for byte in &mut values[col] {
            *byte = GF.mul(inv, *byte);
        }

        for row in 0..3 {
            if row == col {
                continue;
            }
            let factor = matrix[row][col];
            if factor == 0 {
                continue;
            }
            // The pivot row is copied out, because the two rows borrow the
            // matrix at the same time.
            let pivot: Vec<u8> = matrix[col][col..].to_vec();
            for (offset, source) in pivot.iter().enumerate() {
                matrix[row][col + offset] ^= GF.mul(factor, *source);
            }
            let mut scaled = values[col].clone();
            for x in &mut scaled {
                *x = GF.mul(factor, *x);
            }
            for (d, x) in values[row].iter_mut().zip(scaled.iter()) {
                *d ^= *x;
            }
        }
    }

    Some(values)
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
#[must_use]
pub fn compute_parity_with(data: &[Vec<u8>], count: usize) -> Vec<Vec<u8>> {
    if data.is_empty() || count == 0 {
        return Vec::new();
    }
    let len = data.iter().map(Vec::len).max().unwrap_or(0);
    let mut p0 = vec![0u8; len];
    for slice in data {
        accumulate_scaled(&mut p0, slice, 1);
    }
    if count == 1 {
        return vec![p0];
    }
    let mut p1 = vec![0u8; len];
    for (i, slice) in data.iter().enumerate() {
        accumulate_scaled(&mut p1, slice, coefficient(i));
    }
    if count == 2 {
        return vec![p0, p1];
    }
    let mut p2 = vec![0u8; len];
    for (i, slice) in data.iter().enumerate() {
        accumulate_scaled(&mut p2, slice, coefficient_squared(i));
    }
    vec![p0, p1, p2]
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
    fn parity_matches_data_length() {
        let data: Vec<Vec<u8>> = (0..4u8).map(|i| vec![i; 9]).collect();
        let p = compute_parity(&data);
        assert_eq!(p.len(), 2);
        for parity in &p {
            assert_eq!(parity.len(), 9);
        }
    }

    #[test]
    fn gf256_is_a_field() {
        for a in 1..=255u8 {
            assert_eq!(GF.mul(a, GF.inv(a)), 1, "a={a}");
        }
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
