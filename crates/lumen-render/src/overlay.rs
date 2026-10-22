//! Rasterizes the text of the stream's overlay into a premultiplied ARGB image.
//!
//! The glyphs are the public-domain 8x8 bitmap font of the `font8x8` crate. Each line is drawn
//! at a scale `g`, on a translucent black box. The renderers turn the image into a buffer of
//! their own: a `wl_shm` buffer on Wayland, a child window on X11.

use font8x8::{BASIC_FONTS, UnicodeFonts};

/// Background of the overlay box: premultiplied black at alpha 0xB0, in B, G, R, A order.
const BACKGROUND: [u8; 4] = [0, 0, 0, 0xB0];
/// Glyph pixels: opaque white, in B, G, R, A order.
const FOREGROUND: [u8; 4] = [0xFF; 4];

/// A rasterized overlay: premultiplied ARGB8888, little-endian, row by row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OverlayImage {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixels: Vec<u8>,
}

/// Draws `lines` at scale `g`, one below the other, on the overlay box.
///
/// Returns `None` when there is no text, so the caller can hide the overlay.
#[must_use]
pub(crate) fn rasterize(lines: &[String], g: u32) -> Option<OverlayImage> {
    if lines.iter().all(String::is_empty) {
        return None;
    }
    let g = g.max(1);
    let max_chars = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let max_chars = u32::try_from(max_chars).unwrap_or(u32::MAX);
    let rows = u32::try_from(lines.len()).unwrap_or(u32::MAX);
    let width = 12 * g + max_chars.saturating_mul(8 * g);
    let height = (12 * g + rows.saturating_mul(10 * g)).saturating_sub(2 * g);
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    for px in pixels.as_chunks_mut::<4>().0 {
        *px = BACKGROUND;
    }
    for (row, line) in lines.iter().enumerate() {
        let top = 6 * g + u32::try_from(row).unwrap_or(0) * 10 * g;
        for (col, c) in line.chars().enumerate() {
            let left = 6 * g + u32::try_from(col).unwrap_or(0) * 8 * g;
            let glyph = BASIC_FONTS
                .get(c)
                .or_else(|| BASIC_FONTS.get('?'))
                .unwrap_or([0; 8]);
            draw_glyph(&mut pixels, width, glyph, left, top, g);
        }
    }
    Some(OverlayImage {
        width,
        height,
        pixels,
    })
}

/// Paints one 8x8 glyph with its top left corner at (`left`, `top`), each bit `g` pixels square.
fn draw_glyph(pixels: &mut [u8], width: u32, glyph: [u8; 8], left: u32, top: u32, g: u32) {
    for (gy, bits) in glyph.iter().enumerate() {
        for gx in 0..8u32 {
            if bits >> gx & 1 == 0 {
                continue;
            }
            for dy in 0..g {
                for dx in 0..g {
                    let x = left + gx * g + dx;
                    let y = top + u32::try_from(gy).unwrap_or(0) * g + dy;
                    let i = (y as usize * width as usize + x as usize) * 4;
                    if let Some(px) = pixels.get_mut(i..i + 4) {
                        px.copy_from_slice(&FOREGROUND);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_text_has_no_image() {
        assert!(rasterize(&[], 2).is_none());
        assert!(rasterize(&[String::new()], 2).is_none());
    }

    #[test]
    fn the_box_is_sized_by_the_longest_line_and_the_line_count() {
        let lines = ["ab".to_string(), "abc".to_string()];
        let image = rasterize(&lines, 1).unwrap();
        assert_eq!((image.width, image.height), (36, 30));
        assert_eq!(image.pixels.len(), 36 * 30 * 4);
    }

    #[test]
    fn the_corner_is_background_and_a_glyph_is_drawn_in_the_first_cell() {
        let image = rasterize(&["ab".to_string()], 1).unwrap();
        assert_eq!(&image.pixels[0..4], &BACKGROUND);
        let first_cell_has_glyph = (6..14u32).any(|y| {
            (6..14u32).any(|x| {
                let i = (y as usize * image.width as usize + x as usize) * 4;
                image.pixels[i..i + 4] == FOREGROUND
            })
        });
        assert!(first_cell_has_glyph);
    }

    #[test]
    fn a_character_outside_the_font_draws_the_question_mark() {
        let unknown = rasterize(&["\u{2603}".to_string()], 1).unwrap();
        let question = rasterize(&["?".to_string()], 1).unwrap();
        assert_eq!(unknown.pixels, question.pixels);
    }
}
