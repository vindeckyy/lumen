//! Primary source: captured input from the InputCapture portal's EIS socket, read through libei.

use std::collections::VecDeque;
use std::fmt;

use lumen_ffi_libei::{Context, Event};

use crate::portal::PortalSession;
use crate::{InputEvent, InputEventKind, InputSource, PIXELS_PER_NOTCH, now_us, portal, readable};

/// Captured local input from the InputCapture portal.
pub struct EisSource {
    ctx: Context,
    pending: VecDeque<InputEvent>,
    closed: bool,
    /// Owns the portal session and its runtime. Dropping them ends capture.
    _portal: PortalSession,
}

impl EisSource {
    /// Opens an InputCapture session and attaches its EIS socket. Fails when no portal is
    /// running, when libei is missing, or when the user denies capture.
    pub fn open_portal() -> Result<Self, String> {
        let (portal, fd) = portal::connect()?;
        let mut ctx = Context::new_receiver().map_err(|e| e.to_string())?;
        ctx.setup_fd(fd).map_err(|e| e.to_string())?;
        Ok(Self {
            ctx,
            pending: VecDeque::new(),
            closed: false,
            _portal: portal,
        })
    }

    /// True after the server disconnects. No more events follow.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Drains queued libei events into `pending`, dispatching socket data as needed. It stops
    /// once an event is pending, the socket is empty, or the connection closed.
    fn pump(&mut self) {
        loop {
            while let Some(event) = self.ctx.next_event() {
                if matches!(event, Event::Disconnected) {
                    self.closed = true;
                } else if let Some(kind) = to_kind(&event) {
                    self.pending.push_back(InputEvent {
                        kind,
                        received_us: now_us(),
                    });
                }
            }
            if self.closed || !self.pending.is_empty() {
                return;
            }
            let ready = self.ctx.as_fd().is_some_and(readable);
            if !ready {
                return;
            }
            self.ctx.dispatch();
        }
    }
}

impl InputSource for EisSource {
    fn poll_event(&mut self) -> Option<InputEvent> {
        if self.pending.is_empty() {
            self.pump();
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

/// Maps a libei event to an input event. Touch and other types are not part of [`InputEventKind`]
/// and are dropped.
fn to_kind(event: &Event) -> Option<InputEventKind> {
    Some(match *event {
        Event::Key { keycode, pressed } => InputEventKind::Key { keycode, pressed },
        Event::PointerMotion { dx, dy } => InputEventKind::Motion {
            dx,
            dy,
            x: f64::NAN,
            y: f64::NAN,
        },
        Event::PointerMotionAbsolute { x, y } => InputEventKind::Motion {
            dx: 0.0,
            dy: 0.0,
            x,
            y,
        },
        Event::Button { button, pressed } => InputEventKind::Button { button, pressed },
        Event::ScrollDelta { dx, dy } => InputEventKind::Scroll { dx, dy },
        // libei reports wheel steps in units of 120 per notch.
        Event::ScrollDiscrete { dx, dy } => InputEventKind::Scroll {
            dx: f64::from(dx) / 120.0 * PIXELS_PER_NOTCH,
            dy: f64::from(dy) / 120.0 * PIXELS_PER_NOTCH,
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discrete_steps_convert_to_pixels() {
        let kind = to_kind(&Event::ScrollDiscrete { dx: 0, dy: 120 }).expect("maps");
        assert_eq!(
            kind,
            InputEventKind::Scroll {
                dx: 0.0,
                dy: PIXELS_PER_NOTCH
            }
        );
    }

    #[test]
    fn relative_motion_has_no_absolute_position() {
        match to_kind(&Event::PointerMotion { dx: 3.0, dy: -2.0 }).expect("maps") {
            InputEventKind::Motion { dx, dy, x, y } => {
                assert_eq!((dx, dy), (3.0, -2.0));
                assert!(x.is_nan() && y.is_nan());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn touch_is_not_forwarded() {
        assert!(
            to_kind(&Event::TouchDown {
                id: 1,
                x: 0.0,
                y: 0.0
            })
            .is_none()
        );
    }
}
