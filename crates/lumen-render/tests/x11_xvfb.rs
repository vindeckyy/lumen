//! Presents synthetic frames through the X11 backend on a private Xvfb at :98.

use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use lumen_protocol::media::{FrameSink, PixelFormat, VideoFrame};
use lumen_render::X11Renderer;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConfigureWindowAux, ConnectionExt as _, ImageFormat};
use x11rb::rust_connection::RustConnection;

const DISPLAY: &str = ":98";
/// The second display, so the two tests can run at the same time.
const OTHER_DISPLAY: &str = ":97";

/// Kills the Xvfb child when dropped.
struct Xvfb(Child);

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_xvfb(display: &str) -> Result<Xvfb, String> {
    let mut child = Command::new("Xvfb")
        .args([
            display,
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
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "Xvfb {display} exited early with {status}; display may be in use"
            ));
        }
        // The socket appears before the server answers, so wait for a connection that
        // completes the handshake: a socket alone can still be a server that is going away.
        if RustConnection::connect(Some(display)).is_ok() {
            return Ok(Xvfb(child));
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("Xvfb {display} did not answer within 20 s"));
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

/// A picture whose colour says where it came from: blue is the row, green is the column.
fn gradient_frame(width: u32, height: u32, index: u64) -> VideoFrame {
    let mut data = vec![0u8; (width * height * 4) as usize];
    for (i, px) in data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let x = i % width as usize;
        let y = i / width as usize;
        px.copy_from_slice(&[(y % 256) as u8, (x % 256) as u8, 0, 0]);
    }
    VideoFrame::cpu(width, height, PixelFormat::Bgrx, data, 0, index)
}

/// Waits until a frame newer than `previous` was reported as shown.
fn wait_for_present(renderer: &mut X11Renderer, previous: Option<u64>) -> Option<u64> {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        renderer.poll_events().expect("poll X events");
        let now = renderer.last_present_ts_us();
        if now.is_some() && now != previous {
            return now;
        }
        sleep(Duration::from_millis(10));
    }
    None
}

/// Reads one pixel of the window from another connection: blue and green.
fn read_pixel(conn: &RustConnection, window: u32, x: u16, y: u16) -> (u8, u8) {
    let reply = conn
        .get_image(
            ImageFormat::Z_PIXMAP,
            xproto::Drawable::from(window),
            i16::try_from(x).expect("x fits"),
            i16::try_from(y).expect("y fits"),
            1,
            1,
            u32::MAX,
        )
        .expect("get_image")
        .reply()
        .expect("get_image reply");
    (reply.data[0], reply.data[1])
}

/// Asserts that a channel is close to the expected value.
fn assert_close(what: &str, got: u8, want: f64) {
    let want = want.clamp(0.0, 255.0);
    assert!(
        (f64::from(got) - want).abs() <= 2.0,
        "{what}: got {got}, want about {want}"
    );
}

#[test]
fn x11_present_increments_count_and_reports_timestamps() {
    let _xvfb = match start_xvfb(DISPLAY) {
        Ok(x) => x,
        Err(reason) => {
            eprintln!("SKIP x11_present_increments_count_and_reports_timestamps: {reason}");
            return;
        }
    };
    let mut renderer =
        X11Renderer::connect(Some(DISPLAY), "lumen-render test").expect("connect to Xvfb :98");
    assert_eq!(renderer.present_count(), 0);
    // The full-screen size exercises the unredirect path; the smaller one is scaled into the
    // same window.
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

#[test]
fn x11_the_picture_is_scaled_and_placed_in_the_window() {
    let _xvfb = match start_xvfb(OTHER_DISPLAY) {
        Ok(x) => x,
        Err(reason) => {
            eprintln!("SKIP x11_the_picture_is_scaled_and_placed_in_the_window: {reason}");
            return;
        }
    };
    let mut renderer = X11Renderer::connect(Some(OTHER_DISPLAY), "lumen-render test")
        .expect("connect to Xvfb :97");
    let (reader, _screen) =
        RustConnection::connect(Some(OTHER_DISPLAY)).expect("connect a reader to Xvfb :97");

    // The picture fits the screen, so the window opens at the picture's size and shows it 1:1.
    renderer
        .present(&gradient_frame(1280, 720, 0))
        .expect("present the gradient");
    let window = renderer
        .window_id()
        .load(std::sync::atomic::Ordering::Acquire);
    assert_ne!(window, 0, "the window has an id after the first frame");
    assert_eq!(renderer.window_pixels(), Some((1280, 720)));
    let mut shown = wait_for_present(&mut renderer, None).expect("the frame was shown");
    sleep(Duration::from_millis(50));
    let (blue, green) = read_pixel(&reader, window, 100, 100);
    assert_close("blue of the pixel at 100,100", blue, 100.0);
    assert_close("green of the pixel at 100,100", green, 100.0);

    // A window that is not the shape of the picture gets bars, and the picture is scaled.
    reader
        .configure_window(window, &ConfigureWindowAux::new().width(800).height(600))
        .expect("resize the window");
    reader.flush().expect("flush the resize");
    renderer
        .present(&gradient_frame(1280, 720, 1))
        .expect("present the gradient again");
    let placement = renderer.placement().expect("the picture has a placement");
    assert_eq!(
        placement.dst,
        [0, 75, 800, 450],
        "the picture is letterboxed"
    );
    wait_for_present(&mut renderer, Some(shown)).expect("the second frame was shown");
    sleep(Duration::from_millis(50));

    // The picture is scaled by 800/1280 and sits 75 pixels below the top of the window.
    let (blue, green) = read_pixel(&reader, window, 100, 100);
    assert_close("blue of the pixel at 100,100", blue, (100.0 - 75.0) * 1.6);
    assert_close("green of the pixel at 100,100", green, 100.0 * 1.6);

    // The bar above the picture is black.
    let (blue, green) = read_pixel(&reader, window, 100, 30);
    assert_close("blue of the pixel in the bar", blue, 0.0);
    assert_close("green of the pixel in the bar", green, 0.0);

    // The bottom of the picture is the bottom of the source picture.
    let (blue, green) = read_pixel(&reader, window, 100, 500);
    assert_close(
        "blue of the pixel at 100,500",
        blue,
        ((500.0 - 75.0) * 1.6) % 256.0,
    );
    assert_close("green of the pixel at 100,500", green, 100.0 * 1.6);

    // A cropped picture shows the middle of the source, so the source offset is not zero.
    renderer
        .set_fit(lumen_render::VideoFit::Crop)
        .expect("set the crop fit");
    renderer
        .present(&gradient_frame(1280, 720, 2))
        .expect("present the cropped gradient");
    let placement = renderer.placement().expect("the picture has a placement");
    assert!(placement.src[0] > 0.0, "the crop starts inside the source");
    shown = wait_for_present(&mut renderer, Some(shown)).expect("the cropped frame was shown");
    sleep(Duration::from_millis(50));
    let scale = placement.src[2] / f64::from(placement.dst[2]);
    let (blue, green) = read_pixel(&reader, window, 100, 100);
    assert_close(
        "blue of the pixel at 100,100",
        blue,
        (placement.src[1] + 100.0 * scale) % 256.0,
    );
    assert_close(
        "green of the pixel at 100,100",
        green,
        (placement.src[0] + 100.0 * scale) % 256.0,
    );

    // The HUD rides in a child window and does not disturb the picture.
    renderer
        .set_overlay(&["fps 60".to_owned(), "rtt 3 ms".to_owned()])
        .expect("show the HUD");
    renderer
        .present(&gradient_frame(1280, 720, 3))
        .expect("present with the HUD");
    wait_for_present(&mut renderer, Some(shown)).expect("the frame with the HUD was shown");
    sleep(Duration::from_millis(50));
    let (blue, green) = read_pixel(&reader, window, 700, 500);
    assert_close(
        "blue of the pixel at 700,500",
        blue,
        (placement.src[1] + 500.0 * scale) % 256.0,
    );
    assert_close(
        "green of the pixel at 700,500",
        green,
        (placement.src[0] + 700.0 * scale) % 256.0,
    );
    renderer.set_overlay(&[]).expect("hide the HUD");
    renderer
        .present(&gradient_frame(1280, 720, 4))
        .expect("present without the HUD");
    assert_eq!(renderer.present_count(), 5);
}
