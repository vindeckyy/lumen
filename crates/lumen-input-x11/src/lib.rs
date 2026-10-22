//! X11 client-side input capture.
//!
//! The client reads the input of the local X server and turns it into the
//! actions of the Lumen input protocol:
//!
//! * [`raw::RawEventSource`] reads the raw XI2 events of the X server. Raw
//!   events arrive straight from the device, before the server applies
//!   pointer acceleration, which is what a remote-desktop client wants.
//! * [`keyboard::Keymap`] turns the keycodes of the X server into the evdev
//!   codes the protocol carries, and names them with xkbcommon.
//!
//! The source never blocks for longer than the timeout the caller gives it, so
//! a caller stays in control of its own latency.
//!
//! ```
//! use std::time::Duration;
//!
//! use lumen_input_x11::{RawConfig, RawEventSource};
//!
//! // A display is needed to open the source.
//! if let Ok(mut source) = RawEventSource::open(&RawConfig::default()) {
//!     if let Some(action) = source.poll_action(Duration::from_millis(16)).ok() {
//!         println!("{action:?}");
//!     }
//! }
//! ```

#![forbid(unsafe_code)]

pub mod keyboard;
pub mod raw;
pub mod window;

pub use keyboard::{Keymap, KeymapError, evdev_code_from_x11, x11_keycode_from_evdev};
pub use raw::{
    AxisLayout, PointerMotion, PointerTracker, RawConfig, RawError, RawEvent, RawEventSource,
};
pub use window::WindowEventSource;

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_protocol::input::InputAction;

    #[test]
    fn a_keycode_becomes_an_evdev_code() {
        assert_eq!(evdev_code_from_x11(38), Some(30));
        assert_eq!(x11_keycode_from_evdev(30), Some(38));
    }

    #[test]
    fn a_keymap_names_a_key() {
        let map = Keymap::empty();
        assert!(!map.defines(38));
        assert_eq!(map.keysym_name(38), None);
        let action: Option<InputAction> = map.key_action(38, true);
        assert_eq!(
            action,
            Some(InputAction::Key {
                keycode: 30,
                pressed: true
            })
        );
    }

    #[test]
    fn the_source_needs_a_display() {
        // Without a display the source says so instead of failing the test.
        if std::env::var("DISPLAY").is_ok() {
            return;
        }
        let err = RawEventSource::open(&RawConfig::default()).expect_err("no display");
        assert!(matches!(err, RawError::Connect(_)));
    }
}
