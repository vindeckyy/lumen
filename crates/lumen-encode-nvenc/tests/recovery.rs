//! Loss recovery without a keyframe: NVENC encodes, the VA-API decoder of the other GPU
//! decodes, and two frames are lost on the way.

use lumen_decode_vaapi::VaapiDecoder;
use lumen_encode_nvenc::NvencEncoder;
use lumen_encode_vaapi::{EncoderConfig, nal};
use lumen_protocol::media::{
    EncodedFrame, FrameBuffer, PixelFormat, VideoDecoder, VideoEncoder, VideoFrame,
};

const W: u32 = 640;
const H: u32 = 360;

/// NV12 with a luma ramp that moves 4 pixels per frame and flat chroma.
fn source(index: u64) -> Vec<u8> {
    let (w, h) = (W as usize, H as usize);
    let mut data = vec![128u8; PixelFormat::Nv12.frame_len(W, H)];
    for y in 0..h {
        for x in 0..w {
            data[y * w + x] = 16 + ((x + y + index as usize * 4) % 200) as u8;
        }
    }
    data
}

fn encode(encoder: &mut NvencEncoder, index: u64) -> EncodedFrame {
    let frame = VideoFrame::cpu(W, H, PixelFormat::Nv12, source(index), 0, index);
    encoder.encode(&frame).expect("encode")
}

fn check(decoder: &mut VaapiDecoder, au: &EncodedFrame, index: u64) {
    let decoded = decoder
        .decode(au)
        .unwrap_or_else(|e| panic!("decode frame {index}: {e}"))
        .expect("a picture");
    let FrameBuffer::Cpu { data } = &decoded.buffer else {
        panic!("cpu output");
    };
    let luma = (W * H) as usize;
    let mse = source(index)[..luma]
        .iter()
        .zip(&data[..luma])
        .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
        .sum::<f64>()
        / luma as f64;
    let psnr = 10.0 * (255.0f64.powi(2) / mse.max(1e-9)).log10();
    assert!(psnr > 35.0, "frame {index}: PSNR {psnr:.2} dB");
}

#[test]
fn nvenc_recovers_a_loss_without_a_keyframe() {
    let mut encoder = match NvencEncoder::new(EncoderConfig::new(W, H, 30)) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };
    let mut decoder = match VaapiDecoder::new() {
        Ok(d) => d.cpu_output(),
        Err(e) => {
            eprintln!("skipping: no VA-API decoder: {e}");
            return;
        }
    };
    for index in 0..8 {
        let au = encode(&mut encoder, index);
        check(&mut decoder, &au, index);
    }
    encode(&mut encoder, 8);
    encode(&mut encoder, 9);
    let au = encode(&mut encoder, 10);
    assert!(
        decoder.decode(&au).is_err(),
        "frame 10 predicts from a lost frame"
    );

    assert!(encoder.recover_from(7));
    let au = encode(&mut encoder, 11);
    assert!(!au.is_keyframe, "the recovery is not an IDR");
    assert!(nal::contains_nal(&au.data, nal::NAL_SEI));
    check(&mut decoder, &au, 11);
    for index in 12..16 {
        let au = encode(&mut encoder, index);
        check(&mut decoder, &au, index);
    }
    assert!(!encoder.recover_from(9), "invalidated frames cannot anchor");
}
