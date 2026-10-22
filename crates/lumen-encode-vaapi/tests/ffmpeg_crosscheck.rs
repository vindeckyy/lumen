//! Cross-checks the encoder's IPPP stream with an independent decoder (ffmpeg).
//!
//! Encodes 30 frames (one IDR, then 29 P) of a moving 1280x720 NV12 pattern on the preferred
//! VA-API device. The Annex-B stream and the source frames are written to the temp directory,
//! ffmpeg decodes the stream to raw NV12, and luma is compared frame by frame.
//!
//! Skips with a printed reason when there is no VA-API display, no H.264 encode entry point,
//! or no `ffmpeg` on PATH. Any failure after that point is a real failure.

use std::{fs, process::Command};

use lumen_encode_vaapi::{EncoderConfig, H264Encoder};
use lumen_ffi_va::{VaDisplay, sys};
use lumen_protocol::media::{PixelFormat, VideoEncoder, VideoFrame};

const W: u32 = 1280;
const H: u32 = 720;
const FRAMES: u64 = 30;
/// Floor for per-frame luma PSNR. A decode that loses a reference or misparses a slice falls far
/// below this. CQP 22 on this pattern measures about 48 dB.
const MIN_PSNR_DB: f64 = 35.0;

/// Why the cross-check cannot run here, or `None` when it can.
fn skip_reason() -> Option<String> {
    let display = match VaDisplay::open_preferred() {
        Ok(d) => d,
        Err(e) => return Some(format!("no VA-API display: {e}")),
    };
    let has_h264 = [
        sys::VAProfileH264High,
        sys::VAProfileH264Main,
        sys::VAProfileH264ConstrainedBaseline,
    ]
    .iter()
    .any(|&p| display.supports(p, sys::VAEntrypointEncSlice));
    if !has_h264 {
        return Some(format!(
            "{}: no H.264 VAEntrypointEncSlice profile",
            display.vendor()
        ));
    }
    match Command::new("ffmpeg").arg("-version").output() {
        Ok(out) if out.status.success() => None,
        Ok(out) => Some(format!("ffmpeg -version failed: {}", out.status)),
        Err(e) => Some(format!("ffmpeg not found on PATH: {e}")),
    }
}

/// Moving NV12 pattern: a diagonal luma gradient that shifts 3 px per frame, so every P frame
/// has real motion to predict.
fn synthetic(index: u64) -> Vec<u8> {
    let (w, h) = (W as usize, H as usize);
    let shift = index as usize * 3;
    let mut data = Vec::with_capacity(PixelFormat::Nv12.frame_len(W, H));
    for y in 0..h {
        for x in 0..w {
            data.push(((x + y + shift) & 0xff) as u8);
        }
    }
    for _ in 0..h / 2 {
        for x in 0..w / 2 {
            let c = (x & 0x1f) as u8;
            data.push(128 + c);
            data.push(128 + c / 2);
        }
    }
    data
}

/// Luma PSNR in dB between two equally sized planes. Identical planes report 100 dB.
fn luma_psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len(), "plane sizes differ");
    let sse: u64 = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| {
            let d = u64::from(x.abs_diff(y));
            d * d
        })
        .sum();
    if sse == 0 {
        return 100.0;
    }
    let mse = sse as f64 / a.len() as f64;
    10.0 * (255.0_f64 * 255.0 / mse).log10()
}

#[test]
fn ipppp_stream_decodes_with_ffmpeg() {
    if let Some(reason) = skip_reason() {
        eprintln!("SKIP ffmpeg_crosscheck: {reason}");
        return;
    }
    let dir = std::env::temp_dir();
    let stream_path = dir.join("lumen-encode-ipppp-1280x720.h264");
    let source_path = dir.join("lumen-encode-ipppp-1280x720-source.yuv");
    let decoded_path = dir.join("lumen-encode-ipppp-1280x720-decoded.yuv");

    let frame_bytes = PixelFormat::Nv12.frame_len(W, H);
    let luma_bytes = (W * H) as usize;
    let mut enc = H264Encoder::new(EncoderConfig::new(W, H, 60)).expect("H.264 encoder setup");
    let mut stream = Vec::new();
    let mut source = Vec::with_capacity(frame_bytes * FRAMES as usize);
    for i in 0..FRAMES {
        let data = synthetic(i);
        source.extend_from_slice(&data);
        let frame = VideoFrame::cpu(W, H, PixelFormat::Nv12, data, i * 16_667, i);
        let out = enc
            .encode(&frame)
            .unwrap_or_else(|e| panic!("encode frame {i}: {e}"));
        assert_eq!(out.is_keyframe, i == 0, "frame {i}: keyframe flag");
        stream.extend_from_slice(&out.data);
    }
    fs::write(&stream_path, &stream).expect("write Annex-B stream");
    fs::write(&source_path, &source).expect("write source frames");
    let _ = fs::remove_file(&decoded_path);

    let ffmpeg = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(&stream_path)
        .args(["-f", "rawvideo", "-pix_fmt", "nv12"])
        .arg(&decoded_path)
        .output()
        .expect("run ffmpeg");
    let stderr = String::from_utf8_lossy(&ffmpeg.stderr);
    assert!(
        ffmpeg.status.success() && stderr.trim().is_empty(),
        "ffmpeg reported an error ({}): {stderr}",
        ffmpeg.status
    );

    let decoded = fs::read(&decoded_path).expect("read ffmpeg output");
    let expected = frame_bytes * FRAMES as usize;
    assert_eq!(
        decoded.len(),
        expected,
        "ffmpeg output is not {FRAMES} NV12 frames"
    );

    let mut psnr = Vec::with_capacity(FRAMES as usize);
    for i in 0..FRAMES as usize {
        let off = i * frame_bytes;
        let p = luma_psnr(
            &decoded[off..off + luma_bytes],
            &source[off..off + luma_bytes],
        );
        eprintln!("frame {i:2}: luma PSNR {p:.2} dB");
        psnr.push(p);
    }
    let min = psnr.iter().copied().fold(f64::INFINITY, f64::min);
    let mean = psnr.iter().sum::<f64>() / psnr.len() as f64;
    eprintln!("ffmpeg luma PSNR over {FRAMES} frames: min {min:.2} dB, mean {mean:.2} dB");
    assert!(
        min >= MIN_PSNR_DB,
        "worst frame {min:.2} dB is below the {MIN_PSNR_DB} dB floor"
    );
}
