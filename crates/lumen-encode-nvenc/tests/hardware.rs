//! Encodes on the NVIDIA GPU, when there is one, and checks the stream.
//!
//! Skips with a printed reason without NVENC. With `ffmpeg` on the path the stream is decoded
//! and compared with the source.

use std::io::Write;
use std::process::{Command, Stdio};

use lumen_encode_nvenc::NvencEncoder;
use lumen_encode_vaapi::{EncoderConfig, RateControl};
use lumen_protocol::media::{PixelFormat, VideoEncoder, VideoFrame};

const W: u32 = 640;
const H: u32 = 360;
const FRAMES: u64 = 30;

/// A gradient that moves two pixels per frame, as BGRX. An even step keeps the 4:2:0 chroma
/// samples aligned, so the PSNR measures the encoder and not the subsampling.
fn picture(index: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity((W * H * 4) as usize);
    for y in 0..H {
        for x in 0..W {
            let v = ((u64::from(x) + 2 * index) % 256) as u8;
            data.extend_from_slice(&[v, (y % 256) as u8, 128, 0]);
        }
    }
    data
}

#[test]
fn nvenc_encodes_a_decodable_stream() {
    let config = EncoderConfig::new(W, H, 30).with_rate_control(RateControl::Cqp { qp: 20 });
    let mut encoder = match NvencEncoder::new(config) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("skipping: {e}");
            return;
        }
    };
    let mut stream = Vec::new();
    for index in 0..FRAMES {
        if index == 20 {
            encoder.request_keyframe();
        }
        let frame = VideoFrame::cpu(
            W,
            H,
            PixelFormat::Bgrx,
            picture(index),
            index * 33_333,
            index,
        );
        let out = encoder.encode(&frame).expect("encode");
        assert_eq!(out.is_keyframe, index == 0 || index == 20, "frame {index}");
        assert_eq!(out.data[..4], [0, 0, 0, 1], "Annex B start code");
        stream.extend_from_slice(&out.data);
    }
    // The rate changes mid-stream without a keyframe.
    encoder
        .set_rate_control(RateControl::Cbr {
            bits_per_second: 2_000_000,
        })
        .expect("set the rate");
    let frame = VideoFrame::cpu(W, H, PixelFormat::Bgrx, picture(FRAMES), 0, FRAMES);
    assert!(
        !encoder
            .encode(&frame)
            .expect("encode after the rate change")
            .is_keyframe
    );

    if Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("no ffmpeg: the stream is not decoded");
        return;
    }
    let mut ffmpeg = Command::new("ffmpeg")
        .args([
            "-v", "error", "-f", "h264", "-i", "-", "-f", "rawvideo", "-pix_fmt", "bgr0", "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("ffmpeg");
    ffmpeg.stdin.take().unwrap().write_all(&stream).unwrap();
    let out = ffmpeg.wait_with_output().unwrap();
    let frame_len = (W * H * 4) as usize;
    assert_eq!(
        out.stdout.len(),
        frame_len * FRAMES as usize,
        "every frame decodes"
    );
    for (index, decoded) in out.stdout.chunks(frame_len).enumerate() {
        let source = picture(index as u64);
        // PSNR over the first channel (the moving gradient).
        let mse: f64 = source
            .iter()
            .zip(decoded)
            .step_by(4)
            .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
            .sum::<f64>()
            / f64::from(W * H);
        let psnr = 10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10();
        assert!(psnr > 35.0, "frame {index}: PSNR {psnr:.2} dB");
    }
}
