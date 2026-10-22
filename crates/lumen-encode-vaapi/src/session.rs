//! VA-API H.264 encode session: setup, per-frame encode, bitstream output.

use std::{fmt, os::fd::AsFd};

use lumen_ffi_va::{
    DmabufImport, DmabufPlane, OwnedVaBuffer, OwnedVaSurface, VaConfig, VaContext, VaDisplay,
    VaError, sys,
};
use lumen_protocol::media::{
    DRM_FORMAT_MOD_LINEAR, DmabufImage, EncodedFrame, FrameBuffer, PixelFormat, VideoEncoder,
    VideoFrame,
};

use crate::{
    buffer::{self, MiscFrameRate, MiscRateControl, Nv12Planes, Picture, RefPicture, copy_nv12},
    config::{EncoderConfig, H264Profile, RateControl},
    headers, nal,
};

/// Packed headers this crate supplies: one sequence header (SPS then PPS) and slice headers.
/// These are the types FFmpeg's VA-API H.264 encoder declares for this driver.
const PACKED_HEADERS: u32 = sys::VA_ENC_PACKED_HEADER_SEQUENCE | sys::VA_ENC_PACKED_HEADER_SLICE;

/// H.264 encoder on one VA-API display. Each `encode` call is synchronous: the
/// coded buffer is mapped before returning, so there is at most one picture in flight.
///
/// Field order is drop order: the context and buffers go before the config and the display.
pub struct H264Encoder {
    config: EncoderConfig,
    profile: H264Profile,
    ctx: VaContext,
    /// Reconstruction surfaces. One is the current picture, the other the P reference.
    recon: [OwnedVaSurface; 2],
    /// Upload target for CPU frames.
    input: OwnedVaSurface,
    coded: OwnedVaBuffer,
    seq: OwnedVaBuffer,
    misc: Vec<OwnedVaBuffer>,
    /// Sequence (SPS then PPS) packed-header parameter and data buffers, rendered on every IDR.
    packed: [OwnedVaBuffer; 2],
    /// Held so the config outlives the context created from it.
    _va_config: VaConfig,
    display: VaDisplay,
    last: Option<RefPicture>,
    idr_count: u16,
    force_idr: bool,
    frames_encoded: u64,
}

impl H264Encoder {
    /// Open the preferred VA-API display and build an H.264 encode session.
    ///
    /// The device comes from `LUMEN_VA_DEVICE` when set, then `renderD129`, `renderD128`, and
    /// the other `renderD*` nodes. Profile is the first of High, Main, Constrained Baseline
    /// that the driver offers for `VAEntrypointEncSlice`.
    ///
    /// # Errors
    /// Names the device candidates tried when no display opens, the missing profile when the
    /// driver has no H.264 encode entry point, and the failing VA call otherwise.
    pub fn new(config: EncoderConfig) -> Result<Self, String> {
        config.validate()?;
        let display = VaDisplay::open_preferred().map_err(|e| format!("VA-API display: {e}"))?;
        let vendor = display.vendor();

        let profile = H264Profile::LADDER
            .into_iter()
            .find(|p| display.supports(p.va(), sys::VAEntrypointEncSlice))
            .ok_or_else(|| format!("{vendor}: no H.264 VAEntrypointEncSlice profile"))?;
        check_attributes(&display, profile, &config, &vendor)?;

        // Declaring the packed headers tells the driver to expect them from us.
        let config_attribs = [
            sys::VAConfigAttrib {
                type_: sys::VAConfigAttribRTFormat,
                value: sys::VA_RT_FORMAT_YUV420,
            },
            sys::VAConfigAttrib {
                type_: sys::VAConfigAttribRateControl,
                value: config.rate_control.va_bit(),
            },
            sys::VAConfigAttrib {
                type_: sys::VAConfigAttribEncPackedHeaders,
                value: PACKED_HEADERS,
            },
        ];
        let va_config = display
            .create_config(profile.va(), sys::VAEntrypointEncSlice, &config_attribs)
            .map_err(|e| {
                format!(
                    "vaCreateConfig({}, {:?}) on {vendor}: {e}",
                    profile.name(),
                    config.rate_control
                )
            })?;

        let (w, h) = (config.width, config.height);
        let surface = |what: &str| {
            OwnedVaSurface::create(&display, PixelFormat::Nv12, w, h)
                .map_err(|e| format!("create {what} surface {w}x{h} on {vendor}: {e}"))
        };
        let recon = [surface("reconstruction")?, surface("reconstruction")?];
        let input = surface("input")?;
        let ctx = VaContext::create(&va_config, w, h, &[&recon[0], &recon[1]])
            .map_err(|e| format!("vaCreateContext {w}x{h} on {vendor}: {e}"))?;

        let seq = ctx
            .create_buffer(
                sys::VAEncSequenceParameterBufferType,
                &buffer::h264_sequence(&config),
            )
            .map_err(|e| format!("sequence parameter buffer: {e}"))?;
        let mut misc = vec![
            ctx.create_buffer(
                sys::VAEncMiscParameterBufferType,
                &MiscFrameRate::new(config.fps),
            )
            .map_err(|e| format!("frame rate buffer: {e}"))?,
        ];
        if let RateControl::Cbr { bits_per_second } = config.rate_control {
            misc.push(
                ctx.create_buffer(
                    sys::VAEncMiscParameterBufferType,
                    &MiscRateControl::cbr(bits_per_second),
                )
                .map_err(|e| format!("rate control buffer: {e}"))?,
            );
        }

        let packed = packed_buffers(&ctx, &headers::sequence_headers(&config, profile))?;

        // Raw NV12 size, tripled as headroom for the coded output. Edges are capped at
        // MAX_EDGE by validation, so this cannot overflow u32.
        let coded_bytes = w * h * 3;
        let coded = ctx
            .create_empty_buffer(sys::VAEncCodedBufferType, coded_bytes)
            .map_err(|e| format!("coded buffer of {coded_bytes} bytes: {e}"))?;

        Ok(Self {
            config,
            profile,
            ctx,
            recon,
            input,
            coded,
            seq,
            misc,
            packed,
            _va_config: va_config,
            display,
            last: None,
            idr_count: 0,
            force_idr: true,
            frames_encoded: 0,
        })
    }

    /// The H.264 profile the driver accepted.
    #[must_use]
    pub const fn profile(&self) -> H264Profile {
        self.profile
    }

    /// The rate control the encoder is running.
    #[must_use]
    pub const fn rate_control(&self) -> RateControl {
        self.config.rate_control
    }

    /// Change the rate control while the encoder is running.
    ///
    /// The bandwidth policy estimates a target bitrate from the measured loss
    /// and the delay gradient, and pushes it here. libva carries the rate in
    /// a misc-parameter buffer, so the buffer is recreated and the old one
    /// dropped.
    ///
    /// # Errors
    /// Returns an error when the new rate control cannot be expressed, for
    /// example because the driver does not support it.
    pub fn set_rate_control(&mut self, rate_control: RateControl) -> Result<(), String> {
        if rate_control == self.config.rate_control {
            return Ok(());
        }
        // The driver is asked for its attributes again: a driver that did not
        // support CBR at startup will not support it now either.
        let mut probe = self.config;
        probe.rate_control = rate_control;
        let vendor = self.display.vendor();
        check_attributes(&self.display, self.profile, &probe, &vendor)?;
        // The frame rate buffer stays; only the rate control buffer is
        // replaced.
        self.misc.truncate(1);
        if let RateControl::Cbr { bits_per_second } = rate_control {
            let buffer = self
                .ctx
                .create_buffer(
                    sys::VAEncMiscParameterBufferType,
                    &MiscRateControl::cbr(bits_per_second),
                )
                .map_err(|e| format!("rate control buffer: {e}"))?;
            self.misc.push(buffer);
        }
        self.config.rate_control = rate_control;
        Ok(())
    }

    fn check_frame(&self, frame: &VideoFrame) -> Result<(), String> {
        let c = &self.config;
        if frame.width != c.width || frame.height != c.height {
            return Err(format!(
                "frame is {}x{} but the encoder is {}x{}; a resolution change needs a new encoder",
                frame.width, frame.height, c.width, c.height
            ));
        }
        if frame.format != PixelFormat::Nv12 {
            return Err(format!(
                "VA-API H.264 encoder takes NV12 input, got {:?}",
                frame.format
            ));
        }
        Ok(())
    }

    /// Makes the frame an encode input. A CPU frame is copied into `self.input` and yields
    /// `None`. A DMA-BUF frame is imported as a surface that aliases the buffer, so the
    /// encode path copies nothing.
    ///
    /// Fallback, only when the driver rejects the import: copy the buffer into `self.input`
    /// through a read-only mapping. The mapping works only for CPU-mappable DMA-BUFs, such as
    /// linear PipeWire or GBM buffers. Drivers refuse to map their own surfaces, so for
    /// those the fallback error propagates and the producer must deliver `FrameBuffer::Cpu`.
    fn prepare_input(&self, frame: &VideoFrame) -> Result<Option<OwnedVaSurface>, String> {
        let (w, h) = (self.config.width as usize, self.config.height as usize);
        match &frame.buffer {
            FrameBuffer::Cpu { data } => {
                upload_nv12(&self.input, data, Nv12Planes::packed(w, h), w, h)?;
                Ok(None)
            }
            FrameBuffer::Dmabuf(image) => {
                let layout = dmabuf_layout(image, w)?;
                match self.import_dmabuf(image, layout) {
                    Ok(surface) => Ok(Some(surface)),
                    Err(import_err) => {
                        let (fourcc, modifier) = (image.fourcc, image.modifier);
                        if modifier != DRM_FORMAT_MOD_LINEAR {
                            return Err(format!(
                                "DMA-BUF fourcc {fourcc:#010x} modifier {modifier:#018x} rejected by libva: {import_err}; the CPU fallback reads linear layouts only"
                            ));
                        }
                        // Linear CPU fallback. `lumen-ffi-va` holds the mmap `unsafe`, since this
                        // crate forbids it.
                        lumen_ffi_va::read_dmabuf(image.fd.as_fd(), object_size(layout, h), |src| {
                            upload_nv12(&self.input, src, layout, w, h)
                        })
                        .map_err(|e| {
                            format!(
                                "DMA-BUF fourcc {fourcc:#010x} modifier {modifier:#018x} rejected by libva ({import_err}); CPU read fallback: {e}"
                            )
                        })??;
                        Ok(None)
                    }
                }
            }
        }
    }

    /// Imports the frame's DMA-BUF as an NV12 surface through `VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2`.
    /// The fourcc, modifier, and per-plane offsets and pitches all go to libva unchanged.
    fn import_dmabuf(
        &self,
        image: &DmabufImage,
        layout: Nv12Planes,
    ) -> Result<OwnedVaSurface, VaError> {
        let (w, h) = (self.config.width, self.config.height);
        let size = u32::try_from(object_size(layout, h as usize))
            .map_err(|_| VaError::Invalid("DMA-BUF object is larger than libva accepts"))?;
        let planes = [
            DmabufPlane {
                offset: layout.y.0 as u32,
                pitch: layout.y.1 as u32,
            },
            DmabufPlane {
                offset: layout.uv.0 as u32,
                pitch: layout.uv.1 as u32,
            },
        ];
        OwnedVaSurface::import_dmabuf(
            &self.display,
            DmabufImport {
                format: PixelFormat::Nv12,
                width: w,
                height: h,
                fd: image.fd.as_fd(),
                size,
                modifier: image.modifier,
                planes: &planes,
            },
        )
    }
}

/// Parameter and data buffers for one packed header. `has_emulation_bytes` is 1 because
/// `headers` escapes the payload itself.
fn packed_buffers(
    ctx: &VaContext,
    ph: &headers::PackedHeader,
) -> Result<[OwnedVaBuffer; 2], String> {
    let param = sys::VAEncPackedHeaderParameterBuffer {
        type_: ph.ty,
        bit_length: ph.bit_length,
        has_emulation_bytes: 1,
        ..Default::default()
    };
    let param = ctx
        .create_buffer(sys::VAEncPackedHeaderParameterBufferType, &param)
        .map_err(|e| format!("packed header parameters: {e}"))?;
    let data = ctx
        .create_data_buffer(sys::VAEncPackedHeaderDataBufferType, &ph.bytes)
        .map_err(|e| format!("packed header data: {e}"))?;
    Ok([param, data])
}

/// Plane layout of an NV12 DMA-BUF, read from the frame's own plane descriptions. Each plane
/// keeps its own offset and stride, so the chroma plane need not follow the luma plane.
fn dmabuf_layout(image: &DmabufImage, width: usize) -> Result<Nv12Planes, String> {
    let nv12 = PixelFormat::Nv12.drm_fourcc();
    if image.fourcc != nv12 {
        return Err(format!(
            "DMA-BUF fourcc {:#010x} modifier {:#018x} is not NV12 ({nv12:#010x})",
            image.fourcc, image.modifier
        ));
    }
    let &[y, uv] = image.planes() else {
        return Err(format!(
            "NV12 DMA-BUF needs 2 planes, got {} (fourcc {:#010x}, modifier {:#018x})",
            image.planes().len(),
            image.fourcc,
            image.modifier
        ));
    };
    if (y.stride as usize) < width || (uv.stride as usize) < width {
        return Err(format!(
            "NV12 DMA-BUF strides ({} luma, {} chroma) are below the {width}-byte row (fourcc {:#010x}, modifier {:#018x})",
            y.stride, uv.stride, image.fourcc, image.modifier
        ));
    }
    Ok(Nv12Planes {
        y: (y.offset as usize, y.stride as usize),
        uv: (uv.offset as usize, uv.stride as usize),
    })
}

/// Smallest DMA-BUF object that holds both planes: the luma rows, then the chroma rows.
fn object_size(layout: Nv12Planes, height: usize) -> usize {
    let luma_end = layout.y.0 + layout.y.1 * height;
    let chroma_end = layout.uv.0 + layout.uv.1 * height.div_ceil(2);
    luma_end.max(chroma_end)
}

/// Copies a packed or pitched NV12 source into `surface` through a derived VAImage.
///
/// A VA encode input must be a surface, and a VABuffer cannot serve as one. `vaDeriveImage`
/// gives linear CPU access to the surface, which is the closest equivalent to a linear
/// buffer. Drivers that cannot derive an image for the surface fail here with their status.
fn upload_nv12(
    surface: &OwnedVaSurface,
    src: &[u8],
    src_planes: Nv12Planes,
    width: usize,
    height: usize,
) -> Result<(), String> {
    surface
        .sync()
        .map_err(|e| format!("sync input surface: {e}"))?;
    let mut image = surface
        .derive_image()
        .map_err(|e| format!("vaDeriveImage on input surface: {e}"))?;
    let info = image.info();
    if info.format.fourcc != sys::VA_FOURCC_NV12 || info.num_planes != 2 {
        return Err(format!(
            "derived input image is not NV12 (fourcc {:#x}, {} planes)",
            info.format.fourcc, info.num_planes
        ));
    }
    let dst_planes = Nv12Planes {
        y: (info.offsets[0] as usize, info.pitches[0] as usize),
        uv: (info.offsets[1] as usize, info.pitches[1] as usize),
    };
    let mut mapped = image.map().map_err(|e| format!("map input image: {e}"))?;
    copy_nv12(
        src,
        src_planes,
        mapped.bytes_mut(),
        dst_planes,
        width,
        height,
    )
}

/// Rejects drivers that cannot do this configuration, before any buffer is created.
fn check_attributes(
    display: &VaDisplay,
    profile: H264Profile,
    config: &EncoderConfig,
    vendor: &str,
) -> Result<(), String> {
    let mut attribs = [
        sys::VAConfigAttrib {
            type_: sys::VAConfigAttribRTFormat,
            value: 0,
        },
        sys::VAConfigAttrib {
            type_: sys::VAConfigAttribRateControl,
            value: 0,
        },
        sys::VAConfigAttrib {
            type_: sys::VAConfigAttribMaxPictureWidth,
            value: 0,
        },
        sys::VAConfigAttrib {
            type_: sys::VAConfigAttribMaxPictureHeight,
            value: 0,
        },
        sys::VAConfigAttrib {
            type_: sys::VAConfigAttribEncMaxRefFrames,
            value: 0,
        },
        sys::VAConfigAttrib {
            type_: sys::VAConfigAttribEncPackedHeaders,
            value: 0,
        },
    ];
    display
        .config_attributes(profile.va(), sys::VAEntrypointEncSlice, &mut attribs)
        .map_err(|e| format!("vaGetConfigAttributes({}) on {vendor}: {e}", profile.name()))?;
    let [rt, rc, max_w, max_h, refs, packed] = attribs.map(|a| a.value);

    if (rt & sys::VA_RT_FORMAT_YUV420) == 0 {
        return Err(format!(
            "{vendor}: {} lacks 4:2:0 render targets",
            profile.name()
        ));
    }
    if (rc & config.rate_control.va_bit()) == 0 {
        return Err(format!(
            "{vendor}: {} does not offer {:?}",
            profile.name(),
            config.rate_control
        ));
    }
    let unlimited = sys::VA_ATTRIB_NOT_SUPPORTED;
    let too_small = |got: u32, want: u32| got != unlimited && got < want;
    if too_small(max_w, config.width) || too_small(max_h, config.height) {
        return Err(format!(
            "{vendor}: {} max picture {max_w}x{max_h} is smaller than {}x{}",
            profile.name(),
            config.width,
            config.height
        ));
    }
    // `VAConfigAttribEncMaxRefFrames` packs list 0 in the low 16 bits. One reference frame needs list 0.
    if refs.trailing_zeros() >= 16 {
        return Err(format!(
            "{vendor}: {} reports no H.264 reference frames",
            profile.name()
        ));
    }
    if (packed & PACKED_HEADERS) != PACKED_HEADERS {
        return Err(format!(
            "{vendor}: {} lacks packed SPS/PPS support (VAConfigAttribEncPackedHeaders {packed:#x})",
            profile.name()
        ));
    }
    Ok(())
}

impl VideoEncoder for H264Encoder {
    fn encode(&mut self, frame: &VideoFrame) -> Result<EncodedFrame, String> {
        self.check_frame(frame)?;
        let pic = Picture::plan(self.last, self.force_idr, self.idr_count);
        let curr = self.recon[pic.slot].id();
        let imported = self.prepare_input(frame)?;
        let input = imported.as_ref().unwrap_or(&self.input);

        let pic_buf = self
            .ctx
            .create_buffer(
                sys::VAEncPictureParameterBufferType,
                &buffer::h264_picture(
                    self.profile,
                    self.config.rate_control,
                    &pic,
                    curr,
                    self.last,
                    self.coded.id(),
                ),
            )
            .map_err(|e| format!("picture parameter buffer: {e}"))?;
        let slice_buf = self
            .ctx
            .create_buffer(
                sys::VAEncSliceParameterBufferType,
                &buffer::h264_slice(&self.config, &pic, self.last),
            )
            .map_err(|e| format!("slice parameter buffer: {e}"))?;

        // Sequence, rate control and packed SPS/PPS go with each IDR. The driver keeps the
        // sequence state until the next IDR.
        let slice_hdr = packed_buffers(&self.ctx, &headers::slice(self.profile, &pic))?;
        // Same order as FFmpeg's VA-API encoder: sequence parameters, then the picture
        // parameters, then packed headers, then slice parameters.
        let mut bufs: Vec<&OwnedVaBuffer> = Vec::with_capacity(self.misc.len() + 6);
        if pic.idr {
            bufs.push(&self.seq);
            bufs.extend(self.misc.iter());
        }
        bufs.push(&pic_buf);
        if pic.idr {
            bufs.extend(self.packed.iter());
        }
        bufs.extend(slice_hdr.iter());
        bufs.push(&slice_buf);

        self.ctx
            .begin_picture(input)
            .map_err(|e| format!("vaBeginPicture: {e}"))?;
        // Always end the picture, even when rendering failed, so the context is not left mid-picture.
        let rendered = self
            .ctx
            .render_picture(&bufs)
            .map_err(|e| format!("vaRenderPicture: {e}"));
        let ended = self
            .ctx
            .end_picture()
            .map_err(|e| format!("vaEndPicture: {e}"));
        rendered?;
        ended?;

        // Mapping the coded buffer waits for the encode to finish.
        let data = self
            .coded
            .coded_bytes()
            .map_err(|e| format!("map coded buffer: {e}"))?;
        if !nal::starts_with_start_code(&data) {
            return Err("driver coded buffer is not an Annex-B byte stream".into());
        }
        if pic.idr && !nal::contains_nal(&data, nal::NAL_SPS) {
            return Err(format!(
                "IDR access unit has no SPS ({} bytes); the driver did not emit the packed sequence header",
                data.len()
            ));
        }

        self.last = Some(RefPicture {
            slot: pic.slot,
            surface: curr,
            frame_num: pic.frame_num,
            poc: pic.poc,
        });
        if pic.idr {
            self.idr_count = self.idr_count.wrapping_add(1);
        }
        self.force_idr = false;
        self.frames_encoded += 1;
        Ok(EncodedFrame {
            is_keyframe: nal::contains_nal(&data, nal::NAL_IDR_SLICE),
            data,
            capture_ts_us: frame.capture_ts_us,
            index: frame.index,
        })
    }

    fn request_keyframe(&mut self) {
        self.force_idr = true;
    }

    fn frames_encoded(&self) -> u64 {
        self.frames_encoded
    }
}

impl fmt::Debug for H264Encoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("H264Encoder")
            .field("config", &self.config)
            .field("profile", &self.profile)
            .field("frames_encoded", &self.frames_encoded)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::OwnedFd;

    use lumen_protocol::media::DmabufPlane as MediaPlane;

    use super::*;

    fn image(fourcc: u32, modifier: u64, planes: &[MediaPlane]) -> DmabufImage {
        let fd: OwnedFd = std::fs::File::open("/dev/null")
            .expect("open /dev/null")
            .into();
        DmabufImage::new(fd, fourcc, modifier, planes).expect("valid DMA-BUF description")
    }

    #[test]
    fn layout_keeps_each_plane_offset_and_stride() {
        let nv12 = PixelFormat::Nv12.drm_fourcc();
        let planes = [
            MediaPlane {
                offset: 0,
                stride: 1920,
            },
            MediaPlane {
                offset: 1920 * 1080,
                stride: 1920,
            },
        ];
        let layout =
            dmabuf_layout(&image(nv12, DRM_FORMAT_MOD_LINEAR, &planes), 1920).expect("NV12 layout");
        assert_eq!(layout.y, (0, 1920));
        assert_eq!(layout.uv, (1920 * 1080, 1920));
        assert_eq!(object_size(layout, 1080), 1920 * 1080 * 3 / 2);
    }

    #[test]
    fn chroma_need_not_follow_luma_directly() {
        let nv12 = PixelFormat::Nv12.drm_fourcc();
        let planes = [
            MediaPlane {
                offset: 0,
                stride: 2048,
            },
            MediaPlane {
                offset: 1 << 20,
                stride: 2048,
            },
        ];
        let layout = dmabuf_layout(&image(nv12, 0, &planes), 1920).expect("NV12 layout");
        assert_eq!(layout.uv.0, 1 << 20);
        assert_eq!(
            object_size(layout, 1080),
            2048 * 1080,
            "luma rows reach past the chroma start"
        );
    }

    #[test]
    fn rejects_non_nv12_and_names_fourcc_and_modifier() {
        let p010 = PixelFormat::P010.drm_fourcc();
        let planes = [
            MediaPlane {
                offset: 0,
                stride: 1920,
            },
            MediaPlane {
                offset: 1920 * 1080,
                stride: 1920,
            },
        ];
        let err = dmabuf_layout(&image(p010, 0x0100_0000_0000_0001, &planes), 1920)
            .expect_err("P010 is not accepted as NV12");
        assert!(err.contains("not NV12"), "{err}");
        assert!(err.contains("0x30313050"), "fourcc missing: {err}");
        assert!(
            err.contains("0x0100000000000001"),
            "modifier missing: {err}"
        );
    }

    #[test]
    fn rejects_wrong_plane_count_and_short_stride() {
        let nv12 = PixelFormat::Nv12.drm_fourcc();
        let one = [MediaPlane {
            offset: 0,
            stride: 1920,
        }];
        let err = dmabuf_layout(&image(nv12, 0, &one), 1920).expect_err("one plane");
        assert!(err.contains("needs 2 planes"), "{err}");
        let short = [
            MediaPlane {
                offset: 0,
                stride: 100,
            },
            MediaPlane {
                offset: 1920 * 1080,
                stride: 1920,
            },
        ];
        let err = dmabuf_layout(&image(nv12, 0, &short), 1920).expect_err("short stride");
        assert!(err.contains("strides"), "{err}");
    }
}
