//! Software H.264 decoder, the fallback when no hardware decoder is there.
//!
//! The pictures come out as packed NV12 bytes in CPU memory, the same shape the VA-API decoder
//! hands out when it cannot export a DMA-BUF. The client presents both the same way.

use lumen_protocol::media::{EncodedFrame, FrameBuffer, PixelFormat, VideoDecoder, VideoFrame};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;

/// H.264 decoder in software.
pub struct SoftwareDecoder {
    decoder: Decoder,
    frames_decoded: u64,
    decode_errors: u64,
}

impl std::fmt::Debug for SoftwareDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SoftwareDecoder")
            .field("frames_decoded", &self.frames_decoded)
            .field("decode_errors", &self.decode_errors)
            .finish_non_exhaustive()
    }
}

impl SoftwareDecoder {
    /// Open the decoder.
    ///
    /// # Errors
    /// Returns a description when the decoder cannot be opened.
    pub fn new() -> Result<Self, String> {
        let decoder = Decoder::new().map_err(|e| format!("open the software decoder: {e}"))?;
        Ok(Self {
            decoder,
            frames_decoded: 0,
            decode_errors: 0,
        })
    }
}

impl VideoDecoder for SoftwareDecoder {
    fn decode(&mut self, encoded: &EncodedFrame) -> Result<Option<VideoFrame>, String> {
        match self.decode_picture(encoded) {
            Ok(Some(frame)) => {
                self.frames_decoded += 1;
                Ok(Some(frame))
            }
            Ok(None) => Ok(None),
            Err(e) => {
                self.decode_errors += 1;
                Err(e)
            }
        }
    }

    fn frames_decoded(&self) -> u64 {
        self.frames_decoded
    }

    fn decode_errors(&self) -> u64 {
        self.decode_errors
    }
}

impl SoftwareDecoder {
    /// Decodes one access unit into packed NV12 bytes.
    fn decode_picture(&mut self, encoded: &EncodedFrame) -> Result<Option<VideoFrame>, String> {
        let picture = self
            .decoder
            .decode(&encoded.data)
            .map_err(|e| format!("software decode failed: {e}"))?;
        let Some(picture) = picture else {
            // The decoder holds the access unit until it can show a picture.
            return Ok(None);
        };
        let (width, height) = picture.dimensions();
        let (stride_y, stride_u, _) = picture.strides();
        if width == 0 || height == 0 || height % 2 != 0 || width % 2 != 0 {
            return Err(format!(
                "the software decoder gave a picture of {width}x{height}, which is not 4:2:0"
            ));
        }
        let y = picture.y();
        let u = picture.u();
        let v = picture.v();
        let mut data = Vec::with_capacity(width * height + width * height / 2);
        // The luma plane, row by row, without the padding the decoder left in the stride.
        for row in 0..height {
            let start = row * stride_y;
            let line = y
                .get(start..start + width)
                .ok_or_else(|| "the software decoder gave a short luma plane".to_owned())?;
            data.extend_from_slice(line);
        }
        // The chroma plane, interleaved the way NV12 wants it: U and V next to each other.
        for row in 0..height / 2 {
            let start = row * stride_u;
            for column in 0..width / 2 {
                let u = *u
                    .get(start + column)
                    .ok_or_else(|| "the software decoder gave a short chroma plane".to_owned())?;
                let v = *v
                    .get(start + column)
                    .ok_or_else(|| "the software decoder gave a short chroma plane".to_owned())?;
                data.push(u);
                data.push(v);
            }
        }
        Ok(Some(VideoFrame {
            width: u32::try_from(width).map_err(|_| "the picture is too wide".to_owned())?,
            height: u32::try_from(height).map_err(|_| "the picture is too tall".to_owned())?,
            format: PixelFormat::Nv12,
            buffer: FrameBuffer::Cpu { data },
            capture_ts_us: encoded.capture_ts_us,
            index: encoded.index,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The decoder opens without a hardware device.
    #[test]
    fn the_decoder_opens() {
        let decoder = SoftwareDecoder::new().expect("the software decoder opens");
        assert_eq!(decoder.frames_decoded(), 0);
        assert_eq!(decoder.decode_errors(), 0);
    }

    /// A picture that is not H.264 is an error, and it is counted.
    #[test]
    fn rubbish_is_refused_and_counted() {
        let mut decoder = SoftwareDecoder::new().expect("the software decoder opens");
        let rubbish = EncodedFrame {
            data: vec![0xde, 0xad, 0xbe, 0xef],
            is_keyframe: true,
            capture_ts_us: 0,
            index: 0,
        };
        assert!(decoder.decode(&rubbish).is_err());
        assert_eq!(decoder.decode_errors(), 1);
        assert_eq!(decoder.frames_decoded(), 0);
    }
}
