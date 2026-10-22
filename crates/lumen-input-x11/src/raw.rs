//! XI2 raw event capture.
//!
//! The X server delivers raw events straight from the device, before it
//! applies pointer acceleration or any other processing. That is what a
//! remote-desktop client wants: the movement the user made, not the movement
//! the server decided on.
//!
//! Three things have to be right for the raw events to arrive:
//!
//! 1. The XInput 2 extension has to be negotiated with `XIQueryVersion`
//!    before any XI2 request is allowed.
//! 2. `XISelectEvents` takes one 32-bit mask per device, with every event
//!    type in one word. A list of masks is refused with `BadValue`.
//! 3. The events are selected for the master devices, so a key press is
//!    delivered once and not once per device in the chain.
//!
//! A raw motion event of a relative device carries the movement itself, so
//! the values of the event are the delta. The pointer position on the client
//! surface is tracked from those deltas, because the protocol carries the
//! position of the pointer in client-surface pixels.

use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use lumen_protocol::input::InputAction;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use x11rb::connection::Connection;
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

use crate::keyboard::{Keymap, KeymapError, evdev_code_from_x11};

/// Logical pixels that one wheel notch stands for.
pub const PIXELS_PER_NOTCH: f64 = 15.0;

/// The XI2 device id that selects every master device.
pub const MASTER_DEVICES: u16 = 1;

/// The evdev code of the left mouse button.
pub const BTN_LEFT: u32 = 0x110;
/// The evdev code of the right mouse button.
pub const BTN_RIGHT: u32 = 0x111;
/// The evdev code of the middle mouse button.
pub const BTN_MIDDLE: u32 = 0x112;

/// Why the raw event source could not be opened or read.
#[derive(Debug, thiserror::Error)]
pub enum RawError {
    /// The X display could not be opened.
    #[error("cannot open the X display: {0}")]
    Connect(String),
    /// The X server reported no usable screen.
    #[error("the X server reported no screen: {0}")]
    NoScreen(&'static str),
    /// The raw events could not be selected.
    #[error("cannot select the raw events: {0}")]
    Select(String),
    /// The keyboard mapping could not be read.
    #[error("cannot read the keyboard mapping: {0}")]
    Keymap(#[from] KeymapError),
    /// The X connection failed.
    #[error("the X connection failed: {0}")]
    Connection(String),
    /// Waiting for the X socket failed.
    #[error("waiting for the X socket failed: {0}")]
    Poll(String),
}

/// How the source behaves.
#[derive(Debug, Clone, PartialEq)]
pub struct RawConfig {
    /// The display to open. `None` means the `DISPLAY` of the environment.
    pub display: Option<String>,
    /// The width of the client surface in logical pixels. Zero means the width
    /// of the X screen.
    pub surface_width: f64,
    /// The height of the client surface in logical pixels. Zero means the
    /// height of the X screen.
    pub surface_height: f64,
    /// Whether the pointer is reported as an absolute position on the client
    /// surface. When it is false the pointer is reported as a relative
    /// movement.
    pub absolute_motion: bool,
    /// Whether a key that the keymap of the display does not define is
    /// dropped. The X server can report a keycode the layout has no symbol
    /// for, and such a key cannot be typed.
    pub drop_unmapped_keys: bool,
}

impl Default for RawConfig {
    fn default() -> Self {
        Self {
            display: None,
            surface_width: 0.0,
            surface_height: 0.0,
            absolute_motion: true,
            drop_unmapped_keys: true,
        }
    }
}

/// The pointer axes of a device.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AxisLayout {
    /// The valuator number of the X axis.
    pub x: usize,
    /// The valuator number of the Y axis.
    pub y: usize,
    /// Whether the device reports absolute positions.
    pub absolute: bool,
    /// The lowest value of an absolute axis.
    pub min: f64,
    /// The highest value of an absolute axis.
    pub max: f64,
}

impl Default for AxisLayout {
    fn default() -> Self {
        Self {
            x: 0,
            y: 1,
            absolute: false,
            min: 0.0,
            max: 0.0,
        }
    }
}

impl AxisLayout {
    /// Reads the pointer axes of the master pointer of a display.
    ///
    /// The axis of a valuator is named by the label of the device, which the
    /// X server gives as an atom. The labels of a pointer are `Rel X` and
    /// `Rel Y` for a relative device and `Abs X` and `Abs Y` for an absolute
    /// one.
    ///
    /// # Errors
    /// Returns [`RawError::Connection`] when the server refuses the query.
    pub fn for_device(conn: &impl Connection) -> Result<Self, RawError> {
        let reply = conn
            .xinput_xi_query_device(0u16)
            .map_err(|e| RawError::Connection(e.to_string()))?
            .reply()
            .map_err(|e| RawError::Connection(e.to_string()))?;
        for info in &reply.infos {
            if info.type_ != xinput::DeviceType::MASTER_POINTER {
                continue;
            }
            let mut layout = AxisLayout::default();
            for class in &info.classes {
                let xinput::DeviceClassData::Valuator(valuator) = &class.data else {
                    continue;
                };
                let label = atom_name(conn, valuator.label);
                let number = usize::from(valuator.number);
                // The label of a pointer axis ends in the letter of the axis.
                // The server writes it in upper case, as in `Rel X`.
                match axis_of_label(&label) {
                    Some('x') => layout.x = number,
                    Some('y') => layout.y = number,
                    _ => {}
                }
                if valuator.mode == xinput::ValuatorMode::ABSOLUTE {
                    layout.absolute = true;
                }
                layout.min = fp3232_to_f64(valuator.min);
                layout.max = fp3232_to_f64(valuator.max);
            }
            return Ok(layout);
        }
        Ok(AxisLayout::default())
    }
}

/// What one raw motion event says about the pointer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointerMotion {
    /// The movement in device units.
    pub dx: f64,
    /// The movement in device units.
    pub dy: f64,
    /// The position on the client surface, when the device reports one.
    pub position: Option<(f64, f64)>,
}

/// One raw X11 input event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RawEvent {
    /// The pointer moved.
    Motion(PointerMotion),
    /// A key was pressed or released. `keycode` is an evdev code.
    Key { keycode: u32, pressed: bool },
    /// A button was pressed or released. `button` is an evdev code.
    Button { button: u32, pressed: bool },
    /// The wheel turned. The movement is in logical pixels.
    Scroll { dx: f64, dy: f64 },
}

/// Reads the pointer movement of a raw motion event.
///
/// The mask says which valuators the event carries. The values follow in the
/// order of the valuator numbers, so the first value belongs to the lowest
/// valuator that is present.
///
/// A relative device reports the movement itself, so its values are the
/// delta. An absolute device reports the position, which is scaled onto the
/// client surface.
///
/// An axis the event does not carry leaves that movement at zero, so a caller
/// does not have to handle a missing axis.
#[must_use]
pub fn pointer_motion(
    layout: &AxisLayout,
    mask: &[u32],
    values: &[f64],
    surface: (f64, f64),
) -> PointerMotion {
    let present = present_valuators(mask);
    let x = value_of(&present, values, layout.x);
    let y = value_of(&present, values, layout.y);
    if layout.absolute {
        let position = match (x, y) {
            (Some(x), Some(y)) => Some((
                scale(x, layout.min, layout.max, surface.0),
                scale(y, layout.min, layout.max, surface.1),
            )),
            _ => None,
        };
        return PointerMotion {
            dx: 0.0,
            dy: 0.0,
            position,
        };
    }
    PointerMotion {
        dx: x.unwrap_or(0.0),
        dy: y.unwrap_or(0.0),
        position: None,
    }
}

/// The valuator numbers a mask carries.
fn present_valuators(mask: &[u32]) -> Vec<usize> {
    let Some(first) = mask.first() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for bit in 0..32u32 {
        if first & (1u32 << bit) != 0 {
            out.push(usize::try_from(bit).unwrap_or(0));
        }
    }
    out
}

/// The value of one valuator, when the event carries it.
fn value_of(present: &[usize], values: &[f64], number: usize) -> Option<f64> {
    let position = present.iter().position(|p| *p == number)?;
    values.get(position).copied()
}

/// Scales a value of an absolute axis onto the client surface.
fn scale(value: f64, min: f64, max: f64, extent: f64) -> f64 {
    if extent <= 1.0 || max <= min {
        return 0.0;
    }
    ((value - min) / (max - min) * (extent - 1.0)).clamp(0.0, extent - 1.0)
}

/// Tracks the pointer position on the client surface.
///
/// The X server reports the movement of a relative device, so the position has
/// to be tracked here. The pointer starts in the middle of the surface.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointerTracker {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl PointerTracker {
    /// A tracker for a surface of the given size.
    #[must_use]
    pub fn new(width: f64, height: f64) -> Self {
        Self {
            x: ((width - 1.0) / 2.0).max(0.0),
            y: ((height - 1.0) / 2.0).max(0.0),
            width,
            height,
        }
    }

    /// The position of the pointer on the surface.
    #[must_use]
    pub fn position(&self) -> (f64, f64) {
        (self.x, self.y)
    }

    /// Applies a movement and returns the new position.
    pub fn apply(&mut self, motion: &PointerMotion) -> (f64, f64) {
        if let Some((x, y)) = motion.position {
            self.x = x;
            self.y = y;
        } else {
            self.x = clamp(self.x + motion.dx, self.width);
            self.y = clamp(self.y + motion.dy, self.height);
        }
        (self.x, self.y)
    }
}

/// Clamps a coordinate onto a surface of the given size.
fn clamp(value: f64, extent: f64) -> f64 {
    let max = (extent - 1.0).max(0.0);
    value.clamp(0.0, max)
}

/// A source of raw X11 input events.
///
/// The source owns one X connection and nothing else. It never blocks longer
/// than the timeout the caller gives it, so a caller can decide how long it
/// is willing to wait for the next event.
#[derive(Debug)]
pub struct RawEventSource {
    conn: RustConnection,
    keymap: Keymap,
    layout: AxisLayout,
    tracker: PointerTracker,
    width: f64,
    height: f64,
    absolute_motion: bool,
    drop_unmapped_keys: bool,
}

impl RawEventSource {
    /// Opens the display and selects the raw events.
    ///
    /// # Errors
    /// Returns [`RawError::Connect`] when the display cannot be opened,
    /// [`RawError::Select`] when the raw events cannot be selected, and
    /// [`RawError::Keymap`] when the keyboard mapping cannot be read.
    pub fn open(config: &RawConfig) -> Result<Self, RawError> {
        let (conn, screen_num) = RustConnection::connect(config.display.as_deref())
            .map_err(|e| RawError::Connect(e.to_string()))?;
        // The XInput 2 extension has to be negotiated before any XI2 request.
        conn.xinput_xi_query_version(2, 0)
            .map_err(|e| RawError::Connect(e.to_string()))?
            .reply()
            .map_err(|e| RawError::Connect(e.to_string()))?;
        let screen = conn
            .setup()
            .roots
            .get(screen_num)
            .ok_or(RawError::NoScreen("the default screen is missing"))?;
        let root = screen.root;
        let width = if config.surface_width > 0.0 {
            config.surface_width
        } else {
            f64::from(screen.width_in_pixels)
        };
        let height = if config.surface_height > 0.0 {
            config.surface_height
        } else {
            f64::from(screen.height_in_pixels)
        };

        // One mask per device, with every event type in one word.
        let masks = [xinput::EventMask {
            deviceid: MASTER_DEVICES,
            mask: vec![event_mask()],
        }];
        conn.xinput_xi_select_events(root, &masks)
            .map_err(|e| RawError::Select(e.to_string()))?
            .check()
            .map_err(|e| RawError::Select(e.to_string()))?;

        let keymap = Keymap::load(&conn)?;
        let layout = AxisLayout::for_device(&conn)?;
        Ok(Self {
            conn,
            keymap,
            layout,
            tracker: PointerTracker::new(width, height),
            width,
            height,
            absolute_motion: config.absolute_motion,
            drop_unmapped_keys: config.drop_unmapped_keys,
        })
    }

    /// The keyboard mapping of the display.
    #[must_use]
    pub fn keymap(&self) -> &Keymap {
        &self.keymap
    }

    /// The pointer axes of the master pointer.
    #[must_use]
    pub fn axis_layout(&self) -> AxisLayout {
        self.layout
    }

    /// The size of the client surface.
    #[must_use]
    pub fn surface_size(&self) -> (f64, f64) {
        (self.width, self.height)
    }

    /// Waits up to `timeout` for the next raw event.
    ///
    /// `None` means the timeout passed without an event. The wait is on the
    /// socket of the X connection, so the caller keeps control of the latency.
    ///
    /// # Errors
    /// Returns [`RawError::Poll`] when waiting for the socket fails and
    /// [`RawError::Connection`] when the X connection breaks.
    pub fn poll_raw(&mut self, timeout: Duration) -> Result<Option<RawEvent>, RawError> {
        let deadline = Instant::now() + timeout;
        loop {
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
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(RawError::Poll(e.to_string())),
            }
            let event = self
                .conn
                .poll_for_event()
                .map_err(|e| RawError::Connection(e.to_string()))?;
            let Some(event) = event else {
                continue;
            };
            if let Some(raw) = self.translate(&event) {
                return Ok(Some(raw));
            }
        }
    }

    /// Waits up to `timeout` for the next event, as a protocol action.
    ///
    /// The pointer is reported as an absolute position on the client surface
    /// when the source was opened for it, and as a relative movement
    /// otherwise.
    ///
    /// # Errors
    /// Returns [`RawError::Poll`] when waiting for the socket fails and
    /// [`RawError::Connection`] when the X connection breaks.
    pub fn poll_action(&mut self, timeout: Duration) -> Result<Option<InputAction>, RawError> {
        let Some(raw) = self.poll_raw(timeout)? else {
            return Ok(None);
        };
        Ok(Some(self.action_for(raw)))
    }

    /// Turns a raw event into the action the protocol carries.
    fn action_for(&mut self, raw: RawEvent) -> InputAction {
        match raw {
            RawEvent::Motion(motion) => {
                if self.absolute_motion {
                    let (x, y) = self.tracker.apply(&motion);
                    InputAction::MotionAbsolute { x, y }
                } else {
                    InputAction::MotionRelative {
                        dx: motion.dx,
                        dy: motion.dy,
                    }
                }
            }
            RawEvent::Key { keycode, pressed } => InputAction::Key { keycode, pressed },
            RawEvent::Button { button, pressed } => InputAction::Button { button, pressed },
            RawEvent::Scroll { dx, dy } => InputAction::Scroll { dx, dy },
        }
    }

    /// Translates one X event into a raw event.
    fn translate(&self, event: &x11rb::protocol::Event) -> Option<RawEvent> {
        match event {
            x11rb::protocol::Event::XinputRawMotion(ev) => {
                let values: Vec<f64> = ev.axisvalues.iter().copied().map(fp3232_to_f64).collect();
                let motion = pointer_motion(
                    &self.layout,
                    &ev.valuator_mask,
                    &values,
                    (self.width, self.height),
                );
                Some(RawEvent::Motion(motion))
            }
            x11rb::protocol::Event::XinputRawKeyPress(ev) => self.key(ev.detail, true),
            x11rb::protocol::Event::XinputRawKeyRelease(ev) => self.key(ev.detail, false),
            x11rb::protocol::Event::XinputRawButtonPress(ev) => Self::button(ev.detail, true),
            x11rb::protocol::Event::XinputRawButtonRelease(ev) => Self::button(ev.detail, false),
            _ => None,
        }
    }

    /// A key press or release. `detail` is the X keycode.
    fn key(&self, detail: u32, pressed: bool) -> Option<RawEvent> {
        let x11_keycode = u8::try_from(detail).ok()?;
        let keycode = evdev_code_from_x11(x11_keycode)?;
        if self.drop_unmapped_keys && !self.keymap.defines(x11_keycode) {
            return None;
        }
        Some(RawEvent::Key { keycode, pressed })
    }

    /// A button press or release. `detail` is the X button number.
    ///
    /// Buttons four to seven are the wheel of a legacy mouse, and they are
    /// reported as a scroll instead.
    fn button(detail: u32, pressed: bool) -> Option<RawEvent> {
        let x11_button = u8::try_from(detail).ok()?;
        if let Some((dx, dy)) = wheel_scroll(x11_button) {
            if !pressed {
                return None;
            }
            return Some(RawEvent::Scroll { dx, dy });
        }
        let button = evdev_button(x11_button)?;
        Some(RawEvent::Button { button, pressed })
    }
}

/// The axis a valuator label names, if any.
///
/// The server writes the label of a pointer axis in upper case, as in `Rel X`
/// for the X axis of a relative device and `Abs Y` for the Y axis of an
/// absolute one. A label that does not name an axis, such as `Wheel`, names
/// none.
fn axis_of_label(label: &str) -> Option<char> {
    label
        .chars()
        .last()
        .map(|letter| letter.to_ascii_lowercase())
        .filter(|letter| matches!(letter, 'x' | 'y'))
}

/// The mask of every raw event the source wants.
fn event_mask() -> xinput::XIEventMask {
    xinput::XIEventMask::from(
        u32::from(xinput::XIEventMask::RAW_MOTION)
            | u32::from(xinput::XIEventMask::RAW_KEY_PRESS)
            | u32::from(xinput::XIEventMask::RAW_KEY_RELEASE)
            | u32::from(xinput::XIEventMask::RAW_BUTTON_PRESS)
            | u32::from(xinput::XIEventMask::RAW_BUTTON_RELEASE),
    )
}

/// The evdev code of an X11 button number.
fn evdev_button(x11_button: u8) -> Option<u32> {
    match x11_button {
        1 => Some(BTN_LEFT),
        2 => Some(BTN_MIDDLE),
        3 => Some(BTN_RIGHT),
        _ => None,
    }
}

/// The scroll movement of an X11 wheel button.
fn wheel_scroll(x11_button: u8) -> Option<(f64, f64)> {
    match x11_button {
        4 => Some((0.0, -PIXELS_PER_NOTCH)),
        5 => Some((0.0, PIXELS_PER_NOTCH)),
        6 => Some((-PIXELS_PER_NOTCH, 0.0)),
        7 => Some((PIXELS_PER_NOTCH, 0.0)),
        _ => None,
    }
}

/// The name of an atom.
fn atom_name(conn: &impl Connection, atom: xproto::Atom) -> String {
    conn.get_atom_name(atom)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|reply| String::from_utf8_lossy(&reply.name).into_owned())
        .unwrap_or_default()
}

/// Converts a 32.32 fixed point number into a float.
fn fp3232_to_f64(value: xinput::Fp3232) -> f64 {
    f64::from(value.integral) + f64::from(value.frac) / 4_294_967_296.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mask of a motion event that carries the two axes of a pointer.
    const POINTER_MASK: [u32; 2] = [0x3, 0x0];

    fn relative_layout() -> AxisLayout {
        AxisLayout {
            x: 0,
            y: 1,
            absolute: false,
            min: -1.0,
            max: -1.0,
        }
    }

    #[test]
    fn the_mask_says_which_valuators_are_present() {
        assert_eq!(present_valuators(&POINTER_MASK), vec![0, 1]);
        assert_eq!(present_valuators(&[0b101]), vec![0, 2]);
        assert_eq!(present_valuators(&[0]), Vec::<usize>::new());
        assert_eq!(present_valuators(&[]), Vec::<usize>::new());
    }

    #[test]
    fn a_relative_device_reports_the_movement_itself() {
        let motion = pointer_motion(
            &relative_layout(),
            &POINTER_MASK,
            &[7.0, 3.0],
            (1920.0, 1080.0),
        );
        assert_eq!(motion.dx, 7.0);
        assert_eq!(motion.dy, 3.0);
        assert_eq!(motion.position, None);
    }

    #[test]
    fn a_negative_movement_keeps_its_sign() {
        let motion = pointer_motion(
            &relative_layout(),
            &POINTER_MASK,
            &[-4.0, 9.0],
            (1920.0, 1080.0),
        );
        assert_eq!(motion.dx, -4.0);
        assert_eq!(motion.dy, 9.0);
    }

    #[test]
    fn a_missing_axis_leaves_the_movement_at_zero() {
        let motion = pointer_motion(&relative_layout(), &[0b1], &[5.0], (1920.0, 1080.0));
        assert_eq!(motion.dx, 5.0);
        assert_eq!(motion.dy, 0.0);
    }

    #[test]
    fn an_absolute_device_reports_the_position_on_the_surface() {
        let layout = AxisLayout {
            x: 0,
            y: 1,
            absolute: true,
            min: 0.0,
            max: 1919.0,
        };
        let motion = pointer_motion(&layout, &POINTER_MASK, &[1919.0, 0.0], (1920.0, 1080.0));
        assert_eq!(motion.dx, 0.0);
        assert_eq!(motion.dy, 0.0);
        assert_eq!(motion.position, Some((1919.0, 0.0)));
    }

    #[test]
    fn the_pointer_stays_on_the_surface() {
        let mut tracker = PointerTracker::new(1920.0, 1080.0);
        assert_eq!(tracker.position(), (959.5, 539.5));
        let motion = PointerMotion {
            dx: 5000.0,
            dy: 5000.0,
            position: None,
        };
        assert_eq!(tracker.apply(&motion), (1919.0, 1079.0));
        let back = PointerMotion {
            dx: -5000.0,
            dy: -5000.0,
            position: None,
        };
        assert_eq!(tracker.apply(&back), (0.0, 0.0));
    }

    #[test]
    fn an_absolute_position_overrides_the_tracked_one() {
        let mut tracker = PointerTracker::new(1920.0, 1080.0);
        let motion = PointerMotion {
            dx: 0.0,
            dy: 0.0,
            position: Some((100.0, 200.0)),
        };
        assert_eq!(tracker.apply(&motion), (100.0, 200.0));
    }

    #[test]
    fn the_pointer_buttons_have_evdev_codes() {
        assert_eq!(evdev_button(1), Some(BTN_LEFT));
        assert_eq!(evdev_button(2), Some(BTN_MIDDLE));
        assert_eq!(evdev_button(3), Some(BTN_RIGHT));
        assert_eq!(evdev_button(7), None);
    }

    #[test]
    fn the_wheel_is_a_scroll_and_not_a_button() {
        assert_eq!(wheel_scroll(4), Some((0.0, -PIXELS_PER_NOTCH)));
        assert_eq!(wheel_scroll(5), Some((0.0, PIXELS_PER_NOTCH)));
        assert_eq!(wheel_scroll(6), Some((-PIXELS_PER_NOTCH, 0.0)));
        assert_eq!(wheel_scroll(7), Some((PIXELS_PER_NOTCH, 0.0)));
        assert_eq!(wheel_scroll(1), None);
    }

    #[test]
    fn a_fixed_point_number_becomes_a_float() {
        let value = xinput::Fp3232 {
            integral: -2,
            frac: 0x8000_0000,
        };
        assert!((fp3232_to_f64(value) - (-1.5)).abs() < 1e-9);
    }

    #[test]
    fn the_label_of_a_valuator_names_its_axis() {
        assert_eq!(axis_of_label("Rel X"), Some('x'));
        assert_eq!(axis_of_label("Rel Y"), Some('y'));
        assert_eq!(axis_of_label("Abs X"), Some('x'));
        assert_eq!(axis_of_label("Abs Y"), Some('y'));
        assert_eq!(axis_of_label("X"), Some('x'));
        assert_eq!(axis_of_label("Wheel"), None);
        assert_eq!(axis_of_label(""), None);
    }

    #[test]
    fn the_event_mask_carries_every_raw_event() {
        let mask = event_mask();
        let bits = u32::from(mask);
        for event in [
            xinput::XIEventMask::RAW_MOTION,
            xinput::XIEventMask::RAW_KEY_PRESS,
            xinput::XIEventMask::RAW_KEY_RELEASE,
            xinput::XIEventMask::RAW_BUTTON_PRESS,
            xinput::XIEventMask::RAW_BUTTON_RELEASE,
        ] {
            assert_ne!(bits & u32::from(event), 0, "the mask is missing an event");
        }
    }
}
