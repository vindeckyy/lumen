//! Time the X11 capture on its own.
//!
//! The capture is the first stage of the host's pipeline, so what it costs
//! per frame sets the ceiling for the whole stream. This example opens a
//! capture, waits for the frames and reports what each one cost.

use std::time::{Duration, Instant};

use lumen_capture_x11::{CaptureConfig, X11Capture};
use lumen_protocol::media::FrameSource;

fn main() {
    let display = std::env::args().nth(1).unwrap_or_else(|| ":90".to_string());
    let mut capture = X11Capture::open(&CaptureConfig {
        display: Some(display),
        region: None,
        fps: 120,
        backend: None,
    })
    .expect("the capture opens");
    println!("  the backend is {:?}", capture.backend());
    println!("  the region is {:?}", capture.region());

    let mut frames = 0u64;
    let mut bytes = 0u64;
    let mut longest = Duration::ZERO;
    let start = Instant::now();
    // The first frames arrive after the compositor has painted something.
    let deadline = start + Duration::from_secs(10);
    while Instant::now() < deadline {
        let frame_start = Instant::now();
        let Some(frame) = capture.poll_frame() else {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        };
        let took = frame_start.elapsed();
        longest = longest.max(took);
        frames += 1;
        bytes += u64::from(frame.width) * u64::from(frame.height) * 4;
    }
    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "  {frames} frames in {elapsed:.1} s: {:.1} fps, {:.1} MiB a second",
        frames as f64 / elapsed,
        bytes as f64 / elapsed / (1024.0 * 1024.0)
    );
    println!(
        "  the longest wait for a frame: {:.2} ms",
        longest.as_secs_f64() * 1000.0
    );
}
