//! Wayland presentation.
//!
//! One `xdg_toplevel` lives for the whole session. It is the black backdrop: a 1x1 buffer scaled
//! by `wp_viewport` to the window size. The picture is a `wl_subsurface` on top of it, scaled and
//! positioned by [`crate::fit::place`], so a resolution change or a window resize only moves
//! the subsurface and never destroys the window. A third subsurface carries the HUD text.
//!
//! CPU frames are copied into `wl_shm` buffers and dmabuf frames become `zwp_linux_dmabuf_v1`
//! buffers. Every frame is committed as soon as its buffer is attached, without waiting for a
//! frame callback. The window asks for server-side decorations when the compositor offers
//! `zxdg_decoration_manager_v1`, carries the [`crate::APP_ID`] so the desktop shows the Lumen
//! icon, records the user's close request and can be made fullscreen.

use std::collections::{HashSet, VecDeque};
use std::fs::File;
use std::os::fd::AsFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use lumen_protocol::media::{
    DmabufImage, FrameBuffer, FrameSink, KeepAlive, PixelFormat, VideoFrame,
};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use wayland_client::backend::WaylandError;
use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_buffer::{self, WlBuffer};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_region::WlRegion;
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_subcompositor::WlSubcompositor;
use wayland_client::protocol::wl_subsurface::WlSubsurface;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::{
    self, WpFractionalScaleV1,
};
use wayland_protocols::wp::idle_inhibit::zv1::client::zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1;
use wayland_protocols::wp::idle_inhibit::zv1::client::zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1;
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::{
    Flags, ZwpLinuxBufferParamsV1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::{
    self, ZwpLinuxDmabufV1,
};
use wayland_protocols::wp::presentation_time::client::wp_presentation::{self, WpPresentation};
use wayland_protocols::wp::presentation_time::client::wp_presentation_feedback::{
    self, WpPresentationFeedback,
};
use wayland_protocols::wp::tearing_control::v1::client::wp_tearing_control_manager_v1::WpTearingControlManagerV1;
use wayland_protocols::wp::tearing_control::v1::client::wp_tearing_control_v1::{
    PresentationHint, WpTearingControlV1,
};
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use wayland_protocols::xdg::decoration::zv1::client::zxdg_decoration_manager_v1::ZxdgDecorationManagerV1;
use wayland_protocols::xdg::decoration::zv1::client::zxdg_toplevel_decoration_v1::{
    Mode as DecorationMode, ZxdgToplevelDecorationV1,
};
use wayland_protocols::xdg::shell::client::xdg_surface::{self, XdgSurface};
use wayland_protocols::xdg::shell::client::xdg_toplevel::{self, XdgToplevel};
use wayland_protocols::xdg::shell::client::xdg_wm_base::{self, XdgWmBase};

use crate::fit::{self, Placement, VideoFit};
use crate::{format, overlay, shm};

const CONFIGURE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a frame waits for the compositor to release a buffer before presenting fails.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(1);
/// `wl_shm` buffers per picture: the compositor reads one while the next frame fills the other.
const SHM_SLOTS: usize = 2;
/// DMA-BUF buffers in flight per picture. Each one aliases a decoder pool slot, so they are
/// bounded; `present.rs` explains why two are safe.
const DMABUF_SLOTS: usize = 2;
/// Where the HUD sits, in logical pixels from the window's top left corner.
const OVERLAY_OFFSET: i32 = 12;
/// The smallest window the user can make, in logical pixels.
const MIN_VIEW: (i32, i32) = (320, 180);
/// Presented frames kept for the stats, oldest dropped first.
const PRESENTED_CAP: usize = 64;
/// `wp_presentation`'s `CLOCK_MONOTONIC` id.
const CLOCK_MONOTONIC: u32 = 1;

/// One `wl_output` as the compositor described it.
#[derive(Debug, Clone, Copy, Default)]
struct Output {
    /// The current mode in pixels, once the compositor reported one.
    mode: Option<(u32, u32)>,
    /// The integer scale of the output. Zero until the compositor says.
    scale: u32,
}

/// Event-driven state shared by every Wayland object the renderer creates.
#[derive(Default)]
struct State {
    configured: bool,
    /// `(drm fourcc, modifier)` pairs the compositor accepts for dmabuf import.
    dmabuf_formats: HashSet<(u32, u64)>,
    last_present_ts_us: Option<u64>,
    /// Frames the compositor showed, as `(frame index, presentation time)`.
    presented: VecDeque<(u64, u64)>,
    /// Whether `wp_presentation` stamps on `CLOCK_MONOTONIC`, the clock the stats use.
    monotonic: bool,
    /// The user asked to close the window.
    close_requested: bool,
    outputs: Vec<Output>,
    /// The window size the compositor asked for, applied by the next layout.
    pending_view: Option<(u32, u32)>,
    /// The window or the picture changed, so the subsurfaces need placing again.
    needs_layout: bool,
    /// The preferred fractional scale, in 120ths of a pixel.
    scale120: Option<u32>,
}

impl State {
    /// The output with the biggest current mode: it stands for the client's screen.
    fn largest_output(&self) -> Option<Output> {
        self.outputs
            .iter()
            .filter_map(|o| o.mode.map(|(w, h)| (u64::from(w) * u64::from(h), *o)))
            .max_by_key(|(area, _)| *area)
            .map(|(_, output)| output)
    }
}

impl Dispatch<WlOutput, usize> for State {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.get_mut(*index) else {
            return;
        };
        match event {
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } => {
                if flags
                    .into_result()
                    .is_ok_and(|f| f.contains(wl_output::Mode::Current))
                    && let (Ok(w), Ok(h)) = (u32::try_from(width), u32::try_from(height))
                {
                    output.mode = Some((w, h));
                    state.needs_layout = true;
                }
            }
            wl_output::Event::Scale { factor } => {
                if let Ok(factor) = u32::try_from(factor) {
                    output.scale = factor;
                    state.needs_layout = true;
                }
            }
            _ => {}
        }
    }
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
            state.needs_layout = true;
        }
    }
}

impl Dispatch<XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &XdgToplevel,
        event: xdg_toplevel::Event,
        &(): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                // A zero size means the compositor leaves the size to the client.
                if let (Ok(w), Ok(h)) = (u32::try_from(width), u32::try_from(height))
                    && w > 0
                    && h > 0
                {
                    state.pending_view = Some((w, h));
                }
                state.needs_layout = true;
            }
            xdg_toplevel::Event::Close => state.close_requested = true,
            _ => {}
        }
    }
}

impl Dispatch<WpPresentation, ()> for State {
    fn event(
        state: &mut Self,
        _: &WpPresentation,
        event: wp_presentation::Event,
        &(): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_presentation::Event::ClockId { clk_id } = event {
            state.monotonic = clk_id == CLOCK_MONOTONIC;
        }
    }
}

/// The user data of a feedback is the frame index it reports on.
impl Dispatch<WpPresentationFeedback, u64> for State {
    fn event(
        state: &mut Self,
        _: &WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        index: &u64,
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
            let ts = secs * 1_000_000 + u64::from(tv_nsec) / 1_000;
            state.last_present_ts_us = Some(ts);
            if state.monotonic {
                if state.presented.len() == PRESENTED_CAP {
                    state.presented.pop_front();
                }
                state.presented.push_back((*index, ts));
            }
        }
    }
}

impl Dispatch<WpFractionalScaleV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        &(): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state.scale120 = Some(scale);
            state.needs_layout = true;
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
delegate_noop!(State: WlSubcompositor);
delegate_noop!(State: WlSubsurface);
delegate_noop!(State: WlRegion);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: WlShmPool);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: WpViewporter);
delegate_noop!(State: WpViewport);
delegate_noop!(State: WpFractionalScaleManagerV1);
delegate_noop!(State: ZwpIdleInhibitManagerV1);
delegate_noop!(State: ZwpIdleInhibitorV1);
delegate_noop!(State: ZxdgDecorationManagerV1);
delegate_noop!(State: ignore ZxdgToplevelDecorationV1);
delegate_noop!(State: WpTearingControlManagerV1);
delegate_noop!(State: WpTearingControlV1);
delegate_noop!(State: ZwpLinuxBufferParamsV1);

/// Presents frames to a Wayland compositor in one window that is scaled to fit the picture.
pub struct WaylandRenderer {
    conn: Connection,
    queue: EventQueue<State>,
    qh: QueueHandle<State>,
    state: State,
    compositor: WlCompositor,
    shm: WlShm,
    wm_base: XdgWmBase,
    subcompositor: WlSubcompositor,
    viewporter: WpViewporter,
    presentation: Option<WpPresentation>,
    tearing: Option<WpTearingControlManagerV1>,
    dmabuf: Option<ZwpLinuxDmabufV1>,
    decorations: Option<ZxdgDecorationManagerV1>,
    fractional: Option<WpFractionalScaleManagerV1>,
    idle_manager: Option<ZwpIdleInhibitManagerV1>,
    title: String,
    /// Whether the user wants the window fullscreen; kept until the window exists.
    fullscreen: bool,
    /// How the picture is fitted into the window.
    fit: VideoFit,
    /// Whether the window keeps the screen awake while it is shown.
    keep_awake: bool,
    /// The HUD text the window shows; empty hides the HUD.
    overlay_lines: Vec<String>,
    window: Option<Window>,
    /// dmabuf `wl_buffer`s that the compositor may still read; freed once released. The lease of
    /// the pool slot the buffer aliases is kept with it, so the producer does not write over a
    /// buffer the compositor still reads.
    dmabuf_buffers: Vec<(WlBuffer, Arc<AtomicBool>, Option<KeepAlive>)>,
    present_count: u64,
}

/// The window and its surfaces. The toplevel surface is the backdrop, the picture is a subsurface
/// of it, and the HUD is a subsurface above the picture.
struct Window {
    /// The toplevel's surface, which shows the black backdrop.
    surface: WlSurface,
    /// The toplevel role. Its size request follows the layout's viewport.
    toplevel: XdgToplevel,
    backdrop_viewport: WpViewport,
    video: WlSurface,
    video_sub: WlSubsurface,
    video_viewport: WpViewport,
    overlay: Overlay,
    idle: Option<ZwpIdleInhibitorV1>,
    /// The window size in logical pixels.
    view: (u32, u32),
    /// The picture size and format last presented.
    frame: Option<(u32, u32, PixelFormat)>,
    /// Shared memory for CPU pictures, rebuilt when the picture size or format changes.
    shm: Option<ShmSlots>,
    /// Where the picture sits in the window.
    placement: Option<Placement>,
}

/// The HUD subsurface and its current text.
struct Overlay {
    surface: WlSurface,
    viewport: WpViewport,
    buffer: Option<WlBuffer>,
    /// The raster scale the current text was drawn at.
    g: u32,
}

struct ShmSlots {
    file: File,
    slot_len: u64,
    /// The picture size and format the slots hold.
    key: (u32, u32, PixelFormat),
    buffers: Vec<(WlBuffer, Arc<AtomicBool>)>,
}

impl WaylandRenderer {
    /// The connection this renderer presents on. Wayland connections are shared handles.
    #[must_use]
    pub fn connection(&self) -> Connection {
        self.conn.clone()
    }

    /// Connects to the display named by `WAYLAND_DISPLAY`.
    ///
    /// # Errors
    /// Fails when the display is unreachable or the compositor lacks `wl_compositor`,
    /// `wl_subcompositor`, `wl_shm`, `xdg_wm_base` or `wp_viewporter`.
    pub fn connect(title: &str) -> Result<Self, String> {
        let conn =
            Connection::connect_to_env().map_err(|e| format!("connect to Wayland display: {e}"))?;
        let (globals, mut queue): (GlobalList, EventQueue<State>) =
            registry_queue_init(&conn).map_err(|e| format!("read Wayland registry: {e}"))?;
        let qh = queue.handle();
        let compositor: WlCompositor = globals
            .bind(&qh, 1..=6, ())
            .map_err(|e| format!("bind wl_compositor: {e}"))?;
        let subcompositor: WlSubcompositor = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| format!("bind wl_subcompositor: {e}"))?;
        let shm: WlShm = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| format!("bind wl_shm: {e}"))?;
        let wm_base: XdgWmBase = globals
            .bind(&qh, 1..=6, ())
            .map_err(|e| format!("bind xdg_wm_base: {e}"))?;
        let viewporter: WpViewporter = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| format!("bind wp_viewporter: {e}"))?;
        // Optional globals: a missing one disables its feature, it does not fail the connect.
        let presentation: Option<WpPresentation> = globals.bind(&qh, 1..=2, ()).ok();
        let tearing: Option<WpTearingControlManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
        // Version 3 is the last with `modifier` events that list accepted formats.
        let dmabuf: Option<ZwpLinuxDmabufV1> = globals.bind(&qh, 3..=3, ()).ok();
        let decorations: Option<ZxdgDecorationManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
        let fractional: Option<WpFractionalScaleManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
        let idle_manager: Option<ZwpIdleInhibitManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
        let mut state = State::default();
        // Every output is bound so the roundtrip below reports its mode and scale. Each output is
        // numbered by its position, which is the user data its events carry.
        let outputs = globals.contents().with_list(|list| {
            list.iter()
                .filter(|g| g.interface == "wl_output")
                .map(|g| (g.name, g.version))
                .collect::<Vec<_>>()
        });
        state.outputs = vec![Output::default(); outputs.len()];
        for (index, (name, version)) in outputs.into_iter().enumerate() {
            let _: WlOutput = globals.registry().bind(name, version.min(4), &qh, index);
        }
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
            subcompositor,
            viewporter,
            presentation,
            tearing,
            dmabuf,
            decorations,
            fractional,
            idle_manager,
            title: title.to_owned(),
            fullscreen: false,
            fit: VideoFit::default(),
            keep_awake: false,
            overlay_lines: Vec::new(),
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

    /// Whether the compositor draws the window's title bar (`zxdg_decoration_manager_v1`).
    /// Without it the window has no frame to move or close it by.
    #[must_use]
    pub fn server_decorations(&self) -> bool {
        self.decorations.is_some()
    }

    /// The size of the client's screen in pixels, once the compositor reported an output.
    #[must_use]
    pub fn display_size(&self) -> Option<(u32, u32)> {
        self.state.largest_output().and_then(|o| o.mode)
    }

    /// The integer scale of the client's screen, or 1 when the compositor did not say.
    #[must_use]
    pub fn display_scale(&self) -> u32 {
        self.state.largest_output().map_or(1, |o| o.scale.max(1))
    }

    /// Whether the user asked to close the window.
    #[must_use]
    pub fn close_requested(&self) -> bool {
        self.state.close_requested
    }

    /// Makes the window fullscreen, or windowed again. The choice outlives the window.
    pub fn set_fullscreen(&mut self, fullscreen: bool) {
        if self.fullscreen == fullscreen {
            return;
        }
        self.fullscreen = fullscreen;
        if let Some(window) = &self.window {
            apply_fullscreen(&window.toplevel, fullscreen);
            window.surface.commit();
            let _ = self.conn.flush();
        }
    }

    /// Sets how the picture is fitted into the window and places it again.
    ///
    /// # Errors
    /// Returns a description when the compositor connection fails.
    pub fn set_fit(&mut self, fit: VideoFit) -> Result<(), String> {
        if self.fit == fit {
            return Ok(());
        }
        self.fit = fit;
        self.state.needs_layout = true;
        self.layout()
    }

    /// Keeps the screen awake while the window is shown. Applies at once when the window exists.
    ///
    /// # Errors
    /// Returns a description when the request cannot be sent. A compositor without the idle
    /// inhibitor protocol keeps its own setting, and this is not an error.
    pub fn set_keep_awake(&mut self, on: bool) -> Result<(), String> {
        self.keep_awake = on;
        let Some(window) = self.window.as_mut() else {
            return Ok(());
        };
        match (on, window.idle.is_some()) {
            (true, false) => {
                window.idle = self
                    .idle_manager
                    .as_ref()
                    .map(|m| m.create_inhibitor(&window.surface, &self.qh, ()));
            }
            (false, true) => {
                if let Some(idle) = window.idle.take() {
                    idle.destroy();
                }
            }
            _ => {}
        }
        self.conn
            .flush()
            .map_err(|e| format!("flush Wayland requests: {e}"))
    }

    /// Where the picture sits in the window, in logical pixels. `None` before the first frame.
    #[must_use]
    pub fn placement(&self) -> Option<Placement> {
        self.window.as_ref().and_then(|w| w.placement)
    }

    /// The window's size in device pixels, when the window exists.
    #[must_use]
    pub fn window_pixels(&self) -> Option<(u32, u32)> {
        let window = self.window.as_ref()?;
        let s = self.scale();
        Some((
            (f64::from(window.view.0) * s).round() as u32,
            (f64::from(window.view.1) * s).round() as u32,
        ))
    }

    /// The toplevel's surface, for an input reader that locks the pointer on it.
    #[must_use]
    pub fn surface(&self) -> Option<WlSurface> {
        self.window.as_ref().map(|w| w.surface.clone())
    }

    /// The frames the compositor showed since the last call, as `(frame index, presentation
    /// time in microseconds)`.
    pub fn take_presented(&mut self) -> Vec<(u64, u64)> {
        self.state.presented.drain(..).collect()
    }

    /// Sets the HUD text drawn over the picture. Empty lines hide it.
    ///
    /// # Errors
    /// Returns a description when a buffer cannot be created or sent.
    pub fn set_overlay(&mut self, lines: &[String]) -> Result<(), String> {
        lines.clone_into(&mut self.overlay_lines);
        let g = self.overlay_g();
        self.apply_overlay(g)
    }

    /// Dispatches pending compositor events without blocking.
    ///
    /// # Errors
    /// Returns a description if the connection failed.
    pub fn poll_events(&mut self) -> Result<(), String> {
        self.pump(Duration::ZERO)
    }

    /// The scale of the window's pixels: the fractional scale when the compositor sent one.
    fn scale(&self) -> f64 {
        match self.state.scale120 {
            Some(scale120) => f64::from(scale120) / 120.0,
            None => f64::from(self.display_scale()),
        }
    }

    /// The raster scale of the HUD: two font pixels per logical pixel, in device pixels.
    fn overlay_g(&self) -> u32 {
        (2.0 * self.scale()).round().max(1.0) as u32
    }

    fn present_cpu(
        &mut self,
        width: u32,
        height: u32,
        format: PixelFormat,
        data: &[u8],
        index: u64,
    ) -> Result<(), String> {
        format::check_cpu_frame(width, height, format, data.len())?;
        self.ensure_window((width, height))?;
        self.set_frame(width, height, format)?;
        self.ensure_shm(width, height, format)?;
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
        shm::write_pixels(&shm.file, slot as u64 * shm.slot_len, data, false, false)?;
        let (buffer, busy) = &shm.buffers[slot];
        busy.store(true, Ordering::Release);
        commit(
            &self.conn,
            self.presentation.as_ref(),
            &self.qh,
            &win.video,
            buffer,
            (width, height),
            index,
        )?;
        self.layout()
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
        index: u64,
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
        self.ensure_window((width, height))?;
        self.set_frame(width, height, format)?;
        // At most DMABUF_SLOTS buffers are busy. A decoded picture aliases the decoder's pool, so a
        // buffer the compositor still reads must not be overwritten by a new import.
        let deadline = Instant::now() + RELEASE_TIMEOUT;
        while self
            .dmabuf_buffers
            .iter()
            .filter(|(_, busy, _)| busy.load(Ordering::Acquire))
            .count()
            >= DMABUF_SLOTS
        {
            self.pump(remaining(deadline, "a released dmabuf wl_buffer")?)?;
        }
        self.dmabuf_buffers.retain(|(buffer, busy, _)| {
            let in_use = busy.load(Ordering::Acquire);
            if !in_use {
                buffer.destroy();
            }
            in_use
        });
        let params = dmabuf.create_params(&self.qh, ());
        // Each `add` sends the same buffer object as the fd of one plane.
        let (modifier_hi, modifier_lo) = ((image.modifier >> 32) as u32, image.modifier as u32);
        for (plane_index, plane) in image.planes().iter().enumerate() {
            params.add(
                image.fd.as_fd(),
                plane_index as u32,
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
            &win.video,
            &buffer,
            (width, height),
            index,
        )?;
        self.dmabuf_buffers
            .push((buffer, busy, image.keepalive.clone()));
        self.layout()
    }

    /// Creates the window at its first picture's size. The window lives until the renderer does.
    fn ensure_window(&mut self, frame: (u32, u32)) -> Result<(), String> {
        if self.window.is_none() {
            let window = self.create_window(frame)?;
            self.window = Some(window);
            self.state.needs_layout = true;
        }
        Ok(())
    }

    fn create_window(&mut self, frame: (u32, u32)) -> Result<Window, String> {
        let view = fit::initial_size(frame, self.display_size(), self.display_scale());
        let surface = self.compositor.create_surface(&self.qh, ());
        let xdg_surface = self.wm_base.get_xdg_surface(&surface, &self.qh, ());
        let toplevel = xdg_surface.get_toplevel(&self.qh, ());
        toplevel.set_title(self.title.clone());
        toplevel.set_app_id(crate::APP_ID.to_owned());
        toplevel.set_min_size(MIN_VIEW.0, MIN_VIEW.1);
        // The decoration and fractional-scale objects are not stored: they live as long as the
        // connection, and the events they send are dispatched to `State`.
        if let Some(manager) = &self.decorations {
            let decoration = manager.get_toplevel_decoration(&toplevel, &self.qh, ());
            decoration.set_mode(DecorationMode::ServerSide);
        }
        if self.fullscreen {
            apply_fullscreen(&toplevel, true);
        }
        if let Some(manager) = &self.fractional {
            let _ = manager.get_fractional_scale(&surface, &self.qh, ());
        }
        let backdrop_viewport = self.viewporter.get_viewport(&surface, &self.qh, ());

        let video = self.compositor.create_surface(&self.qh, ());
        let video_sub = self
            .subcompositor
            .get_subsurface(&video, &surface, &self.qh, ());
        video_sub.set_desync();
        let video_viewport = self.viewporter.get_viewport(&video, &self.qh, ());
        if let Some(manager) = &self.tearing {
            let tearing = manager.get_tearing_control(&video, &self.qh, ());
            // Async: do not wait for vblank. Only compositors that scan out fullscreen surfaces honour it.
            tearing.set_presentation_hint(PresentationHint::Async);
        }

        let overlay_surface = self.compositor.create_surface(&self.qh, ());
        let overlay_sub =
            self.subcompositor
                .get_subsurface(&overlay_surface, &surface, &self.qh, ());
        overlay_sub.set_desync();
        overlay_sub.set_position(OVERLAY_OFFSET, OVERLAY_OFFSET);
        let overlay_viewport = self.viewporter.get_viewport(&overlay_surface, &self.qh, ());

        // An empty input region: the pointer over the picture and the HUD reaches the toplevel,
        // which is the surface the input reader follows.
        for part in [&video, &overlay_surface] {
            let region = self.compositor.create_region(&self.qh, ());
            part.set_input_region(Some(&region));
            region.destroy();
        }

        let idle = if self.keep_awake {
            self.idle_manager
                .as_ref()
                .map(|manager| manager.create_inhibitor(&surface, &self.qh, ()))
        } else {
            None
        };

        let backdrop = self.backdrop_buffer()?;
        // An empty commit makes the compositor send the first configure, which must be acked
        // before a buffer is attached.
        self.state.configured = false;
        surface.commit();
        let deadline = Instant::now() + CONFIGURE_TIMEOUT;
        while !self.state.configured {
            self.pump_events(remaining(deadline, "the initial xdg_surface configure")?)?;
        }
        surface.attach(Some(&backdrop), 0, 0);
        Ok(Window {
            surface,
            toplevel,
            backdrop_viewport,
            video,
            video_sub,
            video_viewport,
            overlay: Overlay {
                surface: overlay_surface,
                viewport: overlay_viewport,
                buffer: None,
                g: 0,
            },
            idle,
            view,
            frame: None,
            shm: None,
            placement: None,
        })
    }

    /// A 1x1 black `wl_buffer`, which the backdrop viewport scales to the window.
    fn backdrop_buffer(&self) -> Result<WlBuffer, String> {
        // A fresh memfd is zero-filled, so the single XRGB8888 pixel is black.
        let file = shm::create(4)?;
        let pool: WlShmPool = self.shm.create_pool(file.as_fd(), 4, &self.qh, ());
        let buffer = pool.create_buffer(
            0,
            1,
            1,
            4,
            wl_shm::Format::Xrgb8888,
            &self.qh,
            Arc::new(AtomicBool::new(false)),
        );
        pool.destroy();
        Ok(buffer)
    }

    /// Records the picture's size and format. A change re-places the picture.
    fn set_frame(&mut self, width: u32, height: u32, format: PixelFormat) -> Result<(), String> {
        let window = self
            .window
            .as_mut()
            .ok_or("window missing before the frame")?;
        let frame = (width, height, format);
        if window.frame == Some(frame) {
            return Ok(());
        }
        window.frame = Some(frame);
        // The viewport's source takes effect at the next commit, which would pair it with the old
        // buffer. Detach the picture first, so no commit pairs a source with a buffer it does not fit.
        window.video.attach(None, 0, 0);
        window.video.commit();
        self.state.needs_layout = true;
        self.layout()
    }

    /// Places the window's surfaces. Runs when the window, the picture, the fit or the scale
    /// changed. It applies the size the compositor asked for, scales the backdrop to the window,
    /// and positions and crops the picture.
    fn layout(&mut self) -> Result<(), String> {
        if !self.state.needs_layout {
            return Ok(());
        }
        self.state.needs_layout = false;
        if let Some(view) = self.state.pending_view.take()
            && let Some(window) = self.window.as_mut()
        {
            window.view = view;
        }
        let g = self.overlay_g();
        if self
            .window
            .as_ref()
            .is_some_and(|window| window.overlay.g != g)
        {
            self.apply_overlay(g)?;
        }
        let fit = self.fit;
        let Some(window) = self.window.as_mut() else {
            return Ok(());
        };
        window
            .backdrop_viewport
            .set_destination(to_i32(window.view.0), to_i32(window.view.1));
        if let Some((frame_w, frame_h, _)) = window.frame {
            window.placement = fit::place(fit, (frame_w, frame_h), window.view);
            if let Some(p) = window.placement {
                window.video_sub.set_position(p.dst[0], p.dst[1]);
                window.video_viewport.set_destination(p.dst[2], p.dst[3]);
                let [src_x, src_y, src_w, src_h] = p.src;
                window.video_viewport.set_source(src_x, src_y, src_w, src_h);
                window.video.commit();
            }
        }
        window.surface.commit();
        self.conn
            .flush()
            .map_err(|e| format!("flush Wayland requests: {e}"))
    }

    /// Draws the HUD text at raster scale `g` and attaches it to the overlay subsurface. Empty
    /// text detaches the buffer, which hides the HUD.
    fn apply_overlay(&mut self, g: u32) -> Result<(), String> {
        let scale = self.scale();
        let image = overlay::rasterize(&self.overlay_lines, g);
        let Some(window) = self.window.as_mut() else {
            return Ok(());
        };
        window.overlay.g = g;
        match image {
            None => {
                window.overlay.surface.attach(None, 0, 0);
                window.overlay.surface.commit();
                if let Some(old) = window.overlay.buffer.take() {
                    old.destroy();
                }
            }
            Some(image) => {
                let len = image.pixels.len() as u64;
                let file = shm::create(len)?;
                shm::write_pixels(&file, 0, &image.pixels, false, false)?;
                let pool: WlShmPool = self.shm.create_pool(
                    file.as_fd(),
                    i32::try_from(len).map_err(|_| "the HUD is too large".to_owned())?,
                    &self.qh,
                    (),
                );
                let (w, h) = (
                    format::i32_dim(image.width)?,
                    format::i32_dim(image.height)?,
                );
                let buffer = pool.create_buffer(
                    0,
                    w,
                    h,
                    w * 4,
                    wl_shm::Format::Argb8888,
                    &self.qh,
                    Arc::new(AtomicBool::new(false)),
                );
                pool.destroy();
                // The HUD is drawn at `g` device pixels per font pixel; the window shows it at the
                // logical size, which is the device size over the scale.
                window.overlay.viewport.set_destination(
                    (f64::from(w) / scale).round() as i32,
                    (f64::from(h) / scale).round() as i32,
                );
                window.overlay.surface.attach(Some(&buffer), 0, 0);
                window.overlay.surface.damage_buffer(0, 0, w, h);
                window.overlay.surface.commit();
                if let Some(old) = window.overlay.buffer.replace(buffer) {
                    old.destroy();
                }
            }
        }
        self.conn
            .flush()
            .map_err(|e| format!("flush Wayland requests: {e}"))
    }

    /// Rebuilds the `wl_shm` slots when the picture size or format changed.
    fn ensure_shm(&mut self, width: u32, height: u32, format: PixelFormat) -> Result<(), String> {
        let wl_format = wl_shm_format(format)?;
        let window = self
            .window
            .as_mut()
            .ok_or("window missing before shm setup")?;
        let key = (width, height, format);
        if window.shm.as_ref().is_some_and(|shm| shm.key == key) {
            return Ok(());
        }
        if let Some(old) = window.shm.take() {
            old.destroy();
        }
        window.shm = Some(ShmSlots::new(
            &self.shm, &self.qh, width, height, wl_format, key,
        )?);
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

    /// Dispatches events, then places the window's surfaces if they changed.
    fn pump(&mut self, timeout: Duration) -> Result<(), String> {
        self.pump_events(timeout)?;
        self.layout()
    }

    /// Flushes requests, then waits up to `timeout` for the socket to become readable and
    /// dispatches whatever arrived.
    fn pump_events(&mut self, timeout: Duration) -> Result<(), String> {
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
        // A hang-up does not clear by waiting, so fail rather than let callers spin until their
        // deadline. Read and dispatch what the compositor sent before it went: a protocol error
        // it posted is then reported with its object and message.
        let hung_up = fds[0].revents().intersects(PollFlags::HUP | PollFlags::ERR);
        match guard.read() {
            Ok(_) => {}
            // Poll reported the socket readable but no event could be read. That is a spurious
            // wakeup, not a failure; the next pass polls again.
            Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(format!("read Wayland events: {e}")),
        }
        self.dispatch_pending()?;
        if hung_up {
            return Err("Wayland connection closed or failed".to_owned());
        }
        Ok(())
    }
}

impl ShmSlots {
    fn new(
        shm: &WlShm,
        qh: &QueueHandle<State>,
        width: u32,
        height: u32,
        format: wl_shm::Format,
        key: (u32, u32, PixelFormat),
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
            key,
            buffers,
        })
    }

    /// Destroys the buffers. The compositor keeps its own reference to any it still reads.
    fn destroy(self) {
        for (buffer, _) in self.buffers {
            buffer.destroy();
        }
    }
}

impl FrameSink for WaylandRenderer {
    fn present(&mut self, frame: &VideoFrame) -> Result<(), String> {
        self.poll_events()?;
        match &frame.buffer {
            FrameBuffer::Cpu { data } => {
                self.present_cpu(frame.width, frame.height, frame.format, data, frame.index)?;
            }
            FrameBuffer::Dmabuf(image) => {
                self.present_dmabuf(frame.width, frame.height, frame.format, image, frame.index)?;
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

/// Attaches `buffer` to `surface`, requests presentation feedback for frame `index` and commits,
/// in that order.
#[allow(clippy::too_many_arguments)]
fn commit(
    conn: &Connection,
    presentation: Option<&WpPresentation>,
    qh: &QueueHandle<State>,
    surface: &WlSurface,
    buffer: &WlBuffer,
    (width, height): (u32, u32),
    index: u64,
) -> Result<(), String> {
    let (w, h) = (format::i32_dim(width)?, format::i32_dim(height)?);
    surface.attach(Some(buffer), 0, 0);
    surface.damage_buffer(0, 0, w, h);
    // Feedback must be requested before the commit it describes.
    if let Some(presentation) = presentation {
        presentation.feedback(surface, qh, index);
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

fn to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Asks the compositor for the fullscreen state, on the output it chooses.
fn apply_fullscreen(toplevel: &XdgToplevel, fullscreen: bool) {
    if fullscreen {
        toplevel.set_fullscreen(None);
    } else {
        toplevel.unset_fullscreen();
    }
}
