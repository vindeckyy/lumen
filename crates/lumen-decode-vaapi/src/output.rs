//! Turn a decoded picture into frame bytes: a tightly packed NV12 copy, or a DMA-BUF image whose
//! planes are described exactly as the driver exports them.

use lumen_ffi_va::DmabufExport;
use lumen_protocol::media::{DmabufImage, DmabufPlane as FramePlane, FrameBuffer, PixelFormat};

/// `DRM_FORMAT_NV12`, the fourcc VA-API reports for a composed NV12 export.
const DRM_FORMAT_NV12: u32 = u32::from_le_bytes(*b"NV12");

/// Plane placement inside a mapped NV12 image, as VA-API reports it (`VAImage` offsets/pitches).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nv12Layout {
    /// Byte offset of the Y plane.
    pub y_offset: usize,
    /// Byte pitch of one Y row.
    pub y_pitch: usize,
    /// Byte offset of the interleaved UV plane.
    pub uv_offset: usize,
    /// Byte pitch of one UV row.
    pub uv_pitch: usize,
}

/// Visible rectangle inside the coded picture, in luma samples. `x`, `y`, `width` and `height`
/// are all even for 4:2:0 frame pictures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crop {
    /// Left edge.
    pub x: u32,
    /// Top edge.
    pub y: u32,
    /// Visible width.
    pub width: u32,
    /// Visible height.
    pub height: u32,
}

/// Copy the visible rectangle of a mapped NV12 image into tightly packed NV12 (Y then UV, no
/// row padding). Each plane row is taken at its own pitch, so padding never leaks into the output.
///
/// # Errors
/// Returns a description when the layout points outside `image`.
pub fn pack_nv12(image: &[u8], layout: Nv12Layout, crop: Crop) -> Result<Vec<u8>, &'static str> {
    const SHORT: &str = "mapped image is smaller than its layout";
    let width = crop.width as usize;
    let height = crop.height as usize;
    let x = crop.x as usize;
    let y = crop.y as usize;
    let mut out = Vec::with_capacity(PixelFormat::Nv12.frame_len(crop.width, crop.height));
    for row in 0..height {
        let start = layout.y_offset + (y + row) * layout.y_pitch + x;
        out.extend_from_slice(image.get(start..start + width).ok_or(SHORT)?);
    }
    // One UV row carries `width` bytes (width/2 interleaved pairs) for every two luma rows.
    for row in 0..height / 2 {
        let start = layout.uv_offset + (y / 2 + row) * layout.uv_pitch + x;
        out.extend_from_slice(image.get(start..start + width).ok_or(SHORT)?);
    }
    Ok(out)
}

/// Wrap an exported surface as a DMA-BUF frame buffer.
///
/// The two planes keep the offset and pitch the driver reports, and the DRM modifier passes
/// through unchanged: the consumer checks the modifier. Returns `None` when the export is not
/// two-plane NV12, or when the visible picture does not start at the surface origin (a plane
/// offset cannot express a crop inside a tiled layout). The caller then copies the picture out.
#[must_use]
pub fn dmabuf_buffer(export: DmabufExport, crop: Crop) -> Option<FrameBuffer> {
    let origin_kept = crop.x == 0 && crop.y == 0;
    let fits = crop.width <= export.width && crop.height <= export.height;
    if export.drm_format != DRM_FORMAT_NV12 || export.planes.len() != 2 || !origin_kept || !fits {
        return None;
    }
    let planes: Vec<FramePlane> = export
        .planes
        .iter()
        .map(|p| FramePlane {
            offset: p.offset,
            stride: p.pitch,
        })
        .collect();
    DmabufImage::new(export.fd, export.drm_format, export.modifier, &planes)
        .ok()
        .map(FrameBuffer::Dmabuf)
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use lumen_ffi_va::DmabufPlane;

    use super::*;

    /// 8x4 NV12 image with 12-byte pitches (4 bytes of padding per row) and the UV plane after
    /// the Y plane. Y byte = row*16 + col; UV byte = 0x80 + row*16 + col.
    fn padded_image() -> (Vec<u8>, Nv12Layout) {
        let y_pitch = 12;
        let uv_pitch = 12;
        let y_offset = 0;
        let uv_offset = y_pitch * 4;
        let mut image = vec![0xEE; uv_offset + uv_pitch * 2];
        for row in 0..4 {
            for col in 0..8 {
                image[y_offset + row * y_pitch + col] = (row * 16 + col) as u8;
            }
        }
        for row in 0..2 {
            for col in 0..8 {
                image[uv_offset + row * uv_pitch + col] = (0x80 + row * 16 + col) as u8;
            }
        }
        let layout = Nv12Layout {
            y_offset,
            y_pitch,
            uv_offset,
            uv_pitch,
        };
        (image, layout)
    }

    #[test]
    fn packs_without_row_padding() {
        let (image, layout) = padded_image();
        let crop = Crop {
            x: 0,
            y: 0,
            width: 8,
            height: 4,
        };
        let out = pack_nv12(&image, layout, crop).expect("pack");
        assert_eq!(out.len(), PixelFormat::Nv12.frame_len(8, 4));
        assert_eq!(&out[..8], &[0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(&out[8..16], &[16, 17, 18, 19, 20, 21, 22, 23]);
        assert_eq!(
            &out[32..40],
            &[0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87]
        );
        assert!(!out.contains(&0xEE));
    }

    #[test]
    fn crops_to_visible_rectangle() {
        let (image, layout) = padded_image();
        let crop = Crop {
            x: 2,
            y: 2,
            width: 4,
            height: 2,
        };
        let out = pack_nv12(&image, layout, crop).expect("pack");
        assert_eq!(&out[..4], &[34, 35, 36, 37]);
        assert_eq!(&out[4..8], &[50, 51, 52, 53]);
        // Chroma row y/2 = 1, byte x = 2.
        assert_eq!(&out[8..12], &[0x92, 0x93, 0x94, 0x95]);
    }

    #[test]
    fn short_image_is_an_error() {
        let (image, layout) = padded_image();
        let crop = Crop {
            x: 0,
            y: 0,
            width: 8,
            height: 8,
        };
        assert!(pack_nv12(&image, layout, crop).is_err());
    }

    /// A 64x64 NV12 export with the given modifier and planes. The fd is `/dev/null`, which is
    /// enough because these tests inspect the description, not the memory.
    fn export(drm_format: u32, modifier: u64, planes: Vec<DmabufPlane>) -> DmabufExport {
        DmabufExport {
            width: 64,
            height: 64,
            drm_format,
            modifier,
            size: 64 * 64 * 2,
            fd: File::open("/dev/null").expect("/dev/null exists").into(),
            planes,
        }
    }

    fn full_crop() -> Crop {
        Crop {
            x: 0,
            y: 0,
            width: 64,
            height: 64,
        }
    }

    const LINEAR: u64 = 0;
    /// AMD tiled modifier that VA-API reports for decoded NV12 surfaces on renderD129.
    const AMD_TILED: u64 = 0x0200_0000_0000_0901;

    #[test]
    fn dmabuf_accepts_plain_linear_nv12() {
        let planes = vec![
            DmabufPlane {
                offset: 0,
                pitch: 64,
            },
            DmabufPlane {
                offset: 64 * 64,
                pitch: 64,
            },
        ];
        let buffer = dmabuf_buffer(export(DRM_FORMAT_NV12, LINEAR, planes), full_crop())
            .expect("linear NV12 is describable");
        let FrameBuffer::Dmabuf(image) = buffer else {
            panic!("expected a DMA-BUF frame");
        };
        assert_eq!((image.fourcc, image.modifier), (DRM_FORMAT_NV12, LINEAR));
        assert_eq!(image.planes()[1].offset, 4096);
        assert_eq!(image.planes()[1].stride, 64);
    }

    #[test]
    fn dmabuf_passes_any_modifier_and_plane_layout_through() {
        // The exported layout is not linear and the UV plane does not follow Y at one pitch.
        let planes = vec![
            DmabufPlane {
                offset: 0,
                pitch: 256,
            },
            DmabufPlane {
                offset: 65536,
                pitch: 512,
            },
        ];
        let buffer = dmabuf_buffer(export(DRM_FORMAT_NV12, AMD_TILED, planes), full_crop())
            .expect("any modifier is describable");
        let FrameBuffer::Dmabuf(image) = buffer else {
            panic!("expected a DMA-BUF frame");
        };
        assert_eq!(image.modifier, AMD_TILED);
        assert_eq!(
            image
                .planes()
                .iter()
                .map(|p| (p.offset, p.stride))
                .collect::<Vec<_>>(),
            vec![(0, 256), (65536, 512)]
        );
    }

    #[test]
    fn dmabuf_rejects_a_cropped_origin() {
        let planes = vec![
            DmabufPlane {
                offset: 0,
                pitch: 64,
            },
            DmabufPlane {
                offset: 4096,
                pitch: 64,
            },
        ];
        let crop = Crop {
            x: 2,
            y: 0,
            width: 62,
            height: 64,
        };
        assert!(dmabuf_buffer(export(DRM_FORMAT_NV12, LINEAR, planes), crop).is_none());
    }

    #[test]
    fn dmabuf_rejects_other_formats_and_plane_counts() {
        let one_plane = vec![DmabufPlane {
            offset: 0,
            pitch: 64,
        }];
        assert!(dmabuf_buffer(export(DRM_FORMAT_NV12, LINEAR, one_plane), full_crop()).is_none());
        let two_planes = vec![
            DmabufPlane {
                offset: 0,
                pitch: 64,
            },
            DmabufPlane {
                offset: 4096,
                pitch: 64,
            },
        ];
        let p010 = u32::from_le_bytes(*b"P010");
        assert!(dmabuf_buffer(export(p010, LINEAR, two_planes), full_crop()).is_none());
    }
}
