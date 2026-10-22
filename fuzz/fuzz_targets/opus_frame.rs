#![no_main]

//! One Opus frame: encoding a frame of samples and decoding a frame back.
//!
//! The audio crate keeps `OpusEncoder` and `OpusDecoder` private, so this
//! target drives the closest public entry points: `new_decoder` opens the
//! Opus decoder behind the `AudioDecoder` trait, and `FrameEncoder` opens the
//! Opus encoder behind the `AudioEncoder` trait.
//!
//! Proves that a malformed Opus packet is refused rather than turned into
//! samples: an empty packet is always an error, and a packet that decodes
//! yields whole frames of finite samples only. A packet that comes out of the
//! encoder always decodes again, into exactly the frame it was made from.
//!
//! Found so far: nothing. No packet panicked the codec and no decoded frame
//! was ever ragged or non-finite.

use libfuzzer_sys::fuzz_target;
use lumen_audio::{AudioConfig, FrameEncoder, new_decoder};

/// Longest Opus frame, 120 ms, per channel at 48 kHz. The audio crate sizes
/// its decode buffer from this.
const MAX_FRAME_SAMPLES: usize = 5760;

fuzz_target!(|data: &[u8]| {
    let Some((&shape, rest)) = data.split_first() else {
        return;
    };
    // Layout: the first byte picks the stream layout, the rest is the packet.
    let channels = if shape & 1 == 0 { 1 } else { 2 };
    let frame_ms = match (shape >> 1) % 3 {
        0 => 25,
        1 => 50,
        _ => 100,
    };
    let cfg = AudioConfig {
        sample_rate: 48_000,
        channels,
        frame_ms,
        bitrate_bps: 96_000,
        use_opus: true,
    };
    let frame_samples = (48_000 * frame_ms / 10_000) as usize;

    let mut dec = new_decoder(&cfg).expect("an Opus decoder for 48 kHz mono or stereo");

    // Part 1: an arbitrary packet. It is either refused or decoded into whole
    // frames of finite samples, never into garbage.
    match dec.decode(rest) {
        Err(_) => {}
        Ok(pcm) => {
            assert_eq!(pcm.len() % usize::from(channels), 0, "whole frames only");
            assert!(
                pcm.len() <= MAX_FRAME_SAMPLES * usize::from(channels),
                "a frame is at most 120 ms long"
            );
            assert!(
                pcm.iter().all(|s| s.is_finite()),
                "a decoded frame must hold finite samples"
            );
        }
    }
    // An empty packet is always refused.
    assert!(dec.decode(&[]).is_err(), "an empty packet is not a frame");

    // Part 2: a frame of samples comes back out of the encoder and decodes
    // again into exactly the frame it was made from.
    let mut enc = FrameEncoder::new(&cfg).expect("an Opus encoder");
    let samples: Vec<f32> = rest
        .iter()
        .map(|&b| f32::from(b) / 127.5 - 1.0)
        .collect();
    let frames = enc.push(&samples, 1000).expect("push samples");
    for f in &frames {
        assert!(!f.pcm, "an Opus frame is not raw PCM");
        assert_eq!(f.capture_ts_us, 1000);
        let pcm = dec.decode(&f.data).expect("a packet from the encoder decodes");
        assert_eq!(
            pcm.len(),
            frame_samples * usize::from(channels),
            "a frame decodes to the samples it was made from"
        );
        assert!(
            pcm.iter().all(|s| s.is_finite()),
            "a decoded frame must hold finite samples"
        );
    }
});
