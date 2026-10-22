//! VA-API H.264 decoder.
//!
//! Decodes frame pictures made of I and P slices (4:2:0, 8-bit, Baseline, Main or High), with
//! list reordering, adaptive reference marking (MMCO), long-term references, `frame_num` gaps
//! and all three order-count types. Pictures come out in decode order. B slices and interlace
//! are explicit errors.
#![forbid(unsafe_code)]

pub mod bitstream;
pub mod dpb;
#[cfg(test)]
mod fixture;
mod output;
pub mod probe;
mod session;

use lumen_ffi_va::VaDisplay;
use lumen_protocol::media::{EncodedFrame, PixelFormat, VideoDecoder, VideoFrame};

use bitstream::ParamSets;
use session::Session;

/// H.264 decoder on a VA-API device.
#[derive(Debug)]
pub struct VaapiDecoder {
    display: VaDisplay,
    params: ParamSets,
    session: Option<Session>,
    prefer_dmabuf: bool,
    frames_decoded: u64,
    decode_errors: u64,
}

impl VaapiDecoder {
    /// Open the preferred VA-API device: `LUMEN_VA_DEVICE` if set, else `renderD129`, then
    /// `renderD128`, then any other render node.
    ///
    /// Frames come out as packed NV12 bytes by default. [`Self::dmabuf_output`] asks for DMA-BUF
    /// images instead, for each picture whose driver export can be described.
    ///
    /// # Errors
    /// Returns a description when libva cannot be loaded or no render node yields a display.
    pub fn new() -> Result<Self, String> {
        let display = VaDisplay::open_preferred().map_err(|e| e.to_string())?;
        Ok(Self {
            display,
            params: ParamSets::default(),
            session: None,
            prefer_dmabuf: false,
            frames_decoded: 0,
            decode_errors: 0,
        })
    }

    /// Return packed NV12 bytes (the default), never a DMA-BUF.
    #[must_use]
    pub fn cpu_output(mut self) -> Self {
        self.prefer_dmabuf = false;
        self
    }

    /// Return DMA-BUF images for every picture whose driver export describes as one. A picture
    /// the export cannot describe still comes out as CPU NV12.
    #[must_use]
    pub fn dmabuf_output(mut self) -> Self {
        self.prefer_dmabuf = true;
        self
    }

    /// Chooses DMA-BUF or CPU output for the pictures decoded from now on.
    pub fn set_dmabuf_output(&mut self, enabled: bool) {
        self.prefer_dmabuf = enabled;
    }

    /// Barrier: wait until no VA work is in flight. `decode` syncs each picture before it
    /// returns, so nothing is pending between calls.
    ///
    /// # Errors
    /// Returns a description when a sync fails.
    pub fn flush(&mut self) -> Result<(), String> {
        self.session.as_ref().map_or(Ok(()), Session::sync_all)
    }

    fn decode_picture(&mut self, encoded: &EncodedFrame) -> Result<Option<VideoFrame>, String> {
        let Some(picture) = self
            .params
            .ingest_access_unit(&encoded.data)
            .map_err(str::to_owned)?
        else {
            return Ok(None);
        };
        // A session for a different SPS is dropped before the new one allocates, and the new
        // one starts with an empty reference store.
        let reusable = self.session.take().filter(|s| s.matches(&picture.sps));
        let mut session = match reusable {
            Some(session) => session,
            None => Session::new(&self.display, &picture.sps)?,
        };
        let buffer = session.decode(&picture, self.prefer_dmabuf);
        self.session = Some(session);
        Ok(Some(VideoFrame {
            width: picture.sps.width,
            height: picture.sps.height,
            format: PixelFormat::Nv12,
            buffer: buffer?,
            capture_ts_us: encoded.capture_ts_us,
            index: encoded.index,
        }))
    }
}

impl VideoDecoder for VaapiDecoder {
    fn decode(&mut self, encoded: &EncodedFrame) -> Result<Option<VideoFrame>, String> {
        match self.decode_picture(encoded) {
            Ok(Some(frame)) => {
                self.frames_decoded += 1;
                Ok(Some(frame))
            }
            Ok(None) => Ok(None),
            Err(error) => {
                self.decode_errors += 1;
                Err(error)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{
        FLAT_IDR_AU, FLAT_P1_AU, FLAT_P2_AU, FLAT_P3_AU, FLAT_UV, FLAT_Y, IDR_64X64, IDR_64X64_UV,
        IDR_64X64_Y,
    };
    use lumen_protocol::media::FrameBuffer;

    fn access_unit(data: &[u8]) -> EncodedFrame {
        EncodedFrame {
            data: data.to_vec(),
            is_keyframe: true,
            capture_ts_us: 1_000,
            index: 7,
        }
    }

    /// `None` after printing the reason when no VA-API device can open on this machine.
    fn decoder_or_skip(test: &str) -> Option<VaapiDecoder> {
        match VaapiDecoder::new() {
            Ok(decoder) => Some(decoder),
            Err(reason) => {
                eprintln!("SKIP {test}: no usable VA-API device: {reason}");
                None
            }
        }
    }

    /// Asserts that a frame is the flat 64x64 gray picture with the given sample values.
    fn assert_flat_64x64(frame: &VideoFrame, luma: u8, chroma: u8) {
        assert_eq!((frame.width, frame.height), (64, 64));
        let FrameBuffer::Cpu { data } = &frame.buffer else {
            panic!("cpu_output must yield a CPU frame");
        };
        let luma_len = 64 * 64;
        assert_eq!(data.len(), PixelFormat::Nv12.frame_len(64, 64));
        assert!(
            data[..luma_len].iter().all(|&y| y == luma),
            "luma differs from the reference"
        );
        assert!(
            data[luma_len..].iter().all(|&c| c == chroma),
            "chroma differs from the reference"
        );
    }

    #[test]
    fn idr_fixture_decodes_to_reference_samples() {
        let Some(decoder) = decoder_or_skip("idr_fixture_decodes_to_reference_samples") else {
            return;
        };
        let mut decoder = decoder.cpu_output();
        let frame = decoder
            .decode(&access_unit(IDR_64X64))
            .expect("fixture decodes")
            .expect("one complete picture");
        assert_eq!((frame.index, frame.capture_ts_us), (7, 1_000));
        assert_flat_64x64(&frame, IDR_64X64_Y, IDR_64X64_UV);
        assert_eq!((decoder.frames_decoded(), decoder.decode_errors()), (1, 0));
    }

    #[test]
    fn default_output_is_cpu_nv12() {
        let Some(mut decoder) = decoder_or_skip("default_output_is_cpu_nv12") else {
            return;
        };
        let frame = decoder
            .decode(&access_unit(IDR_64X64))
            .expect("fixture decodes")
            .expect("one complete picture");
        assert_flat_64x64(&frame, IDR_64X64_Y, IDR_64X64_UV);
    }

    #[test]
    fn dmabuf_output_exports_nv12_with_the_export_modifier() {
        let Some(decoder) = decoder_or_skip("dmabuf_output_exports_nv12_with_the_export_modifier")
        else {
            return;
        };
        let mut decoder = decoder.dmabuf_output();
        let frame = decoder
            .decode(&access_unit(IDR_64X64))
            .expect("fixture decodes")
            .expect("one complete picture");
        assert_eq!((frame.width, frame.height), (64, 64));
        let FrameBuffer::Dmabuf(image) = &frame.buffer else {
            panic!("dmabuf_output must yield a DMA-BUF frame for a describable export");
        };
        assert_eq!(image.fourcc, u32::from_le_bytes(*b"NV12"));
        // The IDR is the first picture, so it decodes into pool slot 0. The driver's own export
        // of that surface is the reference for the image's fourcc, modifier and planes.
        let session = decoder.session.as_ref().expect("session after decode");
        let export = session
            .export_surface(0)
            .expect("export the decoded surface");
        assert_eq!(
            image.fourcc, export.drm_format,
            "fourcc differs from the export"
        );
        assert_eq!(
            image.modifier, export.modifier,
            "modifier differs from the export"
        );
        let exported: Vec<(u32, u32)> = export.planes.iter().map(|p| (p.offset, p.pitch)).collect();
        let described: Vec<(u32, u32)> = image
            .planes()
            .iter()
            .map(|p| (p.offset, p.stride))
            .collect();
        assert_eq!(described, exported, "planes differ from the export");
        eprintln!(
            "decoded surface DMA-BUF: fourcc {:#x} modifier {:#x} planes {:?}",
            image.fourcc, image.modifier, described
        );
    }

    /// The video processor converts a decoded DMA-BUF picture to BGRX on the GPU. Every colour
    /// channel must match the CPU conversion of the CPU NV12 decode of the same picture within 2.
    #[test]
    fn decoded_dmabuf_converts_to_bgrx_like_the_cpu_path() {
        use lumen_ffi_va::{OwnedVaSurface, VideoProcessor};
        use lumen_protocol::color;

        const TEST: &str = "decoded_dmabuf_converts_to_bgrx_like_the_cpu_path";
        let Some(decoder) = decoder_or_skip(TEST) else {
            return;
        };
        let mut cpu = decoder.cpu_output();
        let reference = cpu
            .decode(&access_unit(IDR_64X64))
            .expect("fixture decodes")
            .expect("one complete picture");
        let FrameBuffer::Cpu { data: nv12 } = &reference.buffer else {
            panic!("cpu_output must yield a CPU frame");
        };
        let mut expected = vec![0u8; PixelFormat::Bgrx.frame_len(64, 64)];
        color::nv12_to_bgrx(nv12, 64, 64, &mut expected).expect("reference conversion");

        let Some(decoder) = decoder_or_skip(TEST) else {
            return;
        };
        let display = decoder.display.clone();
        let mut gpu = decoder.dmabuf_output();
        let frame = gpu
            .decode(&access_unit(IDR_64X64))
            .expect("fixture decodes")
            .expect("one complete picture");
        let FrameBuffer::Dmabuf(image) = &frame.buffer else {
            eprintln!("SKIP {TEST}: the driver export cannot be described as a DMA-BUF frame");
            return;
        };
        let source = OwnedVaSurface::import_frame(&display, image, PixelFormat::Nv12, 64, 64)
            .expect("import the decoded DMA-BUF picture");
        let mut vpp = match VideoProcessor::new(&display, 64, 64) {
            Ok(vpp) => vpp,
            Err(reason) => {
                eprintln!("SKIP {TEST}: no GPU video processing: {reason}");
                return;
            }
        };
        let converted = vpp
            .convert(&source, PixelFormat::Bgrx, 64, 64)
            .expect("convert on the GPU");
        let mut readback = converted
            .get_image_as(PixelFormat::Bgrx)
            .expect("read the converted BGRX picture back");
        let info = *readback.info();
        let mapped = readback.map().expect("map the converted image");
        let bytes = mapped.bytes();
        let pitch = info.pitches[0] as usize;
        let offset = info.offsets[0] as usize;
        let mut worst = 0u8;
        for y in 0..64 {
            for x in 0..64 {
                for channel in 0..3 {
                    let got = bytes[offset + y * pitch + x * 4 + channel];
                    let want = expected[(y * 64 + x) * 4 + channel];
                    worst = worst.max(got.abs_diff(want));
                }
            }
        }
        eprintln!("VPP BGRX vs CPU BGRX: worst channel difference {worst}");
        assert!(
            worst <= 2,
            "GPU conversion differs from the CPU path by {worst}"
        );
        eprintln!(
            "SKIP {TEST}: the 1280x720 assertion needs a 1280x720 decode fixture, none is in the decoder tests"
        );
    }

    #[test]
    fn parameter_sets_alone_are_not_a_frame() {
        let Some(mut decoder) = decoder_or_skip("parameter_sets_alone_are_not_a_frame") else {
            return;
        };
        let parameter_sets = &IDR_64X64[..34];
        assert!(
            decoder
                .decode(&access_unit(parameter_sets))
                .expect("parameter sets are not an error")
                .is_none()
        );
        assert_eq!(decoder.decode_errors(), 0);
    }

    #[test]
    fn b_slice_is_an_error_and_is_counted() {
        let Some(mut decoder) = decoder_or_skip("b_slice_is_an_error_and_is_counted") else {
            return;
        };
        // SPS and PPS, then a B slice (slice_type 1).
        let mut au = IDR_64X64[..34].to_vec();
        au.extend_from_slice(&[0x00, 0x00, 0x01, 0x41, 0xA8]);
        assert!(decoder.decode(&access_unit(&au)).is_err());
        assert_eq!(decoder.decode_errors(), 1);
        assert_eq!(decoder.frames_decoded(), 0);
    }

    #[test]
    fn flush_after_decode_succeeds() {
        let Some(decoder) = decoder_or_skip("flush_after_decode_succeeds") else {
            return;
        };
        let mut decoder = decoder.cpu_output();
        decoder
            .decode(&access_unit(IDR_64X64))
            .expect("fixture decodes");
        decoder.flush().expect("flush");
    }

    #[test]
    fn flat_ippp_frames_decode_to_reference_samples() {
        let Some(decoder) = decoder_or_skip("flat_ippp_frames_decode_to_reference_samples") else {
            return;
        };
        let mut decoder = decoder.cpu_output();
        for (n, au) in [FLAT_IDR_AU, FLAT_P1_AU, FLAT_P2_AU, FLAT_P3_AU]
            .into_iter()
            .enumerate()
        {
            let frame = decoder
                .decode(&access_unit(au))
                .unwrap_or_else(|e| panic!("picture {n} failed: {e}"))
                .unwrap_or_else(|| panic!("picture {n} produced no frame"));
            assert_flat_64x64(&frame, FLAT_Y, FLAT_UV);
        }
        assert_eq!((decoder.frames_decoded(), decoder.decode_errors()), (4, 0));
    }

    #[test]
    fn p_slice_without_reference_is_an_error() {
        let Some(mut decoder) = decoder_or_skip("p_slice_without_reference_is_an_error") else {
            return;
        };
        // Parameter sets and a P slice, but no IDR before it.
        let au = [&FLAT_IDR_AU[..34], FLAT_P1_AU].concat();
        let error = decoder.decode(&access_unit(&au)).expect_err("no reference");
        assert!(error.contains("without a reference"), "{error}");
        assert_eq!(decoder.decode_errors(), 1);
        assert_eq!(decoder.frames_decoded(), 0);
    }

    #[test]
    fn a_picture_that_references_a_lost_frame_is_an_error() {
        let Some(decoder) = decoder_or_skip("a_picture_that_references_a_lost_frame_is_an_error")
        else {
            return;
        };
        let mut decoder = decoder.cpu_output();
        decoder
            .decode(&access_unit(FLAT_IDR_AU))
            .expect("IDR decodes");
        // frame_num 3 directly after frame_num 0: frames 1 and 2 are inferred, and frame 3
        // predicts from frame 2, which was lost.
        let error = decoder
            .decode(&access_unit(FLAT_P3_AU))
            .expect_err("the lost reference is refused");
        assert!(error.contains("frame_num gap"), "{error}");
        assert_eq!((decoder.frames_decoded(), decoder.decode_errors()), (1, 1));
    }
}
