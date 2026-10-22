//! Devices: metadata copied out when the device is added, plus sender-side event emission.

use std::ffi::CStr;
use std::fmt;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::ptr::NonNull;

use crate::sys::{self, EiDevice};
use crate::{Capabilities, Context};

/// Identity of a device, stable while the device exists. Compare only; it is not a pointer to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeviceId(usize);

impl DeviceId {
    pub(crate) fn from_ptr(dev: *mut EiDevice) -> Self {
        Self(dev as usize)
    }
}

/// Whether a device is a virtual (screen-space) or physical (mm-space) device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceType {
    Virtual,
    Physical,
    /// A type value this crate does not know.
    Unknown(i32),
}

/// One absolute-pointer region, in desktop coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// XKB keymap for a keyboard device. `fd` is a duplicate, so it outlives the device.
#[derive(Debug)]
pub struct Keymap {
    pub fd: OwnedFd,
    /// Keymap text length in bytes.
    pub size: usize,
}

/// Metadata for a device, copied when the device is added.
#[derive(Debug)]
pub struct DeviceInfo {
    pub name: String,
    pub device_type: DeviceType,
    pub capabilities: Capabilities,
    /// Physical size in mm. Zero for virtual devices.
    pub width_mm: u32,
    pub height_mm: u32,
    pub regions: Vec<Region>,
    /// Present only for keyboard devices that the server gave a keymap.
    pub keymap: Option<Keymap>,
}

/// A libei device. Holds one reference on the device and one on its context.
pub struct Device {
    raw: NonNull<EiDevice>,
    ctx: NonNull<sys::Ei>,
    api: &'static sys::Api,
    info: DeviceInfo,
}

// SAFETY: a device is moved between threads only as a whole, and libei refcounts are touched only
// by the thread that owns the device at the time. `Device` is not `Sync`, so it cannot be used
// from two threads at once.
unsafe impl Send for Device {}

impl Device {
    /// Takes new references on `dev` and its context and copies the device metadata.
    ///
    /// # Safety
    ///
    /// `dev` must be a live device, for example one borrowed from an event of `ctx`.
    pub(crate) unsafe fn borrow(ctx: &Context, dev: *mut EiDevice) -> Option<Self> {
        let raw = NonNull::new(dev)?;
        // SAFETY: the caller guarantees `dev` is live. Taking references keeps it and its context
        // alive after the event is released. Both are released once, in `Drop`.
        unsafe {
            (ctx.api.ei_device_ref)(raw.as_ptr());
            (ctx.api.ei_ref)(ctx.raw.as_ptr());
        }
        let info = read_info(ctx.api, raw);
        Some(Self {
            raw,
            ctx: ctx.raw,
            api: ctx.api,
            info,
        })
    }

    pub fn id(&self) -> DeviceId {
        DeviceId::from_ptr(self.raw.as_ptr())
    }

    pub fn info(&self) -> &DeviceInfo {
        &self.info
    }

    /// Starts an emulation sequence. Sender only; the sequence number must increase on each call.
    pub fn start_emulating(&self, sequence: u32) {
        // SAFETY: `self.raw` is a live device we hold a reference to. Sending requests is valid on
        // a sender context, which is the only kind that calls this.
        unsafe { (self.api.ei_device_start_emulating)(self.raw.as_ptr(), sequence) };
    }

    /// Ends the emulation sequence started by [`Self::start_emulating`]. Sender only.
    pub fn stop_emulating(&self) {
        // SAFETY: as in `start_emulating`.
        unsafe { (self.api.ei_device_stop_emulating)(self.raw.as_ptr()) };
    }

    /// Groups the events sent since the last frame. `time_us` comes from [`Context::now`].
    pub fn frame(&self, time_us: u64) {
        // SAFETY: as in `start_emulating`.
        unsafe { (self.api.ei_device_frame)(self.raw.as_ptr(), time_us) };
    }

    /// Relative pointer motion in logical pixels.
    pub fn pointer_motion(&self, dx: f64, dy: f64) {
        // SAFETY: as in `start_emulating`.
        unsafe { (self.api.ei_device_pointer_motion)(self.raw.as_ptr(), dx, dy) };
    }

    /// Absolute pointer position in desktop coordinates. Discarded by libei outside the regions.
    pub fn pointer_motion_absolute(&self, x: f64, y: f64) {
        // SAFETY: as in `start_emulating`.
        unsafe { (self.api.ei_device_pointer_motion_absolute)(self.raw.as_ptr(), x, y) };
    }

    /// Button press or release. `button` is an evdev code such as `BTN_LEFT`.
    pub fn button(&self, button: u32, pressed: bool) {
        // SAFETY: as in `start_emulating`.
        unsafe { (self.api.ei_device_button_button)(self.raw.as_ptr(), button, pressed) };
    }

    /// Smooth scroll in logical pixels.
    pub fn scroll_delta(&self, dx: f64, dy: f64) {
        // SAFETY: as in `start_emulating`.
        unsafe { (self.api.ei_device_scroll_delta)(self.raw.as_ptr(), dx, dy) };
    }

    /// Wheel-style scroll in steps of 120 per notch.
    pub fn scroll_discrete(&self, dx: i32, dy: i32) {
        // SAFETY: as in `start_emulating`.
        unsafe { (self.api.ei_device_scroll_discrete)(self.raw.as_ptr(), dx, dy) };
    }

    /// Key press or release. `keycode` is an evdev code, independent of any keymap.
    pub fn keyboard_key(&self, keycode: u32, pressed: bool) {
        // SAFETY: as in `start_emulating`.
        unsafe { (self.api.ei_device_keyboard_key)(self.raw.as_ptr(), keycode, pressed) };
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: the references taken in `borrow` are released here, once each. The device goes
        // first, so the context reference is still held while it is released.
        unsafe {
            (self.api.ei_device_unref)(self.raw.as_ptr());
            (self.api.ei_unref)(self.ctx.as_ptr());
        }
    }
}

impl fmt::Debug for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Device")
            .field("id", &self.id())
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

/// Copies everything from a live device. Borrowed strings and regions are read here, so nothing
/// points into libei after this returns.
fn read_info(api: &sys::Api, dev: NonNull<EiDevice>) -> DeviceInfo {
    let d = dev.as_ptr();
    // SAFETY: `dev` is a live device, as `Device::borrow` requires. Every call is a getter. The name
    // pointer is copied before the function returns, and the region and keymap handles are read
    // while the device is alive.
    unsafe {
        let name_ptr = (api.ei_device_get_name)(d);
        let name = if name_ptr.is_null() {
            String::new()
        } else {
            CStr::from_ptr(name_ptr).to_string_lossy().into_owned()
        };

        let device_type = match (api.ei_device_get_type)(d) {
            sys::EI_DEVICE_TYPE_VIRTUAL => DeviceType::Virtual,
            sys::EI_DEVICE_TYPE_PHYSICAL => DeviceType::Physical,
            other => DeviceType::Unknown(other),
        };

        let mut capabilities = Capabilities::empty();
        for cap in Capabilities::all().iter() {
            if (api.ei_device_has_capability)(d, cap.bits()) {
                capabilities |= cap;
            }
        }

        let mut regions = Vec::new();
        for index in 0.. {
            let Some(r) = NonNull::new((api.ei_device_get_region)(d, index)) else {
                break;
            };
            regions.push(Region {
                x: (api.ei_region_get_x)(r.as_ptr()),
                y: (api.ei_region_get_y)(r.as_ptr()),
                width: (api.ei_region_get_width)(r.as_ptr()),
                height: (api.ei_region_get_height)(r.as_ptr()),
            });
        }

        let keymap = if capabilities.contains(Capabilities::KEYBOARD) {
            read_keymap(api, d)
        } else {
            None
        };

        DeviceInfo {
            name,
            device_type,
            capabilities,
            width_mm: (api.ei_device_get_width)(d),
            height_mm: (api.ei_device_get_height)(d),
            regions,
            keymap,
        }
    }
}

/// Duplicates the XKB keymap descriptor of a keyboard device.
///
/// # Safety
///
/// `dev` must be live. The keymap is owned by the device and is only read while the device lives.
unsafe fn read_keymap(api: &sys::Api, dev: *mut EiDevice) -> Option<Keymap> {
    // SAFETY: the caller guarantees `dev` is live. `ei_device_keyboard_get_keymap` returns a
    // borrowed keymap owned by the device. It is read immediately, and the descriptor is dup'd.
    unsafe {
        let km = NonNull::new((api.ei_device_keyboard_get_keymap)(dev))?.as_ptr();
        if (api.ei_keymap_get_type)(km) != sys::EI_KEYMAP_TYPE_XKB {
            return None;
        }
        let size = (api.ei_keymap_get_size)(km);
        let fd = (api.ei_keymap_get_fd)(km);
        // A negative fd means the keymap has no descriptor, and `borrow_raw` must never see one.
        if fd < 0 {
            return None;
        }
        let borrowed = BorrowedFd::borrow_raw(fd);
        let fd = borrowed.try_clone_to_owned().ok()?;
        Some(Keymap { fd, size })
    }
}
