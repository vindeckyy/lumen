//! Input from one window: the keyboard and pointer events the X server delivers to the viewer's
//! own window, as protocol actions.
//!
//! Unlike [`crate::RawEventSource`], nothing typed into another window is seen: key events only
//! arrive while the window has the focus, and pointer events only while the pointer is over it.
//! Positions are in window pixels. Events made by XTEST or by Lumen's own uinput devices are
//! dropped, so a viewer on the screen it controls does not feed its own injected input back.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use lumen_protocol::input::InputAction;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

use crate::keyboard::evdev_code_from_x11;
use crate::raw::{MASTER_DEVICES, RawError, evdev_button, wheel_scroll};

/// Reads the input events of one window.
#[derive(Debug)]
pub struct WindowEventSource {
    conn: RustConnection,
    window: u32,
    /// Whether each source device is an injector (XTEST or Lumen's uinput), by id.
    injected: HashMap<u16, bool>,
    /// Keys and buttons the host was told are down, released when the focus leaves.
    keys_down: BTreeSet<u32>,
    buttons_down: BTreeSet<u32>,
    pending: VecDeque<InputAction>,
}

impl WindowEventSource {
    /// Opens `display` (`None` reads `$DISPLAY`). No window is watched until [`Self::set_window`].
    ///
    /// # Errors
    /// Returns [`RawError::Connect`] when the display or XInput 2.2 is not available.
    pub fn open(display: Option<&str>) -> Result<Self, RawError> {
        let (conn, _) =
            RustConnection::connect(display).map_err(|e| RawError::Connect(e.to_string()))?;
        conn.xinput_xi_query_version(2, 2)
            .map_err(|e| RawError::Connect(e.to_string()))?
            .reply()
            .map_err(|e| RawError::Connect(e.to_string()))?;
        Ok(Self {
            conn,
            window: 0,
            injected: HashMap::new(),
            keys_down: BTreeSet::new(),
            buttons_down: BTreeSet::new(),
            pending: VecDeque::new(),
        })
    }

    /// Watch `window` from now on. Anything held down on the previous window is released.
    ///
    /// # Errors
    /// Returns [`RawError::Select`] when the events cannot be selected on the window.
    pub fn set_window(&mut self, window: u32) -> Result<(), RawError> {
        if window == self.window {
            return Ok(());
        }
        self.release_all();
        self.window = window;
        if window == 0 {
            return Ok(());
        }
        let mask = xinput::XIEventMask::KEY_PRESS
            | xinput::XIEventMask::KEY_RELEASE
            | xinput::XIEventMask::BUTTON_PRESS
            | xinput::XIEventMask::BUTTON_RELEASE
            | xinput::XIEventMask::MOTION
            | xinput::XIEventMask::FOCUS_OUT;
        let masks = [xinput::EventMask {
            deviceid: MASTER_DEVICES,
            mask: vec![mask],
        }];
        self.conn
            .xinput_xi_select_events(window, &masks)
            .map_err(|e| RawError::Select(e.to_string()))?
            .check()
            .map_err(|e| RawError::Select(e.to_string()))
    }

    /// Waits up to `timeout` for the next action. `None` means the timeout passed.
    ///
    /// # Errors
    /// Returns [`RawError::Poll`] when waiting fails and [`RawError::Connection`] when the X
    /// connection breaks.
    pub fn poll_action(&mut self, timeout: Duration) -> Result<Option<InputAction>, RawError> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(action) = self.pending.pop_front() {
                return Ok(Some(action));
            }
            if let Some(event) = self
                .conn
                .poll_for_event()
                .map_err(|e| RawError::Connection(e.to_string()))?
            {
                self.translate(&event);
                continue;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let fd = self.conn.stream().as_fd();
            let mut fds = [PollFd::new(&fd, PollFlags::IN)];
            let timespec = Timespec {
                tv_sec: i64::try_from(remaining.as_secs()).unwrap_or(i64::MAX),
                tv_nsec: i64::from(remaining.subsec_nanos()),
            };
            match poll(&mut fds, Some(&timespec)) {
                Ok(0) => return Ok(None),
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(RawError::Poll(e.to_string())),
            }
        }
    }

    /// Queue the actions one X event stands for.
    fn translate(&mut self, event: &Event) {
        match event {
            Event::XinputKeyPress(ev) | Event::XinputKeyRelease(ev) => {
                if ev.event != self.window || self.is_injected(ev.sourceid) {
                    return;
                }
                let pressed = matches!(event, Event::XinputKeyPress(_));
                // The host repeats a held key on its own; forwarding the client's repeats would
                // double them.
                if pressed && ev.flags.contains(xinput::KeyEventFlags::KEY_REPEAT) {
                    return;
                }
                let Some(keycode) = u8::try_from(ev.detail).ok().and_then(evdev_code_from_x11)
                else {
                    return;
                };
                if pressed {
                    self.keys_down.insert(keycode);
                } else if !self.keys_down.remove(&keycode) {
                    return;
                }
                self.pending
                    .push_back(InputAction::Key { keycode, pressed });
            }
            Event::XinputButtonPress(ev) | Event::XinputButtonRelease(ev) => {
                if ev.event != self.window || self.is_injected(ev.sourceid) {
                    return;
                }
                let pressed = matches!(event, Event::XinputButtonPress(_));
                self.pending.push_back(position(ev.event_x, ev.event_y));
                let Ok(x_button) = u8::try_from(ev.detail) else {
                    return;
                };
                if let Some((dx, dy)) = wheel_scroll(x_button) {
                    if pressed {
                        self.pending.push_back(InputAction::Scroll { dx, dy });
                    }
                } else if let Some(button) = evdev_button(x_button) {
                    if pressed {
                        self.buttons_down.insert(button);
                    } else if !self.buttons_down.remove(&button) {
                        return;
                    }
                    self.pending
                        .push_back(InputAction::Button { button, pressed });
                }
            }
            Event::XinputMotion(ev) => {
                if ev.event == self.window && !self.is_injected(ev.sourceid) {
                    self.pending.push_back(position(ev.event_x, ev.event_y));
                }
            }
            Event::XinputFocusOut(ev) if ev.event == self.window => self.release_all(),
            _ => {}
        }
    }

    /// Queue a release for everything held down, so nothing stays stuck on the host.
    fn release_all(&mut self) {
        for keycode in std::mem::take(&mut self.keys_down) {
            self.pending.push_back(InputAction::Key {
                keycode,
                pressed: false,
            });
        }
        for button in std::mem::take(&mut self.buttons_down) {
            self.pending.push_back(InputAction::Button {
                button,
                pressed: false,
            });
        }
    }

    /// Whether the device `id` is XTEST or one of Lumen's uinput devices. Looked up once per id.
    fn is_injected(&mut self, id: u16) -> bool {
        if let Some(&injected) = self.injected.get(&id) {
            return injected;
        }
        let injected = self
            .conn
            .xinput_xi_query_device(id)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .and_then(|reply| reply.infos.into_iter().next())
            .is_some_and(|info| is_injector_name(&String::from_utf8_lossy(&info.name)));
        self.injected.insert(id, injected);
        injected
    }
}

/// An absolute position in window pixels, from XI2 16.16 fixed point.
fn position(x: xinput::Fp1616, y: xinput::Fp1616) -> InputAction {
    InputAction::MotionAbsolute {
        x: f64::from(x) / 65536.0,
        y: f64::from(y) / 65536.0,
    }
}

/// Whether a device name is an input injector: the server's XTEST devices or Lumen's own.
fn is_injector_name(name: &str) -> bool {
    name.contains("XTEST") || name.starts_with("Lumen virtual")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injector_devices_are_recognised_by_name() {
        assert!(is_injector_name("Virtual core XTEST pointer"));
        assert!(is_injector_name("Virtual core XTEST keyboard"));
        assert!(is_injector_name("Lumen virtual absolute pointer"));
        assert!(!is_injector_name("Virtual core pointer"));
        assert!(!is_injector_name("SynPS/2 Synaptics TouchPad"));
        assert!(!is_injector_name("AT Translated Set 2 keyboard"));
    }

    #[test]
    fn positions_come_out_of_fixed_point() {
        assert_eq!(
            position(100 * 65536 + 32768, 7 * 65536),
            InputAction::MotionAbsolute { x: 100.5, y: 7.0 }
        );
    }
}
