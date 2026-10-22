#![no_main]

use libfuzzer_sys::fuzz_target;
use lumen_host_service::control::{ControlMessage, decode_message};

fuzz_target!(|data: &[u8]| {
    // Garbage yields Err, never a panic. Decoded lengths never exceed the record.
    match decode_message(data) {
        Ok(ControlMessage::Nack { slices, .. }) => assert!(7 + 2 * slices.len() <= data.len()),
        Ok(ControlMessage::Clipboard { text }) => assert!(5 + text.len() <= data.len()),
        Ok(_) => assert!(!data.is_empty()),
        Err(_) => {}
    }
});
