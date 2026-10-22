#![no_main]

use libfuzzer_sys::fuzz_target;
use lumen_protocol::slice::{
    SLICE_MAX_PAYLOAD, SliceDescriptor, SliceReader, slice_payload, split,
};

/// Cap on reassembly buffers so arbitrary totals cannot exhaust memory.
const MAX_TOTAL: usize = 1 << 20;

fn le_bytes<const N: usize>(data: &[u8], at: usize) -> [u8; N] {
    let mut out = [0u8; N];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = data.get(at + i).copied().unwrap_or(0);
    }
    out
}

fuzz_target!(|data: &[u8]| {
    // Split, then reassemble out of order: must reproduce the input exactly.
    let descs = split(data).expect("slice count fits u16 for fuzz-sized input");
    if data.is_empty() {
        assert!(descs.is_empty());
    } else {
        assert_eq!(descs.len(), data.len().div_ceil(SLICE_MAX_PAYLOAD));
        let mut reader = SliceReader::new(descs.len() as u16, descs.len() * SLICE_MAX_PAYLOAD);
        for d in descs.iter().rev() {
            let padded = slice_payload(data, d);
            assert_eq!(padded.len(), SLICE_MAX_PAYLOAD);
            reader
                .insert(d, &padded)
                .expect("split descriptors are in range");
        }
        assert!(reader.is_complete());
        assert_eq!(reader.finish(data.len()), Some(data));
    }

    // Arbitrary descriptor against arbitrary payload: no panic, padding always full length.
    let desc = SliceDescriptor {
        index: u16::from_le_bytes(le_bytes(data, 0)),
        count: u16::from_le_bytes(le_bytes(data, 2)),
        offset: u32::from_le_bytes(le_bytes(data, 4)),
        length: u16::from_le_bytes(le_bytes(data, 8)),
    };
    let payload = data.get(10..).unwrap_or(&[]);
    assert_eq!(slice_payload(payload, &desc).len(), SLICE_MAX_PAYLOAD);

    let total = u32::from_le_bytes(le_bytes(data, 10)) as usize % MAX_TOTAL;
    let mut reader = SliceReader::new(desc.count, total);
    if reader.insert(&desc, payload).is_ok() {
        let start = desc.offset as usize;
        assert_eq!(&reader.partial()[start..start + payload.len()], payload);
    }
    let _ = reader.missing();
    let _ = reader.finish(total);
});
