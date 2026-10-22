//! Presents synthetic frames through the X11 backend on a private Xvfb at :98.

use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use lumen_protocol::media::{FrameSink, PixelFormat, VideoFrame};
use lumen_render::X11Renderer;

const DISPLAY: &str = ":98";

/// Kills the Xvfb child when dropped.
struct Xvfb(Child);

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_xvfb() -> Result<Xvfb, String> {
    let mut child = Command::new("Xvfb")
        .args([
            DISPLAY,
            "-screen",
            "0",
            "1920x1080x24",
            "-nolisten",
            "tcp",
            "-noreset",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run Xvfb: {e}"))?;
    let socket = format!("/tmp/.X11-unix/X{}", &DISPLAY[1..]);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "Xvfb {DISPLAY} exited early with {status}; display may be in use"
            ));
        }
        if UnixStream::connect(&socket).is_ok() {
            return Ok(Xvfb(child));
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("Xvfb {DISPLAY} did not open {socket} within 20 s"));
        }
        sleep(Duration::from_millis(50));
    }
}

fn bgrx_frame(width: u32, height: u32, index: u64) -> VideoFrame {
    let mut data = vec![0u8; (width * height * 4) as usize];
    for (i, px) in data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        px.copy_from_slice(&[(i % 256) as u8, ((i / width as usize) % 256) as u8, 0x80, 0]);
    }
    VideoFrame::cpu(width, height, PixelFormat::Bgrx, data, 0, index)
}

#[test]
fn x11_present_increments_count_and_reports_timestamps() {
    let _xvfb = match start_xvfb() {
        Ok(x) => x,
        Err(reason) => {
            eprintln!("SKIP x11_present_increments_count_and_reports_timestamps: {reason}");
            return;
        }
    };
    let mut renderer =
        X11Renderer::connect(Some(DISPLAY), "lumen-render test").expect("connect to Xvfb :98");
    assert_eq!(renderer.present_count(), 0);

    // Full-screen size exercises the unredirect path; the smaller size recreates the window.
    renderer
        .present(&bgrx_frame(1920, 1080, 0))
        .expect("present full-screen BGRX frame");
    assert_eq!(renderer.present_count(), 1);
    renderer
        .present(&bgrx_frame(640, 360, 1))
        .expect("present windowed BGRX frame");
    assert_eq!(renderer.present_count(), 2);

    // Completion events arrive asynchronously; wait briefly for the first timestamp.
    let deadline = Instant::now() + Duration::from_secs(2);
    while renderer.last_present_ts_us().is_none() && Instant::now() < deadline {
        renderer.poll_events().expect("poll X events");
        sleep(Duration::from_millis(10));
    }
    assert!(
        renderer.last_present_ts_us().is_some(),
        "no PresentCompleteNotify within 2 s"
    );
}
