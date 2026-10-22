//! The decode and present stage.
//!
//! Decodes each access unit and presents the frame. The decoder hands back
//! CPU NV12, so the stage converts to BGRX before the compositor sees it.
//! That copy is the deviation recorded as D37 and D46.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};

use lumen_decode_vaapi::VaapiDecoder;
use lumen_host_service::control::ControlMessage;
use lumen_protocol::color;
use lumen_protocol::{FrameBuffer, FrameSink, PixelFormat, VideoDecoder, VideoFrame};
use lumen_render::Renderer;

use crate::Counters;
use crate::receive::{AccessUnit, monotonic_us};

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
}

impl PresentStage {
    /// Record that a frame was dropped, so the ladder can ask for a smaller
    /// picture when the client cannot keep up.
    fn record_dropped(&self) {
        if let Ok(mut ladder) = self.ladder.lock() {
            ladder.record_dropped();
        }
    }
}

/// How many frames pass between two progress events.
const PROGRESS_EVERY: u64 = 30;

impl PresentStage {
    /// Decode and present until the pipeline stops.
    pub fn run(mut self) {
        let mut decoder = match VaapiDecoder::new() {
            Ok(d) => d,
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
                    match Self::to_bgrx(&frame) {
                        Ok(converted) => {
                            if let Err(e) = self.renderer.present(&converted) {
                                eprintln!("lumen-client: present failed: {e}");
                            } else {
                                let present = monotonic_us();
                                presented += 1;
                                self.counters
                                    .frames_presented
                                    .fetch_add(1, Ordering::Relaxed);
                                // The picture the host sends decides which
                                // rung of the ladder the client is on.
                                let (frame_w, frame_h) = (frame.width, frame.height);
                                if let Ok(mut ladder) = self.ladder.lock() {
                                    ladder.set_size(frame_w, frame_h);
                                    ladder.record_shown();
                                }
                                // The audio stage places its frames against
                                // the video clock, so it needs to know which
                                // video frame is on screen.
                                if let Ok(mut clock) = self.video_clock.lock() {
                                    *clock = Some((access_unit.encoded.capture_ts_us, present));
                                }
                                // The host maps these onto its own clock, so
                                // it can measure the whole pipeline.
                                let _ = self.requests.try_send(ControlMessage::StageTimes {
                                    frame_id: access_unit.frame_id,
                                    recv_ts_us: access_unit.recv_ts_us,
                                    decode_start_ts_us: decode_start,
                                    decode_ts_us: decode_end,
                                    present_ts_us: present,
                                });
                                // The progress of the stream goes into the
                                // event log, so a script can see that the
                                // frames flow again.
                                if presented.is_multiple_of(PROGRESS_EVERY) {
                                    let errors =
                                        self.counters.decode_errors.load(Ordering::Relaxed);
                                    self.events.write(
                                        "progress",
                                        None,
                                        Some(format!("presented={presented} errors={errors}")),
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            self.counters.decode_errors.fetch_add(1, Ordering::Relaxed);
                            eprintln!("lumen-client: colour conversion failed: {e}");
                        }
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

    /// Convert a decoded frame into a BGRX frame the compositor accepts.
    fn to_bgrx(frame: &VideoFrame) -> Result<VideoFrame, String> {
        let (width, height) = (frame.width, frame.height);
        let FrameBuffer::Cpu { data } = &frame.buffer else {
            return Err(
                "the decoder produced a DMA-BUF frame, which the renderer cannot import yet".into(),
            );
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
