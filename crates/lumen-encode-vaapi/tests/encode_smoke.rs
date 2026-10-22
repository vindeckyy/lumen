//! Hardware smoke test: encode synthetic 1280x720 NV12 frames on the preferred VA-API device.
//!
//! Skips with a printed reason when no VA-API display or no H.264 encode entry point exists.
//! Any failure after that point is a real failure.

use lumen_encode_vaapi::{EncoderConfig, H264Encoder, RateControl};
use lumen_ffi_va::{VaDisplay, sys};
use lumen_protocol::media::{EncodedFrame, PixelFormat, VideoEncoder, VideoFrame};

const W: u32 = 1280;
const H: u32 = 720;

/// Why hardware encode is unavailable, or `None` when the test can run.
fn skip_reason() -> Option<String> {
    let display = match VaDisplay::open_preferred() {
        Ok(d) => d,
        Err(e) => return Some(format!("no VA-API display: {e}")),
    };
    let h264 = [
        sys::VAProfileH264High,
        sys::VAProfileH264Main,
        sys::VAProfileH264ConstrainedBaseline,
    ];
    if h264
        .iter()
        .any(|&p| display.supports(p, sys::VAEntrypointEncSlice))
    {
        None
    } else {
        Some(format!(
            "{}: no H.264 VAEntrypointEncSlice profile",
            display.vendor()
        ))
    }
}

/// Synthetic NV12 frame: a moving diagonal gradient, so the encoder has real content.
fn synthetic(index: u64) -> VideoFrame {
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
    VideoFrame::cpu(W, H, PixelFormat::Nv12, data, 1_000 * index, index)
}

/// Checks the output is Annex-B and contains an IDR slice (nal_unit_type 5).
fn assert_annex_b_with_idr(out: &EncodedFrame) {
    let d = &out.data;
    assert_ne!(d.len(), 0, "encoder returned no bytes");
    assert!(
        d.starts_with(&[0, 0, 0, 1]) || d.starts_with(&[0, 0, 1]),
        "output does not start with an Annex-B start code: {:02x?}",
        &d[..d.len().min(8)]
    );
    let idr_after_3 = d
        .windows(4)
        .any(|w| w[..3] == [0, 0, 1] && (w[3] & 0x1f) == 5);
    let idr_after_4 = d
        .windows(5)
        .any(|w| w[..4] == [0, 0, 0, 1] && (w[4] & 0x1f) == 5);
    assert!(
        idr_after_3 || idr_after_4,
        "no IDR (nal_unit_type 5) in output"
    );
}

fn run_session(rc: RateControl) {
    if let Some(reason) = skip_reason() {
        eprintln!("SKIP encode_smoke ({rc:?}): {reason}");
        return;
    }
    let cfg = EncoderConfig::new(W, H, 60).with_rate_control(rc);
    let mut enc = H264Encoder::new(cfg).expect("H.264 encoder setup on the detected device");

    let idr = enc.encode(&synthetic(0)).expect("first frame (IDR)");
    assert_annex_b_with_idr(&idr);
    assert!(idr.is_keyframe, "first frame must be a keyframe");
    assert_eq!((idr.index, idr.capture_ts_us), (0, 0));

    let p = enc.encode(&synthetic(1)).expect("P frame");
    assert_ne!(p.data.len(), 0, "P frame returned no bytes");
    assert!(!p.is_keyframe, "second frame should be a P frame");
    assert_eq!(p.index, 1);

    enc.request_keyframe();
    let forced = enc.encode(&synthetic(2)).expect("forced IDR");
    assert_annex_b_with_idr(&forced);
    assert!(forced.is_keyframe, "request_keyframe must yield an IDR");
    assert_eq!(enc.frames_encoded(), 3);
}

#[test]
fn encodes_idr_p_and_forced_idr_cqp() {
    run_session(RateControl::Cqp { qp: 22 });
}

#[test]
fn encodes_idr_p_and_forced_idr_cbr() {
    run_session(RateControl::Cbr {
        bits_per_second: 8_000_000,
    });
}

/// The rate control can be changed while the encoder runs, which is how the
/// bandwidth policy pushes a new target bitrate.
#[test]
fn the_rate_control_can_change_while_running() {
    if let Some(reason) = skip_reason() {
        eprintln!("SKIP encode_smoke (rate control change): {reason}");
        return;
    }
    let cfg = EncoderConfig::new(W, H, 60).with_rate_control(RateControl::Cqp { qp: 22 });
    let mut enc = H264Encoder::new(cfg).expect("H.264 encoder setup on the detected device");
    assert_eq!(enc.rate_control(), RateControl::Cqp { qp: 22 });

    let idr = enc.encode(&synthetic(0)).expect("first frame");
    assert_annex_b_with_idr(&idr);

    enc.set_rate_control(RateControl::Cbr {
        bits_per_second: 4_000_000,
    })
    .expect("switch to constant bitrate");
    assert_eq!(
        enc.rate_control(),
        RateControl::Cbr {
            bits_per_second: 4_000_000
        }
    );

    // The encoder keeps working after the change.
    let p = enc
        .encode(&synthetic(1))
        .expect("frame after the rate control change");
    assert_ne!(p.data.len(), 0);

    // And back again.
    enc.set_rate_control(RateControl::Cqp { qp: 30 })
        .expect("switch back to a constant quantiser");
    assert_eq!(enc.rate_control(), RateControl::Cqp { qp: 30 });
    let p = enc
        .encode(&synthetic(2))
        .expect("frame after the second change");
    assert_ne!(p.data.len(), 0);
}
