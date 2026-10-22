//! Host-side input injection.
//!
//! Three backends implement [`InputInjector`]:
//!
//! * [`UinputInjector`]: uinput virtual devices (keyboard, relative and absolute pointer, touch,
//!   pen). Works on any Linux session and needs write access to `/dev/uinput`.
//! * [`XTestInjector`]: XTEST on an X11 display.
//! * [`EiInjector`]: libei sender through the RemoteDesktop portal's EIS socket, for Wayland
//!   compositors that offer it.
//!
//! [`InputReceiver`] reads sealed input datagrams from the client and injects each one through
//! whichever backend the caller picked.
//!
//! This crate contains no `unsafe`. The uinput ioctls live in the `evdev` crate, and libei is
//! reached through `lumen-ffi-libei`.

#![forbid(unsafe_code)]

mod ei;
mod receiver;
mod text;
mod uinput;
mod xtest;

pub use ei::EiInjector;
pub use receiver::{DropReason, InputReceiver, Outcome, ReceiverStats};
pub use uinput::{TouchPhase, UinputInjector};
pub use xtest::XTestInjector;

use lumen_input_wayland::InputEvent;

/// Injects local input events into the host session.
pub trait InputInjector: Send {
    /// Injects one event. Absolute motion uses the client-surface coordinates the event carries.
    fn inject(&mut self, event: &InputEvent) -> Result<(), String>;
    /// Types `text`. Characters outside the US layout are an error.
    fn inject_text(&mut self, text: &str) -> Result<(), String>;
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
