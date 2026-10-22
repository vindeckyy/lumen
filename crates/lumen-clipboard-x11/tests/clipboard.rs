//! The X11 clipboard against a real X server started as `Xvfb`.
//!
//! Skips with a printed reason when Xvfb cannot be started.

use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use lumen_clipboard_x11::{Selection, X11Clipboard};

struct Xvfb {
    child: Child,
    display: String,
    /// The read end of the server's stdout pipe, kept open for the server's lifetime.
    _stdout: BufReader<ChildStdout>,
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Tests in this binary run in parallel; starting two servers at once corrupts both.
static START: Mutex<()> = Mutex::new(());

/// Starts `Xvfb -displayfd 1`. The server picks a free display number itself.
fn start_xvfb() -> Option<Xvfb> {
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
            eprintln!("SKIP lumen-capture-x11 clipboard: cannot start Xvfb: {e}");
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
        eprintln!("SKIP lumen-capture-x11 clipboard: Xvfb exited without a display number");
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let display = format!(":{number}");
    if x11rb::connect(Some(display.as_str())).is_err() {
        eprintln!("SKIP lumen-capture-x11 clipboard: Xvfb reports {display} but refuses clients");
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    Some(Xvfb {
        child,
        display,
        _stdout: stdout,
    })
}

#[test]
fn the_clipboard_round_trips_through_the_selection() {
    let Some(xvfb) = start_xvfb() else {
        return;
    };
    let mut clipboard = X11Clipboard::open(Some(&xvfb.display)).expect("open the clipboard");

    // Nothing owns the selection yet, so it reads as empty rather than failing.
    let empty = clipboard
        .read(Selection::Clipboard, Duration::from_secs(5))
        .expect("read an empty selection");
    assert!(empty.is_none(), "an unowned selection must read as empty");

    let text = "lumen clipboard \u{1F600}";
    clipboard
        .set(Selection::Clipboard, text)
        .expect("take ownership of the clipboard");

    let read_back = clipboard
        .read(Selection::Clipboard, Duration::from_secs(5))
        .expect("read the clipboard back");
    assert_eq!(read_back.as_deref(), Some(text));

    // The PRIMARY selection is a different selection and stays empty.
    let primary = clipboard
        .read(Selection::Primary, Duration::from_secs(5))
        .expect("read the primary selection");
    assert!(primary.is_none(), "the primary selection was never set");
}

#[test]
fn an_oversized_payload_is_refused() {
    let Some(xvfb) = start_xvfb() else {
        return;
    };
    let mut clipboard = X11Clipboard::open(Some(&xvfb.display)).expect("open the clipboard");
    let big = "x".repeat(lumen_clipboard_x11::MAX_CLIPBOARD_BYTES + 1);
    let error = clipboard
        .set(Selection::Clipboard, &big)
        .expect_err("the payload is over the cap");
    assert!(
        error.to_string().contains("limit"),
        "the error must name the limit: {error}"
    );
}
