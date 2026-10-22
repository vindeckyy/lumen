//! The host's input path: sealed input datagrams from the client become events on this machine.
//!
//! The input socket is its own, with its own socket and thread, so no input event waits behind
//! video. A host that captures an X11 display injects into that display through XTEST, so the
//! input lands on the screen being shared. Otherwise it injects through uinput, which reaches
//! native Wayland windows.

use std::net::SocketAddr;
use std::sync::Arc;

use lumen_crypto::PacketCipher;
use lumen_input_inject::{InputInjector, UinputInjector, XTestInjector};
use lumen_input_wayland::{InputEvent, InputEventKind};
use lumen_protocol::input::InputAction;
use lumen_transport::input_channel::{InputReceiver, InputThread, InputThreadOutcome};

use crate::settings::HostSettings;

/// The UDP port of the host's input socket. The client sends its input to this port.
pub const INPUT_PORT: u16 = 24820;

/// Where the injected events go.
#[derive(Debug, Clone)]
pub enum InjectTarget {
    /// The X11 display the host captures (`None` reads `$DISPLAY`), through XTEST.
    X11(Option<String>),
    /// The kernel, through uinput.
    Uinput,
}

/// The input path of one session: a socket thread that injects every event it receives.
#[derive(Debug)]
pub struct InputStage {
    thread: InputThread,
}

impl InputStage {
    /// Binds `addr` and injects the events it receives into `target`. Positions arrive in pixels
    /// of the picture the host serves and are scaled to `capture`, the size of the captured
    /// screen. Without an injector or the socket the session runs with no input, and the log
    /// says why.
    // shortcut: one input socket per host port, so only the first session to start gets input;
    // upgrade to a socket per session when a second client has to drive the same host.
    #[must_use]
    pub fn start(
        addr: SocketAddr,
        cipher: PacketCipher,
        target: &InjectTarget,
        capture: (u32, u32),
        settings: Arc<HostSettings>,
    ) -> Option<Self> {
        let (width, height) = capture;
        // The injector gets positions already in capture pixels, so its own scale is one.
        let injector: Result<Box<dyn InputInjector>, String> = match target {
            InjectTarget::X11(display) => {
                XTestInjector::connect_to(display.as_deref(), width, height)
                    .map(|i| Box::new(i) as Box<dyn InputInjector>)
            }
            InjectTarget::Uinput => {
                UinputInjector::new(width, height).map(|i| Box::new(i) as Box<dyn InputInjector>)
            }
        };
        let mut injector = match injector {
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
        crate::host_log!(info "lumen-host: input from the client is injected through {}", match target {
            InjectTarget::X11(_) => "XTEST",
            InjectTarget::Uinput => "uinput",
        });
        // A failing injector fails on every event, so the first failure is the only one logged.
        let mut reported = false;
        let thread = InputThread::spawn(receiver, move |envelope| {
            let served = settings.quality();
            let scale = (
                f64::from(width) / f64::from(served.width.max(1)),
                f64::from(height) / f64::from(served.height.max(1)),
            );
            let event = to_event(envelope.action, envelope.client_ts_us, scale);
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

/// The injector's event for one wire action. Absolute positions are multiplied by `scale`, from
/// picture pixels to screen pixels. Relative motion carries `NaN` positions, which is how the
/// event type marks it.
fn to_event(action: InputAction, received_us: u64, scale: (f64, f64)) -> InputEvent {
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
            x: x * scale.0,
            y: y * scale.1,
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
    fn relative_motion_has_no_position_and_absolute_is_scaled_to_the_screen() {
        match to_event(
            InputAction::MotionRelative { dx: 2.0, dy: -3.0 },
            1,
            (1.5, 1.5),
        )
        .kind
        {
            InputEventKind::Motion { dx, dy, x, y } => {
                assert_eq!((dx, dy), (2.0, -3.0));
                assert!(x.is_nan() && y.is_nan());
            }
            other => panic!("relative motion became {other:?}"),
        }
        // A 1280x720 picture of a 1920x1080 screen: the centre stays the centre.
        match to_event(
            InputAction::MotionAbsolute { x: 640.0, y: 360.0 },
            1,
            (1.5, 1.5),
        )
        .kind
        {
            InputEventKind::Motion { dx, dy, x, y } => {
                assert_eq!((dx, dy, x, y), (0.0, 0.0, 960.0, 540.0));
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
            (1.0, 1.0),
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
                0,
                (2.0, 2.0)
            )
            .kind,
            InputEventKind::Button {
                button: 0x110,
                pressed: false
            }
        );
    }
}
