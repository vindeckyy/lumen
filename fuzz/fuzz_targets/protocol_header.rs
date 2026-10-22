#![no_main]

use libfuzzer_sys::fuzz_target;
use lumen_protocol::{HEADER_LEN, MediaHeader};

fuzz_target!(|data: &[u8]| {
    let Ok(header) = MediaHeader::parse(data) else {
        return;
    };
    // Every header byte is a field, so re-encoding reproduces the wire bytes exactly.
    let bytes = header.to_bytes();
    assert_eq!(&bytes[..], &data[..HEADER_LEN]);
    assert_eq!(MediaHeader::parse(&bytes), Ok(header));
    // Must not overflow on slice_index == u16::MAX.
    let _ = header.is_last_slice();
});
