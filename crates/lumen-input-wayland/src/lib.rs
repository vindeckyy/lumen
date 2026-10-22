//! Client-side capture of local input.
//!
//! Two sources produce [`InputEvent`] values:
//!
//! * [`EisSource`] reads captured input from the InputCapture portal's EIS connection through
//!   the pure-Rust `reis` crate ([`ei_peer`]). This is the primary path.
//! * [`WaylandSource`] reads `wl_seat` pointer and keyboard events for the client's own surfaces.
//!   It is the fallback when the portal is unavailable.
//!
//! [`open_best`] tries the portal first. [`InputSender`] ships events to the host as sealed UDP
//! datagrams. The event types here depend on nothing else in the workspace, so the host and the
//! client can both use them.

#![forbid(unsafe_code)]

pub mod ei_peer;
mod eis;
mod keyboard;
mod portal;
mod seat;
mod sender;

use std::os::fd::BorrowedFd;

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use wayland_client::Connection;
use wayland_client::protocol::wl_surface::WlSurface;

pub use eis::EisSource;
pub use seat::WaylandSource;
pub use sender::{InputSender, SendError};

/// Logical pixels that one wheel notch stands for. Discrete (notch) scroll events from the portal
/// and Wayland's pixel scroll events are both expressed in pixels using this factor.
pub const PIXELS_PER_NOTCH: f64 = 15.0;

/// One local input event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InputEvent {
    /// What happened.
    pub kind: InputEventKind,
    /// `CLOCK_MONOTONIC` microseconds at which this client received the event.
    pub received_us: u64,
}

/// The payload of an [`InputEvent`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEventKind {
    /// Keyboard key. `keycode` is an evdev code (`KEY_*`), independent of layout.
    Key { keycode: u32, pressed: bool },
    /// Pointer motion.
    ///
    /// Absolute motion has finite `x` and `y`, the pointer position in the client's surface
    /// coordinates, and `dx` and `dy` are zero. Relative motion, used for pointer lock and games,
    /// has `x` and `y` set to `NaN` and `dx` and `dy` set to the unaccelerated delta.
    Motion { dx: f64, dy: f64, x: f64, y: f64 },
    /// Pointer button. `button` is an evdev code (`BTN_*`).
    Button { button: u32, pressed: bool },
    /// Scroll in logical pixels. Positive `dy` scrolls down and positive `dx` scrolls right.
    Scroll { dx: f64, dy: f64 },
}

/// A source of local input events.
pub trait InputSource: Send {
    /// Returns the next event without blocking, or `None` when nothing is pending.
    fn poll_event(&mut self) -> Option<InputEvent>;
}

/// Opens the best available source. The InputCapture portal is tried first, and it may show a
/// consent dialog while it waits. If it fails and `conn` is given, the Wayland seat fallback is
/// used. `surface` is the client surface that the fallback uses for pointer lock.
pub fn open_best(
    conn: Option<&Connection>,
    surface: Option<&WlSurface>,
) -> Result<Box<dyn InputSource>, String> {
    let portal_err = match EisSource::open_portal() {
        Ok(source) => return Ok(Box::new(source)),
        Err(e) => e,
    };
    match conn {
        Some(conn) => Ok(Box::new(WaylandSource::connect(conn, surface)?)),
        None => Err(format!(
            "no input source: InputCapture portal failed ({portal_err}) and no Wayland connection was given"
        )),
    }
}

/// Monotonic time in microseconds (`CLOCK_MONOTONIC`). Same clock as `Context::now` in libei, and
/// the clock that the input stamps use, so client and host readings compare directly.
pub fn now_us() -> u64 {
    let ts = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1_000
}

/// True when `fd` has data to read. Never blocks.
pub fn readable(fd: BorrowedFd<'_>) -> bool {
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    let zero = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    matches!(poll(&mut fds, Some(&zero)), Ok(n) if n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_clock_advances() {
        let a = now_us();
        let b = now_us();
        assert!(b >= a);
        assert!(a > 0);
    }

    #[test]
    fn readable_is_false_for_idle_pipe() {
        let (r, _w) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        assert!(!readable(std::os::fd::AsFd::as_fd(&r)));
    }
}
