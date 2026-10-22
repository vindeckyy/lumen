//! Mapping between `PixelFormat` and VA-API surface formats and fourcc codes.

use lumen_protocol::media::PixelFormat;

use crate::sys;

/// The `VA_RT_FORMAT_*` surface format used to allocate surfaces of `format`.
#[must_use]
pub const fn rt_format(format: PixelFormat) -> u32 {
    match format {
        PixelFormat::Nv12 | PixelFormat::I420 => sys::VA_RT_FORMAT_YUV420,
        PixelFormat::P010 => sys::VA_RT_FORMAT_YUV420_10,
        PixelFormat::I444 => sys::VA_RT_FORMAT_YUV444,
        PixelFormat::Bgrx | PixelFormat::Rgba => sys::VA_RT_FORMAT_RGB32,
    }
}

/// The `VA_FOURCC_*` code for `format`.
#[must_use]
pub const fn fourcc(format: PixelFormat) -> u32 {
    match format {
        PixelFormat::Nv12 => sys::VA_FOURCC_NV12,
        PixelFormat::P010 => sys::VA_FOURCC_P010,
        PixelFormat::I420 => sys::VA_FOURCC_I420,
        PixelFormat::I444 => sys::VA_FOURCC_444P,
        PixelFormat::Bgrx => sys::VA_FOURCC_BGRX,
        PixelFormat::Rgba => sys::VA_FOURCC_RGBA,
    }
}

/// The pixel format named by a `VA_FOURCC_*` code, if it is one of ours.
#[must_use]
pub fn pixel_format(code: u32) -> Option<PixelFormat> {
    [
        PixelFormat::Nv12,
        PixelFormat::P010,
        PixelFormat::I420,
        PixelFormat::I444,
        PixelFormat::Bgrx,
        PixelFormat::Rgba,
    ]
    .into_iter()
    .find(|&format| fourcc(format) == code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourcc_round_trips_for_every_format() {
        for format in [
            PixelFormat::Nv12,
            PixelFormat::P010,
            PixelFormat::I420,
            PixelFormat::I444,
            PixelFormat::Bgrx,
            PixelFormat::Rgba,
        ] {
            assert_eq!(pixel_format(fourcc(format)), Some(format));
        }
        assert_eq!(pixel_format(0), None);
    }

    #[test]
    fn nv12_uses_420_surfaces() {
        assert_eq!(rt_format(PixelFormat::Nv12), sys::VA_RT_FORMAT_YUV420);
        assert_eq!(fourcc(PixelFormat::Nv12), 0x3231_564E);
    }
}
