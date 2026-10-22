//! Error type for the safe libva wrappers.

use std::{ffi::CStr, io, path::PathBuf};

use lumen_protocol::media::PixelFormat;

use crate::{library::VaLibrary, sys};

/// A failure from libva, or from an argument the safe wrappers reject.
#[derive(Debug, thiserror::Error)]
pub enum VaError {
    /// `libva.so.2` or `libva-drm.so.2` cannot be loaded, or lacks a required symbol.
    #[error("libva unavailable: {0}")]
    Unavailable(String),
    /// A DRM render node cannot be opened.
    #[error("cannot open {}: {source}", path.display())]
    Open { path: PathBuf, source: io::Error },
    /// No candidate render node could be opened and initialised.
    #[error("no usable VA-API render node: {0}")]
    NoDevice(String),
    /// `vaGetDisplayDRM` returned no display for the fd.
    #[error("vaGetDisplayDRM returned no display")]
    NoDisplay,
    /// A libva call returned a failure status.
    #[error("{op} failed: {message} (VAStatus {status})")]
    Status {
        op: &'static str,
        status: sys::VAStatus,
        message: String,
    },
    /// An argument is out of range for libva, or belongs to another display.
    #[error("invalid argument: {0}")]
    Invalid(&'static str),
    /// The pixel format is not accepted for this operation.
    #[error("pixel format {0:?} is not supported for this operation")]
    UnsupportedFormat(PixelFormat),
    /// An fd operation failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Result alias for the wrappers in this crate.
pub type VaResult<T> = Result<T, VaError>;

/// Converts a libva status into `Result`. On failure the message is `vaErrorStr`'s text.
pub(crate) fn check(lib: &VaLibrary, op: &'static str, status: sys::VAStatus) -> VaResult<()> {
    if status == sys::VA_STATUS_SUCCESS.cast_signed() {
        return Ok(());
    }
    Err(VaError::Status {
        op,
        status,
        message: error_str(lib, status),
    })
}

/// Converts an integer into a libva argument type, failing instead of truncating.
pub(crate) fn fit<T, U: TryFrom<T>>(value: T) -> VaResult<U> {
    U::try_from(value).map_err(|_| VaError::Invalid("integer argument does not fit the libva type"))
}

fn error_str(lib: &VaLibrary, status: sys::VAStatus) -> String {
    // SAFETY: `vaErrorStr` takes a plain integer and needs no display. It returns NULL or a
    // pointer to a static, NUL-terminated string that libva never frees.
    let ptr = unsafe { lib.core.vaErrorStr(status) };
    if ptr.is_null() {
        return format!("unknown status {status}");
    }
    // SAFETY: `ptr` is non-null, and per the libva contract above it is NUL-terminated and static.
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}
