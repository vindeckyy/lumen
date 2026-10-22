//! VA-API H.264 encoder: CQP or CBR, no B-frames, infinite GOP, on-demand IDR.
//!
//! Every libva call goes through `lumen-ffi-va`, so this crate holds no `unsafe`.

#![forbid(unsafe_code)]

mod buffer;
mod config;
mod headers;
mod knobs;
pub mod nal;
mod refs;
mod session;

pub use config::{DEFAULT_QP, EncoderConfig, H264Profile, MAX_EDGE, RateControl};
pub use session::H264Encoder;
