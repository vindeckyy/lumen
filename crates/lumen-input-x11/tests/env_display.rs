//! Reads raw XI2 events from the display named by `$DISPLAY`.
//!
//! Skips with a printed reason when `DISPLAY` is unset. The events are made
//! with XTEST, so the test needs a display that has the XTEST extension, which
//! every X server has.
//!
//! The checks are exact: the movement of a relative pointer is the movement
//! that was asked for, and an X keycode of 38 is the evdev code 30.

use std::time::{Duration, Instant};

use lumen_input_x11::{RawConfig, RawEventSource};
use lumen_protocol::input::InputAction;
use x11rb::connection::Connection;
use x11rb::protocol::xproto;
use x11rb::protocol::xtest;
use x11rb::rust_connection::RustConnection;

/// The X keycode of the `a` key on a standard layout.
const X_KEYCODE_A: u8 = 38;

/// The movement the test asks for.
const MOVE_X: i16 = 7;
const MOVE_Y: i16 = 3;

/// How long the test waits for the events it asked for.
const DEADLINE: Duration = Duration::from_secs(5);

/// The margin the movements are compared with. The values come out of a
/// 32.32 fixed point number, so they are exact, but the margin keeps the
/// comparison honest.
const MARGIN: f64 = 0.001;

/// The root window of the display.
fn root(conn: &RustConnection) -> xproto::Window {
    conn.setup().roots.first().expect("the first screen").root
}

/// Fakes a relative pointer movement with XTEST.
fn fake_move(conn: &RustConnection, root: xproto::Window, dx: i16, dy: i16) {
    xtest::fake_input(conn, 6, 1, 0, root, dx, dy, 0).expect("fake motion");
    conn.flush().expect("flush the motion");
}

/// Fakes a key press and release with XTEST.
fn fake_key(conn: &RustConnection, root: xproto::Window, keycode: u8) {
    xtest::fake_input(conn, 2, keycode, 0, root, 0, 0, 0).expect("fake key press");
    conn.flush().expect("flush the key press");
    xtest::fake_input(conn, 3, keycode, 0, root, 0, 0, 0).expect("fake key release");
    conn.flush().expect("flush the key release");
}

/// The next action of the source, or `None` when the timeout passed.
fn next_action(source: &mut RawEventSource) -> Option<InputAction> {
    match source.poll_action(Duration::from_millis(100)) {
        Ok(action) => action,
        Err(e) => panic!("polling the raw events failed: {e}"),
    }
}

#[test]
fn raw_events_of_the_display_reach_the_source() {
    let Ok(display) = std::env::var("DISPLAY") else {
        eprintln!("SKIP lumen-input-x11 env_display: DISPLAY is unset");
        return;
    };
    let (conn, _) = RustConnection::connect(None).expect("a second X connection");
    let root = root(&conn);

    // The movement of a relative device is reported as it is, so the source is
    // opened for relative motion.
    let mut source = RawEventSource::open(&RawConfig {
        display: Some(display),
        absolute_motion: false,
        drop_unmapped_keys: false,
        ..RawConfig::default()
    })
    .expect("open the raw event source on $DISPLAY");

    fake_move(&conn, root, MOVE_X, MOVE_Y);

    let want_x = f64::from(MOVE_X);
    let want_y = f64::from(MOVE_Y);
    let mut total_x = 0.0f64;
    let mut total_y = 0.0f64;
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline
        && ((total_x - want_x).abs() > MARGIN || (total_y - want_y).abs() > MARGIN)
    {
        if let Some(InputAction::MotionRelative { dx, dy }) = next_action(&mut source) {
            total_x += dx;
            total_y += dy;
        }
    }
    assert!(
        (total_x - want_x).abs() < MARGIN,
        "the display reported {total_x} pixels of movement, expected {want_x}"
    );
    assert!(
        (total_y - want_y).abs() < MARGIN,
        "the display reported {total_y} pixels of movement, expected {want_y}"
    );

    fake_key(&conn, root, X_KEYCODE_A);

    let mut pressed = None;
    let mut released = None;
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline && released.is_none() {
        if let Some(InputAction::Key {
            keycode,
            pressed: down,
        }) = next_action(&mut source)
        {
            if down {
                pressed = Some(keycode);
            } else {
                released = Some(keycode);
            }
        }
    }
    assert_eq!(pressed, Some(30), "the evdev code of the X keycode 38");
    assert_eq!(released, Some(30), "the evdev code of the X keycode 38");
}
