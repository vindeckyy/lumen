//! The decode and present stage.
//!
//! Decodes each access unit and presents the frame. A picture the decoder exports as a DMA-BUF
//! is presented without a CPU copy when the compositor can take it: either the decoder's NV12
//! buffer as it is, or a BGRX buffer the GPU makes from it. Otherwise the decoder hands back CPU
//! NV12 and the stage converts it to BGRX before the compositor sees it. That copy is the
//! deviation recorded as D37 and D46.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};

use lumen_decode_vaapi::VaapiDecoder;
use lumen_ffi_va::{OwnedVaSurface, VaDisplay, VideoProcessor};
use lumen_host_service::control::ControlMessage;
use lumen_protocol::color;
use lumen_protocol::media::DmabufImage;
use lumen_protocol::{FrameBuffer, FrameSink, PixelFormat, VideoDecoder, VideoFrame};
use lumen_render::Renderer;

use crate::Counters;
use crate::receive::{AccessUnit, monotonic_us};

/// How the stage presents DMA-BUF pictures. Chosen once, from the first one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PresentPlan {
    /// The decoder's NV12 DMA-BUF goes to the compositor as it is.
    DirectNv12,
    /// The GPU makes a BGRX DMA-BUF from the decoder's NV12 one.
    VppBgrx,
    /// Pictures are presented from CPU memory. The decoder is asked for CPU output.
    Cpu,
}

/// Zero-copy presents that fail in a row before the stage gives them up for the session.
const ZERO_COPY_ERROR_LIMIT: u32 = 3;

/// The GPU conversion of the `VppBgrx` plan: the decoder's NV12 DMA-BUF in, a BGRX DMA-BUF out.
#[derive(Debug)]
pub(crate) struct Converter {
    display: VaDisplay,
    processor: VideoProcessor,
    width: u32,
    height: u32,
}

impl Converter {
    /// Opens the device the decoder opens, so the pictures it exports import here.
    fn new(width: u32, height: u32) -> Result<Self, String> {
        let display = VaDisplay::open_preferred().map_err(|e| e.to_string())?;
        let processor = VideoProcessor::new(&display, width, height).map_err(|e| e.to_string())?;
        Ok(Self {
            display,
            processor,
            width,
            height,
        })
    }

    /// The BGRX DMA-BUF of an NV12 DMA-BUF picture of `width` by `height`.
    fn bgrx(
        &mut self,
        image: &DmabufImage,
        width: u32,
        height: u32,
    ) -> Result<DmabufImage, String> {
        if (self.width, self.height) != (width, height) {
            self.processor =
                VideoProcessor::new(&self.display, width, height).map_err(|e| e.to_string())?;
            self.width = width;
            self.height = height;
        }
        let source =
            OwnedVaSurface::import_frame(&self.display, image, PixelFormat::Nv12, width, height)
                .map_err(|e| e.to_string())?;
        let converted = self
            .processor
            .convert(&source, PixelFormat::Bgrx, width, height)
            .map_err(|e| e.to_string())?;
        converted
            .export_dmabuf()
            .map_err(|e| e.to_string())?
            .into_image()
    }
}

/// The decode and present stage.
#[derive(Debug)]
pub struct PresentStage {
    /// The renderer.
    pub(crate) renderer: Renderer,
    /// The access units to decode.
    pub(crate) access_units: Receiver<AccessUnit>,
    /// Where the requests for the host go.
    pub(crate) requests: SyncSender<ControlMessage>,
    /// Whether the pipeline is still running.
    pub(crate) running: Arc<AtomicBool>,
    /// Shared counters.
    pub(crate) counters: Arc<Counters>,
    /// The event log, which carries the progress of the stream.
    pub(crate) events: crate::events::EventLog,
    /// The video clock the audio is placed on. The present stage writes the
    /// capture and presentation timestamps of the frame it just showed.
    pub(crate) video_clock: Arc<Mutex<Option<(u64, u64)>>>,
    /// The quality ladder, shared with the control stage.
    pub(crate) ladder: Arc<Mutex<crate::ladder::Ladder>>,
    /// The plan for DMA-BUF pictures, once the first one has chosen it.
    pub(crate) plan: Option<PresentPlan>,
    /// Zero-copy presents that failed in a row. A success resets it.
    pub(crate) consecutive_errors: u32,
    /// The GPU converter of the `VppBgrx` plan.
    pub(crate) converter: Option<Converter>,
}

/// How many frames pass between two progress events.
const PROGRESS_EVERY: u64 = 30;

impl PresentStage {
    /// Record that a frame was dropped, so the ladder can ask for a smaller
    /// picture when the client cannot keep up.
    fn record_dropped(&self) {
        if let Ok(mut ladder) = self.ladder.lock() {
            ladder.record_dropped();
        }
    }

    /// Decode and present until the pipeline stops.
    pub fn run(mut self) {
        let mut decoder = match VaapiDecoder::new() {
            Ok(d) => d.dmabuf_output(),
            Err(e) => {
                eprintln!("lumen-client: cannot create the decoder: {e}");
                self.running.store(false, Ordering::SeqCst);
                return;
            }
        };

        let mut presented = 0u64;
        // The interval between the frames, which the client estimates from
        // the arrival times. It decides when a frame is late.
        let mut interval_us = 16_667u64;
        let mut last_recv_ts: Option<u64> = None;
        // Set when the decoder lost a reference picture. The client waits
        // for the next keyframe instead of decoding frames that cannot be
        // decoded.
        let mut needs_keyframe = false;
        while self.running.load(Ordering::SeqCst) {
            let Ok(access_unit) = self.access_units.recv() else {
                return;
            };
            if let Some(previous) = last_recv_ts {
                let delta = access_unit.recv_ts_us.saturating_sub(previous);
                // An exponential average, so one slow frame does not move
                // the estimate much.
                interval_us = (interval_us * 7 + delta) / 8;
            }
            last_recv_ts = Some(access_unit.recv_ts_us);
            if needs_keyframe && !access_unit.encoded.is_keyframe {
                // The decoder waits for a keyframe. Decoding a frame without
                // its reference would only produce more errors.
                self.counters.frames_dropped.fetch_add(1, Ordering::Relaxed);
                self.record_dropped();
                continue;
            }
            let decode_start = monotonic_us();
            match decoder.decode(&access_unit.encoded) {
                Ok(Some(frame)) => {
                    let decode_end = monotonic_us();
                    self.counters.frames_decoded.fetch_add(1, Ordering::Relaxed);
                    needs_keyframe = false;
                    // A frame that is already late is decoded, so the
                    // reference chain of the decoder stays intact, but it
                    // is not presented: presenting it would only add
                    // latency.
                    let age = monotonic_us().saturating_sub(access_unit.recv_ts_us);
                    if age > interval_us {
                        self.counters.frames_dropped.fetch_add(1, Ordering::Relaxed);
                        self.record_dropped();
                        continue;
                    }
                    let from_dmabuf = matches!(frame.buffer, FrameBuffer::Dmabuf(_));
                    let result = if from_dmabuf {
                        self.present_dmabuf(&frame)
                    } else {
                        self.present_cpu(&frame)
                    };
                    match result {
                        Ok(zero_copy) => {
                            let present = monotonic_us();
                            self.consecutive_errors = 0;
                            presented += 1;
                            self.shown(
                                &access_unit,
                                &frame,
                                (decode_start, decode_end),
                                present,
                                zero_copy,
                            );
                            // The progress of the stream goes into the
                            // event log, so a script can see that the
                            // frames flow again.
                            if presented.is_multiple_of(PROGRESS_EVERY) {
                                let errors = self.counters.decode_errors.load(Ordering::Relaxed);
                                self.events.write(
                                    "progress",
                                    None,
                                    Some(format!("presented={presented} errors={errors}")),
                                );
                            }
                        }
                        Err(e) if from_dmabuf => self.zero_copy_failed(&e),
                        Err(_) => {}
                    }
                    if self.plan == Some(PresentPlan::Cpu) {
                        // From here on the decoder hands back CPU NV12, and the
                        // stage converts it as it always has.
                        decoder.set_dmabuf_output(false);
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    self.counters.decode_errors.fetch_add(1, Ordering::Relaxed);
                    eprintln!("lumen-client: decode failed: {e}");
                    // The reference picture is missing. Ask the host for a
                    // keyframe so the next frame can be decoded, and wait
                    // for it instead of decoding frames that cannot be
                    // decoded.
                    needs_keyframe = true;
                    let _ = self.requests.try_send(ControlMessage::RequestIdr {
                        frame_id: access_unit.frame_id,
                    });
                }
            }
        }
    }

    /// Presents a CPU picture: converts it to BGRX and hands it to the compositor. Returns
    /// `Ok(false)`, because a CPU present is never zero-copy.
    fn present_cpu(&mut self, frame: &VideoFrame) -> Result<bool, String> {
        let converted = match Self::to_bgrx(frame) {
            Ok(converted) => converted,
            Err(e) => {
                self.counters.decode_errors.fetch_add(1, Ordering::Relaxed);
                eprintln!("lumen-client: colour conversion failed: {e}");
                return Err(e);
            }
        };
        self.renderer
            .present(&converted)
            .map(|()| false)
            .inspect_err(|e| eprintln!("lumen-client: present failed: {e}"))
    }

    /// Presents a DMA-BUF picture, choosing the plan on the first one. Returns `Ok(true)` for a
    /// zero-copy present.
    fn present_dmabuf(&mut self, frame: &VideoFrame) -> Result<bool, String> {
        let FrameBuffer::Dmabuf(image) = &frame.buffer else {
            return Err("the picture is not a DMA-BUF".into());
        };
        if self.plan.is_none() {
            let plan = self.choose_plan(frame, image);
            self.plan = Some(plan);
        }
        match self.plan {
            Some(PresentPlan::DirectNv12) => self.renderer.present(frame).map(|()| true),
            Some(PresentPlan::VppBgrx) => {
                let converter = self
                    .converter
                    .as_mut()
                    .ok_or("the GPU converter is missing")?;
                let bgrx = converter.bgrx(image, frame.width, frame.height)?;
                let bgrx_frame = VideoFrame {
                    width: frame.width,
                    height: frame.height,
                    format: PixelFormat::Bgrx,
                    buffer: FrameBuffer::Dmabuf(bgrx),
                    capture_ts_us: frame.capture_ts_us,
                    index: frame.index,
                };
                self.renderer.present(&bgrx_frame).map(|()| true)
            }
            Some(PresentPlan::Cpu) | None => {
                Err("this picture is presented from CPU memory".into())
            }
        }
    }

    /// Chooses the plan from the first DMA-BUF picture and the formats the compositor takes, and
    /// logs the choice.
    fn choose_plan(&mut self, frame: &VideoFrame, image: &DmabufImage) -> PresentPlan {
        let formats = self.renderer.dmabuf_formats();
        let direct = frame.format == PixelFormat::Nv12
            && formats.contains(&(PixelFormat::Nv12.drm_fourcc(), image.modifier));
        let (plan, reason) = if direct {
            (
                PresentPlan::DirectNv12,
                "the compositor takes the decoder's NV12 buffer".to_string(),
            )
        } else {
            match Self::probe_converter(frame, image, &formats) {
                Ok(converter) => {
                    self.converter = Some(converter);
                    (
                        PresentPlan::VppBgrx,
                        "the compositor takes the GPU's BGRX buffer".to_string(),
                    )
                }
                Err(e) => (PresentPlan::Cpu, e),
            }
        };
        eprintln!(
            "lumen-client: present plan {plan:?}, {} dmabuf formats advertised",
            formats.len()
        );
        self.events
            .write("present_plan", None, Some(format!("{plan:?}: {reason}")));
        plan
    }

    /// Builds the GPU converter and checks that the compositor takes the BGRX buffer it makes.
    fn probe_converter(
        frame: &VideoFrame,
        image: &DmabufImage,
        formats: &[(u32, u64)],
    ) -> Result<Converter, String> {
        let mut converter = Converter::new(frame.width, frame.height)?;
        let bgrx = converter.bgrx(image, frame.width, frame.height)?;
        if bgrx.fourcc == PixelFormat::Bgrx.drm_fourcc()
            && formats.contains(&(bgrx.fourcc, bgrx.modifier))
        {
            Ok(converter)
        } else {
            Err(format!(
                "the compositor does not take the GPU BGRX buffer (fourcc {:#010x}, modifier {:#018x})",
                bgrx.fourcc, bgrx.modifier
            ))
        }
    }

    /// Counts a zero-copy present that failed: the frame is dropped. Three in a row turn the
    /// zero-copy plans off for the rest of the session.
    // shortcut: DMA-BUF frames alias the 8-slot decode pool; safe while at most 2 presents are in
    // flight; upgrade to refcounted release if RELEASE_TIMEOUT fires.
    fn zero_copy_failed(&mut self, error: &str) {
        self.counters.frames_dropped.fetch_add(1, Ordering::Relaxed);
        self.record_dropped();
        if !matches!(
            self.plan,
            Some(PresentPlan::DirectNv12 | PresentPlan::VppBgrx)
        ) {
            return;
        }
        self.consecutive_errors += 1;
        eprintln!("lumen-client: zero-copy present failed: {error}");
        if self.consecutive_errors >= ZERO_COPY_ERROR_LIMIT {
            self.plan = Some(PresentPlan::Cpu);
            self.converter = None;
            eprintln!(
                "lumen-client: zero-copy present disabled after {ZERO_COPY_ERROR_LIMIT} errors: {error}"
            );
            self.events.write(
                "present_plan",
                None,
                Some(format!(
                    "Cpu: zero-copy disabled after {ZERO_COPY_ERROR_LIMIT} errors: {error}"
                )),
            );
        }
    }

    /// Bookkeeping for a picture that reached the screen: the counters, the ladder, the audio
    /// clock and the stage times the host measures the pipeline with.
    fn shown(
        &self,
        access_unit: &AccessUnit,
        frame: &VideoFrame,
        decode: (u64, u64),
        present: u64,
        zero_copy: bool,
    ) {
        self.counters
            .frames_presented
            .fetch_add(1, Ordering::Relaxed);
        if zero_copy {
            self.counters
                .frames_zero_copy
                .fetch_add(1, Ordering::Relaxed);
        }
        // The picture the host sends decides which rung of the ladder the
        // client is on.
        if let Ok(mut ladder) = self.ladder.lock() {
            ladder.set_size(frame.width, frame.height);
            ladder.record_shown();
        }
        // The audio stage places its frames against the video clock, so it
        // needs to know which video frame is on screen.
        if let Ok(mut clock) = self.video_clock.lock() {
            *clock = Some((access_unit.encoded.capture_ts_us, present));
        }
        // The host maps these onto its own clock, so it can measure the whole
        // pipeline.
        let _ = self.requests.try_send(ControlMessage::StageTimes {
            frame_id: access_unit.frame_id,
            recv_ts_us: access_unit.recv_ts_us,
            decode_start_ts_us: decode.0,
            decode_ts_us: decode.1,
            present_ts_us: present,
        });
    }

    /// Convert a decoded CPU frame into a BGRX frame the compositor accepts.
    fn to_bgrx(frame: &VideoFrame) -> Result<VideoFrame, String> {
        let (width, height) = (frame.width, frame.height);
        let FrameBuffer::Cpu { data } = &frame.buffer else {
            return Err("expected a CPU picture".into());
        };
        let mut bgrx = vec![0u8; PixelFormat::Bgrx.frame_len(width, height)];
        color::nv12_to_bgrx(data, width, height, &mut bgrx)?;
        Ok(VideoFrame {
            width,
            height,
            format: PixelFormat::Bgrx,
            buffer: FrameBuffer::Cpu { data: bgrx },
            capture_ts_us: frame.capture_ts_us,
            index: frame.index,
        })
    }
}
