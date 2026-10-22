//! Raw (uncompressed) codec.
//!
//! The raw codec exists so the framing, FEC and transport layers can be
//! tested end to end without any codec present. It is never used for latency
//! measurements.

use crate::media::{EncodedFrame, PixelFormat, VideoDecoder, VideoEncoder, VideoFrame};

/// Encoder that passes frames through unchanged.
#[derive(Debug, Default)]
pub struct RawEncoder {
    frames: u64,
    force_key: bool,
}

impl RawEncoder {
    /// Create a raw encoder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl VideoEncoder for RawEncoder {
    fn encode(&mut self, frame: &VideoFrame) -> Result<EncodedFrame, String> {
        let data = match &frame.buffer {
            crate::media::FrameBuffer::Cpu { data } => data.clone(),
            crate::media::FrameBuffer::Dmabuf { .. } => {
                return Err("raw encoder needs a CPU frame".into());
            }
        };
        self.frames += 1;
        let is_keyframe = self.force_key || self.frames == 1;
        self.force_key = false;
        Ok(EncodedFrame {
            data,
            is_keyframe,
            capture_ts_us: frame.capture_ts_us,
            index: frame.index,
        })
    }

    fn request_keyframe(&mut self) {
        self.force_key = true;
    }

    fn frames_encoded(&self) -> u64 {
        self.frames
    }
}

/// Decoder that passes encoded data back through as a frame.
#[derive(Debug)]
pub struct RawDecoder {
    width: u32,
    height: u32,
    format: PixelFormat,
    frames: u64,
    errors: u64,
}

impl RawDecoder {
    /// Create a raw decoder for a fixed frame geometry.
    #[must_use]
    pub fn new(width: u32, height: u32, format: PixelFormat) -> Self {
        Self {
            width,
            height,
            format,
            frames: 0,
            errors: 0,
        }
    }
}

impl VideoDecoder for RawDecoder {
    fn decode(&mut self, encoded: &EncodedFrame) -> Result<Option<VideoFrame>, String> {
        let expected = self.format.frame_len(self.width, self.height);
        if encoded.data.len() != expected {
            self.errors += 1;
            return Err(format!(
                "raw frame size mismatch: expected {expected}, got {}",
                encoded.data.len()
            ));
        }
        self.frames += 1;
        Ok(Some(VideoFrame::cpu(
            self.width,
            self.height,
            self.format,
            encoded.data.clone(),
            encoded.capture_ts_us,
            encoded.index,
        )))
    }

    fn frames_decoded(&self) -> u64 {
        self.frames
    }

    fn decode_errors(&self) -> u64 {
        self.errors
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let data = vec![7u8; PixelFormat::Nv12.frame_len(16, 16)];
        let frame = VideoFrame::cpu(16, 16, PixelFormat::Nv12, data.clone(), 123, 0);
        let mut enc = RawEncoder::new();
        let mut dec = RawDecoder::new(16, 16, PixelFormat::Nv12);
        let encoded = enc.encode(&frame).unwrap();
        assert!(encoded.is_keyframe);
        let decoded = dec.decode(&encoded).unwrap().unwrap();
        assert_eq!(decoded.buffer.cpu_len(), Some(data.len()));
        assert_eq!(decoded.capture_ts_us, 123);
    }

    #[test]
    fn size_mismatch_is_an_error() {
        let mut dec = RawDecoder::new(16, 16, PixelFormat::Nv12);
        let encoded = EncodedFrame {
            data: vec![0; 3],
            is_keyframe: true,
            capture_ts_us: 0,
            index: 0,
        };
        assert!(dec.decode(&encoded).is_err());
        assert_eq!(dec.decode_errors(), 1);
    }
}
