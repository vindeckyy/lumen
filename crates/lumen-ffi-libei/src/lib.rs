//! Safe wrapper over libei (EI/EIS), loaded at runtime from `libei.so.1`.
//!
//! This crate is the only place in the workspace that holds `unsafe` for libei. Every `unsafe`
//! block states why it is sound.
//!
//! * A *receiver* context ([`Context::new_receiver`]) gets input events from an EIS server, such as
//!   the local input a compositor forwards through the InputCapture portal.
//! * A *sender* context ([`Context::new_sender`]) emulates input on an EIS server, such as the
//!   RemoteDesktop portal's `ConnectToEIS` connection.
//!
//! Usage: hand a socket fd to [`Context::setup_fd`], poll [`Context::as_fd`] for readability, call
//! [`Context::dispatch`], then drain [`Context::next_event`] until it returns `None`.

#![deny(unsafe_op_in_unsafe_fn)]

mod device;
mod event;
mod sys;

use std::ffi::CStr;
use std::fmt;
use std::os::fd::{BorrowedFd, IntoRawFd, OwnedFd};
use std::ptr::{self, NonNull};

pub use device::{Device, DeviceId, DeviceInfo, DeviceType, Keymap, Region};
pub use event::Event;

/// Errors from loading libei or creating and connecting a context.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// `libei.so.1` could not be opened or a symbol is missing.
    #[error("libei unavailable: {0}")]
    Unavailable(String),
    /// libei returned NULL from a `ei_new_*` constructor.
    #[error("libei could not allocate a context")]
    Create,
    /// `ei_setup_backend_fd` failed with this errno.
    #[error("ei_setup_backend_fd failed with errno {0}")]
    Setup(i32),
}

bitflags::bitflags! {
    /// Device capabilities, with the bit values of `enum ei_device_capability`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct Capabilities: u32 {
        const POINTER = 1 << 0;
        const POINTER_ABSOLUTE = 1 << 1;
        const KEYBOARD = 1 << 2;
        const TOUCH = 1 << 3;
        const SCROLL = 1 << 4;
        const BUTTON = 1 << 5;
    }
}

/// One libei context, either a receiver or a sender.
pub struct Context {
    pub(crate) raw: NonNull<sys::Ei>,
    pub(crate) api: &'static sys::Api,
}

// SAFETY: libei keeps no thread-local state for a context. Methods take `&mut self` or `&self`, so
// a context may move to another thread as long as no two threads use it at once. `Context` is not
// `Sync`, which rules out the concurrent use.
unsafe impl Send for Context {}

impl Context {
    /// Creates a receiver context. It gets events from the EIS server.
    pub fn new_receiver() -> Result<Self, Error> {
        let api = sys::api()?;
        // SAFETY: the user-data pointer is opaque to libei and NULL is allowed.
        let raw = unsafe { (api.ei_new_receiver)(ptr::null_mut()) };
        Self::wrap(api, raw)
    }

    /// Creates a sender context. `name` is a hint the server may show in its consent UI.
    pub fn new_sender(name: &CStr) -> Result<Self, Error> {
        let api = sys::api()?;
        // SAFETY: the user-data pointer is opaque to libei and NULL is allowed.
        let raw = unsafe { (api.ei_new_sender)(ptr::null_mut()) };
        let ctx = Self::wrap(api, raw)?;
        // SAFETY: `ctx.raw` is a live context, and `name` is NUL-terminated. libei copies the name,
        // and this call is made before the backend is set up, as libei requires.
        unsafe { (api.ei_configure_name)(ctx.raw.as_ptr(), name.as_ptr()) };
        Ok(ctx)
    }

    fn wrap(api: &'static sys::Api, raw: *mut sys::Ei) -> Result<Self, Error> {
        NonNull::new(raw)
            .map(|raw| Self { raw, api })
            .ok_or(Error::Create)
    }

    /// Attaches the connected EIS socket. libei owns `fd` from this call on and never returns it,
    /// so the caller must not close it.
    pub fn setup_fd(&mut self, fd: OwnedFd) -> Result<(), Error> {
        let raw_fd = fd.into_raw_fd();
        // SAFETY: `self.raw` is a live context. `raw_fd` is an open descriptor whose ownership is
        // transferred to libei by this call, so we never close it ourselves.
        let rc = unsafe { (self.api.ei_setup_backend_fd)(self.raw.as_ptr(), raw_fd) };
        if rc == 0 {
            Ok(())
        } else {
            Err(Error::Setup(rc.saturating_neg()))
        }
    }

    /// The descriptor to poll for readability. `None` until a backend is set up.
    pub fn as_fd(&self) -> Option<BorrowedFd<'_>> {
        // SAFETY: `ei_get_fd` is a getter on a live context and does not modify it.
        let fd = unsafe { (self.api.ei_get_fd)(self.raw.as_ptr()) };
        if fd < 0 {
            return None;
        }
        // SAFETY: libei owns `fd` for as long as the context lives. The returned borrow is tied to
        // `&self`, so it cannot outlive the context.
        Some(unsafe { BorrowedFd::borrow_raw(fd) })
    }

    /// Reads and processes data waiting on the socket. Call it when [`Self::as_fd`] is readable.
    pub fn dispatch(&mut self) {
        // SAFETY: `self.raw` is a live context. `ei_dispatch` may run at any time and only queues
        // events, which [`Self::next_event`] then returns.
        unsafe { (self.api.ei_dispatch)(self.raw.as_ptr()) };
    }

    /// Returns the next queued event, or `None` when the queue is empty.
    pub fn next_event(&mut self) -> Option<Event> {
        // SAFETY: `ei_get_event` returns a new reference that the caller owns. The NULL check comes
        // first, and `event::decode` releases the reference exactly once.
        let raw = NonNull::new(unsafe { (self.api.ei_get_event)(self.raw.as_ptr()) })?;
        Some(event::decode(self, raw))
    }

    /// Current time in microseconds on the clock that [`Device::frame`] expects.
    pub fn now(&self) -> u64 {
        // SAFETY: `ei_now` is a getter on a live context.
        unsafe { (self.api.ei_now)(self.raw.as_ptr()) }
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: we hold the reference acquired in `wrap`, and it is released exactly once here.
        // Devices that still exist hold their own reference, so libei keeps the context alive
        // until they go too.
        unsafe { (self.api.ei_unref)(self.raw.as_ptr()) };
    }
}

impl fmt::Debug for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn libei_or_skip() -> bool {
        match sys::api() {
            Ok(_) => true,
            Err(e) => {
                eprintln!("skip: {e}");
                false
            }
        }
    }

    #[test]
    fn receiver_and_sender_create_and_attach_socket() {
        if !libei_or_skip() {
            return;
        }
        let (a, _b) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        let mut receiver = Context::new_receiver().expect("receiver");
        receiver
            .setup_fd(OwnedFd::from(a))
            .expect("setup receiver fd");
        assert!(receiver.as_fd().is_some());
        // Nothing has been sent by the peer, so the queue must be empty.
        receiver.dispatch();
        assert!(receiver.next_event().is_none());

        let (c, _d) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        let mut sender = Context::new_sender(c"lumen-test").expect("sender");
        sender.setup_fd(OwnedFd::from(c)).expect("setup sender fd");
        assert!(sender.now() > 0);
    }
}
