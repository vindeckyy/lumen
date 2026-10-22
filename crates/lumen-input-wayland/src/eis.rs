//! Primary source: captured input from the InputCapture portal's EIS socket, read through the
//! pure-Rust `reis` crate as an EI receiver.

use std::collections::VecDeque;
use std::fmt;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use reis::ei;

use crate::ei_peer::EiPeer;
use crate::portal::PortalSession;
use crate::{InputEvent, InputEventKind, InputSource, PIXELS_PER_NOTCH, now_us, portal};

/// How long the EIS server has to answer the handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Captured local input from the InputCapture portal.
pub struct EisSource {
    peer: EiPeer,
    pending: VecDeque<InputEvent>,
    closed: bool,
    /// Owns the portal session and its runtime, when the socket came from the portal. Dropping
    /// them ends capture.
    _portal: Option<PortalSession>,
}

impl EisSource {
    /// Connects to `$LIBEI_SOCKET`, or opens an InputCapture session and attaches its EIS
    /// socket. Fails when no portal is running, when the server does not answer, or when the
    /// user denies capture.
    ///
    /// # Errors
    /// Returns the portal or handshake failure.
    pub fn open_portal() -> Result<Self, String> {
        let (context, portal) = if let Some(context) =
            ei::Context::connect_to_env().map_err(|e| format!("LIBEI_SOCKET: {e}"))?
        {
            (context, None)
        } else {
            let (portal, fd) = portal::connect()?;
            let context =
                ei::Context::new(UnixStream::from(fd)).map_err(|e| format!("EI context: {e}"))?;
            (context, Some(portal))
        };
        let peer = EiPeer::connect(
            context,
            "lumen-client",
            ei::handshake::ContextType::Receiver,
            HANDSHAKE_TIMEOUT,
        )?;
        Ok(Self {
            peer,
            pending: VecDeque::new(),
            closed: false,
            _portal: portal,
        })
    }

    /// True after the server disconnects. No more events follow.
    pub fn is_closed(&self) -> bool {
        self.closed
    }
}

impl InputSource for EisSource {
    fn poll_event(&mut self) -> Option<InputEvent> {
        if self.pending.is_empty() && !self.closed {
            let pending = &mut self.pending;
            let result = self.peer.dispatch(|_, event| {
                if let Some(kind) = to_kind(&event) {
                    pending.push_back(InputEvent {
                        kind,
                        received_us: now_us(),
                    });
                }
            });
            self.closed = result.is_err();
        }
        self.pending.pop_front()
    }
}

impl fmt::Debug for EisSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EisSource")
            .field("pending", &self.pending.len())
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

/// Maps an EI input event to an input event. Touch, text and frames are not part of
/// [`InputEventKind`] and are dropped.
fn to_kind(event: &ei::Event) -> Option<InputEventKind> {
    Some(match event {
        ei::Event::Keyboard(_, ei::keyboard::Event::Key { key, state }) => InputEventKind::Key {
            keycode: *key,
            pressed: *state == ei::keyboard::KeyState::Press,
        },
        ei::Event::Pointer(_, ei::pointer::Event::MotionRelative { x, y }) => {
            InputEventKind::Motion {
                dx: f64::from(*x),
                dy: f64::from(*y),
                x: f64::NAN,
                y: f64::NAN,
            }
        }
        ei::Event::PointerAbsolute(_, ei::pointer_absolute::Event::MotionAbsolute { x, y }) => {
            InputEventKind::Motion {
                dx: 0.0,
                dy: 0.0,
                x: f64::from(*x),
                y: f64::from(*y),
            }
        }
        ei::Event::Button(_, ei::button::Event::Button { button, state }) => {
            InputEventKind::Button {
                button: *button,
                pressed: *state == ei::button::ButtonState::Press,
            }
        }
        ei::Event::Scroll(_, ei::scroll::Event::Scroll { x, y }) => InputEventKind::Scroll {
            dx: f64::from(*x),
            dy: f64::from(*y),
        },
        // EI reports wheel steps in units of 120 per notch.
        ei::Event::Scroll(_, ei::scroll::Event::ScrollDiscrete { x, y }) => {
            InputEventKind::Scroll {
                dx: f64::from(*x) / 120.0 * PIXELS_PER_NOTCH,
                dy: f64::from(*y) / 120.0 * PIXELS_PER_NOTCH,
            }
        }
        _ => return None,
    })
}
