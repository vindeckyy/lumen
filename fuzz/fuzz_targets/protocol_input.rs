#![no_main]

use libfuzzer_sys::fuzz_target;
use lumen_crypto::{AeadKind, PacketCipher};
use lumen_protocol::input::{
    InputEnvelope, decode_plaintext, encode_plaintext, open_packet, seal_packet,
};

fuzz_target!(|data: &[u8]| {
    // Plaintext: any event that decodes re-encodes to exactly the same bytes.
    if let Ok(env) = decode_plaintext(data) {
        assert_eq!(
            encode_plaintext(env.sequence, env.client_ts_us, env.action),
            data
        );
    }

    // Layout: key @0..32, kind @32, datagram or plaintext @33.. .
    if data.len() < 33 {
        return;
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&data[..32]);
    let kind = if data[32] & 1 == 0 {
        AeadKind::Aes256Gcm
    } else {
        AeadKind::ChaCha20Poly1305
    };
    let body = &data[33..];
    let rx = PacketCipher::new(kind, key, 9);

    // Arbitrary datagram: must never panic.
    let _ = open_packet(&rx, body);

    // Any decodable event survives seal then open, and a flipped tag byte is refused.
    if let Ok(env) = decode_plaintext(body) {
        let mut tx = PacketCipher::new(kind, key, 9);
        let packet = seal_packet(&mut tx, env.client_ts_us, env.action).unwrap();
        let expected = InputEnvelope {
            sequence: 0,
            client_ts_us: env.client_ts_us,
            action: env.action,
        };
        assert_eq!(open_packet(&rx, &packet).unwrap(), expected);
        let mut bad = packet.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(open_packet(&rx, &bad).is_err());
    }
});
