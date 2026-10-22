//! Captures from a real X server started as `Xvfb :NN`.
//!
//! Skips with a printed reason when Xvfb cannot be started.
use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use lumen_capture_x11::{Backend, CaptureConfig, Rect, X11Capture};
use lumen_protocol::media::{FrameBuffer, FrameSource, PixelFormat, VideoFrame};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, CreateGCAux, Rectangle};

const WIDTH: u32 = 800;
const HEIGHT: u32 = 600;
/// Painted on the root in the test: `(x, y, w, h)`.
const PAINT: (i16, i16, u16, u16) = (10, 10, 100, 50);
/// BGRX bytes of pure red (0x00FF0000 in ZPixmap, little-endian).
const RED_BGRX: [u8; 4] = [0x00, 0x00, 0xff, 0x00];

struct Xvfb {
    child: Child,
    display: String,
    /// Read end of the server's stdout pipe, kept open for the server's lifetime.
    _stdout: BufReader<ChildStdout>,
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Tests in this binary run in parallel; starting two servers on one display number corrupts both.
static START: Mutex<()> = Mutex::new(());

/// Starts `Xvfb -displayfd 1`. The server picks a free display number itself and writes it to stdout
/// once it is listening, so no number is guessed in advance and no other server can be raced for it.
fn start_xvfb() -> Option<Xvfb> {
    // Tests in this binary run in parallel; the mutex keeps one start in flight at a time.
    let _start = START
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut child = match Command::new("Xvfb")
        .args([
            "-displayfd",
            "1",
            "-screen",
            "0",
            "800x600x24",
            "-nolisten",
            "tcp",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            eprintln!("SKIP lumen-capture-x11: cannot start Xvfb: {e}");
            return None;
        }
    };
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
    let mut line = String::new();
    let number = match stdout.read_line(&mut line) {
        Ok(n) if n > 0 => line.trim().parse::<u32>().ok(),
        _ => None,
    };
    let Some(number) = number else {
        eprintln!("SKIP lumen-capture-x11: Xvfb exited without reporting a display number");
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let display = format!(":{number}");
    if x11rb::connect(Some(display.as_str())).is_err() {
        eprintln!("SKIP lumen-capture-x11: Xvfb reported {display} but refuses connections");
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    // The stdout pipe stays open for the server's lifetime, so a late write cannot hit SIGPIPE.
    Some(Xvfb {
        child,
        display,
        _stdout: stdout,
    })
}

fn config(display: &str, region: Option<Rect>, backend: Option<Backend>) -> CaptureConfig {
    CaptureConfig {
        display: Some(display.to_owned()),
        region,
        fps: 60,
        backend,
    }
}

fn next_frame(cap: &mut X11Capture) -> VideoFrame {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(frame) = cap.poll_frame() {
            return frame;
        }
        assert!(Instant::now() < deadline, "no frame within 5 s");
        thread::sleep(Duration::from_millis(2));
    }
}

fn cpu_bytes(frame: &VideoFrame) -> &[u8] {
    match &frame.buffer {
        FrameBuffer::Cpu { data } => data,
        FrameBuffer::Dmabuf(_) => panic!("expected a CPU frame from this rung"),
    }
}

fn pixel(data: &[u8], width: u32, x: u32, y: u32) -> &[u8] {
    let i = ((y * width + x) * 4) as usize;
    &data[i..i + 4]
}

fn paint_red_rectangle(display: &str) {
    let (conn, screen_num) = x11rb::connect(Some(display)).expect("second client connects");
    let root = conn.setup().roots[screen_num].root;
    let gc = conn.generate_id().expect("gc id");
    xproto::create_gc(&conn, gc, root, &CreateGCAux::new().foreground(0x00ff_0000))
        .expect("create gc")
        .check()
        .expect("create gc accepted");
    let (x, y, w, h) = PAINT;
    xproto::poly_fill_rectangle(
        &conn,
        root,
        gc,
        &[Rectangle {
            x,
            y,
            width: w,
            height: h,
        }],
    )
    .expect("fill")
    .check()
    .expect("fill accepted");
    conn.flush().expect("flush");
}

#[test]
fn captures_root_damage_region_and_rungs_from_xvfb() {
    let Some(xvfb) = start_xvfb() else {
        return;
    };
    let display = xvfb.display.as_str();

    // Default ladder: whole root, full-size frame, plain CPU or DMA-BUF depending on the rung.
    let mut cap = X11Capture::open(&config(display, None, None)).expect("open X11 capture");
    eprintln!(
        "lumen-capture-x11: default backend on {display}: {:?}",
        cap.backend()
    );
    assert_eq!(
        cap.root_geometry(),
        Rect {
            x: 0,
            y: 0,
            width: WIDTH,
            height: HEIGHT
        }
    );
    let first = next_frame(&mut cap);
    assert_eq!((first.width, first.height), (WIDTH, HEIGHT));
    assert_eq!(first.format, PixelFormat::Bgrx);
    assert_eq!(first.index, 0);
    assert_eq!(cpu_bytes(&first).len(), (WIDTH * HEIGHT * 4) as usize);

    // Damage gating: nothing changed, so no frame is due even after the rate interval.
    thread::sleep(Duration::from_millis(50));
    assert!(cap.poll_frame().is_none(), "frame emitted without damage");

    // A paint on the root must raise damage and produce a frame showing the red rectangle.
    paint_red_rectangle(display);
    let painted = next_frame(&mut cap);
    assert_eq!(painted.index, 1);
    let data = cpu_bytes(&painted);
    let (x, y, _, _) = PAINT;
    assert_eq!(pixel(data, WIDTH, x as u32 + 5, y as u32 + 5), RED_BGRX);
    assert_eq!(pixel(data, WIDTH, 0, 0), [0, 0, 0, 0]);
    assert!(cap.take_error().is_none(), "capture reported an error");

    // Cursor via XFixes: the image is consistent with its reported size.
    let cursor = cap.cursor().expect("XFixes cursor");
    assert_eq!(
        cursor.argb.len(),
        usize::from(cursor.width) * usize::from(cursor.height)
    );

    // Explicit region: the painted rectangle is the whole region, so every pixel is red.
    let region = Rect {
        x: i32::from(x),
        y: i32::from(y),
        width: u32::from(PAINT.2),
        height: u32::from(PAINT.3),
    };
    let mut sub =
        X11Capture::open(&config(display, Some(region), None)).expect("open region capture");
    assert_eq!(sub.region(), region);
    let frame = next_frame(&mut sub);
    assert_eq!((frame.width, frame.height), (region.width, region.height));
    for px in cpu_bytes(&frame).as_chunks::<4>().0 {
        assert_eq!(*px, RED_BGRX, "region pixel is not red");
    }

    // Each rung that is available here must produce the same pixels.
    for rung in [Backend::Shm, Backend::GetImage] {
        let mut forced = X11Capture::open(&config(display, Some(region), Some(rung)))
            .unwrap_or_else(|e| panic!("forced {rung:?} failed on Xvfb: {e}"));
        assert_eq!(forced.backend(), rung);
        let f = next_frame(&mut forced);
        assert!(
            cpu_bytes(&f)
                .as_chunks::<4>()
                .0
                .iter()
                .all(|px| *px == RED_BGRX),
            "{rung:?} region pixels are not red"
        );
    }

    // DRI3 is not provided by Xvfb, so a forced DRI3 must fail loudly rather than fall back.
    match X11Capture::open(&config(display, None, Some(Backend::Dri3))) {
        Err(e) => eprintln!("lumen-capture-x11: forced DRI3 refused as expected: {e}"),
        Ok(c) => eprintln!(
            "lumen-capture-x11: this Xvfb offers DRI3 ({:?})",
            c.backend()
        ),
    }
}

#[test]
fn rejects_region_outside_root() {
    let Some(xvfb) = start_xvfb() else {
        return;
    };
    let outside = Rect {
        x: 700,
        y: 0,
        width: 200,
        height: 10,
    };
    let err = X11Capture::open(&config(&xvfb.display, Some(outside), None))
        .expect_err("region past the right edge must be refused");
    eprintln!("lumen-capture-x11: refused as expected: {err}");
}
