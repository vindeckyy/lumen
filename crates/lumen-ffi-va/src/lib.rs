//! Safe wrappers over libva (VA-API).
//!
//! libva is loaded at runtime (see [`VaLibrary`]). The crate has no link-time dependency on
//! it, so it builds where libva is absent, and a missing library is reported as
//! [`VaError::Unavailable`].
//!
//! Every object keeps an `Arc` to its display, so the display outlives the objects made from
//! it. Each object destroys its VA handle on drop. Calls on one display are serialised by an
//! internal lock.
//!
//! This is the only crate in the workspace that contains `unsafe`. Each `unsafe` block states
//! why the call is sound.

// FFI boundary: out-pointers are passed as `&mut x`, and bindgen keeps libva's leading-underscore
// type names. Each `unsafe` block carries its own SAFETY comment.
#![allow(clippy::borrow_as_ptr, clippy::used_underscore_items)]

mod buffer;
mod config;
mod context;
mod display;
mod dmabuf;
mod error;
mod format;
mod image;
pub mod knobs;
mod library;
mod surface;
pub mod sys;
mod vpp;

pub use buffer::OwnedVaBuffer;
pub use config::VaConfig;
pub use context::VaContext;
pub use display::VaDisplay;
pub use dmabuf::{read_dmabuf, sync_dmabuf};
pub use error::{VaError, VaResult};
pub use format::{fourcc, pixel_format, rt_format};
pub use image::{MappedVaImage, OwnedVaImage};
pub use library::VaLibrary;
pub use surface::{DmabufExport, DmabufImport, DmabufPlane, OwnedVaSurface};
pub use vpp::VideoProcessor;
