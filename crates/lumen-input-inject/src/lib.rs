//! Host-side input injection.
//!
//! Four backends implement [`InputInjector`]:
//!
//! * [`UinputInjector`]: uinput virtual devices (keyboard, relative and absolute pointer). Works
//!   on any Linux session and needs write access to `/dev/uinput`.
//! * [`WlrInjector`]: the wlroots virtual pointer and keyboard protocols, as a Wayland client.
//!   Reaches a headless wlroots compositor that ignores uinput, and needs no device access.
//! * [`XTestInjector`]: XTEST on an X11 display.
//! * [`EiInjector`]: EI sender through `$LIBEI_SOCKET` or the RemoteDesktop portal's EIS
//!   socket, for Wayland compositors that offer it.
//!
//! Touch and pen samples go through [`TouchInjector`], which only [`UinputTouch`] implements: no
//! display protocol carries them, so they are always injected into the kernel.
//!
//! [`InputReceiver`] reads sealed input datagrams from the client and injects each one through
//! whichever backend the caller picked. [`Gamepads`] turns the client's gamepads into virtual
//! DualSense pads through `/dev/uhid`, whatever the display backend is.
//!
//! This crate contains no `unsafe`. The uinput ioctls live in the `evdev` crate, the EI
//! protocol is spoken by the pure-Rust `reis` crate, and uhid takes plain reads and writes.

#![forbid(unsafe_code)]

mod ei;
mod gamepad;
#[cfg(test)]
mod knobs;
mod receiver;
mod text;
mod uinput;
mod wlr;
mod xtest;

pub use ei::EiInjector;
pub use gamepad::{Gamepads, PAD_MAC_PREFIX, Rumble};
pub use receiver::{DropReason, InputReceiver, Outcome, ReceiverStats};
pub use uinput::{UinputInjector, UinputTouch};
pub use wlr::WlrInjector;
pub use xtest::XTestInjector;

use lumen_input_wayland::InputEvent;
use lumen_protocol::input::TouchPhase;

/// Injects local input events into the host session.
pub trait InputInjector: Send {
    /// Injects one event. Absolute motion uses the client-surface coordinates the event carries.
    fn inject(&mut self, event: &InputEvent) -> Result<(), String>;
    /// Types `text`. Characters outside the US layout are an error.
    fn inject_text(&mut self, text: &str) -> Result<(), String>;
}

/// Injects touch contacts and pen samples.
pub trait TouchInjector: Send {
    /// Injects one touch contact. `slot` tells the contacts apart, and the coordinates are
    /// client-surface pixels.
    fn touch(&mut self, slot: u8, phase: TouchPhase, x: f64, y: f64) -> Result<(), String>;
    /// Injects one pen sample. `pressure` is 0 to 4095, and `in_range` says the pen is near the
    /// surface.
    fn pen(&mut self, x: f64, y: f64, pressure: u16, in_range: bool) -> Result<(), String>;
}

/// Turns fractional deltas into whole steps. The remainder carries over, so slow motion is not
/// lost to rounding.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Residual(f64);

impl Residual {
    /// Adds `delta` and returns the whole steps accumulated so far.
    pub(crate) fn take(&mut self, delta: f64) -> i32 {
        self.0 += delta;
        let whole = self.0.trunc();
        self.0 -= whole;
        // Float to int casts saturate in Rust, so a huge delta cannot wrap.
        whole as i32
    }
}

#[cfg(test)]
mod tests {
    use super::Residual;

    #[test]
    fn residual_carries_fractions() {
        let mut r = Residual::default();
        assert_eq!(r.take(0.5), 0);
        assert_eq!(r.take(0.5), 1);
        assert_eq!(r.take(-1.25), -1);
        assert_eq!(r.take(0.0), 0);
        assert_eq!(r.take(-0.75), -1);
    }
}
