//! Lumen host daemon.
//!
//! Establishes sessions (Noise handshake, control channel, media keys) and
//! owns the host side of the control channel.
//!
//! [`pipeline`] wires the capture, encode and send stages together into the
//! host media pipeline.
//!
//! The modules around it hold the parts of the host that the plan asks for
//! beyond the plain video path:
//!
//! | Module | What it holds |
//! |---|---|
//! | [`backend`] | the capture backend and the backend matrix of the plan |
//! | [`clipboard`] | the Wayland clipboard of the machine the host runs on |
//! | [`log`] | the host's log, which the control plane shows |
//! | [`monitors`] | the enumeration of the outputs the host can capture |
//! | [`multi`] | the sessions the host serves and the shadow mode |
//! | [`persist`] | the session state across a disconnect or a restart |
//! | [`resize`] | the mode changes and the encoder reconfiguration |
//! | [`settings`] | the settings the control plane changes while the host runs |
//! | [`virtual_display`] | the virtual display of a headless host |
//! | [`web`] | the control plane: the dashboard and what it can do |

#![forbid(unsafe_code)]

pub mod audio;
pub mod backend;
pub mod clipboard;
pub mod control;
pub mod gpu_convert;
pub mod input;
pub mod log;
pub mod monitors;
pub mod multi;
pub mod pairing;
pub mod persist;
pub mod pipeline;
pub mod resize;
pub mod session;
pub mod settings;
pub mod stages;
pub mod virtual_display;
pub mod web;
