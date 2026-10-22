#![no_main]

//! The H.264 bitstream parser on arbitrary bytes.
//!
//! The parser reads what a hostile or broken stream can hold: the slice header, the reference
//! list modification and the adaptive reference marking. None of it may panic, and the parser
//! must never report a picture it did not read.
//!
//! Found so far: nothing.

use libfuzzer_sys::fuzz_target;
use lumen_decode_vaapi::bitstream::ParamSets;

fuzz_target!(|data: &[u8]| {
    let mut params = ParamSets::default();
    // Every failure is a description, never a panic.
    let _ = params.ingest_access_unit(data);
});
