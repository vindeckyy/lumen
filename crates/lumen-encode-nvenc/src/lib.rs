//! NVENC H.264 encoder.
//!
//! Takes the same [`EncoderConfig`] as the VA-API encoder and implements the same
//! [`VideoEncoder`] trait, so the host can use either. Frames are copied into NVENC's input
//! buffer from the CPU: a CPU frame directly, a linear DMA-BUF through a synced read-only
//! mapping. NV12, BGRX and RGBA are accepted; NVENC converts RGB to YUV itself.

use std::collections::VecDeque;
use std::os::fd::AsFd;

use lumen_encode_vaapi::{EncoderConfig, RateControl};
use lumen_ffi_nvenc::{InputFormat, Nvenc, Rate, SessionConfig};
use lumen_protocol::media::{
    DRM_FORMAT_MOD_LINEAR, DmabufImage, EncodedFrame, FrameBuffer, PixelFormat, VideoEncoder,
    VideoFrame,
};

/// An NVENC H.264 encoder.
#[derive(Debug)]
pub struct NvencEncoder {
    config: EncoderConfig,
    session: Option<Nvenc>,
    keyframe: bool,
    frames: u64,
    /// Source indices of the frames that are valid references, oldest first: NVENC's window.
    references: VecDeque<u64>,
    /// The next frame recovers from a loss and carries a recovery point.
    recovery: bool,
}

impl NvencEncoder {
    /// Opens an NV12 session. A frame of another format reopens it for that format.
    ///
    /// # Errors
    /// Returns why the configuration is invalid or NVENC cannot be opened.
    pub fn new(config: EncoderConfig) -> Result<Self, String> {
        config.validate()?;
        // Opened now, so a host without NVENC fails at start-up and not at the first frame.
        let session =
            Nvenc::open(session_config(&config, InputFormat::Nv12)).map_err(|e| e.to_string())?;
        Ok(Self {
            config,
            session: Some(session),
            keyframe: true,
            frames: 0,
            references: VecDeque::new(),
            recovery: false,
        })
    }

    /// The rate control in use.
    #[must_use]
    pub const fn rate_control(&self) -> RateControl {
        self.config.rate_control
    }

    /// Changes the rate control without a keyframe.
    ///
    /// # Errors
    /// Returns why the rate is invalid or the reconfiguration failed.
    pub fn set_rate_control(&mut self, rate_control: RateControl) -> Result<(), String> {
        self.config.with_rate_control(rate_control).validate()?;
        if let Some(session) = &mut self.session {
            session
                .set_rate(rate(rate_control))
                .map_err(|e| e.to_string())?;
        }
        self.config.rate_control = rate_control;
        Ok(())
    }
}

impl VideoEncoder for NvencEncoder {
    fn encode(&mut self, frame: &VideoFrame) -> Result<EncodedFrame, String> {
        if frame.width != self.config.width || frame.height != self.config.height {
            return Err(format!(
                "frame is {}x{}, the encoder is {}x{}",
                frame.width, frame.height, self.config.width, self.config.height
            ));
        }
        let format = input_format(frame.format)?;
        if self
            .session
            .as_ref()
            .is_none_or(|s| s.session().format != format)
        {
            self.session = None;
            self.session =
                Some(Nvenc::open(session_config(&self.config, format)).map_err(|e| e.to_string())?);
            self.keyframe = true;
        }
        let session = self.session.as_mut().expect("opened above");
        let (width, height) = (frame.width as usize, frame.height as usize);
        let force = std::mem::take(&mut self.keyframe);
        let packet = session
            // The timestamp is the source index, which names a frame for invalidation.
            .encode(force, frame.index, |dst, pitch| match &frame.buffer {
                FrameBuffer::Cpu { data } => {
                    let layout = packed_layout(format, width, height);
                    copy_planes(dst, pitch, data, layout, format, width, height)
                }
                FrameBuffer::Dmabuf(image) => read_dmabuf(image, dst, pitch, format, width, height),
            })
            .map_err(|e| e.to_string())?;
        self.frames += 1;
        if packet.keyframe {
            self.references.clear();
        }
        let window = lumen_ffi_nvenc::ref_frames(self.config.width, self.config.height) as usize;
        if self.references.len() >= window {
            self.references.pop_front();
        }
        self.references.push_back(frame.index);
        let recovery = std::mem::take(&mut self.recovery) && !packet.keyframe;
        Ok(EncodedFrame {
            data: if recovery {
                lumen_encode_vaapi::nal::with_recovery_point(&packet.data)
            } else {
                packet.data
            },
            is_keyframe: packet.keyframe,
            capture_ts_us: frame.capture_ts_us,
            index: frame.index,
        })
    }

    fn request_keyframe(&mut self) {
        self.keyframe = true;
    }

    fn recover_from(&mut self, index: u64) -> bool {
        let Some(session) = self.session.as_mut() else {
            self.keyframe = true;
            return false;
        };
        if !self.references.contains(&index) {
            self.keyframe = true;
            return false;
        }
        // Every frame after the anchor is lost to the client; none of them may be predicted
        // from, now or by a later recovery.
        while let Some(&newest) = self.references.back()
            && newest > index
        {
            if session.invalidate(newest).is_err() {
                self.keyframe = true;
                return false;
            }
            self.references.pop_back();
        }
        self.recovery = true;
        true
    }

    fn frames_encoded(&self) -> u64 {
        self.frames
    }
}

fn session_config(config: &EncoderConfig, format: InputFormat) -> SessionConfig {
    SessionConfig {
        width: config.width,
        height: config.height,
        fps: config.fps,
        rate: rate(config.rate_control),
        format,
    }
}

const fn rate(rate_control: RateControl) -> Rate {
    match rate_control {
        RateControl::Cqp { qp } => Rate::Cqp { qp },
        RateControl::Cbr { bits_per_second } => Rate::Cbr { bits_per_second },
    }
}

fn input_format(format: PixelFormat) -> Result<InputFormat, String> {
    match format {
        PixelFormat::Nv12 => Ok(InputFormat::Nv12),
        PixelFormat::Bgrx => Ok(InputFormat::Bgrx),
        PixelFormat::Rgba => Ok(InputFormat::Rgba),
        other => Err(format!("NVENC input does not take {other:?}")),
    }
}

/// Where the planes of a source are: (offset, stride) of the first plane, and of the chroma
/// plane for NV12.
type Layout = [(usize, usize); 2];

fn packed_layout(format: InputFormat, width: usize, height: usize) -> Layout {
    match format {
        InputFormat::Nv12 => [(0, width), (width * height, width)],
        InputFormat::Bgrx | InputFormat::Rgba => [(0, width * 4), (0, 0)],
    }
}

/// Copies the rows of `src` into the pitched input buffer.
fn copy_planes(
    dst: &mut [u8],
    pitch: usize,
    src: &[u8],
    layout: Layout,
    format: InputFormat,
    width: usize,
    height: usize,
) -> Result<(), String> {
    let mut copy = |dst_start: usize, (offset, stride): (usize, usize), row: usize, rows: usize| {
        for y in 0..rows {
            let from = offset + y * stride;
            let to = dst_start + y * pitch;
            let source = src.get(from..from + row).ok_or_else(|| {
                format!("the frame is {} bytes, too short for its rows", src.len())
            })?;
            dst[to..to + row].copy_from_slice(source);
        }
        Ok::<(), String>(())
    };
    match format {
        InputFormat::Nv12 => {
            copy(0, layout[0], width, height)?;
            copy(pitch * height, layout[1], width, height.div_ceil(2))
        }
        InputFormat::Bgrx | InputFormat::Rgba => copy(0, layout[0], width * 4, height),
    }
}

fn read_dmabuf(
    image: &DmabufImage,
    dst: &mut [u8],
    pitch: usize,
    format: InputFormat,
    width: usize,
    height: usize,
) -> Result<(), String> {
    if image.modifier != DRM_FORMAT_MOD_LINEAR {
        return Err(format!(
            "DMA-BUF modifier {:#018x} is tiled; the NVENC path reads linear buffers only",
            image.modifier
        ));
    }
    let planes = image.planes();
    let plane = |i: usize| {
        planes
            .get(i)
            .map(|p| (p.offset as usize, p.stride as usize))
            .ok_or_else(|| format!("the DMA-BUF has {} planes", planes.len()))
    };
    let layout = match format {
        InputFormat::Nv12 => [plane(0)?, plane(1)?],
        InputFormat::Bgrx | InputFormat::Rgba => [plane(0)?, (0, 0)],
    };
    let len = match format {
        InputFormat::Nv12 => {
            (layout[0].0 + layout[0].1 * height).max(layout[1].0 + layout[1].1 * height.div_ceil(2))
        }
        InputFormat::Bgrx | InputFormat::Rgba => layout[0].0 + layout[0].1 * height,
    };
    lumen_ffi_va::read_dmabuf(image.fd.as_fd(), len, |src| {
        copy_planes(dst, pitch, src, layout, format, width, height)
    })
    .map_err(|e| format!("cannot read the DMA-BUF: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_rows_land_at_the_pitch() {
        // 4x2 NV12: two luma rows, one chroma row.
        let src: Vec<u8> = (0..12).collect();
        let pitch = 8;
        let mut dst = vec![0xff; pitch * 3];
        let layout = packed_layout(InputFormat::Nv12, 4, 2);
        copy_planes(&mut dst, pitch, &src, layout, InputFormat::Nv12, 4, 2).unwrap();
        assert_eq!(&dst[0..4], &[0, 1, 2, 3]);
        assert_eq!(&dst[8..12], &[4, 5, 6, 7]);
        assert_eq!(&dst[16..20], &[8, 9, 10, 11]);
        assert_eq!(dst[4], 0xff, "padding is not written");
    }

    #[test]
    fn a_short_frame_is_refused() {
        let mut dst = vec![0; 64];
        let layout = packed_layout(InputFormat::Bgrx, 4, 2);
        assert!(copy_planes(&mut dst, 16, &[0; 20], layout, InputFormat::Bgrx, 4, 2).is_err());
    }
}
