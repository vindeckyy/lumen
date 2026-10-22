//! Camera frame analysis for the photodiode rig.
//!
//! The rig records the screen with a high-speed camera. This module turns
//! those recordings into the latency numbers: for each camera frame it finds
//! the barcode in the top left corner, reads the frame counter and the host
//! timestamp, and pairs it with the photodiode firing time to compute the
//! glass-to-glass latency.
//!
//! The real camera path needs a capture device. The v4l2 capture loop is a
//! shell script (`harness/photodiode/capture.sh`) that writes raw frames;
//! this module analyses the frames it produces.

/// One raw camera frame: BGRX pixels, width, height and camera timestamp in microseconds.
pub type CameraFrame = (Vec<u8>, u32, u32, u64);

use std::path::Path;

use crate::trigger::{CameraObservation, glass_to_glass};

/// One analysed camera frame.
#[derive(Debug, Clone)]
pub struct AnalysedFrame {
    /// The frame index read from the barcode.
    pub frame_index: u64,
    /// The host timestamp read from the barcode.
    pub host_timestamp_us: u64,
    /// The camera timestamp of this frame, microseconds.
    pub camera_ts_us: u64,
}

/// The result of analysing a whole recording.
#[derive(Debug, Clone, Default)]
pub struct RigResult {
    /// The per-frame observations.
    pub observations: Vec<CameraObservation>,
    /// The glass-to-glass latency percentiles, microseconds.
    pub p50_us: u64,
    /// The 99th percentile.
    pub p99_us: u64,
    /// The largest value.
    pub max_us: u64,
    /// The number of frames the recording contained.
    pub frames: usize,
}

impl RigResult {
    /// The glass-to-glass latency in milliseconds.
    #[must_use]
    pub fn p50_ms(&self) -> f64 {
        self.p50_us as f64 / 1000.0
    }

    /// The 99th percentile in milliseconds.
    #[must_use]
    pub fn p99_ms(&self) -> f64 {
        self.p99_us as f64 / 1000.0
    }
}

/// Analyse a recording.
///
/// `frames` is the sequence of camera frames: each entry is the BGRX pixel
/// data, the image width and height, and the camera timestamp. `triggers`
/// holds the camera timestamps at which the photodiode fired, in order.
///
/// # Errors
/// Returns an error string when a frame carries no readable barcode or when
/// there are no triggers.
pub fn analyse(frames: &[(Vec<u8>, u32, u32, u64)], triggers: &[u64]) -> Result<RigResult, String> {
    if triggers.is_empty() {
        return Err("no photodiode triggers in the recording".into());
    }
    let mut observations = Vec::new();
    for (pixels, width, height, camera_ts) in frames {
        let sample_width = crate::pattern::barcode_width().min(*width);
        let (frame_index, host_timestamp_us) =
            crate::trigger::read_barcode(pixels, *width, *height, sample_width)?;
        // The trigger nearest in time to this camera frame.
        let trigger_ts = triggers
            .iter()
            .min_by_key(|t| t.abs_diff(*camera_ts))
            .copied()
            .unwrap_or_default();
        observations.push(CameraObservation {
            observed_frame_index: frame_index,
            observed_timestamp_us: host_timestamp_us,
            camera_ts_us: *camera_ts,
            trigger_ts_us: trigger_ts,
        });
    }

    let mut latencies: Vec<u64> = observations
        .iter()
        .filter_map(|o| glass_to_glass(o).ok())
        .collect();
    latencies.sort_unstable();
    let p50 = latencies
        .get(latencies.len() / 2)
        .copied()
        .unwrap_or_default();
    let p99 = latencies
        .get((latencies.len() * 99 / 100).saturating_sub(1))
        .copied()
        .unwrap_or_default();
    let max = latencies.last().copied().unwrap_or_default();
    Ok(RigResult {
        observations,
        p50_us: p50,
        p99_us: p99,
        max_us: max,
        frames: frames.len(),
    })
}

/// Load raw BGRX frames from a directory of raw files.
///
/// The rig writes each camera frame as a raw BGRX buffer; the file name
/// carries the camera timestamp in microseconds.
///
/// # Errors
/// Returns an error string when a file cannot be read or is the wrong size.
pub fn load_raw_frames(dir: &Path, width: u32, height: u32) -> Result<Vec<CameraFrame>, String> {
    let mut out = Vec::new();
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    let mut names: Vec<_> = entries
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "raw"))
        .collect();
    names.sort();
    let expected = (width * height * 4) as usize;
    for path in names {
        let data =
            std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if data.len() != expected {
            return Err(format!(
                "{} is {} bytes, expected {expected}",
                path.display(),
                data.len()
            ));
        }
        let ts = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        out.push((data, width, height, ts));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyse_needs_triggers() {
        assert!(analyse(&[], &[]).is_err());
    }

    #[test]
    fn analyse_computes_latency() {
        let (pixels, w, h) = crate::trigger::synthetic_camera_image(1, 1000);
        let frames = vec![(pixels, w, h, 5_000u64)];
        let result = analyse(&frames, &[3_000]).unwrap();
        assert_eq!(result.frames, 1);
        assert_eq!(result.p50_us, 2_000);
    }
}
