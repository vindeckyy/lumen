#![no_main]

use libfuzzer_sys::fuzz_target;
use lumen_crypto::{AeadKind, PacketCipher};

fuzz_target!(|data: &[u8]| {
    // Layout: key @0..32, kind @32, session @33..37, counter @37..45, then split selector + body.
    if data.len() < 46 {
        return;
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&data[..32]);
    let kind = if data[32] & 1 == 0 {
        AeadKind::Aes256Gcm
    } else {
        AeadKind::ChaCha20Poly1305
    };
    let session = u32::from_le_bytes(data[33..37].try_into().unwrap());
    let counter = u64::from_le_bytes(data[37..45].try_into().unwrap());
    let body = &data[46..];
    let cut = if body.is_empty() {
        0
    } else {
        usize::from(data[45]) % (body.len() + 1)
    };
    let (header, ciphertext) = body.split_at(cut);

    // Arbitrary ciphertext: never panics; success implies a tag-sized suffix was stripped.
    let rx = PacketCipher::new(kind, key, session);
    if let Ok(plain) = rx.open(header, ciphertext, counter) {
        assert_eq!(plain.len() + kind.tag_len(), ciphertext.len());
    }

    // Sealed packets open with the matching counter only; any tamper fails.
    let mut tx = PacketCipher::new(kind, key, session);
    let sealed = tx.seal(header, ciphertext).unwrap();
    let (h, c) = sealed.split_at(header.len());
    assert_eq!(rx.open(h, c, 0).unwrap(), ciphertext);
    assert!(rx.open(h, c, 1).is_err());
    let mut bad_body = c.to_vec();
    bad_body[0] ^= 1;
    assert!(rx.open(h, &bad_body, 0).is_err());
    if !h.is_empty() {
        let mut bad_header = h.to_vec();
        bad_header[0] ^= 0x80;
        assert!(rx.open(&bad_header, c, 0).is_err());
    }
});
