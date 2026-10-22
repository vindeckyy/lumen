//! The host's input path: sealed input datagrams from the client become events on this machine.
//!
//! The input socket is its own, with its own socket and thread, so no input event waits behind
//! video. Events are injected through uinput, which reaches native Wayland windows. XTEST does not.

use std::net::SocketAddr;

use lumen_crypto::PacketCipher;
use lumen_input_inject::{InputInjector, UinputInjector};
use lumen_input_wayland::{InputEvent, InputEventKind};
use lumen_protocol::input::InputAction;
use lumen_transport::input_channel::{InputReceiver, InputThread, InputThreadOutcome};

/// The UDP port of the host's input socket. The client sends its input to this port.
pub const INPUT_PORT: u16 = 24820;

/// The input path of one session: a socket thread that injects every event it receives.
#[derive(Debug)]
pub struct InputStage {
    thread: InputThread,
}

impl InputStage {
    /// Binds `addr` and injects the events it receives into a uinput device of `width` by
    /// `height`. Without uinput or without the socket the session runs with no input, and the log
    /// says why.
    // shortcut: one input socket per host port, so only the first session to start gets input;
    // upgrade to a socket per session when a second client has to drive the same host.
    #[must_use]
    pub fn start(addr: SocketAddr, cipher: PacketCipher, width: u32, height: u32) -> Option<Self> {
        let mut injector = match UinputInjector::new(width, height) {
            Ok(injector) => injector,
            Err(e) => {
                crate::host_log!(warn "lumen-host: input injection unavailable: {e}");
                return None;
            }
        };
        let receiver = match InputReceiver::bind(addr, cipher) {
            Ok(receiver) => receiver,
            Err(e) => {
                crate::host_log!(warn "lumen-host: input socket {addr} unavailable: {e}");
                return None;
            }
        };
        // A failing injector fails on every event, so the first failure is the only one logged.
        let mut reported = false;
        let thread = InputThread::spawn(receiver, move |envelope| {
            let event = to_event(envelope.action, envelope.client_ts_us);
            if let Err(e) = injector.inject(&event)
                && !reported
            {
                reported = true;
                crate::host_log!(warn "lumen-host: input injection failed: {e}");
            }
        });
        Some(Self { thread })
    }

    /// Stops the input thread and returns its counters.
    pub fn stop(self) -> InputThreadOutcome {
        self.thread.stop()
    }
}

/// The injector's event for one wire action. Relative motion carries `NaN` positions, which is how
/// the event type marks it.
fn to_event(action: InputAction, received_us: u64) -> InputEvent {
    let kind = match action {
        InputAction::Key { keycode, pressed } => InputEventKind::Key { keycode, pressed },
        InputAction::MotionRelative { dx, dy } => InputEventKind::Motion {
            dx,
            dy,
            x: f64::NAN,
            y: f64::NAN,
        },
        InputAction::MotionAbsolute { x, y } => InputEventKind::Motion {
            dx: 0.0,
            dy: 0.0,
            x,
            y,
        },
        InputAction::Button { button, pressed } => InputEventKind::Button { button, pressed },
        InputAction::Scroll { dx, dy } => InputEventKind::Scroll { dx, dy },
    };
    InputEvent { kind, received_us }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_motion_has_no_position_and_absolute_has_no_delta() {
        match to_event(InputAction::MotionRelative { dx: 2.0, dy: -3.0 }, 1).kind {
            InputEventKind::Motion { dx, dy, x, y } => {
                assert_eq!((dx, dy), (2.0, -3.0));
                assert!(x.is_nan() && y.is_nan());
            }
            other => panic!("relative motion became {other:?}"),
        }
        match to_event(InputAction::MotionAbsolute { x: 10.0, y: 20.0 }, 1).kind {
            InputEventKind::Motion { dx, dy, x, y } => {
                assert_eq!((dx, dy, x, y), (0.0, 0.0, 10.0, 20.0));
            }
            other => panic!("absolute motion became {other:?}"),
        }
    }

    #[test]
    fn keys_and_buttons_keep_their_codes_and_state() {
        let key = to_event(
            InputAction::Key {
                keycode: 30,
                pressed: true,
            },
            7,
        );
        assert_eq!(key.received_us, 7);
        assert_eq!(
            key.kind,
            InputEventKind::Key {
                keycode: 30,
                pressed: true
            }
        );
        assert_eq!(
            to_event(
                InputAction::Button {
                    button: 0x110,
                    pressed: false
                },
                0
            )
            .kind,
            InputEventKind::Button {
                button: 0x110,
                pressed: false
            }
        );
    }
}
