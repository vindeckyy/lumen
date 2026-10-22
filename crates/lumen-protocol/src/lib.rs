//! Lumen wire protocol.
//!
//! This crate defines the on-the-wire format shared by the Lumen host and
//! client: the fixed-size media packet header, the message kinds, the
//! capability bitmap, the media pipeline traits and the auxiliary message
//! encodings (input events, audio frames, slice descriptors).
//!
//! The layout is specified in `docs/protocol.md`; this crate is the
//! normative implementation of it.

#![forbid(unsafe_code)]

pub mod audio;
pub mod capability;
pub mod color;
pub mod error;
pub mod header;
pub mod input;
pub mod media;
pub mod message;
pub mod raw;
pub mod slice;

pub use capability::{Capabilities, VersionHandshake};
pub use error::ProtocolError;
pub use header::{HEADER_LEN, MAGIC, MediaHeader, VERSION};
pub use input::{InputAction, InputCodecError, InputEnvelope, ReplayWindow};
pub use media::{
    DRM_FORMAT_MOD_INVALID, DRM_FORMAT_MOD_LINEAR, DmabufImage, DmabufPlane, EncodedFrame,
    FrameBuffer, FrameSink, FrameSource, MAX_DMABUF_PLANES, PixelFormat, VideoDecoder,
    VideoEncoder, VideoFrame, drm_fourcc,
};
pub use message::MessageKind;
pub use slice::{SLICE_MAX_PAYLOAD, SliceDescriptor, SliceReader};

/// Protocol version implemented by this build.
pub const SUPPORTED_VERSIONS: &[u8] = &[VERSION];
