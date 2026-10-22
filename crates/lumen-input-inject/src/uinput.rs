//! uinput injection: the virtual devices, created through the `evdev` crate's safe uinput API.
//!
//! Absolute axes are set with `UI_ABS_SETUP`, so their range is exactly the client resolution. The
//! compositor then maps that range onto the host screen.
//!
//! The keyboard, the pointer and the absolute pointer are one device set ([`UinputInjector`]), and
//! the touch and the pen are another ([`UinputTouch`]): a host that injects its pointer through
//! the compositor still needs the touch and the pen in the kernel, because no compositor protocol
//! carries them.

use std::fmt;
use std::io::ErrorKind;

use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{
    AbsInfo, AbsoluteAxisCode, AbsoluteAxisEvent, AttributeSet, InputEvent as RawEvent, KeyCode,
    KeyEvent, PropType, RelativeAxisCode, RelativeAxisEvent, SynchronizationCode,
    SynchronizationEvent, UinputAbsSetup,
};
use lumen_input_wayland::{InputEvent, InputEventKind, PIXELS_PER_NOTCH};
use lumen_protocol::input::TouchPhase;

use crate::text::{KEY_LEFTSHIFT, for_each_tap};
use crate::{InputInjector, Residual, TouchInjector};

/// Highest evdev keycode the virtual keyboard accepts.
const KEY_MAX: u16 = 248;
/// `BTN_LEFT` through `BTN_TASK`: the mouse buttons.
const BUTTONS: std::ops::RangeInclusive<u16> = 0x110..=0x117;
/// Touch contacts supported by the multitouch slots.
const TOUCH_SLOTS: u8 = 10;
/// Largest pen pressure value.
const PEN_PRESSURE_MAX: u16 = 4095;

/// Injects through three virtual devices: keyboard, relative pointer and absolute pointer.
pub struct UinputInjector {
    keyboard: VirtualDevice,
    pointer: VirtualDevice,
    absolute: VirtualDevice,
    max_x: i32,
    max_y: i32,
    rel_x: Residual,
    rel_y: Residual,
    wheel_v: Residual,
    wheel_h: Residual,
}

impl fmt::Debug for UinputInjector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UinputInjector")
            .field("max_x", &self.max_x)
            .field("max_y", &self.max_y)
            .finish_non_exhaustive()
    }
}

impl UinputInjector {
    /// Creates the virtual devices. `width` and `height` are the client resolution, and the
    /// absolute axes span `0..width` and `0..height`.
    ///
    /// Fails with a message that names `/dev/uinput` when the device cannot be opened. That needs
    /// root or membership in the group that owns the node.
    pub fn new(width: u32, height: u32) -> Result<Self, String> {
        let max_x = i32::try_from(width.max(1) - 1).map_err(|e| format!("width: {e}"))?;
        let max_y = i32::try_from(height.max(1) - 1).map_err(|e| format!("height: {e}"))?;
        Ok(Self {
            keyboard: keyboard_device()?,
            pointer: pointer_device()?,
            absolute: absolute_device(max_x, max_y)?,
            max_x,
            max_y,
            rel_x: Residual::default(),
            rel_y: Residual::default(),
            wheel_v: Residual::default(),
            wheel_h: Residual::default(),
        })
    }

    fn key(&mut self, keycode: u32, pressed: bool) -> Result<(), String> {
        let code = u16::try_from(keycode).map_err(|_| format!("keycode {keycode} out of range"))?;
        emit(
            &mut self.keyboard,
            &[key(KeyCode::new(code), i32::from(pressed)), sync()],
        )
    }

    fn tap(&mut self, keycode: u32, shift: bool) -> Result<(), String> {
        if shift {
            self.key(KEY_LEFTSHIFT, true)?;
        }
        self.key(keycode, true)?;
        self.key(keycode, false)?;
        if shift {
            self.key(KEY_LEFTSHIFT, false)?;
        }
        Ok(())
    }

    fn absolute_motion(&mut self, x: f64, y: f64) -> Result<(), String> {
        let (px, py) = (clamp_round(x, self.max_x), clamp_round(y, self.max_y));
        emit(
            &mut self.absolute,
            &[
                abs(AbsoluteAxisCode::ABS_X, px),
                abs(AbsoluteAxisCode::ABS_Y, py),
                sync(),
            ],
        )
    }

    fn relative_motion(&mut self, dx: f64, dy: f64) -> Result<(), String> {
        let (rx, ry) = (self.rel_x.take(dx), self.rel_y.take(dy));
        emit(
            &mut self.pointer,
            &[
                rel(RelativeAxisCode::REL_X, rx),
                rel(RelativeAxisCode::REL_Y, ry),
                sync(),
            ],
        )
    }

    fn button(&mut self, button: u32, pressed: bool) -> Result<(), String> {
        let code = u16::try_from(button).map_err(|_| format!("button {button} out of range"))?;
        emit(
            &mut self.pointer,
            &[key(KeyCode::new(code), i32::from(pressed)), sync()],
        )
    }

    fn scroll(&mut self, dx: f64, dy: f64) -> Result<(), String> {
        // Positive `dy` scrolls down, which is a negative REL_WHEEL. Positive `dx` scrolls right,
        // which is a positive REL_HWHEEL.
        let v = self.wheel_v.take(dy / PIXELS_PER_NOTCH);
        let h = self.wheel_h.take(dx / PIXELS_PER_NOTCH);
        emit(
            &mut self.pointer,
            &[
                rel(RelativeAxisCode::REL_WHEEL, -v),
                rel(RelativeAxisCode::REL_HWHEEL, h),
                sync(),
            ],
        )
    }
}

impl InputInjector for UinputInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<(), String> {
        match event.kind {
            InputEventKind::Key { keycode, pressed } => self.key(keycode, pressed),
            // Absolute events carry finite coordinates. Relative events carry NaN there.
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

/// The touch and the pen of one session, as two virtual devices.
pub struct UinputTouch {
    touch: VirtualDevice,
    pen: VirtualDevice,
    max_x: i32,
    max_y: i32,
    /// Bit `n` is set while touch slot `n` is down.
    touch_mask: u16,
}

impl fmt::Debug for UinputTouch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UinputTouch")
            .field("max_x", &self.max_x)
            .field("max_y", &self.max_y)
            .finish_non_exhaustive()
    }
}

impl UinputTouch {
    /// Creates the touch and the pen. `width` and `height` are the client resolution, and the
    /// absolute axes span `0..width` and `0..height`.
    ///
    /// Fails with a message that names `/dev/uinput` when the device cannot be opened. That needs
    /// root or membership in the group that owns the node.
    pub fn new(width: u32, height: u32) -> Result<Self, String> {
        let max_x = i32::try_from(width.max(1) - 1).map_err(|e| format!("width: {e}"))?;
        let max_y = i32::try_from(height.max(1) - 1).map_err(|e| format!("height: {e}"))?;
        Ok(Self {
            touch: touch_device(max_x, max_y)?,
            pen: pen_device(max_x, max_y)?,
            max_x,
            max_y,
            touch_mask: 0,
        })
    }
}

impl TouchInjector for UinputTouch {
    fn touch(&mut self, slot: u8, phase: TouchPhase, x: f64, y: f64) -> Result<(), String> {
        if slot >= TOUCH_SLOTS {
            return Err(format!("touch slot {slot} out of range 0..{TOUCH_SLOTS}"));
        }
        let (px, py) = (clamp_round(x, self.max_x), clamp_round(y, self.max_y));
        let bit = 1u16 << slot;
        let id = i32::from(slot) + 1;
        let slot = i32::from(slot);
        match phase {
            TouchPhase::Down => {
                self.touch_mask |= bit;
                emit(
                    &mut self.touch,
                    &[
                        abs(AbsoluteAxisCode::ABS_MT_SLOT, slot),
                        abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, id),
                        abs(AbsoluteAxisCode::ABS_MT_POSITION_X, px),
                        abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, py),
                        key(KeyCode::BTN_TOUCH, 1),
                        sync(),
                    ],
                )
            }
            TouchPhase::Motion => emit(
                &mut self.touch,
                &[
                    abs(AbsoluteAxisCode::ABS_MT_SLOT, slot),
                    abs(AbsoluteAxisCode::ABS_MT_POSITION_X, px),
                    abs(AbsoluteAxisCode::ABS_MT_POSITION_Y, py),
                    sync(),
                ],
            ),
            TouchPhase::Up => {
                self.touch_mask &= !bit;
                emit(
                    &mut self.touch,
                    &[
                        abs(AbsoluteAxisCode::ABS_MT_SLOT, slot),
                        abs(AbsoluteAxisCode::ABS_MT_TRACKING_ID, -1),
                        key(KeyCode::BTN_TOUCH, i32::from(self.touch_mask != 0)),
                        sync(),
                    ],
                )
            }
        }
    }

    fn pen(&mut self, x: f64, y: f64, pressure: u16, in_range: bool) -> Result<(), String> {
        let pressure = pressure.min(PEN_PRESSURE_MAX);
        emit(
            &mut self.pen,
            &[
                abs(AbsoluteAxisCode::ABS_X, clamp_round(x, self.max_x)),
                abs(AbsoluteAxisCode::ABS_Y, clamp_round(y, self.max_y)),
                abs(AbsoluteAxisCode::ABS_PRESSURE, i32::from(pressure)),
                key(KeyCode::BTN_TOOL_PEN, i32::from(in_range)),
                key(KeyCode::BTN_TOUCH, i32::from(pressure > 0)),
                sync(),
            ],
        )
    }
}

/// Sends `events` as one frame, which means one SYN_REPORT at the end. `events` must not contain a
/// SYN_REPORT already, except as a trailing duplicate.
fn emit(dev: &mut VirtualDevice, events: &[RawEvent]) -> Result<(), String> {
    dev.emit(events).map_err(|e| format!("uinput write: {e}"))
}

fn key(code: KeyCode, value: i32) -> RawEvent {
    KeyEvent::new(code, value).into()
}

fn rel(code: RelativeAxisCode, value: i32) -> RawEvent {
    RelativeAxisEvent::new(code, value).into()
}

fn abs(code: AbsoluteAxisCode, value: i32) -> RawEvent {
    AbsoluteAxisEvent::new(code, value).into()
}

fn sync() -> RawEvent {
    SynchronizationEvent::new(SynchronizationCode::SYN_REPORT, 0).into()
}

fn clamp_round(v: f64, max: i32) -> i32 {
    v.round().clamp(0.0, f64::from(max)) as i32
}

fn uinput_err(what: &str, e: &std::io::Error) -> String {
    if e.kind() == ErrorKind::PermissionDenied {
        format!(
            "{what}: cannot open /dev/uinput ({e}); run as root or add the user to the group that owns it"
        )
    } else {
        format!("{what}: {e}")
    }
}

fn buttons() -> AttributeSet<KeyCode> {
    BUTTONS.map(KeyCode::new).collect()
}

fn keyboard_device() -> Result<VirtualDevice, String> {
    let keys: AttributeSet<KeyCode> = (1..=KEY_MAX).map(KeyCode::new).collect();
    VirtualDevice::builder()
        .and_then(|b| b.name("Lumen virtual keyboard").with_keys(&keys))
        .and_then(VirtualDeviceBuilder::build)
        .map_err(|e| uinput_err("virtual keyboard", &e))
}

fn pointer_device() -> Result<VirtualDevice, String> {
    let axes: AttributeSet<RelativeAxisCode> = [
        RelativeAxisCode::REL_X,
        RelativeAxisCode::REL_Y,
        RelativeAxisCode::REL_WHEEL,
        RelativeAxisCode::REL_HWHEEL,
    ]
    .into_iter()
    .collect();
    VirtualDevice::builder()
        .and_then(|b| b.name("Lumen virtual pointer").with_keys(&buttons()))
        .and_then(|b| b.with_relative_axes(&axes))
        .and_then(VirtualDeviceBuilder::build)
        .map_err(|e| uinput_err("virtual pointer", &e))
}

fn absolute_device(max_x: i32, max_y: i32) -> Result<VirtualDevice, String> {
    VirtualDevice::builder()
        .map(|b| b.name("Lumen virtual absolute pointer"))
        .and_then(|b| b.with_keys(&buttons()))
        .and_then(|b| b.with_absolute_axis(&axis(AbsoluteAxisCode::ABS_X, 0, max_x)))
        .and_then(|b| b.with_absolute_axis(&axis(AbsoluteAxisCode::ABS_Y, 0, max_y)))
        .and_then(VirtualDeviceBuilder::build)
        .map_err(|e| uinput_err("virtual absolute pointer", &e))
}

fn touch_device(max_x: i32, max_y: i32) -> Result<VirtualDevice, String> {
    let keys: AttributeSet<KeyCode> = [KeyCode::BTN_TOUCH, KeyCode::BTN_TOOL_FINGER]
        .into_iter()
        .collect();
    let props: AttributeSet<PropType> = [PropType::DIRECT].into_iter().collect();
    VirtualDevice::builder()
        .and_then(|b| b.name("Lumen virtual touch").with_keys(&keys))
        .and_then(|b| b.with_properties(&props))
        .and_then(|b| {
            b.with_absolute_axis(&axis(
                AbsoluteAxisCode::ABS_MT_SLOT,
                0,
                i32::from(TOUCH_SLOTS) - 1,
            ))
        })
        .and_then(|b| b.with_absolute_axis(&axis(AbsoluteAxisCode::ABS_MT_TRACKING_ID, 0, 65_535)))
        .and_then(|b| b.with_absolute_axis(&axis(AbsoluteAxisCode::ABS_MT_POSITION_X, 0, max_x)))
        .and_then(|b| b.with_absolute_axis(&axis(AbsoluteAxisCode::ABS_MT_POSITION_Y, 0, max_y)))
        .and_then(VirtualDeviceBuilder::build)
        .map_err(|e| uinput_err("virtual touch", &e))
}

fn pen_device(max_x: i32, max_y: i32) -> Result<VirtualDevice, String> {
    let keys: AttributeSet<KeyCode> = [
        KeyCode::BTN_TOOL_PEN,
        KeyCode::BTN_TOUCH,
        KeyCode::BTN_STYLUS,
    ]
    .into_iter()
    .collect();
    VirtualDevice::builder()
        .and_then(|b| b.name("Lumen virtual pen").with_keys(&keys))
        .and_then(|b| b.with_absolute_axis(&axis(AbsoluteAxisCode::ABS_X, 0, max_x)))
        .and_then(|b| b.with_absolute_axis(&axis(AbsoluteAxisCode::ABS_Y, 0, max_y)))
        .and_then(|b| {
            b.with_absolute_axis(&axis(
                AbsoluteAxisCode::ABS_PRESSURE,
                0,
                i32::from(PEN_PRESSURE_MAX),
            ))
        })
        .and_then(VirtualDeviceBuilder::build)
        .map_err(|e| uinput_err("virtual pen", &e))
}

/// An absolute axis spanning `min..=max`, with zero fuzz, flat, and resolution.
fn axis(code: AbsoluteAxisCode, min: i32, max: i32) -> UinputAbsSetup {
    UinputAbsSetup::new(code, AbsInfo::new(min, min, max, 0, 0, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_values_clamp_into_the_client_range() {
        assert_eq!(clamp_round(-5.0, 100), 0);
        assert_eq!(clamp_round(1.0e9, 100), 100);
        assert_eq!(clamp_round(10.4, 100), 10);
        assert_eq!(clamp_round(10.6, 100), 11);
    }

    #[test]
    fn creating_devices_needs_uinput_access() {
        match UinputInjector::new(1920, 1080) {
            Ok(mut injector) => {
                // Only zero motion is sent, so no visible input happens.
                injector
                    .inject(&InputEvent {
                        kind: InputEventKind::Motion {
                            dx: 0.0,
                            dy: 0.0,
                            x: f64::NAN,
                            y: f64::NAN,
                        },
                        received_us: 0,
                    })
                    .expect("zero relative motion writes");
            }
            Err(e) => eprintln!("skip: uinput devices unavailable: {e}"),
        }
    }
}
