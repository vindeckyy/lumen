//! The `lumen-x11-source` tool: a moving picture on an X11 screen.
//!
//! The X11 capture is gated by XDamage, so a screen that never changes
//! produces no frames at all. This tool paints the root window of an X
//! display over and over again, at a fixed rate, so the capture has
//! something to capture and the end-to-end run of the X11 path has frames
//! to measure.
//!
//! It is a test tool, not part of the product.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _, CreateGCAux, Rectangle};

/// Usage text for the command line.
const USAGE: &str = "\
lumen-x11-source - paint an X11 screen so the capture has something to capture

Usage: lumen-x11-source [options]

Options:
  --display <name>  The X display to paint, e.g. :77 (default $DISPLAY)
  --fps <n>         How many times a second the screen is painted (default 30)
  --duration <s>    Stop after this many seconds (default: run until killed)
  -h, --help        Print this text
";

/// The settings of the tool.
#[derive(Debug)]
struct Settings {
    /// The display to paint.
    display: Option<String>,
    /// How many times a second the screen is painted.
    fps: u32,
    /// How long the tool runs.
    duration: Option<Duration>,
}

/// The errors of the tool.
#[derive(Debug)]
enum Error {
    /// The display could not be reached.
    Connect(x11rb::errors::ConnectError),
    /// An X request failed.
    Request(x11rb::errors::ConnectionError),
    /// The X server refused a request.
    Refused(x11rb::errors::ReplyError),
    /// An X resource id could not be allocated.
    Id(x11rb::errors::ReplyOrIdError),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect(e) => write!(f, "cannot connect to the X display: {e}"),
            Self::Request(e) => write!(f, "X connection: {e}"),
            Self::Refused(e) => write!(f, "X request: {e}"),
            Self::Id(e) => write!(f, "X resource id: {e}"),
        }
    }
}

/// Parse the command line.
fn parse(args: &[String]) -> Result<Settings, String> {
    let mut settings = Settings {
        display: None,
        fps: 30,
        duration: None,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |flag: &str| -> Result<String, String> {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match arg.as_str() {
            "--display" => settings.display = Some(value("--display")?),
            "--fps" => {
                settings.fps = value("--fps")?.parse().map_err(|e| format!("--fps: {e}"))?;
                if settings.fps == 0 {
                    return Err("--fps must be at least 1".to_string());
                }
            }
            "--duration" => {
                let seconds: f64 = value("--duration")?
                    .parse()
                    .map_err(|e| format!("--duration: {e}"))?;
                settings.duration = Some(Duration::from_secs_f64(seconds));
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(settings)
}

/// Paint the root window of the display until the duration is over.
fn run(settings: &Settings) -> Result<(), Error> {
    let (conn, screen_num) = x11rb::connect(settings.display.as_deref()).map_err(Error::Connect)?;
    let (root, width, height) = {
        let screen = conn
            .setup()
            .roots
            .get(screen_num)
            .ok_or(Error::Id(x11rb::errors::ReplyOrIdError::IdsExhausted))?;
        (
            screen.root,
            u32::from(screen.width_in_pixels),
            u32::from(screen.height_in_pixels),
        )
    };
    let gc = conn.generate_id().map_err(Error::Id)?;
    conn.create_gc(gc, root, &CreateGCAux::new())
        .map_err(Error::Request)?
        .check()
        .map_err(Error::Refused)?;

    let interval = Duration::from_secs_f64(1.0 / f64::from(settings.fps.max(1)));
    let deadline = settings.duration.map(|d| Instant::now() + d);
    let mut index = 0u32;
    loop {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        // A colour that changes every time, so every paint is a change the
        // damage extension reports.
        let colour = index & 0x00ff_ffff;
        conn.change_gc(gc, &xproto::ChangeGCAux::new().foreground(colour))
            .map_err(Error::Request)?
            .check()
            .map_err(Error::Refused)?;
        conn.poly_fill_rectangle(
            root,
            gc,
            &[Rectangle {
                x: 0,
                y: 0,
                width: u16::try_from(width).unwrap_or(u16::MAX),
                height: u16::try_from(height).unwrap_or(u16::MAX),
            }],
        )
        .map_err(Error::Request)?
        .check()
        .map_err(Error::Refused)?;
        conn.flush().map_err(Error::Request)?;
        index = index.wrapping_add(0x0001_0000);
        std::thread::sleep(interval);
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let settings = match parse(&args) {
        Ok(settings) => settings,
        Err(reason) => {
            eprintln!("lumen-x11-source: {reason}");
            print!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(&settings) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lumen-x11-source: {e}");
            ExitCode::FAILURE
        }
    }
}
