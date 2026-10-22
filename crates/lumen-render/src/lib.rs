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
}

impl FrameSink for Renderer {
    fn present(&mut self, frame: &VideoFrame) -> Result<(), String> {
        match self {
            Self::Wayland(r) => r.present(frame),
            Self::X11(r) => r.present(frame),
        }
    }
}
