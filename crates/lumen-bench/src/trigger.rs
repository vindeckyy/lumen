//! The photodiode trigger simulation.
//!
//! A real rig works like this: the host drives a photodiode (or an LED) in
//! step with the frame it just captured, and a high-speed camera records
//! both the screen and the photodiode. The camera frame where the diode
//! fires is the moment the host presented the frame; the camera frame where
//! the barcode shows the same frame counter is the moment the client
//! presented it. The difference is the glass-to-glass latency.
//!
//! This module implements the same arithmetic against synthetic camera
//! frames, so the harness itself is verified without the rig. The simulated
//! trigger is written to a fixture file that `rig-selftest` reads back.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::pattern::{PATTERN_WIDTH, TestPattern, decode_barcode};

/// One simulated camera observation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraObservation {
    /// Frame counter read from the barcode in the camera image.
    pub observed_frame_index: u64,
    /// Host timestamp read from the barcode in the camera image.
    pub observed_timestamp_us: u64,
    /// Camera timestamp of the observation, microseconds.
    pub camera_ts_us: u64,
    /// Camera timestamp of the photodiode firing, microseconds.
    pub trigger_ts_us: u64,
}

/// A simulated rig run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulatedTrigger {
    /// The observations, in camera order.
    pub observations: Vec<CameraObservation>,
    /// The latency the rig would have measured, microseconds.
    pub glass_to_glass_us: u64,
}

/// Build a simulated rig run.
///
/// `frame_count` observations are produced. Each observation has a trigger
/// delay of `trigger_delay_us` between the host presenting the frame and the
/// diode firing, and the camera sees the client `client_latency_us` after
/// the trigger.
///
/// # Errors
/// Returns an error string when the parameters are inconsistent.
pub fn simulate(
    frame_count: usize,
    trigger_delay_us: u64,
    client_latency_us: u64,
) -> Result<SimulatedTrigger, String> {
    if frame_count == 0 {
        return Err("need at least one frame".into());
    }
    let mut observations = Vec::with_capacity(frame_count);
    for i in 0..frame_count {
        let frame_index = i as u64;
        let host_ts = 1_000_000 + i as u64 * 16_667;
        let trigger_ts = host_ts + trigger_delay_us;
        let camera_ts = trigger_ts + client_latency_us;
        observations.push(CameraObservation {
            observed_frame_index: frame_index,
            observed_timestamp_us: host_ts,
            camera_ts_us: camera_ts,
            trigger_ts_us: trigger_ts,
        });
    }
    // The glass-to-glass latency is what the camera measures: the gap between
    // the frame where the diode fires and the frame where the barcode shows
    // the same counter. The trigger delay is the offset between the host's
    // own clock stamp and the diode firing. It aligns the two clocks; it is
    // not part of the latency.
    let glass_to_glass_us = client_latency_us;
    Ok(SimulatedTrigger {
        observations,
        glass_to_glass_us,
    })
}

/// Write a simulated rig run to a fixture file.
///
/// # Errors
/// Returns an io error when the file cannot be written.
pub fn write_fixture(path: &Path, trigger: &SimulatedTrigger) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(trigger)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(path, json)
}

/// Read a simulated rig run from a fixture file.
///
/// # Errors
/// Returns an io error when the file cannot be read or parsed.
pub fn read_fixture(path: &Path) -> std::io::Result<SimulatedTrigger> {
    let text = std::fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Compute the glass-to-glass latency from a camera observation.
///
/// The diode fires when the host presented the frame; the camera sees the
/// same frame counter on the client later. The difference between the
/// camera timestamps of the two events is the glass-to-glass latency.
///
/// # Errors
/// Returns an error string when the observation is inconsistent.
pub fn glass_to_glass(observation: &CameraObservation) -> Result<u64, String> {
    if observation.camera_ts_us < observation.trigger_ts_us {
        return Err("camera timestamp precedes the trigger".into());
    }
    Ok(observation.camera_ts_us - observation.trigger_ts_us)
}

/// Extract the barcode strip from a camera image of the screen.
///
/// The image is the full screen; the barcode sits in the top left corner at
/// the pattern's native size, so the strip is cropped from there.
///
/// # Errors
/// Returns an error string when the image is too small to contain the strip.
pub fn read_barcode(
    image: &[u8],
    width: u32,
    height: u32,
    sample_width: u32,
) -> Result<(u64, u64), String> {
    if width < sample_width || height < crate::pattern::BARCODE_HEIGHT {
        return Err("camera image too small for the barcode".into());
    }
    let strip_height = crate::pattern::BARCODE_HEIGHT.min(height);
    let mut strip = Vec::with_capacity((sample_width * strip_height * 4) as usize);
    for y in 0..strip_height {
        for x in 0..sample_width {
            let offset = ((y * width + x) * 4) as usize;
            if offset + 4 > image.len() {
                return Err("image buffer too small".into());
            }
            strip.extend_from_slice(&image[offset..offset + 4]);
        }
    }
    decode_barcode(&strip, sample_width, strip_height, sample_width)
}

/// Render the pattern so a test can build a synthetic camera image.
#[must_use]
pub fn synthetic_camera_image(frame_index: u64, timestamp_us: u64) -> (Vec<u8>, u32, u32) {
    let pattern = TestPattern::render(frame_index, timestamp_us);
    (
        pattern.pixels,
        PATTERN_WIDTH,
        crate::pattern::PATTERN_HEIGHT,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simulation_reports_the_expected_latency() {
        let t = simulate(10, 1000, 5000).unwrap();
        // The camera measures the gap between the diode firing and the
        // barcode appearing, which is the client latency alone.
        assert_eq!(t.glass_to_glass_us, 5000);
        assert_eq!(t.observations.len(), 10);
    }

    #[test]
    fn the_simulation_and_the_measurement_agree() {
        let t = simulate(10, 1000, 5000).unwrap();
        for observation in &t.observations {
            assert_eq!(glass_to_glass(observation).unwrap(), t.glass_to_glass_us);
        }
    }

    #[test]
    fn simulation_rejects_zero_frames() {
        assert!(simulate(0, 1, 1).is_err());
    }

    #[test]
    fn glass_to_glass_uses_the_observation() {
        let t = simulate(1, 1000, 5000).unwrap();
        let g = glass_to_glass(&t.observations[0]).unwrap();
        assert_eq!(g, 5000);
    }

    #[test]
    fn fixture_roundtrips() {
        let dir = std::env::temp_dir().join("lumen-trigger-test");
        let path = dir.join("simulated-trigger.json");
        let t = simulate(4, 100, 200).unwrap();
        write_fixture(&path, &t).unwrap();
        let back = read_fixture(&path).unwrap();
        assert_eq!(back.glass_to_glass_us, 200);
        assert_eq!(back.observations.len(), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn synthetic_image_barcode_reads_back() {
        let (image, width, height) = synthetic_camera_image(11, 22);
        let (frame, ts) = read_barcode(&image, width, height, crate::pattern::barcode_width())
            .expect("barcode should read back");
        assert_eq!(frame, 11);
        assert_eq!(ts, 22);
    }
}
