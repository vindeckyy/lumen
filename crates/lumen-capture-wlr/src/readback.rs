//! CPU readback of a captured DMA-BUF frame.
//!
//! Screencopy copies into a gbm buffer that was allocated with an explicit
//! linear modifier, so the mapping holds the pixels in linear order. Rows are
//! padded: the stride is wider than `width * 4`. A consumer that assumes a
//! tightly packed row corrupts the picture, so the stride is honoured here.
//!
//! The mapping is bracketed by `DMA_BUF_IOCTL_SYNC`, so the CPU sees the
//! compositor's completed writes even when the copy is still in flight.

use std::os::fd::AsFd;

use lumen_ffi_va::read_dmabuf;
use lumen_protocol::media::DmabufImage;

use crate::Error;

/// Bytes per pixel of the RGB formats screencopy produces.
const BYTES_PER_PIXEL: usize = 4;

/// Read a captured DMA-BUF frame into tightly packed 4-byte-per-pixel rows.
///
/// The result has `width * height * 4` bytes in row order, with no padding.
///
/// # Errors
/// Returns an error when the image has no plane, when the stride is narrower
/// than one row, or when the buffer cannot be mapped.
pub fn read_pixels(image: &DmabufImage, width: u32, height: u32) -> Result<Vec<u8>, Error> {
    let planes = image.planes();
    let Some(plane) = planes.first() else {
        return Err(Error::Dmabuf("image has no planes".into()));
    };
    let row_bytes = width as usize * BYTES_PER_PIXEL;
    let stride = plane.stride as usize;
    if stride < row_bytes {
        return Err(Error::Dmabuf(format!(
            "stride {stride} is narrower than a row of {row_bytes} bytes"
        )));
    }
    let offset = plane.offset as usize;
    let rows = height as usize;
    let len = offset + stride * rows;
    read_dmabuf(image.fd.as_fd(), len, |bytes| {
        extract_rows(bytes, offset, stride, row_bytes, rows)
    })
    .map_err(|e| Error::Dmabuf(e.to_string()))
}

/// Copy the rows of a padded image out of a linear mapping.
fn extract_rows(
    bytes: &[u8],
    offset: usize,
    stride: usize,
    row_bytes: usize,
    rows: usize,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(row_bytes * rows);
    for row in 0..rows {
        let start = offset + row * stride;
        out.extend_from_slice(&bytes[start..start + row_bytes]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::extract_rows;

    #[test]
    fn rows_are_copied_out_of_a_padded_image() {
        // Two rows of four bytes with two bytes of padding after each.
        let image: Vec<u8> = vec![
            1, 2, 3, 4, 0xaa, 0xbb, //
            5, 6, 7, 8, 0xcc, 0xdd,
        ];
        let out = extract_rows(&image, 0, 6, 4, 2);
        assert_eq!(out, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn a_plane_offset_skips_the_leading_bytes() {
        let image: Vec<u8> = vec![
            0xaa, 0xbb, 1, 2, 3, 4, //
            0xcc, 0xdd, 5, 6, 7, 8,
        ];
        let out = extract_rows(&image, 2, 6, 4, 2);
        assert_eq!(out, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }
}
