//! Memory shared with the display server or compositor.
//!
//! Pixels are written with `pwrite` on a memfd. The server maps the same pages, so this path
//! needs no `mmap` and therefore no `unsafe`.

use std::fs::File;
use std::os::unix::fs::FileExt;

use rustix::fs::{MemfdFlags, ftruncate, memfd_create};

/// Creates an anonymous shared-memory file of `len` bytes.
pub(crate) fn create(len: u64) -> Result<File, String> {
    let fd = memfd_create("lumen-frame", MemfdFlags::CLOEXEC)
        .map_err(|e| format!("memfd_create: {e}"))?;
    ftruncate(&fd, len).map_err(|e| format!("size shared memory to {len} bytes: {e}"))?;
    Ok(File::from(fd))
}

/// Writes `src` at byte `offset`. With `rgba_to_bgrx`, each RGBA pixel is written as BGRX.
pub(crate) fn write_pixels(
    file: &File,
    offset: u64,
    src: &[u8],
    rgba_to_bgrx: bool,
) -> Result<(), String> {
    let io_err = |e: std::io::Error| format!("write frame to shared memory: {e}");
    if !rgba_to_bgrx {
        return file.write_all_at(src, offset).map_err(io_err);
    }
    // Swizzle through a stack chunk so the conversion needs no heap buffer.
    let mut chunk = [0u8; 4096];
    for (i, part) in src.chunks(chunk.len()).enumerate() {
        let (dst_px, _) = chunk.as_chunks_mut::<4>();
        let (src_px, _) = part.as_chunks::<4>();
        for (dst, px) in dst_px.iter_mut().zip(src_px) {
            *dst = [px[2], px[1], px[0], 0xff];
        }
        let at = offset + (i * chunk.len()) as u64;
        file.write_all_at(&chunk[..part.len()], at)
            .map_err(io_err)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_is_swizzled_to_bgrx() {
        // Three pixels: checks RGBA to BGRX byte order and the forced alpha byte.
        let src = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let file = create(12).expect("memfd");
        write_pixels(&file, 0, &src, true).expect("write");
        let mut out = [0u8; 12];
        file.read_exact_at(&mut out, 0).expect("read back");
        assert_eq!(out, [3, 2, 1, 0xff, 7, 6, 5, 0xff, 11, 10, 9, 0xff]);
    }
}
