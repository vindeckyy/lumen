//! Fallback source: `wl_seat` pointer and keyboard events, plus relative pointer motion while the
//! pointer is locked. These events only reach surfaces owned by this client, so the caller passes
//! in its connection and, for pointer lock, one of its surfaces.

use std::collections::VecDeque;
use std::fmt;
use std::io::ErrorKind;

use wayland_client::backend::WaylandError;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_keyboard::{self, WlKeyboard};
use wayland_client::protocol::wl_pointer::{self, WlPointer};
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_locked_pointer_v1::{
    self, ZwpLockedPointerV1,
};
use wayland_protocols::wp::pointer_constraints::zv1::client::zwp_pointer_constraints_v1::{
    Lifetime, ZwpPointerConstraintsV1,
};
use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1;
use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::{
    self, ZwpRelativePointerV1,
};

use crate::keyboard::KeyFilter;
use crate::{InputEvent, InputEventKind, InputSource, now_us};

/// Fallback input source built on `wl_seat`.
pub struct WaylandSource {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    closed: bool,
}

#[derive(Default)]
struct State {
    pending: VecDeque<InputEvent>,
    keys: Option<KeyFilter>,
    surface: Option<WlSurface>,
    pointer: Option<WlPointer>,
    relative_manager: Option<ZwpRelativePointerManagerV1>,
    relative: Option<ZwpRelativePointerV1>,
    constraints: Option<ZwpPointerConstraintsV1>,
    locked: Option<ZwpLockedPointerV1>,
    pointer_locked: bool,
    // Keeps the seat and keyboard proxies alive for as long as the source exists.
    seat: Option<WlSeat>,
    keyboard: Option<WlKeyboard>,
}

impl WaylandSource {
    /// Binds `wl_seat` and optional pointer protocols on the connection the app already uses.
    /// `surface` is used only by [`Self::set_pointer_locked`].
    pub fn connect(conn: &Connection, surface: Option<&WlSurface>) -> Result<Self, String> {
        let (globals, mut queue) =
            registry_queue_init::<State>(conn).map_err(|e| format!("wl_registry: {e}"))?;
        let qh = queue.handle();
        let mut state = State {
            surface: surface.cloned(),
            ..State::default()
        };
        let seat: WlSeat = globals
            .bind(&qh, 1..=7, ())
            .map_err(|e| format!("no wl_seat global: {e}"))?;
        state.seat = Some(seat);
        state.relative_manager = globals.bind(&qh, 1..=1, ()).ok();
        state.constraints = globals.bind(&qh, 1..=1, ()).ok();
        // The roundtrip delivers the seat capabilities, which create the pointer and keyboard.
        queue
            .roundtrip(&mut state)
            .map_err(|e| format!("wl_seat roundtrip: {e}"))?;
        Ok(Self {
            conn: conn.clone(),
            queue,
            state,
            closed: false,
        })
    }

    /// True after the connection failed. No more events follow.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Locks (`true`) or unlocks (`false`) the pointer on the surface passed to [`Self::connect`].
    /// While locked, the source reports relative motion. Fails when the compositor lacks pointer
    /// constraints or when no surface or pointer is available.
    pub fn set_pointer_locked(&mut self, locked: bool) -> Result<(), String> {
        let qh = self.queue.handle();
        let st = &mut self.state;
        if locked {
            if st.locked.is_some() {
                return Ok(());
            }
            let (Some(constraints), Some(surface), Some(pointer)) =
                (&st.constraints, &st.surface, &st.pointer)
            else {
                return Err(
                    "pointer lock needs pointer constraints, a surface and a pointer".to_owned(),
                );
            };
            st.locked = Some(constraints.lock_pointer(
                surface,
                pointer,
                None,
                Lifetime::Persistent,
                &qh,
                (),
            ));
        } else if let Some(locked_pointer) = st.locked.take() {
            locked_pointer.destroy();
            st.pointer_locked = false;
        }
        self.conn.flush().map_err(|e| format!("flush: {e}"))
    }

    /// Sends queued requests and reads whatever the compositor has sent. Never blocks.
    fn pump(&mut self) {
        if self.closed {
            return;
        }
        if self.queue.dispatch_pending(&mut self.state).is_err() {
            self.closed = true;
            return;
        }
        if let Err(e) = self.conn.flush()
            && is_fatal(&e)
        {
            self.closed = true;
            return;
        }
        if !crate::readable(std::os::fd::AsFd::as_fd(&self.conn)) {
            return;
        }
        if let Some(guard) = self.conn.prepare_read() {
            match guard.read() {
                Ok(_) => {}
                Err(e) if !is_fatal(&e) => {}
                Err(_) => {
                    self.closed = true;
                    return;
                }
            }
        }
        if self.queue.dispatch_pending(&mut self.state).is_err() {
            self.closed = true;
        }
    }
}

/// A `WouldBlock` I/O error only means "try again later". Anything else ends the connection.
fn is_fatal(e: &WaylandError) -> bool {
    !matches!(e, WaylandError::Io(io) if io.kind() == ErrorKind::WouldBlock)
}

impl InputSource for WaylandSource {
    fn poll_event(&mut self) -> Option<InputEvent> {
        if self.state.pending.is_empty() {
            self.pump();
        }
        self.state.pending.pop_front()
    }
}

impl fmt::Debug for WaylandSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WaylandSource")
            .field("pending", &self.state.pending.len())
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        (): &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities { capabilities } = event else {
            return;
        };
        let WEnum::Value(caps) = capabilities else {
            return;
        };
        if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
            let pointer = seat.get_pointer(qh, ());
            if let Some(manager) = &state.relative_manager {
                state.relative = Some(manager.get_relative_pointer(&pointer, qh, ()));
            }
            state.pointer = Some(pointer);
        }
        if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
            state.keyboard = Some(seat.get_keyboard(qh, ()));
        }
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlPointer,
        event: wl_pointer::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let kind = match event {
            // Absolute motion stops while the pointer is locked, so only report it when unlocked.
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } if !state.pointer_locked => Some(InputEventKind::Motion {
                dx: 0.0,
                dy: 0.0,
                x: surface_x,
                y: surface_y,
            }),
            wl_pointer::Event::Button {
                button,
                state: button_state,
                ..
            } => Some(InputEventKind::Button {
                button,
                pressed: matches!(button_state, WEnum::Value(wl_pointer::ButtonState::Pressed)),
            }),
            wl_pointer::Event::Axis { axis, value, .. } => match axis {
                WEnum::Value(wl_pointer::Axis::VerticalScroll) => {
                    Some(InputEventKind::Scroll { dx: 0.0, dy: value })
                }
                WEnum::Value(wl_pointer::Axis::HorizontalScroll) => {
                    Some(InputEventKind::Scroll { dx: value, dy: 0.0 })
                }
                _ => None,
            },
            _ => None,
        };
        if let Some(kind) = kind {
            state.pending.push_back(InputEvent {
                kind,
                received_us: now_us(),
            });
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => {
                // Anything other than XKB v1 is not a format we can filter by, so every key is kept.
                state.keys = match format {
                    WEnum::Value(wl_keyboard::KeymapFormat::XkbV1) => {
                        KeyFilter::from_fd(fd, size as usize).ok()
                    }
                    _ => None,
                };
            }
            wl_keyboard::Event::Key {
                key,
                state: key_state,
                ..
            } if state.keys.as_ref().is_none_or(|k| k.contains(key)) => {
                let pressed = matches!(key_state, WEnum::Value(wl_keyboard::KeyState::Pressed));
                state.pending.push_back(InputEvent {
                    kind: InputEventKind::Key {
                        keycode: key,
                        pressed,
                    },
                    received_us: now_us(),
                });
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpRelativePointerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpRelativePointerV1,
        event: zwp_relative_pointer_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Relative deltas are reported only while locked. When unlocked, absolute motion covers
        // the pointer, and reporting both would move it twice.
        if let zwp_relative_pointer_v1::Event::RelativeMotion {
            dx_unaccel,
            dy_unaccel,
            ..
        } = event
            && state.pointer_locked
        {
            state.pending.push_back(InputEvent {
                kind: InputEventKind::Motion {
                    dx: dx_unaccel,
                    dy: dy_unaccel,
                    x: f64::NAN,
                    y: f64::NAN,
                },
                received_us: now_us(),
            });
        }
    }
}

impl Dispatch<ZwpLockedPointerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpLockedPointerV1,
        event: zwp_locked_pointer_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_locked_pointer_v1::Event::Locked => state.pointer_locked = true,
            zwp_locked_pointer_v1::Event::Unlocked => state.pointer_locked = false,
            _ => {}
        }
    }
}

delegate_noop!(State: ZwpRelativePointerManagerV1);
delegate_noop!(State: ZwpPointerConstraintsV1);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binds_seat_on_a_running_compositor() {
        let conn = match Connection::connect_to_env() {
            Ok(conn) => conn,
            Err(e) => {
                eprintln!("skip: no Wayland compositor reachable ({e}); set WAYLAND_DISPLAY");
                return;
            }
        };
        let mut source = WaylandSource::connect(&conn, None).expect("wl_seat binds");
        assert!(!source.is_closed());
        // Nothing is focused, so no input is expected. The point is that polling never blocks
        // and never fails.
        assert!(source.poll_event().is_none());
        assert!(!source.is_closed());
    }
}
