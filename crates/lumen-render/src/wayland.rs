//! Wayland presentation.
//!
//! One `xdg_toplevel` sized to the frame. CPU frames are copied into `wl_shm` buffers and
//! dmabuf frames become `zwp_linux_dmabuf_v1` buffers. Every frame is committed as soon as its
//! buffer is attached, without waiting for a frame callback.

use std::collections::HashSet;
use std::fs::File;
use std::os::fd::AsFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use lumen_protocol::media::{DmabufImage, FrameBuffer, FrameSink, PixelFormat, VideoFrame};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use wayland_client::backend::WaylandError;
use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_buffer::{self, WlBuffer};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::{
    Flags, ZwpLinuxBufferParamsV1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::{
    self, ZwpLinuxDmabufV1,
};
use wayland_protocols::wp::presentation_time::client::wp_presentation::WpPresentation;
use wayland_protocols::wp::presentation_time::client::wp_presentation_feedback::{
    self, WpPresentationFeedback,
};
use wayland_protocols::wp::tearing_control::v1::client::wp_tearing_control_manager_v1::WpTearingControlManagerV1;
use wayland_protocols::wp::tearing_control::v1::client::wp_tearing_control_v1::{
    PresentationHint, WpTearingControlV1,
};
use wayland_protocols::xdg::shell::client::xdg_surface::{self, XdgSurface};
use wayland_protocols::xdg::shell::client::xdg_toplevel::XdgToplevel;
use wayland_protocols::xdg::shell::client::xdg_wm_base::{self, XdgWmBase};

use crate::format;
use crate::shm;

const CONFIGURE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a frame waits for the compositor to release a buffer before presenting fails.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(1);
/// `wl_shm` buffers per window: the compositor reads one while the next frame fills the other.
const SHM_SLOTS: usize = 2;

/// Event-driven state shared by every Wayland object the renderer creates.
#[derive(Default)]
struct State {
    configured: bool,
    /// `(drm fourcc, modifier)` pairs the compositor accepts for dmabuf import.
    dmabuf_formats: HashSet<(u32, u64)>,
    last_present_ts_us: Option<u64>,
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// User data of each `wl_buffer` is its busy flag. The compositor's `release` clears it.
impl Dispatch<WlBuffer, Arc<AtomicBool>> for State {
    fn event(
        _: &mut Self,
        _: &WlBuffer,
        event: wl_buffer::Event,
        busy: &Arc<AtomicBool>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_buffer::Event::Release) {
            busy.store(false, Ordering::Release);
        }
    }
}

impl Dispatch<XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm_base: &XdgWmBase,
        event: xdg_wm_base::Event,
        &(): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        surface: &XdgSurface,
        event: xdg_surface::Event,
        &(): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            state.configured = true;
        }
    }
}

impl Dispatch<WpPresentationFeedback, ()> for State {
    fn event(
        state: &mut Self,
        _: &WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        &(): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_presentation_feedback::Event::Presented {
            tv_sec_hi,
            tv_sec_lo,
            tv_nsec,
            ..
        } = event
        {
            // Presentation clock timestamp; CLOCK_MONOTONIC on mainstream compositors.
            let secs = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
            state.last_present_ts_us = Some(secs * 1_000_000 + u64::from(tv_nsec) / 1_000);
        }
    }
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpLinuxDmabufV1,
        event: zwp_linux_dmabuf_v1::Event,
        &(): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Version 3 is bound, so `format` events are ignored: `modifier` events are authoritative.
        if let zwp_linux_dmabuf_v1::Event::Modifier {
            format,
            modifier_hi,
            modifier_lo,
        } = event
        {
            let modifier = (u64::from(modifier_hi) << 32) | u64::from(modifier_lo);
            state.dmabuf_formats.insert((format, modifier));
        }
    }
}

delegate_noop!(State: WlCompositor);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: WlShmPool);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore XdgToplevel);
delegate_noop!(State: ignore WpPresentation);
delegate_noop!(State: WpTearingControlManagerV1);
delegate_noop!(State: WpTearingControlV1);
delegate_noop!(State: ZwpLinuxBufferParamsV1);

/// Presents frames to a Wayland compositor. The window is recreated when the frame size or
/// format changes.
pub struct WaylandRenderer {
    conn: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    compositor: WlCompositor,
    shm: WlShm,
    wm_base: XdgWmBase,
    presentation: Option<WpPresentation>,
    tearing: Option<WpTearingControlManagerV1>,
    dmabuf: Option<ZwpLinuxDmabufV1>,
    title: String,
    window: Option<Window>,
    /// dmabuf `wl_buffer`s that the compositor may still read; freed once released.
    dmabuf_buffers: Vec<(WlBuffer, Arc<AtomicBool>)>,
    present_count: u64,
}

struct Window {
    width: u32,
    height: u32,
    format: PixelFormat,
    surface: WlSurface,
    xdg_surface: XdgSurface,
    toplevel: XdgToplevel,
    tearing: Option<WpTearingControlV1>,
    /// Created on the first CPU frame, so dmabuf-only windows never allocate shared memory.
    shm: Option<ShmSlots>,
}

struct ShmSlots {
    file: File,
    slot_len: u64,
    buffers: Vec<(WlBuffer, Arc<AtomicBool>)>,
}

impl WaylandRenderer {
    /// Connects to the display named by `WAYLAND_DISPLAY`.
    ///
    /// # Errors
    /// Fails when the display is unreachable or the compositor lacks `wl_compositor`,
    /// `wl_shm` or `xdg_wm_base`.
    pub fn connect(title: &str) -> Result<Self, String> {
        let conn =
            Connection::connect_to_env().map_err(|e| format!("connect to Wayland display: {e}"))?;
        let (globals, mut queue): (GlobalList, EventQueue<State>) =
            registry_queue_init(&conn).map_err(|e| format!("read Wayland registry: {e}"))?;
        let qh = queue.handle();
        let compositor: WlCompositor = globals
            .bind(&qh, 1..=6, ())
            .map_err(|e| format!("bind wl_compositor: {e}"))?;
        let shm: WlShm = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| format!("bind wl_shm: {e}"))?;
        let wm_base: XdgWmBase = globals
            .bind(&qh, 1..=6, ())
            .map_err(|e| format!("bind xdg_wm_base: {e}"))?;
        // Optional globals: a missing one disables its feature, it does not fail the connect.
        let presentation: Option<WpPresentation> = globals.bind(&qh, 1..=2, ()).ok();
        let tearing: Option<WpTearingControlManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
        // Version 3 is the last with `modifier` events that list accepted formats.
        let dmabuf: Option<ZwpLinuxDmabufV1> = globals.bind(&qh, 3..=3, ()).ok();
        let mut state = State::default();
        queue
            .roundtrip(&mut state)
            .map_err(|e| format!("Wayland roundtrip: {e}"))?;
        Ok(Self {
            conn,
            queue,
            qh,
            state,
            compositor,
            shm,
            wm_base,
            presentation,
            tearing,
            dmabuf,
            title: title.to_owned(),
            window: None,
            dmabuf_buffers: Vec::new(),
            present_count: 0,
        })
    }

    /// Timestamp of the most recent `wp_presentation` feedback, in microseconds on the
    /// presentation clock. `None` until the first frame is reported as presented.
    #[must_use]
    pub fn last_present_ts_us(&self) -> Option<u64> {
        self.state.last_present_ts_us
    }

    /// `(DRM fourcc, modifier)` pairs the compositor advertised for dmabuf import, sorted.
    /// `DRM_FORMAT_MOD_INVALID` marks support for implicit modifiers.
    #[must_use]
    pub fn dmabuf_formats(&self) -> Vec<(u32, u64)> {
        let mut formats: Vec<(u32, u64)> = self.state.dmabuf_formats.iter().copied().collect();
        formats.sort_unstable();
        formats
    }

    /// Number of frames committed so far.
    #[must_use]
    pub fn present_count(&self) -> u64 {
        self.present_count
    }

    /// Dispatches pending compositor events without blocking.
    ///
    /// # Errors
    /// Returns a description if the connection failed.
    pub fn poll_events(&mut self) -> Result<(), String> {
        self.pump(Duration::ZERO)
    }

    fn present_cpu(
        &mut self,
        width: u32,
        height: u32,
        format: PixelFormat,
        data: &[u8],
    ) -> Result<(), String> {
        format::check_cpu_frame(width, height, format, data.len())?;
        self.ensure_window(width, height, format)?;
        self.ensure_shm()?;
        let slot = self.free_shm_slot()?;
        let win = self
            .window
            .as_ref()
            .ok_or("window missing after ensure_window")?;
        let shm = win
            .shm
            .as_ref()
            .ok_or("shm slots missing after ensure_shm")?;
        // Memory layout already matches wl_shm Xrgb8888 and Abgr8888, so no swizzle.
        shm::write_pixels(&shm.file, slot as u64 * shm.slot_len, data, false)?;
        let (buffer, busy) = &shm.buffers[slot];
        busy.store(true, Ordering::Release);
        commit(
            &self.conn,
            self.presentation.as_ref(),
            &self.qh,
            &win.surface,
            buffer,
            width,
            height,
        )
    }

    /// Imports `image` as a `zwp_linux_dmabuf_v1` buffer and commits it.
    ///
    /// The `(fourcc, modifier)` pair must have been advertised by the compositor. An unadvertised
    /// pair is refused here, because `create_immed` with it is a protocol error that disconnects
    /// the client.
    ///
    /// The implicit modifier (`DRM_FORMAT_MOD_INVALID`) goes out as hi/lo INVALID. The protocol
    /// says a plane with INVALID is "as if no explicit modifier is specified", so the compositor
    /// derives the layout from the dmabuf. Only an advertised `(fourcc, INVALID)` gets here, which
    /// the check above guarantees. Explicit modifiers are split into hi/lo the same way.
    fn present_dmabuf(
        &mut self,
        width: u32,
        height: u32,
        format: PixelFormat,
        image: &DmabufImage,
    ) -> Result<(), String> {
        let dmabuf = self
            .dmabuf
            .clone()
            .ok_or("compositor lacks zwp_linux_dmabuf_v1 version 3")?;
        format::check_dmabuf(width, height, format, image)?;
        if !self
            .state
            .dmabuf_formats
            .contains(&(image.fourcc, image.modifier))
        {
            return Err(format!(
                "compositor did not advertise fourcc {:#010x} with modifier {:#018x} for dmabuf import",
                image.fourcc, image.modifier
            ));
        }
        let (w, h) = (format::i32_dim(width)?, format::i32_dim(height)?);
        self.ensure_window(width, height, format)?;
        self.dmabuf_buffers.retain(|(buffer, busy)| {
            let in_use = busy.load(Ordering::Acquire);
            if !in_use {
                buffer.destroy();
            }
            in_use
        });
        let params = dmabuf.create_params(&self.qh, ());
        // Each `add` sends the same buffer object as the fd of one plane.
        let (modifier_hi, modifier_lo) = ((image.modifier >> 32) as u32, image.modifier as u32);
        for (index, plane) in image.planes().iter().enumerate() {
            params.add(
                image.fd.as_fd(),
                index as u32,
                plane.offset,
                plane.stride,
                modifier_hi,
                modifier_lo,
            );
        }
        let busy = Arc::new(AtomicBool::new(true));
        let buffer = params.create_immed(
            w,
            h,
            image.fourcc,
            Flags::empty(),
            &self.qh,
            Arc::clone(&busy),
        );
        params.destroy();
        let win = self
            .window
            .as_ref()
            .ok_or("window missing after ensure_window")?;
        commit(
            &self.conn,
            self.presentation.as_ref(),
            &self.qh,
            &win.surface,
            &buffer,
            width,
            height,
        )?;
        self.dmabuf_buffers.push((buffer, busy));
        Ok(())
    }

    /// Recreates the window when size or format differ from the current one.
    fn ensure_window(
        &mut self,
        width: u32,
        height: u32,
        format: PixelFormat,
    ) -> Result<(), String> {
        let current = self.window.as_ref().map(|w| {
            (
                crate::resize::SurfaceSize {
                    width: w.width,
                    height: w.height,
                },
                w.format,
            )
        });
        let action = crate::resize::decide(current, width, height, format);
        if action == crate::resize::ResizeAction::Keep {
            return Ok(());
        }
        if let Some(old) = self.window.take() {
            old.destroy();
        }
        for (buffer, _) in self.dmabuf_buffers.drain(..) {
            buffer.destroy();
        }
        self.window = Some(self.create_window(width, height, format)?);
        Ok(())
    }

    fn create_window(
        &mut self,
        width: u32,
        height: u32,
        format: PixelFormat,
    ) -> Result<Window, String> {
        let (w, h) = (format::i32_dim(width)?, format::i32_dim(height)?);
        let surface = self.compositor.create_surface(&self.qh, ());
        let xdg_surface = self.wm_base.get_xdg_surface(&surface, &self.qh, ());
        let toplevel = xdg_surface.get_toplevel(&self.qh, ());
        toplevel.set_title(self.title.clone());
        // Equal min and max size keeps the window at the frame size.
        toplevel.set_min_size(w, h);
        toplevel.set_max_size(w, h);
        let tearing = self.tearing.as_ref().map(|manager| {
            let tearing = manager.get_tearing_control(&surface, &self.qh, ());
            // Async: do not wait for vblank. Only compositors that scan out fullscreen surfaces honour it.
            tearing.set_presentation_hint(PresentationHint::Async);
            tearing
        });
        // An empty commit makes the compositor send the first configure, which must be acked
        // before a buffer is attached.
        self.state.configured = false;
        surface.commit();
        let deadline = Instant::now() + CONFIGURE_TIMEOUT;
        while !self.state.configured {
            self.pump(remaining(deadline, "the initial xdg_surface configure")?)?;
        }
        Ok(Window {
            width,
            height,
            format,
            surface,
            xdg_surface,
            toplevel,
            tearing,
            shm: None,
        })
    }

    fn ensure_shm(&mut self) -> Result<(), String> {
        let win = self
            .window
            .as_mut()
            .ok_or("window missing before shm setup")?;
        if win.shm.is_none() {
            let format = wl_shm_format(win.format)?;
            win.shm = Some(ShmSlots::new(
                &self.shm, &self.qh, win.width, win.height, format,
            )?);
        }
        Ok(())
    }

    /// Waits until the compositor releases one of the `wl_shm` slots.
    fn free_shm_slot(&mut self) -> Result<usize, String> {
        let deadline = Instant::now() + RELEASE_TIMEOUT;
        loop {
            self.dispatch_pending()?;
            if let Some(shm) = self.window.as_ref().and_then(|w| w.shm.as_ref())
                && let Some(slot) = shm
                    .buffers
                    .iter()
                    .position(|(_, busy)| !busy.load(Ordering::Acquire))
            {
                return Ok(slot);
            }
            self.pump(remaining(deadline, "a released wl_buffer")?)?;
        }
    }

    fn dispatch_pending(&mut self) -> Result<(), String> {
        self.queue
            .dispatch_pending(&mut self.state)
            .map(|_| ())
            .map_err(|e| format!("dispatch Wayland events: {e}"))
    }

    /// Flushes requests, then waits up to `timeout` for the socket to become readable and
    /// dispatches whatever arrived.
    fn pump(&mut self, timeout: Duration) -> Result<(), String> {
        self.dispatch_pending()?;
        self.conn
            .flush()
            .map_err(|e| format!("flush Wayland requests: {e}"))?;
        let Some(guard) = self.conn.prepare_read() else {
            return self.dispatch_pending();
        };
        let timeout = Timespec {
            tv_sec: i64::try_from(timeout.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: i64::from(timeout.subsec_nanos()),
        };
        let mut fds = [PollFd::from_borrowed_fd(
            guard.connection_fd(),
            PollFlags::IN,
        )];
        let ready =
            poll(&mut fds, Some(&timeout)).map_err(|e| format!("poll Wayland socket: {e}"))?;
        if ready == 0 {
            return Ok(());
        }
        // A hang-up or error does not clear by waiting. Report it rather than let callers spin
        // until their deadline.
        if fds[0].revents().intersects(PollFlags::HUP | PollFlags::ERR) {
            return Err("Wayland connection closed or failed".to_owned());
        }
        match guard.read() {
            Ok(_) => {}
            // Poll reported the socket readable but no event could be read. That is a spurious
            // wakeup, not a failure; the next pass polls again.
            Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(format!("read Wayland events: {e}")),
        }
        self.dispatch_pending()
    }
}

impl ShmSlots {
    fn new(
        shm: &WlShm,
        qh: &QueueHandle<State>,
        width: u32,
        height: u32,
        format: wl_shm::Format,
    ) -> Result<Self, String> {
        let stride = width.checked_mul(4).ok_or("frame row exceeds 32 bits")?;
        let slot_len = u64::from(stride) * u64::from(height);
        let total = slot_len * SHM_SLOTS as u64;
        let file = shm::create(total)?;
        let pool_size = i32::try_from(total)
            .map_err(|_| "shm pool exceeds the wl_shm size limit".to_owned())?;
        let pool: WlShmPool = shm.create_pool(file.as_fd(), pool_size, qh, ());
        let (w, h, stride_i) = (
            format::i32_dim(width)?,
            format::i32_dim(height)?,
            format::i32_dim(stride)?,
        );
        let mut buffers = Vec::with_capacity(SHM_SLOTS);
        for slot in 0..SHM_SLOTS as u64 {
            let busy = Arc::new(AtomicBool::new(false));
            let offset = i32::try_from(slot * slot_len)
                .map_err(|_| "shm slot offset exceeds the wl_shm limit".to_owned())?;
            let buffer = pool.create_buffer(offset, w, h, stride_i, format, qh, Arc::clone(&busy));
            buffers.push((buffer, busy));
        }
        // The buffers stay valid after the pool is destroyed.
        pool.destroy();
        Ok(Self {
            file,
            slot_len,
            buffers,
        })
    }
}

impl Window {
    fn destroy(self) {
        if let Some(tearing) = self.tearing {
            tearing.destroy();
        }
        if let Some(shm) = self.shm {
            for (buffer, _) in shm.buffers {
                buffer.destroy();
            }
        }
        self.toplevel.destroy();
        self.xdg_surface.destroy();
        self.surface.destroy();
    }
}

/// Attaches `buffer`, requests presentation feedback and commits, in that order.
fn commit(
    conn: &Connection,
    presentation: Option<&WpPresentation>,
    qh: &QueueHandle<State>,
    surface: &WlSurface,
    buffer: &WlBuffer,
    width: u32,
    height: u32,
) -> Result<(), String> {
    let (w, h) = (format::i32_dim(width)?, format::i32_dim(height)?);
    surface.attach(Some(buffer), 0, 0);
    surface.damage_buffer(0, 0, w, h);
    // Feedback must be requested before the commit it describes.
    if let Some(presentation) = presentation {
        presentation.feedback(surface, qh, ());
    }
    surface.commit();
    conn.flush()
        .map_err(|e| format!("flush Wayland requests: {e}"))
}

fn wl_shm_format(format: PixelFormat) -> Result<wl_shm::Format, String> {
    match format {
        // Xrgb8888 is BGRX in memory; Abgr8888 is RGBA in memory.
        PixelFormat::Bgrx => Ok(wl_shm::Format::Xrgb8888),
        PixelFormat::Rgba => Ok(wl_shm::Format::Abgr8888),
        other => Err(format!(
            "{other:?} has no wl_shm format; convert to BGRX or RGBA first"
        )),
    }
}

fn remaining(deadline: Instant, waiting_for: &str) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| format!("timed out waiting for {waiting_for}"))
}

impl FrameSink for WaylandRenderer {
    fn present(&mut self, frame: &VideoFrame) -> Result<(), String> {
        self.poll_events()?;
        match &frame.buffer {
            FrameBuffer::Cpu { data } => {
                self.present_cpu(frame.width, frame.height, frame.format, data)?;
            }
            FrameBuffer::Dmabuf(image) => {
                self.present_dmabuf(frame.width, frame.height, frame.format, image)?;
            }
        }
        self.present_count += 1;
        Ok(())
    }
}

impl std::fmt::Debug for WaylandRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaylandRenderer")
            .field("title", &self.title)
            .field("present_count", &self.present_count)
            .finish_non_exhaustive()
    }
}
