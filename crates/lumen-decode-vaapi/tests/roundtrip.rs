//! Encoder to decoder round trip on the live VA-API device.
//!
//! `lumen-encode-vaapi` encodes 30 frames of a synthetic 1280x720 moving pattern (one IDR and
//! 29 P pictures), and `VaapiDecoder` decodes every access unit. Each decoded luma plane is
//! compared with its source.

use lumen_decode_vaapi::VaapiDecoder;
use lumen_encode_vaapi::{EncoderConfig, H264Encoder};
use lumen_protocol::media::{FrameBuffer, PixelFormat, VideoDecoder, VideoEncoder, VideoFrame};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const FRAMES: u64 = 30;
/// Minimum luma PSNR of every decoded frame against its source, in dB.
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

#[test]
fn encoder_output_decodes_frame_for_frame() {
    let mut encoder = match H264Encoder::new(EncoderConfig::new(WIDTH, HEIGHT, 30)) {
        Ok(encoder) => encoder,
        Err(reason) => {
            eprintln!(
                "SKIP encoder_output_decodes_frame_for_frame: no VA-API H.264 encoder: {reason}"
            );
            return;
        }
    };
    let mut decoder = match VaapiDecoder::new() {
        Ok(decoder) => decoder.cpu_output(),
        Err(reason) => {
            eprintln!("SKIP encoder_output_decodes_frame_for_frame: no VA-API decoder: {reason}");
            return;
        }
    };

    let luma_len = (WIDTH * HEIGHT) as usize;
    let mut psnrs = Vec::with_capacity(FRAMES as usize);
    for index in 0..FRAMES {
        let source = source_frame(index);
        let frame = VideoFrame::cpu(
            WIDTH,
            HEIGHT,
            PixelFormat::Nv12,
            source.clone(),
            index * 33_333,
            index,
        );
        let access_unit = encoder
            .encode(&frame)
            .unwrap_or_else(|e| panic!("encode frame {index}: {e}"));
        let decoded = decoder
            .decode(&access_unit)
            .unwrap_or_else(|e| panic!("decode frame {index}: {e}"))
            .unwrap_or_else(|| panic!("frame {index}: access unit produced no picture"));
        assert_eq!(
            (decoded.width, decoded.height),
            (WIDTH, HEIGHT),
            "frame {index} has the wrong size"
        );
        assert_eq!(decoded.index, index, "frame {index} lost its index");
        let FrameBuffer::Cpu { data } = &decoded.buffer else {
            panic!("frame {index}: cpu_output must yield CPU NV12");
        };
        assert_eq!(data.len(), PixelFormat::Nv12.frame_len(WIDTH, HEIGHT));
        psnrs.push(luma_psnr(&source[..luma_len], &data[..luma_len]));
    }

    assert_eq!(psnrs.len() as u64, FRAMES, "decoded frame count");
    assert_eq!(decoder.frames_decoded(), FRAMES, "decoder frame count");
    assert_eq!(decoder.decode_errors(), 0, "decode errors");
    let (worst, min) = psnrs
        .iter()
        .copied()
        .enumerate()
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .expect("at least one frame");
    let mean = psnrs.iter().sum::<f64>() / psnrs.len() as f64;
    eprintln!(
        "roundtrip {FRAMES} frames {WIDTH}x{HEIGHT}: luma PSNR min {min:.2} dB (frame {worst}), mean {mean:.2} dB"
    );
    assert!(
        min >= MIN_PSNR_DB,
        "frame {worst} luma PSNR {min:.2} dB is below {MIN_PSNR_DB} dB"
    );
}
