//! The decode and present stage.
//!
//! Decodes each access unit and presents the frame. A picture the decoder exports as a DMA-BUF
//! is presented without a CPU copy when the compositor can take it: either the decoder's NV12
//! buffer as it is, or a BGRX buffer the GPU makes from it. Otherwise the decoder hands back CPU
//! NV12 and the stage converts it to BGRX before the compositor sees it. That copy is the
//! deviation recorded as D37 and D46.

use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};

use lumen_decode_openh264::SoftwareDecoder;
use lumen_decode_vaapi::VaapiDecoder;
use lumen_ffi_va::{OwnedVaSurface, VaDisplay, VideoProcessor};
use lumen_host_service::control::ControlMessage;
use lumen_protocol::color;
use lumen_protocol::media::{DmabufImage, EncodedFrame};
use lumen_protocol::{FrameBuffer, FrameSink, PixelFormat, VideoDecoder, VideoFrame};
use lumen_render::Renderer;

use crate::Counters;
use crate::receive::{AccessUnit, monotonic_us};

/// How long the client waits for the answer to a recovery request before it asks again.
const RECOVERY_RETRY: std::time::Duration = std::time::Duration::from_millis(250);

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
        // Linear, because X11's DRI3 import describes no tiling; Wayland takes linear too.
        let converted = self
            .processor
            .convert_linear(&source, PixelFormat::Bgrx, width, height)
            .map_err(|e| e.to_string())?;
        converted
            .export_dmabuf()
            .map_err(|e| e.to_string())?
            .into_image()
    }
}

/// The decoder the client presents with.
///
/// The hardware decoder is the one that is wanted, but a machine without a VA-API driver still
/// has to show the stream, so the software decoder takes over when the hardware one is not
/// there. The software decoder hands out CPU pictures, never DMA-BUFs.
enum Decoder {
    /// The VA-API decoder.
    Hardware(Box<VaapiDecoder>),
    /// The software decoder.
    Software(SoftwareDecoder),
}

impl Decoder {
    /// Opens the hardware decoder, and falls back to the software one.
    fn open() -> Result<Self, String> {
        match VaapiDecoder::new() {
            Ok(decoder) => Ok(Self::Hardware(Box::new(decoder.dmabuf_output()))),
            Err(e) => {
                eprintln!("lumen-client: no hardware decoder ({e}), using the software one");
                SoftwareDecoder::new().map(Self::Software)
            }
        }
    }

    /// Decodes one access unit.
    fn decode(&mut self, encoded: &EncodedFrame) -> Result<Option<VideoFrame>, String> {
        match self {
            Self::Hardware(decoder) => decoder.decode(encoded),
            Self::Software(decoder) => decoder.decode(encoded),
        }
    }

    /// Asks for DMA-BUF pictures. The software decoder only has CPU pictures.
    fn set_dmabuf_output(&mut self, enabled: bool) {
        if let Self::Hardware(decoder) = self {
            decoder.set_dmabuf_output(enabled);
        }
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
    /// The i915 media-engine clock boost, on an Intel GPU.
    pub(crate) boost: Option<lumen_drm::I915Boost>,
    /// The state of the viewer window that other threads ask for.
    pub(crate) window: Arc<crate::WindowControl>,
    /// Whether the window is fullscreen now.
    pub(crate) fullscreen: bool,
    /// Whether the screen is kept awake now.
    pub(crate) keep_awake: bool,
    /// The heads-up display of the window.
    pub(crate) hud: crate::hud::Hud,
}

/// How many frames pass between two progress events.
const PROGRESS_EVERY: u64 = 30;

/// How long the present stage waits for a frame before it looks at the window again.
const WINDOW_POLL: std::time::Duration = std::time::Duration::from_millis(50);

impl PresentStage {
    /// Handles the window's events and the requests other threads made for it. Returns `false`
    /// when the user closed the window, which ends the session.
    fn serve_window(&mut self) -> bool {
        if let Err(e) = self.renderer.poll_events() {
            eprintln!("lumen-client: the window failed: {e}");
        }
        if self.renderer.close_requested() {
            self.window.closed.store(true, Ordering::SeqCst);
            self.running.store(false, Ordering::SeqCst);
            return false;
        }
        let wanted = self.window.fullscreen.load(Ordering::Relaxed);
        if wanted != self.fullscreen {
            self.fullscreen = wanted;
            if let Err(e) = self.renderer.set_fullscreen(wanted) {
                eprintln!("lumen-client: cannot change the fullscreen state: {e}");
            }
        }
        // The screen stays awake while the session runs, unless the user asks otherwise.
        let awake = self.window.keep_awake.load(Ordering::Relaxed);
        if awake != self.keep_awake {
            self.keep_awake = awake;
            if let Err(e) = self.renderer.set_keep_awake(awake) {
                eprintln!("lumen-client: cannot change the idle inhibit: {e}");
            }
        }
        true
    }

    /// Record that a frame was dropped, so the ladder can ask for a smaller
    /// picture when the client cannot keep up.
    fn record_dropped(&self) {
        if let Ok(mut ladder) = self.ladder.lock() {
            ladder.record_dropped();
        }
    }

    /// Draws the heads-up display over the picture, when it is due.
    fn draw_hud(&mut self, picture: (u32, u32)) {
        let Some(lines) = self.hud.lines(picture, &self.counters) else {
            return;
        };
        if let Err(e) = self.renderer.set_overlay(&lines) {
            eprintln!("lumen-client: cannot draw the display: {e}");
        }
    }

    /// Decode and present until the pipeline stops. The renderer comes back, so the window
    /// survives a reconnect.
    pub fn run(mut self) -> Renderer {
        self.run_inner();
        self.renderer
    }

    /// The presenting loop.
    fn run_inner(&mut self) {
        let mut decoder = match Decoder::open() {
            Ok(decoder) => decoder,
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
        // decoded, and the last good picture stays on screen.
        let mut gate = crate::recovery_gate::RecoveryGate::new();
        // The newest frame that decoded: after a loss the host recovers from it.
        let mut last_good: Option<u32> = None;
        // When the client last asked to recover. The answer can be lost like any frame, and
        // the gate refuses everything until it arrives, so the request is repeated.
        let mut asked_at: Option<std::time::Instant> = None;
        while self.running.load(Ordering::SeqCst) {
            // The window is served between frames too, so a close or a fullscreen request is
            // answered while the host sends nothing.
            if !self.serve_window() {
                return;
            }
            let access_unit = match self.access_units.recv_timeout(WINDOW_POLL) {
                Ok(access_unit) => access_unit,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            };
            if let Some(previous) = last_recv_ts {
                let delta = access_unit.recv_ts_us.saturating_sub(previous);
                // An exponential average, so one slow frame does not move
                // the estimate much.
                interval_us = (interval_us * 7 + delta) / 8;
            }
            last_recv_ts = Some(access_unit.recv_ts_us);
            if !gate.accepts(access_unit.encoded.is_keyframe) {
                // The decoder waits for a keyframe. Decoding a frame without
                // its reference would only produce more errors.
                self.counters.frames_dropped.fetch_add(1, Ordering::Relaxed);
                self.record_dropped();
                if asked_at.is_some_and(|at| at.elapsed() > RECOVERY_RETRY) {
                    // The recovery did not arrive: ask for a keyframe, which needs nothing.
                    let _ = self.requests.try_send(ControlMessage::RequestIdr {
                        frame_id: access_unit.frame_id,
                    });
                    asked_at = Some(std::time::Instant::now());
                }
                continue;
            }
            let decode_start = monotonic_us();
            match decoder.decode(&access_unit.encoded) {
                Ok(Some(frame)) => {
                    let decode_end = monotonic_us();
                    self.counters.frames_decoded.fetch_add(1, Ordering::Relaxed);
                    gate.on_decoded(access_unit.encoded.is_keyframe);
                    last_good = Some(access_unit.frame_id);
                    if access_unit.encoded.is_keyframe {
                        asked_at = None;
                    }
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
                    if from_dmabuf {
                        // On an Intel GPU the media engine drops to its floor clock unless
                        // something waits on the buffer through i915, so the wait is done here.
                        self.boost_clock(&frame);
                    }
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
                            // frames flow again, and a launcher sees the first one.
                            if presented == 1 || presented.is_multiple_of(PROGRESS_EVERY) {
                                let errors = self.counters.decode_errors.load(Ordering::Relaxed);
                                self.events.write(
                                    "progress",
                                    None,
                                    Some(format!("presented={presented} errors={errors}")),
                                );
                            }
                            // The heads-up display shows the picture, the rate and the round
                            // trip, a few times a second.
                            if self.window.hud.load(Ordering::Relaxed) {
                                self.draw_hud((frame.width, frame.height));
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
                    // The reference picture is missing. The gate closes, the
                    // last good picture stays on screen, and the host is
                    // asked once to recover from the last frame that decoded,
                    // or for a keyframe when none did.
                    if gate.on_decode_error() {
                        let request = match last_good {
                            Some(last_good) => ControlMessage::Recover { last_good },
                            None => ControlMessage::RequestIdr {
                                frame_id: access_unit.frame_id,
                            },
                        };
                        let _ = self.requests.try_send(request);
                        asked_at = Some(std::time::Instant::now());
                    }
                }
            }
        }
    }

    /// Waits on the decoded picture through i915, which keeps the media engine clocked.
    fn boost_clock(&mut self, frame: &VideoFrame) {
        let Some(boost) = self.boost.as_mut() else {
            return;
        };
        let FrameBuffer::Dmabuf(image) = &frame.buffer else {
            return;
        };
        if boost.track(image.fd.as_fd()) {
            // A millisecond is enough: the wait raises the clock, it does not wait for work.
            boost.wait(1_000_000);
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
            drop(ladder);
            // The report says which picture the link carried.
            if let Ok(mut picture) = self.counters.final_picture.lock() {
                *picture = format!("{}x{}", frame.width, frame.height);
            }
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
