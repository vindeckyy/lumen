//! Raw libva types and constants, generated from the system headers by `build.rs`.
//!
//! The `VaCore` and `VaDrm` loader structs wrap `unsafe` entry points. Safe code
//! reaches them only through this crate's wrappers. Parameter structs implement `Default`
//! (bindgen emits zeroed defaults where it cannot derive them).

#![allow(
    non_snake_case,
    non_camel_case_types,
    non_upper_case_globals,
    dead_code,
    missing_debug_implementations,
    clippy::all,
    clippy::pedantic
)]

mod core_gen {
    include!(concat!(env!("OUT_DIR"), "/va_core.rs"));
}

mod drm_gen {
    include!(concat!(env!("OUT_DIR"), "/va_drm.rs"));
}

pub use core_gen::*;
pub(crate) use drm_gen::VaDrm;
