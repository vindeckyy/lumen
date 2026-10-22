//! Raw libei C ABI, hand-written from `/usr/include/libei-1.0/libei.h` (libei 1.5).
//!
//! Nothing here links at build time. `libei.so.1` is opened with `dlopen` on first use and every
//! entry point is copied out of it as a function pointer. Only the symbols this crate calls are
//! declared.

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::sync::LazyLock;

use libloading::Library;

use crate::Error;

/// Opaque `struct ei`.
#[repr(C)]
pub struct Ei {
    _private: [u8; 0],
}

/// Opaque `struct ei_event`.
#[repr(C)]
pub struct EiEvent {
    _private: [u8; 0],
}

/// Opaque `struct ei_device`.
#[repr(C)]
pub struct EiDevice {
    _private: [u8; 0],
}

/// Opaque `struct ei_seat`.
#[repr(C)]
pub struct EiSeat {
    _private: [u8; 0],
}

/// Opaque `struct ei_region`.
#[repr(C)]
pub struct EiRegion {
    _private: [u8; 0],
}

/// Opaque `struct ei_keymap`.
#[repr(C)]
pub struct EiKeymap {
    _private: [u8; 0],
}

pub const EI_EVENT_CONNECT: c_int = 1;
pub const EI_EVENT_DISCONNECT: c_int = 2;
pub const EI_EVENT_SEAT_ADDED: c_int = 3;
pub const EI_EVENT_SEAT_REMOVED: c_int = 4;
pub const EI_EVENT_DEVICE_ADDED: c_int = 5;
pub const EI_EVENT_DEVICE_REMOVED: c_int = 6;
pub const EI_EVENT_DEVICE_PAUSED: c_int = 7;
pub const EI_EVENT_DEVICE_RESUMED: c_int = 8;
pub const EI_EVENT_PONG: c_int = 90;
pub const EI_EVENT_FRAME: c_int = 100;
pub const EI_EVENT_DEVICE_START_EMULATING: c_int = 200;
pub const EI_EVENT_DEVICE_STOP_EMULATING: c_int = 201;
pub const EI_EVENT_POINTER_MOTION: c_int = 300;
pub const EI_EVENT_POINTER_MOTION_ABSOLUTE: c_int = 400;
pub const EI_EVENT_BUTTON_BUTTON: c_int = 500;
pub const EI_EVENT_SCROLL_DELTA: c_int = 600;
pub const EI_EVENT_SCROLL_STOP: c_int = 601;
pub const EI_EVENT_SCROLL_CANCEL: c_int = 602;
pub const EI_EVENT_SCROLL_DISCRETE: c_int = 603;
pub const EI_EVENT_KEYBOARD_KEY: c_int = 700;
pub const EI_EVENT_TOUCH_DOWN: c_int = 800;
pub const EI_EVENT_TOUCH_UP: c_int = 801;
pub const EI_EVENT_TOUCH_MOTION: c_int = 802;

pub const EI_DEVICE_TYPE_VIRTUAL: c_int = 1;
pub const EI_DEVICE_TYPE_PHYSICAL: c_int = 2;
pub const EI_KEYMAP_TYPE_XKB: c_int = 1;

macro_rules! libei_api {
    ($($name:ident : $ty:ty;)*) => {
        /// Function table resolved from `libei.so.1`. `_lib` keeps the mapping alive.
        pub(crate) struct Api {
            _lib: Library,
            $(pub(crate) $name: $ty,)*
        }

        impl Api {
            fn load(lib: Library) -> Result<Self, libloading::Error> {
                $(
                    // SAFETY: the symbol name and signature match libei.h. The pointer is copied
                    // out of the Symbol, and `_lib` stays in the same struct so it remains mapped.
                    let $name = unsafe { *lib.get::<$ty>(concat!(stringify!($name), "\0").as_bytes())? };
                )*
                Ok(Self { _lib: lib, $($name,)* })
            }
        }
    };
}

libei_api! {
    ei_new_sender: unsafe extern "C" fn(*mut c_void) -> *mut Ei;
    ei_new_receiver: unsafe extern "C" fn(*mut c_void) -> *mut Ei;
    ei_ref: unsafe extern "C" fn(*mut Ei) -> *mut Ei;
    ei_unref: unsafe extern "C" fn(*mut Ei) -> *mut Ei;
    ei_configure_name: unsafe extern "C" fn(*mut Ei, *const c_char);
    ei_setup_backend_fd: unsafe extern "C" fn(*mut Ei, c_int) -> c_int;
    ei_get_fd: unsafe extern "C" fn(*mut Ei) -> c_int;
    ei_dispatch: unsafe extern "C" fn(*mut Ei);
    ei_get_event: unsafe extern "C" fn(*mut Ei) -> *mut EiEvent;
    ei_now: unsafe extern "C" fn(*mut Ei) -> u64;

    ei_event_unref: unsafe extern "C" fn(*mut EiEvent) -> *mut EiEvent;
    ei_event_get_type: unsafe extern "C" fn(*mut EiEvent) -> c_int;
    ei_event_get_device: unsafe extern "C" fn(*mut EiEvent) -> *mut EiDevice;
    ei_event_get_seat: unsafe extern "C" fn(*mut EiEvent) -> *mut EiSeat;
    ei_event_get_time: unsafe extern "C" fn(*mut EiEvent) -> u64;
    ei_event_emulating_get_sequence: unsafe extern "C" fn(*mut EiEvent) -> u32;
    ei_event_pointer_get_dx: unsafe extern "C" fn(*mut EiEvent) -> f64;
    ei_event_pointer_get_dy: unsafe extern "C" fn(*mut EiEvent) -> f64;
    ei_event_pointer_get_absolute_x: unsafe extern "C" fn(*mut EiEvent) -> f64;
    ei_event_pointer_get_absolute_y: unsafe extern "C" fn(*mut EiEvent) -> f64;
    ei_event_button_get_button: unsafe extern "C" fn(*mut EiEvent) -> u32;
    ei_event_button_get_is_press: unsafe extern "C" fn(*mut EiEvent) -> bool;
    ei_event_scroll_get_dx: unsafe extern "C" fn(*mut EiEvent) -> f64;
    ei_event_scroll_get_dy: unsafe extern "C" fn(*mut EiEvent) -> f64;
    ei_event_scroll_get_stop_x: unsafe extern "C" fn(*mut EiEvent) -> bool;
    ei_event_scroll_get_stop_y: unsafe extern "C" fn(*mut EiEvent) -> bool;
    ei_event_scroll_get_discrete_dx: unsafe extern "C" fn(*mut EiEvent) -> i32;
    ei_event_scroll_get_discrete_dy: unsafe extern "C" fn(*mut EiEvent) -> i32;
    ei_event_keyboard_get_key: unsafe extern "C" fn(*mut EiEvent) -> u32;
    ei_event_keyboard_get_key_is_press: unsafe extern "C" fn(*mut EiEvent) -> bool;
    ei_event_touch_get_id: unsafe extern "C" fn(*mut EiEvent) -> u32;
    ei_event_touch_get_x: unsafe extern "C" fn(*mut EiEvent) -> f64;
    ei_event_touch_get_y: unsafe extern "C" fn(*mut EiEvent) -> f64;
    ei_event_touch_get_is_cancel: unsafe extern "C" fn(*mut EiEvent) -> bool;

    // Variadic and NULL-terminated: libei reads the capability list as C `int`s.
    ei_seat_bind_capabilities: unsafe extern "C" fn(*mut EiSeat, ...);

    ei_device_ref: unsafe extern "C" fn(*mut EiDevice) -> *mut EiDevice;
    ei_device_unref: unsafe extern "C" fn(*mut EiDevice) -> *mut EiDevice;
    ei_device_get_name: unsafe extern "C" fn(*mut EiDevice) -> *const c_char;
    ei_device_get_type: unsafe extern "C" fn(*mut EiDevice) -> c_int;
    ei_device_has_capability: unsafe extern "C" fn(*mut EiDevice, c_uint) -> bool;
    ei_device_get_width: unsafe extern "C" fn(*mut EiDevice) -> u32;
    ei_device_get_height: unsafe extern "C" fn(*mut EiDevice) -> u32;
    ei_device_get_region: unsafe extern "C" fn(*mut EiDevice, usize) -> *mut EiRegion;
    ei_device_keyboard_get_keymap: unsafe extern "C" fn(*mut EiDevice) -> *mut EiKeymap;
    ei_device_start_emulating: unsafe extern "C" fn(*mut EiDevice, u32);
    ei_device_stop_emulating: unsafe extern "C" fn(*mut EiDevice);
    ei_device_frame: unsafe extern "C" fn(*mut EiDevice, u64);
    ei_device_pointer_motion: unsafe extern "C" fn(*mut EiDevice, f64, f64);
    ei_device_pointer_motion_absolute: unsafe extern "C" fn(*mut EiDevice, f64, f64);
    ei_device_button_button: unsafe extern "C" fn(*mut EiDevice, u32, bool);
    ei_device_scroll_delta: unsafe extern "C" fn(*mut EiDevice, f64, f64);
    ei_device_scroll_discrete: unsafe extern "C" fn(*mut EiDevice, i32, i32);
    ei_device_keyboard_key: unsafe extern "C" fn(*mut EiDevice, u32, bool);

    ei_region_get_x: unsafe extern "C" fn(*mut EiRegion) -> u32;
    ei_region_get_y: unsafe extern "C" fn(*mut EiRegion) -> u32;
    ei_region_get_width: unsafe extern "C" fn(*mut EiRegion) -> u32;
    ei_region_get_height: unsafe extern "C" fn(*mut EiRegion) -> u32;

    ei_keymap_get_type: unsafe extern "C" fn(*mut EiKeymap) -> c_int;
    ei_keymap_get_size: unsafe extern "C" fn(*mut EiKeymap) -> usize;
    ei_keymap_get_fd: unsafe extern "C" fn(*mut EiKeymap) -> c_int;
}

static API: LazyLock<Result<Api, String>> = LazyLock::new(|| {
    // SAFETY: loading libei runs its ELF constructors. That is the documented way to use the
    // system library, and libei has no constructor with observable side effects for us.
    let lib = unsafe { Library::new("libei.so.1") }.map_err(|e| e.to_string())?;
    Api::load(lib).map_err(|e| e.to_string())
});

/// Returns the process-wide libei function table, loading `libei.so.1` on first call.
pub(crate) fn api() -> Result<&'static Api, Error> {
    API.as_ref().map_err(|msg| Error::Unavailable(msg.clone()))
}
