//! Captures one frame from the running wlroots compositor.
//!
//! Skips with a printed reason when no Wayland display is present.

use std::thread;
use std::time::{Duration, Instant};

use lumen_capture_wlr::{DRM_FORMAT_MOD_INVALID, StreamConfig, WlrCapture, list_outputs};
use lumen_protocol::media::{
    DRM_FORMAT_MOD_LINEAR, DmabufImage, FrameBuffer, FrameSource, PixelFormat, drm_fourcc,
};

#[test]
fn captures_a_frame_from_the_first_output() {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        eprintln!(
            "SKIP lumen-capture-wlr: WAYLAND_DISPLAY is unset; no Wayland compositor to capture from"
        );
        return;
    }
    let outputs = list_outputs().expect("list outputs from the running compositor");
    let out = outputs
        .first()
        .expect("compositor advertised no output with a current mode");
    // Plain copy path: requested rate must stay below refresh / 1.5 so the frame is not damage-gated.
    let fps = if out.refresh_mhz == 0 {
        30
    } else {
        (out.refresh_mhz / 1000).max(1)
    };
    let mut cap = WlrCapture::open(&StreamConfig {
        output: out.name.clone(),
        width: out.width,
        height: out.height,
        fps,
        prefer_cpu: false,
    })
    .expect("open screencopy capture");

    let deadline = Instant::now() + Duration::from_secs(5);
    let frame = loop {
        if let Some(f) = cap.poll_frame() {
            break f;
        }
        assert!(
            cap.take_error().is_none(),
            "capture error before first frame"
        );
        assert!(Instant::now() < deadline, "no frame within 5 s");
        thread::sleep(Duration::from_millis(1));
    };

    assert_eq!((frame.width, frame.height), (out.width, out.height));
    assert!(matches!(
        frame.format,
        PixelFormat::Bgrx | PixelFormat::Rgba
    ));
    let layout = match &frame.buffer {
        FrameBuffer::Cpu { data } => {
            assert_eq!(data.len(), out.width as usize * out.height as usize * 4);
            assert!(data.iter().any(|&b| b != 0), "frame is all zero bytes");
            "cpu (wl_shm)".to_owned()
        }
        FrameBuffer::Dmabuf(img) => {
            let (fourcc, modifier, planes) = check_dmabuf(img, out.width, out.height);
            format!("dmabuf fourcc {fourcc} modifier {modifier:#018x} planes {planes}")
        }
    };
    assert!(frame.capture_ts_us > 0);
    eprintln!(
        "lumen-capture-wlr: output {:?} {}x{}, dmabuf={}, damage_copy={}, frame format {:?}, {layout}",
        out.name,
        out.width,
        out.height,
        cap.uses_dmabuf(),
        cap.uses_damage_copy(),
        frame.format
    );
}

#[test]
fn damage_copy_path_delivers_a_frame() {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        eprintln!(
            "SKIP lumen-capture-wlr damage path: WAYLAND_DISPLAY is unset; no Wayland compositor"
        );
        return;
    }
    let outputs = list_outputs().expect("list outputs from the running compositor");
    let out = outputs
        .first()
        .expect("compositor advertised no output with a current mode");
    if out.refresh_mhz == 0 {
        eprintln!(
            "SKIP lumen-capture-wlr damage path: output reports unknown refresh (headless sway does)"
        );
        return;
    }
    // Stream at half the refresh: copy_with_damage is selected (refresh >= 1.5x fps).
    let fps = (out.refresh_mhz / 2000).max(1);
    let mut cap = WlrCapture::open(&StreamConfig {
        output: out.name.clone(),
        width: out.width,
        height: out.height,
        fps,
        prefer_cpu: false,
    })
    .expect("open screencopy capture");
    assert!(
        cap.uses_damage_copy(),
        "refresh {} mHz should select copy_with_damage at {fps} fps",
        out.refresh_mhz
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if cap.poll_frame().is_some() {
            break;
        }
        assert!(
            cap.take_error().is_none(),
            "capture error on the damage path"
        );
        assert!(
            Instant::now() < deadline,
            "no damage-gated frame within 5 s"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

/// Checks a DMA-BUF frame's image against what screencopy and gbm should produce.
///
/// Returns the fourcc text, modifier and plane count for the caller to report.
fn check_dmabuf(img: &DmabufImage, width: u32, height: u32) -> (String, u64, usize) {
    let fourcc = String::from_utf8_lossy(&img.fourcc.to_le_bytes()).into_owned();
    assert!(
        img.fourcc == drm_fourcc(b'X', b'R', b'2', b'4')
            || img.fourcc == drm_fourcc(b'A', b'R', b'2', b'4'),
        "unexpected fourcc {fourcc}"
    );
    // The allocation asks for an explicit LINEAR modifier list. INVALID would mean the driver fell back to an implicit layout.
    assert!(
        img.modifier == DRM_FORMAT_MOD_LINEAR || img.modifier == DRM_FORMAT_MOD_INVALID,
        "unexpected modifier {:#018x}",
        img.modifier
    );
    let [plane] = img.planes() else {
        panic!(
            "RGB screencopy buffers are single-plane, got {} planes",
            img.planes().len()
        );
    };
    assert!(
        plane.stride >= width * 4,
        "stride {} smaller than a row",
        plane.stride
    );
    let size = rustix::fs::fstat(&img.fd).expect("fstat dmabuf").st_size;
    let needed = u64::from(plane.offset) + u64::from(plane.stride) * u64::from(height);
    assert!(
        u64::try_from(size).is_ok_and(|s| s >= needed),
        "dmabuf is {size} bytes but its plane needs {needed}"
    );
    (fourcc, img.modifier, img.planes().len())
}
