//! Presents synthetic frames through the Wayland backend when a compositor is available.
//!
//! Every test skips with a printed reason when `WAYLAND_DISPLAY` is unset. The dmabuf import test
//! also skips when the compositor advertises no XRGB8888 modifier or no render node can allocate
//! a matching buffer.

use std::fs::{File, OpenOptions};
use std::os::fd::OwnedFd;
use std::thread::sleep;
use std::time::{Duration, Instant};

use lumen_protocol::{
    DRM_FORMAT_MOD_INVALID, DRM_FORMAT_MOD_LINEAR, DmabufImage, DmabufPlane, FrameBuffer,
    FrameSink, PixelFormat, VideoFrame,
};
use lumen_render::WaylandRenderer;
use rustix::fs::{MemfdFlags, ftruncate, memfd_create};

/// Render nodes tried in order for the live import. The AMD iGPU comes first.
const RENDER_NODES: [&str; 2] = ["/dev/dri/renderD129", "/dev/dri/renderD128"];

fn wayland_available(test: &str) -> bool {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        eprintln!("SKIP {test}: WAYLAND_DISPLAY is not set, no Wayland compositor to present to");
        return false;
    }
    true
}

fn bgrx_frame(width: u32, height: u32, index: u64) -> VideoFrame {
    let data = vec![0x40u8; (width * height * 4) as usize];
    VideoFrame::cpu(width, height, PixelFormat::Bgrx, data, 0, index)
}

fn dmabuf_frame(image: DmabufImage, width: u32, height: u32, index: u64) -> VideoFrame {
    VideoFrame {
        width,
        height,
        format: PixelFormat::Bgrx,
        buffer: FrameBuffer::Dmabuf(image),
        capture_ts_us: 0,
        index,
    }
}

fn fourcc_name(fourcc: u32) -> String {
    String::from_utf8_lossy(&fourcc.to_le_bytes()).into_owned()
}

fn modifier_name(modifier: u64) -> String {
    format!("{:?} ({modifier:#018x})", gbm::Modifier::from(modifier))
}

/// An anonymous fd of `len` bytes, standing in for a dmabuf where only the size matters.
fn memfd(len: u64) -> OwnedFd {
    let fd = memfd_create("lumen-test-dmabuf", MemfdFlags::CLOEXEC).expect("memfd_create");
    ftruncate(&fd, len).expect("ftruncate");
    fd
}

#[test]
fn wayland_present_increments_count() {
    if !wayland_available("wayland_present_increments_count") {
        return;
    }
    let mut renderer =
        WaylandRenderer::connect("lumen-render test").expect("connect to WAYLAND_DISPLAY");
    renderer
        .present(&bgrx_frame(320, 240, 0))
        .expect("present BGRX frame");
    assert_eq!(renderer.present_count(), 1);
    renderer
        .present(&bgrx_frame(320, 240, 1))
        .expect("present second BGRX frame");
    assert_eq!(renderer.present_count(), 2);
    // A size change recreates the surface.
    renderer
        .present(&bgrx_frame(160, 120, 2))
        .expect("present resized BGRX frame");
    assert_eq!(renderer.present_count(), 3);

    // Presentation feedback arrives asynchronously; wait briefly for the first timestamp.
    let deadline = Instant::now() + Duration::from_secs(2);
    while renderer.last_present_ts_us().is_none() && Instant::now() < deadline {
        renderer.poll_events().expect("poll Wayland events");
        sleep(Duration::from_millis(10));
    }
    assert!(
        renderer.last_present_ts_us().is_some(),
        "no wp_presentation feedback within 2 s"
    );
}

#[test]
fn wayland_dmabuf_unadvertised_pair_is_refused() {
    if !wayland_available("wayland_dmabuf_unadvertised_pair_is_refused") {
        return;
    }
    let mut renderer =
        WaylandRenderer::connect("lumen-render test").expect("connect to WAYLAND_DISPLAY");
    let advertised = renderer.dmabuf_formats();
    eprintln!(
        "compositor advertised {} dmabuf (fourcc, modifier) pair(s)",
        advertised.len()
    );
    // A modifier value no driver defines, so no compositor advertises it.
    let bogus = 0x0123_4567_89ab_cdef;
    let image = DmabufImage::new(
        memfd(16),
        PixelFormat::Bgrx.drm_fourcc(),
        bogus,
        &[DmabufPlane {
            offset: 0,
            stride: 8,
        }],
    )
    .expect("image");
    let err = renderer
        .present(&dmabuf_frame(image, 2, 2, 0))
        .expect_err("an unadvertised (fourcc, modifier) pair must be refused");
    eprintln!("refused as expected: {err}");
    assert_eq!(
        renderer.present_count(),
        0,
        "a refused frame must not count as presented"
    );
}

/// A gbm buffer kept alive with the device that made it.
struct Allocated {
    _device: gbm::Device<File>,
    bo: gbm::BufferObject<()>,
    node: &'static str,
}

/// Allocates one XRGB8888 buffer on the first render node that can do it. An explicit modifier
/// makes gbm use exactly that modifier. `DRM_FORMAT_MOD_INVALID` allocates with the driver's implicit choice.
fn allocate(width: u32, height: u32, modifier: u64) -> Result<Allocated, String> {
    let mut reasons = Vec::new();
    for node in RENDER_NODES {
        let result = (|| {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(node)
                .map_err(|e| format!("open: {e}"))?;
            let device = gbm::Device::new(file).map_err(|e| format!("gbm device: {e}"))?;
            let bo = if modifier == DRM_FORMAT_MOD_INVALID {
                device.create_buffer_object::<()>(
                    width,
                    height,
                    gbm::Format::Xrgb8888,
                    gbm::BufferObjectFlags::RENDERING,
                )
            } else {
                device.create_buffer_object_with_modifiers::<()>(
                    width,
                    height,
                    gbm::Format::Xrgb8888,
                    std::iter::once(gbm::Modifier::from(modifier)),
                )
            }
            .map_err(|e| format!("allocate {width}x{height} XRGB8888: {e}"))?;
            if bo.plane_count() != 1 {
                return Err(format!(
                    "buffer has {} planes; BGRX frames carry one",
                    bo.plane_count()
                ));
            }
            Ok(Allocated {
                _device: device,
                bo,
                node,
            })
        })();
        match result {
            Ok(allocated) => return Ok(allocated),
            Err(reason) => reasons.push(format!("{node}: {reason}")),
        }
    }
    Err(reasons.join("; "))
}

#[test]
fn wayland_dmabuf_import_live() {
    if !wayland_available("wayland_dmabuf_import_live") {
        return;
    }
    let (width, height) = (320u32, 240u32);
    let mut renderer =
        WaylandRenderer::connect("lumen-render test").expect("connect to WAYLAND_DISPLAY");
    let advertised = renderer.dmabuf_formats();
    eprintln!(
        "compositor advertised {} dmabuf (fourcc, modifier) pair(s):",
        advertised.len()
    );
    for (fourcc, modifier) in &advertised {
        eprintln!("  {} {}", fourcc_name(*fourcc), modifier_name(*modifier));
    }

    let xr24 = PixelFormat::Bgrx.drm_fourcc();
    let mut modifiers: Vec<u64> = advertised
        .iter()
        .filter(|(fourcc, _)| *fourcc == xr24)
        .map(|(_, modifier)| *modifier)
        .collect();
    if modifiers.is_empty() {
        eprintln!(
            "SKIP wayland_dmabuf_import_live: compositor advertises no XR24 dmabuf modifiers"
        );
        return;
    }
    // Prefer linear, then explicit modifiers, then the implicit one.
    modifiers.sort_by_key(|m| match *m {
        DRM_FORMAT_MOD_LINEAR => 0,
        DRM_FORMAT_MOD_INVALID => 2,
        _ => 1,
    });
    let chosen = modifiers[0];
    eprintln!("importing XR24 with modifier {}", modifier_name(chosen));

    let mut frames = 0u64;
    for index in 0..2u64 {
        let allocated = match allocate(width, height, chosen) {
            Ok(a) => a,
            Err(reason) => {
                eprintln!(
                    "SKIP wayland_dmabuf_import_live: no render node could allocate: {reason}"
                );
                return;
            }
        };
        // Implicit buffers go out as INVALID; the compositor derives the layout from the dmabuf.
        let image_modifier = if chosen == DRM_FORMAT_MOD_INVALID {
            DRM_FORMAT_MOD_INVALID
        } else {
            let got = u64::from(allocated.bo.modifier());
            assert_eq!(
                got, chosen,
                "gbm returned a different modifier than requested"
            );
            got
        };
        let stride = allocated.bo.stride();
        let offset = allocated.bo.offset(0);
        let fd = allocated.bo.fd().expect("gbm export dmabuf fd");
        let image = DmabufImage::new(fd, xr24, image_modifier, &[DmabufPlane { offset, stride }])
            .expect("image");
        eprintln!(
            "imported on {}: fourcc {} modifier {} stride {} offset {} buffer {} bytes",
            allocated.node,
            fourcc_name(xr24),
            modifier_name(image_modifier),
            stride,
            offset,
            allocated.bo.stride() * allocated.bo.height(),
        );
        renderer
            .present(&dmabuf_frame(image, width, height, index))
            .expect("present dmabuf frame");
        frames += 1;
        assert_eq!(renderer.present_count(), frames);
    }
    assert_eq!(renderer.present_count(), 2);
}
