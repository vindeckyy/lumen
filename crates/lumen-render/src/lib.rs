//! Client presentation path.
//!
//! [`Renderer`] implements [`FrameSink`] for a Wayland or X11 window. The window is sized to
//! the frame and recreated when the frame size or format changes. Frames are committed as soon
//! as they are ready. Nothing waits for frame callbacks.
//!
//! Pixel formats: CPU frames must be BGRX or RGBA. dmabuf frames must be BGRX, RGBA, NV12 or P010,
//! with a `(fourcc, modifier)` pair the server advertised. Wayland takes any advertised modifier,
//! and X11 takes linear and implicit modifiers only. Anything else is rejected with `Err`.
#![forbid(unsafe_code)]

mod format;
pub mod resize;
mod shm;
mod wayland;
mod x11;

use lumen_protocol::media::{FrameSink, VideoFrame};

pub use wayland::WaylandRenderer;
pub use x11::X11Renderer;

/// The application id of the stream window: the Wayland `app_id` and the X11 `WM_CLASS`. It
/// matches the desktop entry, so the desktop shows the Lumen icon and name for the window.
pub const APP_ID: &str = "io.lumen.Lumen";

/// Which display server the renderer presents to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderBackend {
    /// The display named by `WAYLAND_DISPLAY`.
    Wayland,
    /// The display named by `DISPLAY`.
    X11,
}

/// A presenter for either backend, chosen at construction.
#[derive(Debug)]
pub enum Renderer {
    /// Wayland presenter.
    Wayland(WaylandRenderer),
    /// X11 presenter.
    X11(X11Renderer),
}

impl Renderer {
    /// Connects to the backend's display from the environment.
    ///
    /// # Errors
    /// Fails when the display cannot be reached or lacks a required protocol.
    pub fn new(backend: RenderBackend, title: &str) -> Result<Self, String> {
        match backend {
            RenderBackend::Wayland => WaylandRenderer::connect(title).map(Self::Wayland),
            RenderBackend::X11 => X11Renderer::connect(None, title).map(Self::X11),
        }
    }

    /// Presentation timestamp of the last frame the server reported as shown, in microseconds.
    #[must_use]
    pub fn last_present_ts_us(&self) -> Option<u64> {
        match self {
            Self::Wayland(r) => r.last_present_ts_us(),
            Self::X11(r) => r.last_present_ts_us(),
        }
    }

    /// Number of frames presented so far.
    #[must_use]
    pub fn present_count(&self) -> u64 {
        match self {
            Self::Wayland(r) => r.present_count(),
            Self::X11(r) => r.present_count(),
        }
    }

    /// Handles queued server events without blocking, so completion timestamps stay current.
    ///
    /// # Errors
    /// Returns a description if the connection failed or the server reported an error.
    pub fn poll_events(&mut self) -> Result<(), String> {
        match self {
            Self::Wayland(r) => r.poll_events(),
            Self::X11(r) => r.poll_events(),
        }
    }

    /// Whether the user asked to close the window, through its close button or the window
    /// manager. Read after [`Renderer::poll_events`].
    #[must_use]
    pub fn close_requested(&self) -> bool {
        match self {
            Self::Wayland(r) => r.close_requested(),
            Self::X11(r) => r.close_requested(),
        }
    }

    /// Makes the window fullscreen, or windowed again. The choice outlives a window that is
    /// recreated for a new picture size.
    ///
    /// # Errors
    /// Returns a description when the X server refused the request.
    pub fn set_fullscreen(&mut self, fullscreen: bool) -> Result<(), String> {
        match self {
            Self::Wayland(r) => {
                r.set_fullscreen(fullscreen);
                Ok(())
            }
            Self::X11(r) => r.set_fullscreen(fullscreen),
        }
    }

    /// The size of the client's screen in pixels, when the display server reports one.
    #[must_use]
    pub fn display_size(&self) -> Option<(u32, u32)> {
        match self {
            Self::Wayland(r) => r.display_size(),
            Self::X11(r) => r.display_size(),
        }
    }

    /// `(DRM fourcc, modifier)` pairs the presenter imports as DMA-BUFs: what the Wayland
    /// compositor advertised, or linear BGRX on X11 with DRI3 and Present.
    #[must_use]
    pub fn dmabuf_formats(&self) -> Vec<(u32, u64)> {
        match self {
            Self::Wayland(r) => r.dmabuf_formats(),
            Self::X11(r) => r.dmabuf_formats(),
        }
    }

    /// The id of the X11 window frames are presented in (0 before the first frame), shared so an
    /// input reader can follow it. `None` on Wayland.
    #[must_use]
    pub fn x11_window(&self) -> Option<std::sync::Arc<std::sync::atomic::AtomicU32>> {
        match self {
            Self::Wayland(_) => None,
            Self::X11(r) => Some(r.window_id()),
        }
    }

    /// The Wayland connection frames are presented on, so an input reader on the same client
    /// receives the seat events of the viewer's own surfaces. `None` on X11.
    #[must_use]
    pub fn wayland_connection(&self) -> Option<wayland_client::Connection> {
        match self {
            Self::Wayland(r) => Some(r.connection()),
            Self::X11(_) => None,
        }
    }
}

impl FrameSink for Renderer {
    fn present(&mut self, frame: &VideoFrame) -> Result<(), String> {
        match self {
            Self::Wayland(r) => r.present(frame),
            Self::X11(r) => r.present(frame),
        }
    }
}
