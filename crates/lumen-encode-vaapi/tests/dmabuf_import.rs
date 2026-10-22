//! Encodes one frame from a DMA-BUF that VA-API itself exported.
//!
//! The surface is allocated by libva, so its layout and DRM modifier are whatever the driver
//! chose. That is the case that matters for zero-copy: a tiled modifier, if the driver uses one.
//! The test prints the exported fourcc and modifier. The encoder must either import the buffer
//! and produce an IDR, or reject it with an error that names the fourcc and modifier.
//!
//! Skips with a printed reason when there is no VA-API display or no H.264 encode entry point.

use lumen_encode_vaapi::{EncoderConfig, H264Encoder};
use lumen_ffi_va::{OwnedVaSurface, VaDisplay, sys};
use lumen_protocol::media::{
    DmabufImage, DmabufPlane, FrameBuffer, PixelFormat, VideoEncoder, VideoFrame,
};

const W: u32 = 1280;
const H: u32 = 720;

#[test]
fn encodes_from_va_exported_dmabuf() {
    let display = match VaDisplay::open_preferred() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("SKIP dmabuf_import: no VA-API display: {e}");
            return;
        }
    };
    let has_h264 = [
        sys::VAProfileH264High,
        sys::VAProfileH264Main,
        sys::VAProfileH264ConstrainedBaseline,
    ]
    .iter()
    .any(|&p| display.supports(p, sys::VAEntrypointEncSlice));
    if !has_h264 {
        eprintln!(
            "SKIP dmabuf_import: {}: no H.264 VAEntrypointEncSlice profile",
            display.vendor()
        );
        return;
    }

    let surface = OwnedVaSurface::create(&display, PixelFormat::Nv12, W, H)
        .expect("allocate an NV12 surface");
    let export = surface.export_dmabuf().expect("export surface as DMA-BUF");
    println!(
        "exported DMA-BUF: fourcc {:#010x}, modifier {:#018x}, size {} bytes, planes {:?}",
        export.drm_format, export.modifier, export.size, export.planes
    );
    let planes: Vec<DmabufPlane> = export
        .planes
        .iter()
        .map(|p| DmabufPlane {
            offset: p.offset,
            stride: p.pitch,
        })
        .collect();
    let (fourcc, modifier) = (export.drm_format, export.modifier);
    let image =
        DmabufImage::new(export.fd, fourcc, modifier, &planes).expect("describe exported DMA-BUF");
    let frame = VideoFrame {
        width: W,
        height: H,
        format: PixelFormat::Nv12,
        buffer: FrameBuffer::Dmabuf(image),
        capture_ts_us: 0,
        index: 0,
    };

    let mut enc = H264Encoder::new(EncoderConfig::new(W, H, 60)).expect("H.264 encoder setup");
    match enc.encode(&frame) {
        Ok(out) => {
            assert!(out.is_keyframe, "first frame must be an IDR");
            assert!(
                out.data.starts_with(&[0, 0, 0, 1]) || out.data.starts_with(&[0, 0, 1]),
                "output is not Annex-B"
            );
            println!(
                "imported fourcc {fourcc:#010x} modifier {modifier:#018x}: encoded {} bytes, IDR",
                out.data.len()
            );
        }
        Err(e) => {
            assert!(
                e.contains("fourcc") && e.contains("modifier"),
                "rejection must name the fourcc and modifier: {e}"
            );
            println!("import rejected for fourcc {fourcc:#010x} modifier {modifier:#018x}: {e}");
        }
    }
}
