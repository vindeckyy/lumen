//! Opens the display named by `$DISPLAY` and captures one frame.
//!
//! Skips with a printed reason when `DISPLAY` is unset. Set `LUMEN_X11_TEST_BACKEND` to `dri3`,
//! `shm` or `get` to force one rung; the default walks the ladder. A DRI3 frame is checked for its
//! DMA-BUF geometry, not its pixels, because the DRI3 copy is not fenced (see `dri3.rs`).

use std::thread;
use std::time::{Duration, Instant};

use lumen_capture_x11::{Backend, CaptureConfig, X11Capture};
use lumen_protocol::media::{
    DRM_FORMAT_MOD_LINEAR, DmabufImage, FrameBuffer, FrameSource, PixelFormat, drm_fourcc,
};

/// `DRM_FORMAT_MOD_INVALID`: the driver chose an implicit layout, reported unchanged.
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

#[test]
fn captures_one_frame_from_display_env() {
    let Ok(display) = std::env::var("DISPLAY") else {
        eprintln!("SKIP lumen-capture-x11 env_display: DISPLAY is unset");
        return;
    };
    let forced = match std::env::var("LUMEN_X11_TEST_BACKEND").ok().as_deref() {
        None => None,
        Some("dri3") => Some(Backend::Dri3),
        Some("shm") => Some(Backend::Shm),
        Some("get") => Some(Backend::GetImage),
        Some(other) => panic!("LUMEN_X11_TEST_BACKEND={other}: expected dri3, shm or get"),
    };
    let mut cap = X11Capture::open(&CaptureConfig {
        display: None,
        region: None,
        fps: 30,
        backend: forced,
    })
    .expect("open capture on $DISPLAY");
    if let Some(rung) = forced {
        assert_eq!(cap.backend(), rung, "the forced rung was not used");
    }
    let root = cap.root_geometry();
    eprintln!(
        "lumen-capture-x11: DISPLAY={display} root {}x{} backend {:?}",
        root.width,
        root.height,
        cap.backend()
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    let frame = loop {
        if let Some(f) = cap.poll_frame() {
            break f;
        }
        assert!(Instant::now() < deadline, "no frame within 10 s");
        thread::sleep(Duration::from_millis(2));
    };
    assert_eq!((frame.width, frame.height), (root.width, root.height));
    assert_eq!(frame.format, PixelFormat::Bgrx);
    let layout = match &frame.buffer {
        FrameBuffer::Cpu { data } => {
            assert_eq!(
                data.len(),
                frame.format.frame_len(frame.width, frame.height)
            );
            "cpu (MIT-SHM or GetImage)".to_owned()
        }
        FrameBuffer::Dmabuf(img) => check_dmabuf(img, frame.width, frame.height),
    };
    assert!(cap.take_error().is_none(), "capture reported an error");
    eprintln!(
        "lumen-capture-x11: frame {}x{} {layout}",
        frame.width, frame.height
    );
}

/// Checks a DRI3 image's geometry against its fd and returns a one-line description for the log.
fn check_dmabuf(img: &DmabufImage, width: u32, height: u32) -> String {
    assert_eq!(
        img.fourcc,
        drm_fourcc(b'X', b'R', b'2', b'4'),
        "DRI3 allocates XRGB8888"
    );
    assert!(
        img.modifier == DRM_FORMAT_MOD_LINEAR || img.modifier == DRM_FORMAT_MOD_INVALID,
        "unexpected modifier {:#018x}",
        img.modifier
    );
    let [plane] = img.planes() else {
        panic!("expected one plane, got {}", img.planes().len());
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
    format!(
        "dmabuf fourcc XR24 modifier {:#018x} planes {}",
        img.modifier,
        img.planes().len()
    )
}
