//! X11 host injection through the XTEST extension.

use lumen_input_wayland::{InputEvent, InputEventKind, PIXELS_PER_NOTCH};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::Window;
use x11rb::protocol::xtest::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

use crate::text::{KEY_LEFTSHIFT, for_each_tap};
use crate::{InputInjector, Residual};

// XTEST event types, from the X protocol headers.
const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const MOTION_NOTIFY: u8 = 6;
/// `detail` for an absolute motion: root coordinates.
const MOTION_ABSOLUTE: u8 = 0;
/// `detail` for a relative motion: offsets from the current position.
const MOTION_RELATIVE: u8 = 1;
/// Wheel buttons, one press and release per notch.
const WHEEL_UP: u8 = 4;
const WHEEL_DOWN: u8 = 5;
const WHEEL_LEFT: u8 = 6;
const WHEEL_RIGHT: u8 = 7;
/// `CurrentTime`.
const TIME_NOW: u32 = 0;

/// Injects into an X11 display through XTEST. Absolute positions are scaled from the client
/// surface to the host root window.
#[derive(Debug)]
pub struct XTestInjector {
    conn: RustConnection,
    root: Window,
    host_width: f64,
    host_height: f64,
    client_width: f64,
    client_height: f64,
    rel_x: Residual,
    rel_y: Residual,
    wheel_v: Residual,
    wheel_h: Residual,
}

impl XTestInjector {
    /// Connects to `$DISPLAY` and checks that the XTEST extension is present.
    pub fn connect(client_width: u32, client_height: u32) -> Result<Self, String> {
        if client_width == 0 || client_height == 0 {
            return Err("client resolution must be non-zero".to_owned());
        }
        let (conn, screen_num) = RustConnection::connect(None)
            .map_err(|e| format!("cannot connect to X display: {e}"))?;
        let screen = conn
            .setup()
            .roots
            .get(screen_num)
            .ok_or("X server reported no screen at the default index")?;
        let (root, host_width, host_height) = (
            screen.root,
            f64::from(screen.width_in_pixels),
            f64::from(screen.height_in_pixels),
        );
        conn.xtest_get_version(2, 2)
            .map_err(|e| format!("XTEST query: {e}"))?
            .reply()
            .map_err(|e| format!("XTEST not available on this display: {e}"))?;
        Ok(Self {
            conn,
            root,
            host_width,
            host_height,
            client_width: f64::from(client_width),
            client_height: f64::from(client_height),
            rel_x: Residual::default(),
            rel_y: Residual::default(),
            wheel_v: Residual::default(),
            wheel_h: Residual::default(),
        })
    }

    /// Sends one XTEST event and flushes it.
    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<(), String> {
        xtest::fake_input(&self.conn, kind, detail, TIME_NOW, self.root, x, y, 0)
            .map_err(|e| format!("XTEST fake_input: {e}"))?;
        self.conn.flush().map_err(|e| format!("X flush: {e}"))
    }

    /// Key press or release. X keycodes are evdev codes plus 8.
    fn key(&self, evdev: u32, pressed: bool) -> Result<(), String> {
        let xkey = u8::try_from(evdev.saturating_add(8))
            .map_err(|_| format!("keycode {evdev} has no X keycode"))?;
        let kind = if pressed { KEY_PRESS } else { KEY_RELEASE };
        self.fake(kind, xkey, 0, 0)
    }

    fn tap(&self, evdev: u32, shift: bool) -> Result<(), String> {
        if shift {
            self.key(KEY_LEFTSHIFT, true)?;
        }
        self.key(evdev, true)?;
        self.key(evdev, false)?;
        if shift {
            self.key(KEY_LEFTSHIFT, false)?;
        }
        Ok(())
    }

    fn absolute_motion(&self, x: f64, y: f64) -> Result<(), String> {
        let px = scale(x, self.host_width, self.client_width);
        let py = scale(y, self.host_height, self.client_height);
        self.fake(MOTION_NOTIFY, MOTION_ABSOLUTE, px, py)
    }

    fn relative_motion(&mut self, dx: f64, dy: f64) -> Result<(), String> {
        let (rx, ry) = (self.rel_x.take(dx), self.rel_y.take(dy));
        if rx == 0 && ry == 0 {
            return Ok(());
        }
        self.fake(MOTION_NOTIFY, MOTION_RELATIVE, to_i16(rx), to_i16(ry))
    }

    fn button(&self, evdev: u32, pressed: bool) -> Result<(), String> {
        let x_button =
            x_button(evdev).ok_or_else(|| format!("no X button for evdev code {evdev:#x}"))?;
        let kind = if pressed {
            BUTTON_PRESS
        } else {
            BUTTON_RELEASE
        };
        self.fake(kind, x_button, 0, 0)
    }

    fn scroll(&mut self, dx: f64, dy: f64) -> Result<(), String> {
        let v = self.wheel_v.take(dy / PIXELS_PER_NOTCH);
        let h = self.wheel_h.take(dx / PIXELS_PER_NOTCH);
        let (vertical, v_count) = if v >= 0 {
            (WHEEL_DOWN, v)
        } else {
            (WHEEL_UP, -v)
        };
        let (horizontal, h_count) = if h >= 0 {
            (WHEEL_RIGHT, h)
        } else {
            (WHEEL_LEFT, -h)
        };
        for _ in 0..v_count {
            self.click(vertical)?;
        }
        for _ in 0..h_count {
            self.click(horizontal)?;
        }
        Ok(())
    }

    fn click(&self, button: u8) -> Result<(), String> {
        self.fake(BUTTON_PRESS, button, 0, 0)?;
        self.fake(BUTTON_RELEASE, button, 0, 0)
    }
}

impl InputInjector for XTestInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<(), String> {
        match event.kind {
            InputEventKind::Key { keycode, pressed } => self.key(keycode, pressed),
            InputEventKind::Motion { dx, dy, x, y } => {
                if x.is_finite() && y.is_finite() {
                    self.absolute_motion(x, y)
                } else {
                    self.relative_motion(dx, dy)
                }
            }
            InputEventKind::Button { button, pressed } => self.button(button, pressed),
            InputEventKind::Scroll { dx, dy } => self.scroll(dx, dy),
        }
    }

    fn inject_text(&mut self, text: &str) -> Result<(), String> {
        for_each_tap(text, |code, shift| self.tap(code, shift))
    }
}

/// Maps a client-surface coordinate to a host root-window coordinate, clamped to the screen.
fn scale(v: f64, host: f64, client: f64) -> i16 {
    let max = (host - 1.0).clamp(0.0, f64::from(i16::MAX));
    (v * host / client).round().clamp(0.0, max) as i16
}

fn to_i16(v: i32) -> i16 {
    v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

/// X button numbers for evdev button codes. Buttons 8 and 9 are back and forward.
fn x_button(evdev: u32) -> Option<u8> {
    match evdev {
        0x110 => Some(1), // BTN_LEFT
        0x112 => Some(2), // BTN_MIDDLE
        0x111 => Some(3), // BTN_RIGHT
        0x113 => Some(8), // BTN_SIDE
        0x114 => Some(9), // BTN_EXTRA
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::protocol::xproto::ConnectionExt as _;

    #[test]
    fn scaling_maps_client_corners_onto_the_host_screen() {
        assert_eq!(scale(0.0, 3840.0, 1920.0), 0);
        assert_eq!(scale(1919.0, 3840.0, 1920.0), 3838);
        assert_eq!(scale(-50.0, 3840.0, 1920.0), 0);
        assert_eq!(scale(1.0e6, 3840.0, 1920.0), 3839);
    }

    #[test]
    fn buttons_map_to_x_numbers() {
        assert_eq!(x_button(0x110), Some(1));
        assert_eq!(x_button(0x111), Some(3));
        assert_eq!(x_button(0x1ff), None);
    }

    #[test]
    fn connect_skips_without_a_display() {
        match XTestInjector::connect(1920, 1080) {
            Ok(_) => eprintln!("note: X display and XTEST reachable; injection not exercised here"),
            Err(e) => eprintln!("skip: no XTEST display ({e})"),
        }
    }

    /// Moves the real pointer, so it runs only when `LUMEN_XTEST_MOVE_POINTER=1` is set. Use a
    /// throwaway display such as Xvfb for that.
    #[test]
    fn absolute_motion_reaches_the_pointer() {
        if std::env::var("LUMEN_XTEST_MOVE_POINTER").as_deref() != Ok("1") {
            eprintln!("skip: set LUMEN_XTEST_MOVE_POINTER=1 to move the pointer on the X display");
            return;
        }
        let mut injector = match XTestInjector::connect(1920, 1080) {
            Ok(injector) => injector,
            Err(e) => {
                eprintln!("skip: no XTEST display ({e})");
                return;
            }
        };
        injector
            .inject(&InputEvent {
                kind: InputEventKind::Motion {
                    dx: 0.0,
                    dy: 0.0,
                    x: 400.0,
                    y: 300.0,
                },
                received_us: 0,
            })
            .expect("absolute motion injects");

        // A second connection reads the pointer back. The server handles requests from each
        // client in arrival order, so retry briefly in case the motion is still in flight.
        let (conn, screen_num) = RustConnection::connect(None).expect("second X connection");
        let root = conn.setup().roots[screen_num].root;
        let mut last = (0, 0);
        for _ in 0..50 {
            let reply = conn
                .query_pointer(root)
                .expect("query_pointer")
                .reply()
                .expect("query_pointer reply");
            last = (reply.root_x, reply.root_y);
            if last == (400, 300) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(last, (400, 300), "pointer after absolute XTEST motion");
    }
}
