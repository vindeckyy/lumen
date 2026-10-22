//! Pixel-format and DMA-BUF validation shared by the Wayland and X11 backends.

use std::os::fd::AsFd;

use lumen_protocol::media::{DmabufImage, PixelFormat};
use rustix::fs::fstat;

/// Number of planes a DMA-BUF of `format` carries.
fn plane_count(format: PixelFormat) -> Result<usize, String> {
    match format {
        PixelFormat::Bgrx | PixelFormat::Rgba => Ok(1),
        PixelFormat::Nv12 | PixelFormat::P010 => Ok(2),
        PixelFormat::I420 | PixelFormat::I444 => Err(format!(
            "{format:?} dmabuf import is not supported; use NV12, P010, BGRX or RGBA"
        )),
    }
}

/// Minimum bytes in one row of `plane`, and the number of rows the plane spans. Only valid for
/// formats that passed [`plane_count`].
fn plane_shape(format: PixelFormat, plane: usize, width: u32, height: u32) -> (u64, u64) {
    let (w, h) = (u64::from(width), u64::from(height));
    match (format, plane) {
        (PixelFormat::Bgrx | PixelFormat::Rgba, _) => (w * 4, h),
        (PixelFormat::Nv12, 0) => (w, h),
        (PixelFormat::P010, 0) => (w * 2, h),
        // Interleaved chroma: one byte pair (NV12) or one 16-bit pair (P010) per 2x2 luma block.
        (PixelFormat::Nv12, _) => (w.div_ceil(2) * 2, h.div_ceil(2)),
        (PixelFormat::P010, _) => (w.div_ceil(2) * 4, h.div_ceil(2)),
        // Unreachable after `plane_count` accepted the format.
        (PixelFormat::I420 | PixelFormat::I444, _) => (0, 0),
    }
}

/// Checks a DMA-BUF frame against its declared size and format, and returns the buffer size in
/// bytes.
///
/// Checked: nonzero size, fourcc matches the frame format, plane count, every stride holds one
/// row, and every plane lies inside the fd. The compositor validates buffers too, but an invalid
/// buffer there is a protocol error that disconnects the client, so it is refused here first.
///
/// # Errors
/// Returns a description of the first check that fails.
pub(crate) fn check_dmabuf(
    width: u32,
    height: u32,
    format: PixelFormat,
    image: &DmabufImage,
) -> Result<u64, String> {
    if width == 0 || height == 0 {
        return Err(format!("dmabuf frame has zero size {width}x{height}"));
    }
    if image.fourcc != format.drm_fourcc() {
        return Err(format!(
            "dmabuf fourcc {:#010x} does not match {format:?} ({:#010x})",
            image.fourcc,
            format.drm_fourcc()
        ));
    }
    let planes = image.planes();
    let expected = plane_count(format)?;
    if planes.len() != expected {
        return Err(format!(
            "{format:?} dmabuf needs {expected} plane(s), got {}",
            planes.len()
        ));
    }
    let size = fstat(image.fd.as_fd())
        .map_err(|e| format!("fstat dmabuf: {e}"))?
        .st_size;
    let size = u64::try_from(size).map_err(|_| format!("dmabuf reports negative size {size}"))?;
    for (index, plane) in planes.iter().enumerate() {
        let (min_row, rows) = plane_shape(format, index, width, height);
        let stride = u64::from(plane.stride);
        if stride < min_row {
            return Err(format!(
                "plane {index} stride {stride} is below the {min_row}-byte row {format:?} needs"
            ));
        }
        // The last row needs only `min_row` bytes, so the plane ends there.
        let end = u64::from(plane.offset) + stride * (rows - 1) + min_row;
        if end > size {
            return Err(format!(
                "plane {index} ends at byte {end} but the dmabuf is {size} bytes"
            ));
        }
    }
    Ok(size)
}

/// Checks that a CPU frame is one of the packed 32-bit formats the presenters accept and that
/// its byte length matches its size.
pub(crate) fn check_cpu_frame(
    width: u32,
    height: u32,
    format: PixelFormat,
    len: usize,
) -> Result<(), String> {
    match format {
        PixelFormat::Bgrx | PixelFormat::Rgba => {}
        other => {
            return Err(format!(
                "CPU {other:?} frames are not presentable; convert to BGRX or RGBA first"
            ));
        }
    }
    let expected = format.frame_len(width, height);
    if len != expected {
        return Err(format!(
            "CPU frame is {len} bytes, expected {expected} for {width}x{height} {format:?}"
        ));
    }
    Ok(())
}

/// Converts a frame dimension to the `i32` the Wayland protocol uses.
pub(crate) fn i32_dim(value: u32) -> Result<i32, String> {
    i32::try_from(value).map_err(|_| format!("frame dimension {value} exceeds the protocol limit"))
}

#[cfg(test)]
mod tests {
    use std::os::fd::OwnedFd;

    use lumen_protocol::media::{DRM_FORMAT_MOD_LINEAR, DmabufPlane};

    use super::*;

    /// A DMA-BUF whose fd is a memfd of `len` bytes. Good enough for the checks above, which
    /// only read the size.
    fn image(len: u64, fourcc: u32, planes: &[DmabufPlane]) -> DmabufImage {
        let fd = OwnedFd::from(crate::shm::create(len).expect("memfd"));
        DmabufImage::new(fd, fourcc, DRM_FORMAT_MOD_LINEAR, planes).expect("image")
    }

    fn plane(offset: u32, stride: u32) -> DmabufPlane {
        DmabufPlane { offset, stride }
    }

    #[test]
    fn bgrx_single_plane_is_accepted_when_it_fits() {
        let fourcc = PixelFormat::Bgrx.drm_fourcc();
        let ok = image(16, fourcc, &[plane(0, 8)]);
        assert_eq!(check_dmabuf(2, 2, PixelFormat::Bgrx, &ok), Ok(16));
        // A row offset that pushes the last row past the end is refused.
        let shifted = image(16, fourcc, &[plane(9, 8)]);
        assert!(check_dmabuf(2, 2, PixelFormat::Bgrx, &shifted).is_err());
    }

    #[test]
    fn nv12_planes_use_the_chroma_rows_and_size() {
        let fourcc = PixelFormat::Nv12.drm_fourcc();
        // 1920x1080 NV12, luma then interleaved chroma, both at stride 1920.
        let planes = [plane(0, 1920), plane(1920 * 1080, 1920)];
        let exact = 1920 * 1080 * 3 / 2;
        assert!(
            check_dmabuf(
                1920,
                1080,
                PixelFormat::Nv12,
                &image(exact, fourcc, &planes)
            )
            .is_ok()
        );
        assert!(
            check_dmabuf(
                1920,
                1080,
                PixelFormat::Nv12,
                &image(exact - 1, fourcc, &planes)
            )
            .is_err()
        );
    }

    #[test]
    fn p010_chroma_stride_is_two_samples_per_pair() {
        let fourcc = PixelFormat::P010.drm_fourcc();
        // 4x2 P010: luma is 2 rows of 8 bytes; chroma is 1 row of 8 bytes at byte 16. 24 in all.
        let planes = [plane(0, 8), plane(16, 8)];
        assert_eq!(
            check_dmabuf(4, 2, PixelFormat::P010, &image(24, fourcc, &planes)),
            Ok(24)
        );
        let narrow = [plane(0, 8), plane(16, 4)];
        assert!(check_dmabuf(4, 2, PixelFormat::P010, &image(64, fourcc, &narrow)).is_err());
    }

    #[test]
    fn wrong_fourcc_plane_count_and_stride_are_refused() {
        let bgrx = PixelFormat::Bgrx.drm_fourcc();
        let nv12 = PixelFormat::Nv12.drm_fourcc();
        assert!(check_dmabuf(2, 2, PixelFormat::Bgrx, &image(16, nv12, &[plane(0, 8)])).is_err());
        assert!(
            check_dmabuf(
                2,
                2,
                PixelFormat::Bgrx,
                &image(32, bgrx, &[plane(0, 8), plane(16, 8)])
            )
            .is_err()
        );
        assert!(check_dmabuf(2, 2, PixelFormat::Nv12, &image(16, nv12, &[plane(0, 2)])).is_err());
        assert!(check_dmabuf(2, 2, PixelFormat::Bgrx, &image(16, bgrx, &[plane(0, 4)])).is_err());
        assert!(check_dmabuf(0, 2, PixelFormat::Bgrx, &image(16, bgrx, &[plane(0, 8)])).is_err());
        assert!(check_dmabuf(2, 2, PixelFormat::I420, &image(16, bgrx, &[plane(0, 8)])).is_err());
    }

    #[test]
    fn cpu_frame_length_is_checked() {
        assert!(check_cpu_frame(2, 2, PixelFormat::Bgrx, 16).is_ok());
        assert!(check_cpu_frame(2, 2, PixelFormat::Bgrx, 15).is_err());
        assert!(check_cpu_frame(2, 2, PixelFormat::Nv12, 6).is_err());
    }
}
