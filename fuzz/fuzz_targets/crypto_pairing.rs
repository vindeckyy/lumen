#![no_main]

use libfuzzer_sys::fuzz_target;
use lumen_crypto::PairingCode;

fuzz_target!(|data: &[u8]| {
    let input = String::from_utf8_lossy(data);
    let digits = input.chars().filter(char::is_ascii_digit).count();
    match PairingCode::parse(&input) {
        Ok(code) => {
            assert_eq!(digits, 6);
            assert!(code.value() < 1_000_000);
            let text = code.to_string();
            assert_eq!(text.len(), 6);
            assert_eq!(PairingCode::parse(&text).unwrap(), code);
        }
        Err(_) => assert_ne!(digits, 6),
    }
});
