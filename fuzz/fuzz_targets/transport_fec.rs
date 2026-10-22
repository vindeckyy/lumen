#![no_main]

use libfuzzer_sys::fuzz_target;
use lumen_transport::fec::{BLOCK_DATA_SLICES, FecBlock, compute_parity};

fuzz_target!(|data: &[u8]| {
    let Some((&shape, rest)) = data.split_first() else {
        return;
    };
    let byte = |i: usize| rest.get(i).copied().unwrap_or(0);
    let src = rest.get(3..).unwrap_or(&[]);

    // Part 1: uniform block with one or two losses. Recovery must be exact.
    let n = 1 + usize::from(shape) % BLOCK_DATA_SLICES;
    let len = 1 + usize::from(shape >> 3) % 32;
    let data_slices: Vec<Vec<u8>> = (0..n)
        .map(|i| {
            (0..len)
                .map(|j| src.get(i * len + j).copied().unwrap_or(0))
                .collect()
        })
        .collect();
    let block = FecBlock::new(data_slices.clone());
    let a = usize::from(byte(0)) % n;
    let b = usize::from(byte(1)) % n;
    let mut missing = vec![a];
    if byte(2) & 1 == 1 && b != a {
        missing.push(b);
        missing.sort_unstable();
    }
    // Lost slots hold garbage: recovery must not read them.
    let mut damaged = block.clone();
    for &m in &missing {
        damaged.data[m] = vec![0xAA; len];
    }
    for &m in &missing {
        assert_eq!(
            damaged.recover(m, &missing).as_deref(),
            Some(data_slices[m].as_slice())
        );
    }
    // A repeated index is never recoverable.
    assert_eq!(block.recover(a, &[a, a]), None);

    // Part 2: arbitrary block shape and missing list. Must not panic; a missing index
    // outside the block can never be recovered.
    let count = usize::from(byte(3)) % 12;
    let raw_data: Vec<Vec<u8>> = (0..count)
        .map(|i| {
            src.iter()
                .skip(i)
                .take(usize::from(byte(4)) % 40)
                .copied()
                .collect()
        })
        .collect();
    let raw_parity = compute_parity(&raw_data);
    let raw = FecBlock {
        data: raw_data,
        parity: raw_parity,
    };
    let x = usize::from(u16::from_le_bytes([byte(5), byte(6)]));
    let y = usize::from(u16::from_le_bytes([byte(7), byte(8)]));
    let arbitrary = match byte(9) % 3 {
        0 => vec![x],
        1 => vec![x, y],
        _ => vec![y, x, y],
    };
    for &index in &arbitrary {
        let got = raw.recover(index, &arbitrary);
        if arbitrary.iter().any(|&m| m >= raw.data.len()) {
            assert!(got.is_none());
        }
    }
});
