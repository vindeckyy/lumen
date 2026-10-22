//! Decoding of libei events into owned Rust values.

use std::ffi::c_void;
use std::ptr::{self, NonNull};

use crate::device::{Device, DeviceId};
use crate::sys::{self, EiEvent};
use crate::{Capabilities, Context};

/// An event from the EIS server or from one of its devices.
///
/// libei adds event types over time, so match with a wildcard arm.
#[derive(Debug)]
#[non_exhaustive]
pub enum Event {
    /// The server accepted this connection.
    Connected,
    /// The connection ended. No further events follow.
    Disconnected,
    /// A seat appeared. Every device capability is bound on it before this event is returned.
    SeatAdded,
    SeatRemoved,
    DeviceAdded(Device),
    DeviceRemoved(DeviceId),
    DevicePaused(DeviceId),
    DeviceResumed(DeviceId),
    /// Ends a group of events that belong to one hardware event. `time_us` is in microseconds.
    Frame {
        time_us: u64,
    },
    StartEmulating {
        device: DeviceId,
        sequence: u32,
    },
    StopEmulating {
        device: DeviceId,
    },
    /// Relative pointer motion in logical pixels.
    PointerMotion {
        dx: f64,
        dy: f64,
    },
    /// Absolute pointer position in the device's region coordinates.
    PointerMotionAbsolute {
        x: f64,
        y: f64,
    },
    /// Button press or release. `button` is an evdev code such as `BTN_LEFT`.
    Button {
        button: u32,
        pressed: bool,
    },
    /// Smooth scroll in logical pixels.
    ScrollDelta {
        dx: f64,
        dy: f64,
    },
    /// Wheel-style scroll in steps of 120 per notch.
    ScrollDiscrete {
        dx: i32,
        dy: i32,
    },
    /// Scroll sequence stopped on the given axes.
    ScrollStop {
        x: bool,
        y: bool,
    },
    /// Scroll sequence cancelled on the given axes.
    ScrollCancel {
        x: bool,
        y: bool,
    },
    /// Key press or release. `keycode` is an evdev code.
    Key {
        keycode: u32,
        pressed: bool,
    },
    TouchDown {
        id: u32,
        x: f64,
        y: f64,
    },
    TouchMotion {
        id: u32,
        x: f64,
        y: f64,
    },
    TouchUp {
        id: u32,
        cancelled: bool,
    },
    /// Reply to a ping.
    Pong,
    /// An event type this crate does not decode, with its raw `ei_event_type` value.
    Other(i32),
}

/// Capability bits in the form the seat binding expects. Every capability is bound so the server
/// can offer any device it has.
fn bind_all_capabilities(api: &sys::Api, seat: *mut sys::EiSeat) {
    // SAFETY: `seat` is a live seat borrowed from the current event. The argument list is the
    // six capability bits followed by NULL, which matches the variadic signature in libei.h.
    // Each value is passed as a C `unsigned int`, the width libei reads.
    unsafe {
        (api.ei_seat_bind_capabilities)(
            seat,
            Capabilities::POINTER.bits(),
            Capabilities::POINTER_ABSOLUTE.bits(),
            Capabilities::KEYBOARD.bits(),
            Capabilities::TOUCH.bits(),
            Capabilities::SCROLL.bits(),
            Capabilities::BUTTON.bits(),
            ptr::null::<c_void>(),
        );
    }
}

/// Decodes `raw` and releases it. `raw` must be a reference owned by the caller.
pub(crate) fn decode(ctx: &Context, raw: NonNull<EiEvent>) -> Event {
    let api = ctx.api;
    let ev = raw.as_ptr();
    // SAFETY: `ev` is a live event and we hold its only reference. Each getter is called only for
    // the event type that defines it, and the device pointer is dereferenced only while `ev` is
    // alive, before the reference is released below.
    let event = unsafe {
        let kind = (api.ei_event_get_type)(ev);
        let device = || (api.ei_event_get_device)(ev);
        match kind {
            sys::EI_EVENT_CONNECT => Event::Connected,
            sys::EI_EVENT_DISCONNECT => Event::Disconnected,
            sys::EI_EVENT_SEAT_ADDED => {
                bind_all_capabilities(api, (api.ei_event_get_seat)(ev));
                Event::SeatAdded
            }
            sys::EI_EVENT_SEAT_REMOVED => Event::SeatRemoved,
            sys::EI_EVENT_DEVICE_ADDED => match Device::borrow(ctx, device()) {
                Some(dev) => Event::DeviceAdded(dev),
                None => Event::Other(kind),
            },
            sys::EI_EVENT_DEVICE_REMOVED => Event::DeviceRemoved(DeviceId::from_ptr(device())),
            sys::EI_EVENT_DEVICE_PAUSED => Event::DevicePaused(DeviceId::from_ptr(device())),
            sys::EI_EVENT_DEVICE_RESUMED => Event::DeviceResumed(DeviceId::from_ptr(device())),
            sys::EI_EVENT_FRAME => Event::Frame {
                time_us: (api.ei_event_get_time)(ev),
            },
            sys::EI_EVENT_DEVICE_START_EMULATING => Event::StartEmulating {
                device: DeviceId::from_ptr(device()),
                sequence: (api.ei_event_emulating_get_sequence)(ev),
            },
            sys::EI_EVENT_DEVICE_STOP_EMULATING => Event::StopEmulating {
                device: DeviceId::from_ptr(device()),
            },
            sys::EI_EVENT_POINTER_MOTION => Event::PointerMotion {
                dx: (api.ei_event_pointer_get_dx)(ev),
                dy: (api.ei_event_pointer_get_dy)(ev),
            },
            sys::EI_EVENT_POINTER_MOTION_ABSOLUTE => Event::PointerMotionAbsolute {
                x: (api.ei_event_pointer_get_absolute_x)(ev),
                y: (api.ei_event_pointer_get_absolute_y)(ev),
            },
            sys::EI_EVENT_BUTTON_BUTTON => Event::Button {
                button: (api.ei_event_button_get_button)(ev),
                pressed: (api.ei_event_button_get_is_press)(ev),
            },
            sys::EI_EVENT_SCROLL_DELTA => Event::ScrollDelta {
                dx: (api.ei_event_scroll_get_dx)(ev),
                dy: (api.ei_event_scroll_get_dy)(ev),
            },
            sys::EI_EVENT_SCROLL_DISCRETE => Event::ScrollDiscrete {
                dx: (api.ei_event_scroll_get_discrete_dx)(ev),
                dy: (api.ei_event_scroll_get_discrete_dy)(ev),
            },
            sys::EI_EVENT_SCROLL_STOP => Event::ScrollStop {
                x: (api.ei_event_scroll_get_stop_x)(ev),
                y: (api.ei_event_scroll_get_stop_y)(ev),
            },
            sys::EI_EVENT_SCROLL_CANCEL => Event::ScrollCancel {
                x: (api.ei_event_scroll_get_stop_x)(ev),
                y: (api.ei_event_scroll_get_stop_y)(ev),
            },
            sys::EI_EVENT_KEYBOARD_KEY => Event::Key {
                keycode: (api.ei_event_keyboard_get_key)(ev),
                pressed: (api.ei_event_keyboard_get_key_is_press)(ev),
            },
            sys::EI_EVENT_TOUCH_DOWN => Event::TouchDown {
                id: (api.ei_event_touch_get_id)(ev),
                x: (api.ei_event_touch_get_x)(ev),
                y: (api.ei_event_touch_get_y)(ev),
            },
            sys::EI_EVENT_TOUCH_MOTION => Event::TouchMotion {
                id: (api.ei_event_touch_get_id)(ev),
                x: (api.ei_event_touch_get_x)(ev),
                y: (api.ei_event_touch_get_y)(ev),
            },
            sys::EI_EVENT_TOUCH_UP => Event::TouchUp {
                id: (api.ei_event_touch_get_id)(ev),
                cancelled: (api.ei_event_touch_get_is_cancel)(ev),
            },
            sys::EI_EVENT_PONG => Event::Pong,
            other => Event::Other(other),
        }
    };
    // SAFETY: `ev` came from `ei_get_event`, so this is the only reference to release. For a
    // sync event this also sends the reply, and every earlier request has already been handled.
    unsafe { (api.ei_event_unref)(ev) };
    event
}
