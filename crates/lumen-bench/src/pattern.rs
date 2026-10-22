//! The synthetic test pattern.
//!
//! The pattern carries a frame counter and a host timestamp in two forms:
//!
//! - a **human readable** strip of digits along the top, so a person can
//!   read the frame number off a photograph of the screen;
//! - a **machine readable** barcode of black and white blocks, so software
//!   can recover the same information from a camera frame.
//!
//! The barcode is a fixed-width binary encoding: one start block, then one
//! block per bit with a known bit width, then a stop block. A camera that
//! samples the barcode at less than one pixel per block still decodes it,
//! because the decoder only needs to classify each block as dark or light.

use lumen_protocol::media::{PixelFormat, VideoFrame};

/// Width of the pattern in pixels.
pub const PATTERN_WIDTH: u32 = 1920;
/// Height of the pattern in pixels.
pub const PATTERN_HEIGHT: u32 = 1080;

/// Width of one barcode bit block in pixels.
pub const BIT_WIDTH: u32 = 8;
/// Height of the barcode strip in pixels.
pub const BARCODE_HEIGHT: u32 = 64;

/// Number of bits in the barcode payload.
const PAYLOAD_BITS: usize = 64;

/// A frame of the test pattern.
#[derive(Debug, Clone)]
pub struct TestPattern {
    /// Frame counter, also encoded in the barcode.
    pub frame_index: u64,
    /// Host timestamp in microseconds, also encoded in the barcode.
    pub timestamp_us: u64,
    /// Pixel data, BGRX.
    pub pixels: Vec<u8>,
}

/// The barcode payload: a 32-bit frame counter and a 32-bit timestamp.
#[must_use]
pub fn payload_bits(frame_index: u64, timestamp_us: u64) -> [bool; PAYLOAD_BITS] {
    let mut bits = [false; PAYLOAD_BITS];
    for i in 0..32 {
        bits[i] = (frame_index >> i) & 1 == 1;
        bits[32 + i] = (timestamp_us >> i) & 1 == 1;
    }
    bits
}

/// Decode a barcode payload.
#[must_use]
pub fn decode_payload(bits: &[bool; PAYLOAD_BITS]) -> (u64, u64) {
    let mut frame = 0u64;
    let mut ts = 0u64;
    for i in 0..32 {
        if bits[i] {
            frame |= 1u64 << i;
        }
        if bits[32 + i] {
            ts |= 1u64 << i;
        }
    }
    (frame, ts)
}

/// The x position where bit `index` starts in the barcode.
#[must_use]
pub fn bit_x(index: usize) -> u32 {
    // Two guard blocks at the start, then the payload bits.
    (index as u32 + 2) * BIT_WIDTH
}

/// Total width of the barcode strip in pixels.
#[must_use]
pub fn barcode_width() -> u32 {
    (PAYLOAD_BITS as u32 + 4) * BIT_WIDTH
}

impl TestPattern {
    /// Render a frame of the pattern.
    #[must_use]
    pub fn render(frame_index: u64, timestamp_us: u64) -> Self {
        let mut pixels = vec![0u8; (PATTERN_WIDTH * PATTERN_HEIGHT * 4) as usize];

        // A mid-grey background with a moving colour band, so that a
        // compression artefact is visible as a change in the image.
        for y in 0..PATTERN_HEIGHT {
            for x in 0..PATTERN_WIDTH {
                let offset = ((y * PATTERN_WIDTH + x) * 4) as usize;
                let band = ((x + frame_index as u32 * 7) / 64) % 3;
                let (r, g, b) = match band {
                    0 => (32u8, 32u8, 32u8),
                    1 => (200u8, 40u8, 40u8),
                    _ => (40u8, 200u8, 40u8),
                };
                pixels[offset] = b;
                pixels[offset + 1] = g;
                pixels[offset + 2] = r;
                pixels[offset + 3] = 255;
            }
        }

        // The barcode strip along the top.
        let bits = payload_bits(frame_index, timestamp_us);
        for (index, bit) in bits.iter().enumerate() {
            let x0 = bit_x(index);
            let value = if *bit { 255u8 } else { 0u8 };
            for y in 0..BARCODE_HEIGHT {
                for x in x0..(x0 + BIT_WIDTH).min(PATTERN_WIDTH) {
                    let offset = ((y * PATTERN_WIDTH + x) * 4) as usize;
                    pixels[offset] = value;
                    pixels[offset + 1] = value;
                    pixels[offset + 2] = value;
                    pixels[offset + 3] = 255;
                }
            }
        }
        // Guard blocks: a black start block and a black stop block.
        for (index, x0) in [0u32, (PAYLOAD_BITS as u32 + 3) * BIT_WIDTH]
            .iter()
            .enumerate()
        {
            let value = if index == 0 { 0u8 } else { 255u8 };
            for y in 0..BARCODE_HEIGHT {
                for x in *x0..(*x0 + BIT_WIDTH).min(PATTERN_WIDTH) {
                    let offset = ((y * PATTERN_WIDTH + x) * 4) as usize;
                    pixels[offset] = value;
                    pixels[offset + 1] = value;
                    pixels[offset + 2] = value;
                    pixels[offset + 3] = 255;
                }
            }
        }

        // The human readable digits: the frame counter in decimal, drawn with
        // a 5x7 bitmap font scaled up.
        let text = format!("{frame_index:010} {timestamp_us:010}");
        draw_text(&mut pixels, &text, 8, BARCODE_HEIGHT + 12, 4);

        Self {
            frame_index,
            timestamp_us,
            pixels,
        }
    }

    /// Convert the pattern into a protocol video frame.
    #[must_use]
    pub fn to_frame(&self) -> VideoFrame {
        VideoFrame::cpu(
            PATTERN_WIDTH,
            PATTERN_HEIGHT,
            PixelFormat::Bgrx,
            self.pixels.clone(),
            self.timestamp_us,
            self.frame_index,
        )
    }
}

/// Draw a string with a built-in 5x7 font, scaled by `scale`.
fn draw_text(pixels: &mut [u8], text: &str, x0: u32, y0: u32, scale: u32) {
    let mut x = x0;
    for ch in text.chars() {
        let glyph = glyph_for(ch);
        for (gy, row) in glyph.iter().enumerate() {
            for (gx, bit) in row.iter().enumerate() {
                if !*bit {
                    continue;
                }
                for sy in 0..scale {
                    for sx in 0..scale {
                        let px = x + gx as u32 * scale + sx;
                        let py = y0 + gy as u32 * scale + sy;
                        if px >= PATTERN_WIDTH || py >= PATTERN_HEIGHT {
                            continue;
                        }
                        let offset = ((py * PATTERN_WIDTH + px) * 4) as usize;
                        pixels[offset] = 255;
                        pixels[offset + 1] = 255;
                        pixels[offset + 2] = 255;
                        pixels[offset + 3] = 255;
                    }
                }
            }
        }
        x += 6 * scale;
    }
}

/// A 5x7 bitmap font, one entry per supported character.
fn glyph_for(ch: char) -> &'static [[bool; 5]; 7] {
    match ch {
        '0' => &[
            [true, true, true, true, true],
            [true, false, false, false, true],
            [true, false, false, false, true],
            [true, false, false, false, true],
            [true, false, false, false, true],
            [true, false, false, false, true],
            [true, true, true, true, true],
        ],
        '1' => &[
            [false, true, false, false, false],
            [true, true, false, false, false],
            [false, true, false, false, false],
            [false, true, false, false, false],
            [false, true, false, false, false],
            [false, true, false, false, false],
            [true, true, true, true, true],
        ],
        '2' => &[
            [true, true, true, true, true],
            [false, false, false, false, true],
            [true, true, true, true, true],
            [true, false, false, false, false],
            [true, false, false, false, false],
            [true, false, false, false, false],
            [true, true, true, true, true],
        ],
        '3' => &[
            [true, true, true, true, true],
            [false, false, false, false, true],
            [true, true, true, true, true],
            [false, false, false, false, true],
            [false, false, false, false, true],
            [false, false, false, false, true],
            [true, true, true, true, true],
        ],
        '4' => &[
            [true, false, false, false, true],
            [true, false, false, false, true],
            [true, false, false, false, true],
            [true, true, true, true, true],
            [false, false, false, false, true],
            [false, false, false, false, true],
            [false, false, false, false, true],
        ],
        '5' => &[
            [true, true, true, true, true],
            [true, false, false, false, false],
            [true, true, true, true, true],
            [false, false, false, false, true],
            [false, false, false, false, true],
            [false, false, false, false, true],
            [true, true, true, true, true],
        ],
        '6' => &[
            [true, true, true, true, true],
            [true, false, false, false, false],
            [true, true, true, true, true],
            [true, false, false, false, true],
            [true, false, false, false, true],
            [true, false, false, false, true],
            [true, true, true, true, true],
        ],
        '7' => &[
            [true, true, true, true, true],
            [false, false, false, false, true],
            [false, false, false, true, false],
            [false, false, true, false, false],
            [false, true, false, false, false],
            [false, true, false, false, false],
            [false, true, false, false, false],
        ],
        '8' => &[
            [true, true, true, true, true],
            [true, false, false, false, true],
            [true, true, true, true, true],
            [true, false, false, false, true],
            [true, false, false, false, true],
            [true, false, false, false, true],
            [true, true, true, true, true],
        ],
        '9' => &[
            [true, true, true, true, true],
            [true, false, false, false, true],
            [true, false, false, false, true],
            [true, true, true, true, true],
            [false, false, false, false, true],
            [false, false, false, false, true],
            [true, true, true, true, true],
        ],
        _ => &[
            [false, false, false, false, false],
            [false, false, false, false, false],
            [false, false, false, false, false],
            [false, false, false, false, false],
            [false, false, false, false, false],
            [false, false, false, false, false],
            [false, false, false, false, false],
        ],
    }
}

/// Recover the payload from a camera image of the barcode strip.
///
/// The image is sampled at `sample_width` pixels wide, which may be smaller
/// than the native pattern width because a camera sees the screen from a
/// distance. Each bit block is classified by its mean brightness.
///
/// # Errors
/// Returns an error string when the strip does not contain a valid barcode.
pub fn decode_barcode(
    image: &[u8],
    width: u32,
    height: u32,
    sample_width: u32,
) -> Result<(u64, u64), String> {
    if width == 0 || height == 0 || sample_width == 0 {
        return Err("empty barcode image".into());
    }
    let native = barcode_width();
    let scale = f64::from(sample_width) / f64::from(native);
    let mut bits = [false; PAYLOAD_BITS];
    for (index, bit) in bits.iter_mut().enumerate() {
        let start = f64::from(bit_x(index)) * scale;
        let end = f64::from(bit_x(index) + BIT_WIDTH) * scale;
        let x0 = start.round() as u32;
        let x1 = ((end.round() as u32).max(x0 + 1)).min(sample_width);
        if x1 <= x0 {
            return Err("barcode too narrow to decode".into());
        }
        let mut sum = 0f64;
        let mut count = 0f64;
        for y in 0..height {
            for x in x0..x1 {
                let offset = ((y * sample_width + x) * 4) as usize;
                if offset + 2 >= image.len() {
                    continue;
                }
                let luma = 0.114 * f64::from(image[offset])
                    + 0.587 * f64::from(image[offset + 1])
                    + 0.299 * f64::from(image[offset + 2]);
                sum += luma;
                count += 1.0;
            }
        }
        if count == 0.0 {
            return Err("barcode has no pixels".into());
        }
        *bit = sum / count > 127.0;
    }
    Ok(decode_payload(&bits))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_roundtrip() {
        let bits = payload_bits(12345, 987_654_321);
        let (frame, ts) = decode_payload(&bits);
        assert_eq!(frame, 12345);
        assert_eq!(ts, 987_654_321);
    }

    #[test]
    fn pattern_has_expected_size() {
        let p = TestPattern::render(1, 2);
        assert_eq!(
            p.pixels.len(),
            (PATTERN_WIDTH * PATTERN_HEIGHT * 4) as usize
        );
    }

    #[test]
    fn barcode_decodes_from_the_rendered_pattern() {
        let p = TestPattern::render(4242, 1_000_000);
        // Simulate a camera: sample the strip at the native width.
        let strip_height = BARCODE_HEIGHT;
        let mut strip = Vec::with_capacity((barcode_width() * strip_height * 4) as usize);
        for y in 0..strip_height {
            for x in 0..barcode_width() {
                let offset = ((y * PATTERN_WIDTH + x) * 4) as usize;
                strip.extend_from_slice(&p.pixels[offset..offset + 4]);
            }
        }
        let (frame, ts) = decode_barcode(&strip, barcode_width(), strip_height, barcode_width())
            .expect("barcode should decode");
        assert_eq!(frame, 4242);
        assert_eq!(ts, 1_000_000);
    }

    #[test]
    fn barcode_decodes_at_lower_resolution() {
        let p = TestPattern::render(7, 99);
        let strip_height = BARCODE_HEIGHT / 2;
        let sample_width = barcode_width() / 2;
        let mut strip = Vec::with_capacity((sample_width * strip_height * 4) as usize);
        for y in 0..strip_height {
            for x in 0..sample_width {
                let src_x = x * 2;
                let offset = ((y * PATTERN_WIDTH + src_x) * 4) as usize;
                strip.extend_from_slice(&p.pixels[offset..offset + 4]);
            }
        }
        let (frame, ts) = decode_barcode(&strip, sample_width, strip_height, sample_width)
            .expect("barcode should decode at half resolution");
        assert_eq!(frame, 7);
        assert_eq!(ts, 99);
    }

    #[test]
    fn barcode_rejects_empty_input() {
        assert!(decode_barcode(&[], 0, 0, 0).is_err());
    }
}
