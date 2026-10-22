//! Loss recovery without a keyframe, on the live VA-API device.
//!
//! The encoder streams to the decoder, two frames are lost on the way, and the picture after
//! them fails because it predicts from a lost frame. The encoder is told the last frame the
//! decoder has; its next picture references that frame, carries a recovery point, and decodes
//! exactly, as does every picture after it. No IDR is sent.

use lumen_decode_vaapi::VaapiDecoder;
use lumen_encode_vaapi::{EncoderConfig, H264Encoder, nal};
use lumen_protocol::media::{
    EncodedFrame, FrameBuffer, PixelFormat, VideoDecoder, VideoEncoder, VideoFrame,
};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const MIN_PSNR_DB: f64 = 30.0;

/// Deterministic synthetic NV12 frame: a diagonal luma ramp, a bright square that moves
/// 8 pixels per frame, and chroma ramps that drift with the frame index.
fn source_frame(index: u64) -> Vec<u8> {
    let (w, h) = (WIDTH as usize, HEIGHT as usize);
    let t = index as usize;
    let mut data = vec![0u8; PixelFormat::Nv12.frame_len(WIDTH, HEIGHT)];
    let square_x = (t * 8) % (w - 160);
    for y in 0..h {
        for x in 0..w {
            let in_square = (square_x..square_x + 160).contains(&x) && (200..360).contains(&y);
            data[y * w + x] = if in_square {
                235
            } else {
                16 + ((x * 3 + y * 2 + t * 4) % 200) as u8
            };
        }
    }
    let uv = w * h;
    for row in 0..h / 2 {
        for col in 0..w / 2 {
            let c = (col + row / 2 + t) % 32;
            data[uv + row * w + 2 * col] = 112 + c as u8;
            data[uv + row * w + 2 * col + 1] = 144 - c as u8;
        }
    }
    data
}

/// Luma PSNR in dB. Identical planes give infinity.
fn luma_psnr(reference: &[u8], decoded: &[u8]) -> f64 {
    let squared: u64 = reference
        .iter()
        .zip(decoded)
        .map(|(&a, &b)| {
            let d = u64::from(a.abs_diff(b));
            d * d
        })
        .sum();
    if squared == 0 {
        return f64::INFINITY;
    }
    let mse = squared as f64 / reference.len() as f64;
    10.0 * (255.0_f64 * 255.0 / mse).log10()
}

fn encode(encoder: &mut H264Encoder, index: u64) -> EncodedFrame {
    let frame = VideoFrame::cpu(
        WIDTH,
        HEIGHT,
        PixelFormat::Nv12,
        source_frame(index),
        0,
        index,
    );
    encoder.encode(&frame).expect("encode")
}

fn check(decoder: &mut VaapiDecoder, au: &EncodedFrame, index: u64) {
    let luma_len = (WIDTH * HEIGHT) as usize;
    let decoded = decoder
        .decode(au)
        .unwrap_or_else(|e| panic!("decode frame {index}: {e}"))
        .expect("a picture");
    let FrameBuffer::Cpu { data } = &decoded.buffer else {
        panic!("cpu output");
    };
    let psnr = luma_psnr(&source_frame(index)[..luma_len], &data[..luma_len]);
    eprintln!("frame {index}: PSNR {psnr:.2} dB");
    assert!(psnr >= MIN_PSNR_DB, "frame {index}: PSNR {psnr:.2} dB");
}

#[test]
fn a_lost_frame_is_recovered_without_a_keyframe() {
    let mut encoder = match H264Encoder::new(EncoderConfig::new(WIDTH, HEIGHT, 30)) {
        Ok(encoder) => encoder,
        Err(reason) => {
            eprintln!("SKIP: no VA-API H.264 encoder: {reason}");
            return;
        }
    };
    let mut decoder = match VaapiDecoder::new() {
        Ok(decoder) => decoder.cpu_output(),
        Err(reason) => {
            eprintln!("SKIP: no VA-API decoder: {reason}");
            return;
        }
    };

    for index in 0..8 {
        let au = encode(&mut encoder, index);
        check(&mut decoder, &au, index);
    }
    // Frames 8 and 9 are lost on the way.
    encode(&mut encoder, 8);
    encode(&mut encoder, 9);
    // Frame 10 predicts from frame 9.
    let au = encode(&mut encoder, 10);
    let error = decoder
        .decode(&au)
        .expect_err("frame 10 predicts from a lost frame");
    assert!(error.contains("frame_num gap"), "{error}");

    // The client reports frame 7 as its last good frame.
    assert!(encoder.recover_from(7), "frame 7 is still a reference");
    let au = encode(&mut encoder, 11);
    assert!(!au.is_keyframe, "the recovery is not an IDR");
    assert!(
        nal::contains_nal(&au.data, nal::NAL_SEI),
        "the recovery point is marked"
    );
    check(&mut decoder, &au, 11);
    for index in 12..16 {
        let au = encode(&mut encoder, index);
        assert!(!nal::contains_nal(&au.data, nal::NAL_SEI));
        check(&mut decoder, &au, index);
    }
    // Frames 8 to 10 were distrusted: they cannot anchor a later recovery.
    assert!(!encoder.recover_from(9));
    assert_eq!(decoder.decode_errors(), 1);
}
