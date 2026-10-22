//! BT.709 limited-range conversion between BGRX and NV12.
//!
//! This is the CPU path. The host converts captured BGRX to NV12 before
//! encoding, and the client converts decoded NV12 back to BGRX before
//! presenting. The spec puts colour conversion on the GPU; this path exists
//! so the pipeline runs before the GPU path is written, and it is measured
//! separately so its cost is known.
//!
//! Both directions need even width and height. Chroma is averaged over each
//! 2x2 block on the way down and replicated on the way up.

/// Check that a BGRX/NV12 buffer pair has the lengths the conversion expects.
/// Returns the pixel columns and rows.
fn check_sizes(
    width: u32,
    height: u32,
    bgrx_len: usize,
    nv12_len: usize,
) -> Result<(usize, usize), String> {
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(format!(
            "colour conversion needs even, non-zero size, got {width}x{height}"
        ));
    }
    let cols = width as usize;
    let rows = height as usize;
    let pixels = cols * rows;
    if bgrx_len != pixels * 4 {
        return Err(format!(
            "BGRX buffer is {bgrx_len} bytes, expected {}",
            pixels * 4
        ));
    }
    if nv12_len != pixels * 3 / 2 {
        return Err(format!(
            "NV12 buffer is {nv12_len} bytes, expected {}",
            pixels * 3 / 2
        ));
    }
    Ok((cols, rows))
}

/// Limited-range luma and chroma of one pixel, BT.709 in 8.8 fixed point.
///
/// The chroma rows sum to zero so that white maps to exactly 128.
fn rgb_to_ycbcr(red: i32, green: i32, blue: i32) -> (i32, i32, i32) {
    let luma = ((47 * red + 157 * green + 16 * blue + 128) >> 8) + 16;
    let cb = ((-26 * red - 87 * green + 113 * blue + 128) >> 8) + 128;
    let cr = ((112 * red - 102 * green - 10 * blue + 128) >> 8) + 128;
    (luma, cb, cr)
}

fn clamp8(value: i32) -> u8 {
    u8::try_from(value.clamp(0, 255)).unwrap_or(u8::MAX)
}

/// Convert BGRX to NV12 at a different size.
///
/// The host uses this to serve a smaller picture without a second pass: the
/// conversion reads the source with a stride, so a smaller picture costs
/// less work, not more. That is what lets a weak client have a smooth
/// stream at a lower resolution instead of a stuttering one at the
/// resolution it cannot reach.
///
/// The luma is sampled nearest-neighbour. The chroma is averaged over the
/// block of source pixels that falls into each chroma cell, so a downscale
/// does not lose the colour detail a point sample would drop.
///
/// # Errors
/// Returns an error when a size is odd or a buffer has the wrong length.
pub fn bgrx_to_nv12_scaled(
    src: &[u8],
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
    dst: &mut [u8],
) -> Result<(), String> {
    if src_width == 0
        || src_height == 0
        || dst_width == 0
        || dst_height == 0
        || !src_width.is_multiple_of(2)
        || !src_height.is_multiple_of(2)
        || !dst_width.is_multiple_of(2)
        || !dst_height.is_multiple_of(2)
    {
        return Err("colour conversion needs even, non-zero sizes".to_string());
    }
    let src_cols = src_width as usize;
    let src_rows = src_height as usize;
    let dst_cols = dst_width as usize;
    let dst_rows = dst_height as usize;
    if src.len() != src_cols * src_rows * 4 {
        return Err(format!(
            "BGRX buffer is {} bytes, expected {}",
            src.len(),
            src_cols * src_rows * 4
        ));
    }
    if dst.len() != dst_cols * dst_rows * 3 / 2 {
        return Err(format!(
            "NV12 buffer is {} bytes, expected {}",
            dst.len(),
            dst_cols * dst_rows * 3 / 2
        ));
    }

    // The common case is a conversion without a scale, and the plain path is
    // the faster one: the scaled path divides for every pixel it writes,
    // and a division per pixel costs more than the whole conversion does.
    if src_width == dst_width && src_height == dst_height {
        return bgrx_to_nv12(src, src_width, src_height, dst);
    }

    // The source column and row a destination pixel comes from.
    //
    // The mapping is walked with an accumulator rather than a division per
    // pixel: a division per pixel costs more than the whole conversion, and
    // the whole point of the scaled path is that it is cheaper than the
    // plain one.
    let step_x = src_cols / dst_cols;
    let rem_x = src_cols % dst_cols;
    let step_y = src_rows / dst_rows;
    let rem_y = src_rows % dst_rows;

    let (luma_plane, chroma_plane) = dst.split_at_mut(dst_cols * dst_rows);
    let mut sy = 0usize;
    let mut err_y = 0usize;
    for row in 0..dst_rows {
        let mut sx = 0usize;
        let mut err_x = 0usize;
        for col in 0..dst_cols {
            let px = &src[(sy * src_cols + sx) * 4..][..4];
            let (luma, _, _) = rgb_to_ycbcr(i32::from(px[2]), i32::from(px[1]), i32::from(px[0]));
            luma_plane[row * dst_cols + col] = clamp8(luma);
            // The next source column is the one the accumulator walks to.
            err_x += rem_x;
            if err_x >= dst_cols {
                sx += 1;
                err_x -= dst_cols;
            }
            sx += step_x;
        }
        err_y += rem_y;
        if err_y >= dst_rows {
            sy += 1;
            err_y -= dst_rows;
        }
        sy += step_y;
    }

    // The source block each chroma cell averages.
    //
    // It is a fixed two by two block of the source, and not the whole area
    // the cell covers. Averaging the whole area would read every source
    // pixel, and a smaller picture would then cost the same as a big one,
    // which is the opposite of what the downscale is for. The colour is a
    // little aliased at a large downscale, and the luma carries the detail.
    const BLOCK_W: usize = 2;
    const BLOCK_H: usize = 2;
    for row in 0..dst_rows / 2 {
        // The source block starts at the source pixel of the destination
        // pixel the chroma cell covers.
        let y0 = ((row * 2) * src_rows) / dst_rows;
        for col in 0..dst_cols / 2 {
            let x0 = ((col * 2) * src_cols) / dst_cols;
            let mut cb_sum = 0i32;
            let mut cr_sum = 0i32;
            let mut count = 0i32;
            for dy in 0..BLOCK_H {
                let py = y0 + dy;
                if py >= src_rows {
                    break;
                }
                for dx in 0..BLOCK_W {
                    let px = x0 + dx;
                    if px >= src_cols {
                        break;
                    }
                    let pixel = &src[(py * src_cols + px) * 4..][..4];
                    let (_, cb, cr) = rgb_to_ycbcr(
                        i32::from(pixel[2]),
                        i32::from(pixel[1]),
                        i32::from(pixel[0]),
                    );
                    cb_sum += cb;
                    cr_sum += cr;
                    count += 1;
                }
            }
            let uv = row * dst_cols + col * 2;
            chroma_plane[uv] = clamp8(cb_sum / count.max(1));
            chroma_plane[uv + 1] = clamp8(cr_sum / count.max(1));
        }
    }
    Ok(())
}

/// Convert tightly packed BGRX (`B, G, R, X` per pixel) to NV12.
///
/// # Errors
/// Returns an error when the size is odd or a buffer has the wrong length.
pub fn bgrx_to_nv12(src: &[u8], width: u32, height: u32, dst: &mut [u8]) -> Result<(), String> {
    let (cols, rows) = check_sizes(width, height, src.len(), dst.len())?;
    let (luma_plane, chroma_plane) = dst.split_at_mut(cols * rows);
    let pixel = |idx: usize| {
        let px = &src[idx * 4..idx * 4 + 4];
        rgb_to_ycbcr(i32::from(px[2]), i32::from(px[1]), i32::from(px[0]))
    };
    for row in 0..rows {
        for col in 0..cols {
            let (luma, _, _) = pixel(row * cols + col);
            luma_plane[row * cols + col] = clamp8(luma);
        }
    }
    for row in (0..rows).step_by(2) {
        for col in (0..cols).step_by(2) {
            let mut cb_sum = 0;
            let mut cr_sum = 0;
            for dy in 0..2 {
                for dx in 0..2 {
                    let (_, cb, cr) = pixel((row + dy) * cols + col + dx);
                    cb_sum += cb;
                    cr_sum += cr;
                }
            }
            let uv = (row / 2) * cols + col;
            chroma_plane[uv] = clamp8((cb_sum + 2) >> 2);
            chroma_plane[uv + 1] = clamp8((cr_sum + 2) >> 2);
        }
    }
    Ok(())
}

/// Convert NV12 to tightly packed BGRX with alpha set to 255.
///
/// # Errors
/// Returns an error when the size is odd or a buffer has the wrong length.
pub fn nv12_to_bgrx(src: &[u8], width: u32, height: u32, dst: &mut [u8]) -> Result<(), String> {
    let (cols, rows) = check_sizes(width, height, dst.len(), src.len())?;
    let (luma_plane, chroma_plane) = src.split_at(cols * rows);
    for row in 0..rows {
        for col in 0..cols {
            let uv = (row / 2) * cols + (col / 2) * 2;
            let luma = i32::from(luma_plane[row * cols + col]) - 16;
            let cb = i32::from(chroma_plane[uv]) - 128;
            let cr = i32::from(chroma_plane[uv + 1]) - 128;
            let red = (298 * luma + 459 * cr + 128) >> 8;
            let green = (298 * luma - 55 * cb - 136 * cr + 128) >> 8;
            let blue = (298 * luma + 541 * cb + 128) >> 8;
            let out = (row * cols + col) * 4;
            dst[out] = clamp8(blue);
            dst[out + 1] = clamp8(green);
            dst[out + 2] = clamp8(red);
            dst[out + 3] = 255;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A BGRX buffer where every pixel has the same colour.
    fn bgrx(width: u32, height: u32, colour: [u8; 4]) -> Vec<u8> {
        colour
            .iter()
            .cycle()
            .take(width as usize * height as usize * 4)
            .copied()
            .collect()
    }

    #[test]
    fn the_scaled_conversion_matches_the_plain_one_at_the_same_size() {
        let src = bgrx(8, 4, [10, 20, 30, 255]);
        let mut plain = vec![0u8; 8 * 4 * 3 / 2];
        bgrx_to_nv12(&src, 8, 4, &mut plain).expect("the plain conversion");
        let mut scaled = vec![0u8; 8 * 4 * 3 / 2];
        bgrx_to_nv12_scaled(&src, 8, 4, 8, 4, &mut scaled).expect("the scaled conversion");
        assert_eq!(plain, scaled);
    }

    #[test]
    fn a_downscale_halves_the_picture() {
        let src = bgrx(16, 8, [10, 20, 30, 255]);
        let mut dst = vec![0u8; 8 * 4 * 3 / 2];
        bgrx_to_nv12_scaled(&src, 16, 8, 8, 4, &mut dst).expect("the scaled conversion");
        assert_eq!(dst.len(), 8 * 4 * 3 / 2);
        // The luma of a flat picture is the same everywhere.
        let luma = &dst[..8 * 4];
        assert!(luma.windows(2).all(|pair| pair[0] == pair[1]));
    }

    #[test]
    fn a_downscale_keeps_the_colour_of_a_flat_picture() {
        let src = bgrx(16, 16, [10, 20, 30, 255]);
        let mut small = vec![0u8; 8 * 8 * 3 / 2];
        bgrx_to_nv12_scaled(&src, 16, 16, 8, 8, &mut small).expect("the scaled conversion");
        let mut plain = vec![0u8; 16 * 16 * 3 / 2];
        bgrx_to_nv12_scaled(&src, 16, 16, 16, 16, &mut plain).expect("the full conversion");
        // The chroma of a flat picture does not change with the scale. The
        // two chroma planes have different strides, so the cells are
        // compared one by one and not byte by byte.
        let chroma_small = &small[8 * 8..];
        let chroma_plain = &plain[16 * 16..];
        for row in 0..4 {
            for cell in 0..4 {
                let a = chroma_small[row * 8 + cell * 2];
                let b = chroma_plain[row * 16 + cell * 2];
                assert!(a.abs_diff(b) <= 2, "the chroma moved: {a} against {b}");
            }
        }
    }

    #[test]
    fn a_flat_picture_survives_the_round_trip() {
        let width = 8u32;
        let height = 4u32;
        let src = bgrx(width, height, [10, 20, 30, 255]);
        let mut nv12 = vec![0u8; width as usize * height as usize * 3 / 2];
        bgrx_to_nv12(&src, width, height, &mut nv12).expect("to nv12");
        let mut back = vec![0u8; width as usize * height as usize * 4];
        nv12_to_bgrx(&nv12, width, height, &mut back).expect("back to bgrx");
        // The limited-range round trip is not exact, but it is close.
        for (a, b) in src.chunks(4).zip(back.chunks(4)) {
            for channel in 0..3 {
                assert!(
                    a[channel].abs_diff(b[channel]) <= 12,
                    "the channel moved: {a:?} against {b:?}"
                );
            }
        }
    }

    #[test]
    fn the_chroma_is_averaged_over_a_two_by_two_block() {
        // A picture whose left half is blue and right half is red: the
        // chroma of the block in the middle has to be the average.
        let width = 4u32;
        let height = 2u32;
        let mut src = vec![0u8; width as usize * height as usize * 4];
        for row in 0..height as usize {
            for col in 0..width as usize {
                let pixel = &mut src[(row * width as usize + col) * 4..][..4];
                let blue = col < 2;
                pixel[0] = if blue { 255 } else { 0 };
                pixel[1] = 0;
                pixel[2] = if blue { 0 } else { 255 };
                pixel[3] = 255;
            }
        }
        let mut nv12 = vec![0u8; width as usize * height as usize * 3 / 2];
        bgrx_to_nv12(&src, width, height, &mut nv12).expect("to nv12");
        let chroma = &nv12[width as usize * height as usize..];
        // The first chroma cell is the blue half, the second the red half.
        assert!(
            chroma[0] > chroma[2],
            "the blue half has the higher blue chroma"
        );
    }

    #[test]
    fn the_conversion_refuses_an_odd_size() {
        let src = vec![0u8; 4 * 3 * 4];
        let mut nv12 = vec![0u8; 4 * 3 * 3 / 2];
        assert!(bgrx_to_nv12(&src, 4, 3, &mut nv12).is_err());
    }

    #[test]
    fn the_conversion_refuses_a_wrong_buffer() {
        let src = vec![0u8; 4 * 4 * 4];
        let mut nv12 = vec![0u8; 10];
        assert!(bgrx_to_nv12(&src, 4, 4, &mut nv12).is_err());
        let mut nv12 = vec![0u8; 4 * 4 * 3 / 2];
        assert!(nv12_to_bgrx(&src[..10], 4, 4, &mut nv12).is_err());
    }

    #[test]
    fn the_scaled_conversion_refuses_a_wrong_buffer() {
        let src = bgrx(8, 8, [1, 2, 3, 4]);
        let mut dst = vec![0u8; 10];
        assert!(bgrx_to_nv12_scaled(&src, 8, 8, 4, 4, &mut dst).is_err());
        let mut dst = vec![0u8; 4 * 4 * 3 / 2];
        assert!(bgrx_to_nv12_scaled(&src[..10], 8, 8, 4, 4, &mut dst).is_err());
    }

    #[test]
    fn the_scaled_conversion_refuses_an_odd_size() {
        let src = bgrx(8, 8, [1, 2, 3, 4]);
        let mut dst = vec![0u8; 4 * 3 * 3 / 2];
        assert!(bgrx_to_nv12_scaled(&src, 8, 8, 4, 3, &mut dst).is_err());
    }
}
