//! Lumen measurement harness.
//!
//! The harness provides the test pattern, the per-stage timestamp recorder,
//! the percentile report and the photodiode trigger simulation. Everything
//! that claims a latency number goes through this crate, so every number
//! carries the same provenance.

#![forbid(unsafe_code)]

pub mod camera;
pub mod e2e;
pub mod input_latency;
pub mod pattern;
pub mod record;
pub mod report;
pub mod trigger;

pub use e2e::{ClientReport, HostReport};
pub use pattern::TestPattern;
pub use record::{Recorder, Stage, Stats};
pub use report::{Counters, RunReport};
pub use trigger::{CameraObservation, SimulatedTrigger};
