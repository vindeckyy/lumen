//! Screen capture through the ScreenCast portal and PipeWire.
//!
//! This is the capture path for compositors that do not offer wlroots screencopy, such as GNOME
//! and KDE, and for any session where the user should grant the capture. The portal asks the
//! user once; with a restore token from an earlier grant it does not ask again. The frames
//! arrive as shared-memory BGRx/BGRA/RGBx/RGBA buffers and leave as CPU [`VideoFrame`]s.
//!
//! The handshake follows punktfunk's `pf-capture/src/linux/portal.rs` (MIT OR Apache-2.0,
//! Copyright (c) 2026 unom - Enrico Bühler): one monitor, the cursor embedded, persistence until
//! revoked, and the PipeWire remote opened through the portal so the stream sees only the
//! granted node.

use std::fmt;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::JoinHandle;

use ashpd::desktop::screencast::{
    CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
    StartCastOptions,
};
use ashpd::desktop::{CreateSessionOptions, PersistMode, Session};
use lumen_protocol::media::{FrameSource, PixelFormat, VideoFrame};
use pipewire as pw;
use pw::spa;
use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
use pw::spa::pod::Pod;
use tokio::runtime::Runtime;

/// Frames queued between the PipeWire thread and the consumer. A slow consumer loses the
/// oldest frames, not latency.
const QUEUE: usize = 2;

/// A granted ScreenCast session and its PipeWire stream.
pub struct PortalCapture {
    frames: Receiver<VideoFrame>,
    quit: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
    restore_token: Option<String>,
    node_id: u32,
    // The portal ends the cast when the session closes, so the session lives as long as the
    // capture and is closed when it ends.
    session: Option<Session<Screencast>>,
}

/// The runtime every portal call runs on. ashpd keeps one session-bus connection per process,
/// driven by the runtime that opened it; a runtime per capture would take the connection down
/// with the first capture, and every later call would wait forever.
fn runtime() -> Result<&'static Runtime, String> {
    static RUNTIME: std::sync::OnceLock<Runtime> = std::sync::OnceLock::new();
    if let Some(runtime) = RUNTIME.get() {
        return Ok(runtime);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;
    Ok(RUNTIME.get_or_init(|| runtime))
}

impl fmt::Debug for PortalCapture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PortalCapture")
            .field("node_id", &self.node_id)
            .finish_non_exhaustive()
    }
}

impl PortalCapture {
    /// Asks the portal for one monitor and starts the stream. `restore_token` is the token of
    /// an earlier grant; the portal uses it to skip the dialog.
    ///
    /// # Errors
    /// Returns why the portal refused or the stream could not start.
    pub fn open(restore_token: Option<&str>) -> Result<Self, String> {
        let (session, node_id, fd, token) = runtime()?.block_on(async {
            let portal = Screencast::new()
                .await
                .map_err(|e| format!("ScreenCast portal: {e}"))?;
            let session = portal
                .create_session(CreateSessionOptions::default())
                .await
                .map_err(|e| format!("create the session: {e}"))?;
            portal
                .select_sources(
                    &session,
                    SelectSourcesOptions::default()
                        .set_cursor_mode(CursorMode::Embedded)
                        .set_sources(ashpd::enumflags2::BitFlags::from(SourceType::Monitor))
                        .set_multiple(false)
                        .set_persist_mode(PersistMode::ExplicitlyRevoked)
                        .set_restore_token(restore_token),
                )
                .await
                .map_err(|e| format!("select the sources: {e}"))?
                .response()
                .map_err(|e| format!("select the sources: {e}"))?;
            let streams = portal
                .start(&session, None, StartCastOptions::default())
                .await
                .map_err(|e| format!("start the cast: {e}"))?
                .response()
                .map_err(|e| format!("the cast was refused: {e}"))?;
            let stream = streams
                .streams()
                .first()
                .ok_or("the portal granted no stream")?;
            let node_id = stream.pipe_wire_node_id();
            let token = streams.restore_token().map(str::to_owned);
            let fd = portal
                .open_pipe_wire_remote(&session, OpenPipeWireRemoteOptions::default())
                .await
                .map_err(|e| format!("open the PipeWire remote: {e}"))?;
            Ok::<_, String>((session, node_id, fd, token))
        })?;

        let (frame_tx, frames) = mpsc::sync_channel(QUEUE);
        let (quit, quit_rx) = pw::channel::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("lumen-pipewire-capture".into())
            .spawn(move || {
                if let Err(e) = run_stream(fd, node_id, &frame_tx, quit_rx, &ready_tx) {
                    let _ = ready_tx.send(Err(e));
                }
            })
            .map_err(|e| format!("spawn the capture thread: {e}"))?;
        ready_rx
            .recv()
            .map_err(|_| "the capture thread ended before it was ready".to_owned())??;
        Ok(Self {
            frames,
            quit,
            thread: Some(thread),
            restore_token: token,
            node_id,
            session: Some(session),
        })
    }

    /// The token that restores this grant without a dialog, when the portal handed one out.
    #[must_use]
    pub fn restore_token(&self) -> Option<&str> {
        self.restore_token.as_deref()
    }

    /// Waits up to `timeout` for the next frame.
    #[must_use]
    pub fn recv_timeout(&self, timeout: std::time::Duration) -> Option<VideoFrame> {
        self.frames.recv_timeout(timeout).ok()
    }
}

impl FrameSource for PortalCapture {
    fn poll_frame(&mut self) -> Option<VideoFrame> {
        self.frames.try_recv().ok()
    }
}

impl Drop for PortalCapture {
    fn drop(&mut self) {
        let _ = self.quit.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let (Some(session), Ok(runtime)) = (self.session.take(), runtime()) {
            let _ = runtime.block_on(session.close());
        }
    }
}

/// The negotiated format of the stream.
#[derive(Debug, Default)]
struct Negotiated {
    info: VideoInfoRaw,
    index: u64,
}

fn run_stream(
    fd: std::os::fd::OwnedFd,
    node_id: u32,
    frames: &SyncSender<VideoFrame>,
    quit: pw::channel::Receiver<()>,
    ready: &SyncSender<Result<(), String>>,
) -> Result<(), String> {
    pw::init();
    let mainloop =
        pw::main_loop::MainLoopRc::new(None).map_err(|e| format!("PipeWire main loop: {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|e| format!("PipeWire context: {e}"))?;
    let core = context
        .connect_fd_rc(fd, None)
        .map_err(|e| format!("PipeWire remote of the portal: {e}"))?;
    let stream = pw::stream::StreamBox::new(
        &core,
        "lumen-screen-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| format!("capture stream: {e}"))?;

    let frames = frames.clone();
    let _listener = stream
        .add_local_listener_with_user_data(Negotiated::default())
        .param_changed(|_, negotiated, id, param| {
            let Some(param) = param else {
                return;
            };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let _ = negotiated.info.parse(param);
        })
        .process(move |stream, negotiated| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let size = negotiated.info.size();
            let Some(format) = pixel_format(negotiated.info.format()) else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let chunk = data.chunk();
            let (offset, stride) = (chunk.offset() as usize, chunk.stride());
            let Some(bytes) = data.data() else {
                // Not a mapped buffer, such as a DMA-BUF the producer chose anyway.
                return;
            };
            let Some(pixels) = pack_rows(bytes, offset, stride, size.width, size.height) else {
                return;
            };
            let index = negotiated.index;
            negotiated.index += 1;
            // Dropped when the consumer is behind: this runs on the PipeWire thread.
            let _ = frames.try_send(VideoFrame::cpu(
                size.width,
                size.height,
                format,
                pixels,
                now_us(),
                index,
            ));
        })
        .register()
        .map_err(|e| format!("capture listener: {e}"))?;

    let pod = format_pod()?;
    let mut params = [Pod::from_bytes(&pod).ok_or("the format pod is not valid")?];
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| format!("connect to node {node_id}: {e}"))?;

    let quit_loop = mainloop.clone();
    let _quit = quit.attach(mainloop.loop_(), move |()| quit_loop.quit());
    let _ = ready.send(Ok(()));
    mainloop.run();
    Ok(())
}

/// The formats the stream takes. No modifier is offered, so the producer sends shared memory.
fn format_pod() -> Result<Vec<u8>, String> {
    let object = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle {
                width: 1920,
                height: 1080
            },
            spa::utils::Rectangle {
                width: 2,
                height: 2
            },
            spa::utils::Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 240, denom: 1 }
        ),
        // Screen producers send variable-rate frames paced by this maximum; without it
        // xdg-desktop-portal-wlr negotiates 0/0 and never captures.
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoMaxFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 60, denom: 1 },
            spa::utils::Fraction { num: 1, denom: 1 },
            spa::utils::Fraction { num: 240, denom: 1 }
        ),
    );
    let (cursor, _) = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .map_err(|e| format!("format pod: {e:?}"))?;
    Ok(cursor.into_inner())
}

/// The protocol format of a negotiated PipeWire format. Alpha is ignored: BGRA travels as
/// BGRx and RGBx as RGBA, which have the same byte order.
fn pixel_format(format: VideoFormat) -> Option<PixelFormat> {
    match format {
        VideoFormat::BGRx | VideoFormat::BGRA => Some(PixelFormat::Bgrx),
        VideoFormat::RGBx | VideoFormat::RGBA => Some(PixelFormat::Rgba),
        _ => None,
    }
}

/// Copies `height` rows of `width` 4-byte pixels out of a buffer with `stride` bytes per row.
/// `None` when the buffer is shorter than its rows say.
fn pack_rows(bytes: &[u8], offset: usize, stride: i32, width: u32, height: u32) -> Option<Vec<u8>> {
    let row = width as usize * 4;
    let stride = usize::try_from(stride).ok().filter(|&s| s >= row)?;
    let mut out = Vec::with_capacity(row * height as usize);
    for y in 0..height as usize {
        let start = offset + y * stride;
        out.extend_from_slice(bytes.get(start..start + row)?);
    }
    Some(out)
}

/// `CLOCK_MONOTONIC` in microseconds, the clock every capture stamps frames on.
fn now_us() -> u64 {
    let ts = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let secs = u64::try_from(ts.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(ts.tv_nsec).unwrap_or(0);
    secs * 1_000_000 + nanos / 1_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_packed_without_the_padding() {
        // 2x2 pixels, stride 12: 4 bytes of padding per row.
        let bytes: Vec<u8> = (0..24).collect();
        let packed = pack_rows(&bytes, 0, 12, 2, 2).unwrap();
        assert_eq!(
            packed,
            [(0..8).collect::<Vec<u8>>(), (12..20).collect()].concat()
        );
    }

    #[test]
    fn a_short_buffer_or_a_narrow_stride_is_refused() {
        assert!(pack_rows(&[0; 10], 0, 8, 2, 2).is_none());
        assert!(pack_rows(&[0; 64], 0, 4, 2, 2).is_none());
    }
}
