//! Hardware smoke tests. Each prints a SKIP reason and passes when libva or a usable
//! render node is missing.

use std::{io::ErrorKind, os::fd::AsFd};

use lumen_ffi_va::{DmabufImport, OwnedVaSurface, VaDisplay, VaError, VaLibrary, read_dmabuf};
use lumen_protocol::media::PixelFormat;

fn display_or_skip(test: &str) -> Option<VaDisplay> {
    if let Err(e) = VaLibrary::load() {
        eprintln!("SKIP {test}: {e}");
        return None;
    }
    match VaDisplay::open_preferred() {
        Ok(display) => Some(display),
        Err(e) => {
            eprintln!("SKIP {test}: {e}");
            None
        }
    }
}

#[test]
fn vainitialize_on_render_node() {
    let Some(display) = display_or_skip("vainitialize_on_render_node") else {
        return;
    };
    eprintln!(
        "vaInitialize succeeded: version {:?}, vendor {:?}, profiles {:?}",
        display.version(),
        display.vendor(),
        display.profiles()
    );
}

#[test]
fn image_readback_and_dmabuf_round_trip() {
    let Some(display) = display_or_skip("image_readback_and_dmabuf_round_trip") else {
        return;
    };
    let surface = OwnedVaSurface::create(&display, PixelFormat::Nv12, 256, 128)
        .expect("allocate NV12 surface");

    match surface.derive_image() {
        Ok(mut image) => {
            let info = *image.info();
            let mapped = image.map();
            eprintln!(
                "derive_image ok: pitches {:?} offsets {:?} data_size {}, map {}",
                info.pitches,
                info.offsets,
                info.data_size,
                if mapped.is_ok() { "ok" } else { "failed" }
            );
        }
        Err(e) => eprintln!("SKIP derive_image: {e}"),
    }

    let mut image = surface
        .get_image()
        .expect("vaGetImage copy of the NV12 surface");
    let info = *image.info();
    eprintln!(
        "get_image ok: fourcc {:#x} pitches {:?} offsets {:?} data_size {}",
        info.format.fourcc, info.pitches, info.offsets, info.data_size
    );
    let mapped = image.map().expect("map the de-tiled NV12 image");
    assert_eq!(mapped.bytes().len(), info.data_size as usize);
    drop(mapped);

    let export = match surface.export_dmabuf() {
        Ok(export) => export,
        Err(e) => {
            eprintln!("SKIP dmabuf round trip: export unsupported: {e}");
            return;
        }
    };
    eprintln!(
        "exported DMA-BUF: drm_format {:#x}, modifier {:#x}, size {}, planes {:?}",
        export.drm_format, export.modifier, export.size, export.planes
    );

    match read_dmabuf(export.fd.as_fd(), export.size as usize, <[u8]>::len) {
        Ok(len) => assert_eq!(len, export.size as usize),
        Err(VaError::Io(e)) if e.kind() == ErrorKind::PermissionDenied => eprintln!(
            "SKIP read_dmabuf: VA-exported buffer is not CPU-mappable ({e}); CPU readback uses get_image"
        ),
        Err(e) => panic!("read_dmabuf failed: {e}"),
    }

    let imported = OwnedVaSurface::import_dmabuf(
        &display,
        DmabufImport {
            format: PixelFormat::Nv12,
            width: export.width,
            height: export.height,
            fd: export.fd.as_fd(),
            size: export.size,
            modifier: export.modifier,
            planes: &export.planes,
        },
    )
    .expect("re-import exported NV12 DMA-BUF");
    imported.sync().expect("sync imported surface");
}
