//! Media pipeline traits and frame types.
//!
//! These types are the contract between capture, encode, decode and render.
//! They live in `lumen-protocol` so that no crate needs to depend on another
//! to agree on what a frame is.

use std::os::fd::OwnedFd;

/// Pixel format of a video frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// 8-bit 4:2:0 planar, Y plane then interleaved UV.
    Nv12,
    /// 10-bit 4:2:0, Y plane then interleaved UV (P010 semantics).
    P010,
    /// 8-bit 4:2:0 planar I420.
    I420,
    /// 8-bit 4:4:4 planar.
    I444,
    /// 32-bit BGRX, X11 `ZPixmap` order.
    Bgrx,
    /// 32-bit RGBA.
    Rgba,
}

impl PixelFormat {
    /// Exact size in bytes of one tightly packed frame of this format.
    ///
    /// Chroma planes round up for odd dimensions. P010 stores 16 bits per sample.
    #[must_use]
    pub fn frame_len(self, width: u32, height: u32) -> usize {
        let w = width as usize;
        let h = height as usize;
        let chroma = w.div_ceil(2) * h.div_ceil(2);
        match self {
            Self::Nv12 | Self::I420 => w * h + 2 * chroma,
            Self::P010 => 2 * w * h + 4 * chroma,
            Self::I444 => 3 * w * h,
            Self::Bgrx | Self::Rgba => 4 * w * h,
        }
    }

    /// Whether the format carries 10 bits per component.
    #[must_use]
    pub const fn is_ten_bit(self) -> bool {
        matches!(self, Self::P010)
    }

    /// Whether chroma is subsampled relative to luma.
    #[must_use]
    pub const fn is_subsampled(self) -> bool {
        matches!(self, Self::Nv12 | Self::I420 | Self::P010)
    }

    /// DRM fourcc of this format's memory layout.
    #[must_use]
    pub const fn drm_fourcc(self) -> u32 {
        match self {
            Self::Nv12 => drm_fourcc(b'N', b'V', b'1', b'2'),
            Self::P010 => drm_fourcc(b'P', b'0', b'1', b'0'),
            Self::I420 => drm_fourcc(b'Y', b'U', b'1', b'2'),
            Self::I444 => drm_fourcc(b'Y', b'U', b'2', b'4'),
            Self::Bgrx => drm_fourcc(b'X', b'R', b'2', b'4'),
            Self::Rgba => drm_fourcc(b'A', b'B', b'2', b'4'),
        }
    }
}

/// One plane of a DMA-BUF image.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DmabufPlane {
    /// Byte offset of the plane inside the buffer object.
    pub offset: u32,
    /// Row stride in bytes.
    pub stride: u32,
}

/// Most planes a DMA-BUF image can carry.
pub const MAX_DMABUF_PLANES: usize = 4;

/// DRM format modifier for an untiled (linear) layout.
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// DRM format modifier meaning the buffer uses the implicit (driver-chosen) layout.
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// Build a DRM fourcc code from four ASCII bytes, in the order DRM defines.
#[must_use]
pub const fn drm_fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    u32::from_le_bytes([a, b, c, d])
}

/// A DMA-BUF image: one buffer object that holds every plane.
#[derive(Debug)]
pub struct DmabufImage {
    /// The DMA-BUF handle that holds all planes.
    pub fd: OwnedFd,
    /// DRM fourcc of the layout, for example `DRM_FORMAT_NV12`.
    pub fourcc: u32,
    /// DRM format modifier (tiling and compression). `DRM_FORMAT_MOD_LINEAR` is untiled.
    pub modifier: u64,
    planes: [DmabufPlane; MAX_DMABUF_PLANES],
    plane_count: usize,
}

impl DmabufImage {
    /// Build a DMA-BUF image description.
    ///
    /// # Errors
    /// Returns an error when `planes` is empty or has more than `MAX_DMABUF_PLANES` entries.
    pub fn new(
        fd: OwnedFd,
        fourcc: u32,
        modifier: u64,
        planes: &[DmabufPlane],
    ) -> Result<Self, String> {
        if planes.is_empty() || planes.len() > MAX_DMABUF_PLANES {
            return Err(format!(
                "dmabuf needs 1 to {MAX_DMABUF_PLANES} planes, got {}",
                planes.len()
            ));
        }
        let mut array = [DmabufPlane::default(); MAX_DMABUF_PLANES];
        array[..planes.len()].copy_from_slice(planes);
        Ok(Self {
            fd,
            fourcc,
            modifier,
            planes: array,
            plane_count: planes.len(),
        })
    }

    /// The valid planes, in order.
    #[must_use]
    pub fn planes(&self) -> &[DmabufPlane] {
        &self.planes[..self.plane_count]
    }
}

/// A video frame, either in host memory or as a DMA-BUF object.
#[derive(Debug)]
pub enum FrameBuffer {
    /// Frame data in ordinary host memory.
    Cpu {
        /// Tightly packed pixel data.
        data: Vec<u8>,
    },
    /// Frame data in one DMA-BUF object.
    Dmabuf(DmabufImage),
}

impl FrameBuffer {
    /// Number of bytes the frame occupies in host memory, if it is a CPU frame.
    #[must_use]
    pub fn cpu_len(&self) -> Option<usize> {
        match self {
            Self::Cpu { data } => Some(data.len()),
            Self::Dmabuf(_) => None,
        }
    }
}

/// A captured video frame.
#[derive(Debug)]
pub struct VideoFrame {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Pixel format of the buffer.
    pub format: PixelFormat,
    /// The pixels.
    pub buffer: FrameBuffer,
    /// Capture timestamp on the host clock, microseconds.
    pub capture_ts_us: u64,
    /// Monotonic frame counter.
    pub index: u64,
}

impl VideoFrame {
    /// Build a CPU frame.
    #[must_use]
    pub fn cpu(
        width: u32,
        height: u32,
        format: PixelFormat,
        data: Vec<u8>,
        capture_ts_us: u64,
        index: u64,
    ) -> Self {
        Self {
            width,
            height,
            format,
            buffer: FrameBuffer::Cpu { data },
            capture_ts_us,
            index,
        }
    }
}

/// An encoded access unit produced by an encoder.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
    /// The compressed bitstream.
    pub data: Vec<u8>,
    /// Whether the access unit can be decoded without any reference frame.
    pub is_keyframe: bool,
    /// Capture timestamp of the source frame, host clock microseconds.
    pub capture_ts_us: u64,
    /// Frame index of the source frame.
    pub index: u64,
}

/// Video encoder interface.
pub trait VideoEncoder: Send {
    /// Encode one frame.
    ///
    /// # Errors
    /// Returns a human-readable description of the failure.
    fn encode(&mut self, frame: &VideoFrame) -> Result<EncodedFrame, String>;

    /// Force the next encoded frame to be a keyframe.
    fn request_keyframe(&mut self);

    /// Number of frames encoded so far.
    fn frames_encoded(&self) -> u64;
}

/// Video decoder interface.
pub trait VideoDecoder: Send {
    /// Decode one access unit.
    ///
    /// # Errors
    /// Returns a human-readable description of the failure.
    fn decode(&mut self, encoded: &EncodedFrame) -> Result<Option<VideoFrame>, String>;

    /// Number of frames decoded so far.
    fn frames_decoded(&self) -> u64;

    /// Number of decode errors so far.
    fn decode_errors(&self) -> u64;
}

/// A source of captured frames.
pub trait FrameSource: Send {
    /// Take the next captured frame, if one is available without blocking.
    fn poll_frame(&mut self) -> Option<VideoFrame>;
}

/// A sink for decoded frames.
pub trait FrameSink: Send {
    /// Present a decoded frame.
    ///
    /// # Errors
    /// Returns a human-readable description of the failure.
    fn present(&mut self, frame: &VideoFrame) -> Result<(), String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_format_sizes() {
        assert_eq!(PixelFormat::Nv12.frame_len(2, 2), 6);
        assert_eq!(PixelFormat::Nv12.frame_len(3, 3), 17);
        assert_eq!(PixelFormat::P010.frame_len(2, 2), 12);
        assert_eq!(PixelFormat::I444.frame_len(2, 2), 12);
        assert_eq!(PixelFormat::Bgrx.frame_len(2, 2), 16);
        assert!(PixelFormat::P010.is_ten_bit());
        assert!(PixelFormat::Nv12.is_subsampled());
        assert!(!PixelFormat::I444.is_subsampled());
    }

    #[test]
    fn cpu_frame_len() {
        let f = VideoFrame::cpu(2, 2, PixelFormat::Nv12, vec![0; 6], 0, 0);
        assert_eq!(f.buffer.cpu_len(), Some(6));
    }

    fn null_fd() -> OwnedFd {
        std::fs::File::open("/dev/null").unwrap().into()
    }

    #[test]
    fn drm_fourcc_matches_the_drm_headers() {
        assert_eq!(PixelFormat::Nv12.drm_fourcc(), 0x3231_564e);
        assert_eq!(PixelFormat::Bgrx.drm_fourcc(), 0x3432_5258);
        assert_eq!(PixelFormat::Rgba.drm_fourcc(), 0x3432_4241);
    }

    #[test]
    fn dmabuf_rejects_bad_plane_counts() {
        assert!(DmabufImage::new(null_fd(), 0, DRM_FORMAT_MOD_LINEAR, &[]).is_err());
        let five = [DmabufPlane::default(); 5];
        assert!(DmabufImage::new(null_fd(), 0, DRM_FORMAT_MOD_LINEAR, &five).is_err());
    }

    #[test]
    fn dmabuf_keeps_its_valid_planes_in_order() {
        let planes = [
            DmabufPlane {
                offset: 0,
                stride: 256,
            },
            DmabufPlane {
                offset: 65536,
                stride: 256,
            },
        ];
        let img = DmabufImage::new(null_fd(), PixelFormat::Nv12.drm_fourcc(), 0, &planes).unwrap();
        assert_eq!(img.planes(), &planes);
        assert_eq!(img.fourcc, 0x3231_564e);
    }
}
