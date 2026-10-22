//! Monitor enumeration.
//!
//! The host lists the outputs it can capture and sends the list to the
//! client over the control channel, one [`ControlMessage::Monitor`] per
//! output. The client shows the list and picks one.
//!
//! Two enumerations exist, one per display server:
//!
//! * Wayland: the `wl_output` globals the compositor announces, which the
//!   capture crate already knows how to read.
//! * X11: the RandR outputs and the mode of the CRTC each output drives.
//!
//! The list is a list of *outputs*, not of modes: one output is one
//! capturable surface.

use lumen_capture_wlr;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::randr;

/// One capturable output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorInfo {
    /// The index of the output in the enumeration, counting from zero.
    pub index: u32,
    /// The connector name, such as `DP-1` or `HEADLESS-1`.
    pub name: String,
    /// The width of the current mode in pixels.
    pub width: u32,
    /// The height of the current mode in pixels.
    pub height: u32,
    /// The refresh rate of the current mode in hertz.
    pub refresh_hz: u32,
    /// Whether the output is the primary one.
    pub primary: bool,
}

impl MonitorInfo {
    /// The name and the size, as a single line for a report.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "{} {}x{}@{}",
            self.name, self.width, self.height, self.refresh_hz
        )
    }
}

/// Which display server to enumerate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorSource {
    /// A Wayland compositor.
    Wayland,
    /// An X11 server.
    X11,
}

/// Errors of the enumeration.
#[derive(Debug, thiserror::Error)]
pub enum MonitorError {
    /// The Wayland compositor could not be reached.
    #[error("cannot list the Wayland outputs: {0}")]
    Wayland(#[from] lumen_capture_wlr::Error),
    /// The X server could not be reached.
    #[error("cannot connect to the X display: {0}")]
    Connect(#[from] x11rb::errors::ConnectError),
    /// The X connection failed.
    #[error("X connection: {0}")]
    Connection(#[from] x11rb::errors::ConnectionError),
    /// An X request failed.
    #[error("X request: {0}")]
    Reply(#[from] x11rb::errors::ReplyError),
    /// An X resource id could not be allocated.
    #[error("X resource id: {0}")]
    Id(#[from] x11rb::errors::ReplyOrIdError),
    /// The X server has no RandR extension.
    #[error("the X server does not offer RandR")]
    NoRandr,
    /// The X server has no such screen.
    #[error("the X display has no screen {0}")]
    NoScreen(usize),
}

/// The outputs of a Wayland compositor.
///
/// The outputs keep the order the compositor announced them in, so the index
/// of an output is stable for as long as the compositor does not change.
pub fn list_wayland() -> Result<Vec<MonitorInfo>, MonitorError> {
    let outputs = lumen_capture_wlr::list_outputs()?;
    Ok(outputs
        .into_iter()
        .enumerate()
        .map(|(index, output)| MonitorInfo {
            index: u32::try_from(index).unwrap_or(u32::MAX),
            name: output.name.unwrap_or_else(|| format!("output-{index}")),
            width: output.width,
            height: output.height,
            refresh_hz: output.refresh_mhz / 1000,
            primary: false,
        })
        .collect())
}

/// The outputs of an X server, with the mode of each one.
///
/// `display` is `None` for the display named by `DISPLAY`.
pub fn list_x11(display: Option<&str>) -> Result<Vec<MonitorInfo>, MonitorError> {
    let (conn, screen_num) = x11rb::connect(display)?;
    let root = conn
        .setup()
        .roots
        .get(screen_num)
        .map(|screen| screen.root)
        .ok_or(MonitorError::NoScreen(screen_num))?;
    if conn
        .extension_information(randr::X11_EXTENSION_NAME)?
        .is_none()
    {
        return Err(MonitorError::NoRandr);
    }
    randr::query_version(&conn, 1, 5)?.reply()?;

    let resources = randr::get_screen_resources_current(&conn, root)?.reply()?;
    let primary = randr::get_output_primary(&conn, root)?
        .reply()
        .ok()
        .map(|r| r.output);

    let mut monitors = Vec::new();
    for output in &resources.outputs {
        let info = randr::get_output_info(&conn, *output, resources.config_timestamp)?.reply()?;
        if info.crtc == 0 {
            // The output drives no CRTC, so it has no mode and cannot be
            // captured.
            continue;
        }
        let crtc = randr::get_crtc_info(&conn, info.crtc, resources.config_timestamp)?.reply()?;
        if crtc.mode == 0 || crtc.width == 0 || crtc.height == 0 {
            continue;
        }
        let mode = resources
            .modes
            .iter()
            .find(|mode| mode.id == crtc.mode)
            .ok_or(MonitorError::NoRandr)?;
        let refresh_hz = refresh_from_mode(
            mode.dot_clock,
            u32::from(mode.htotal),
            u32::from(mode.vtotal),
            u64::from(u32::from(mode.mode_flags)),
        );
        monitors.push(MonitorInfo {
            index: 0,
            name: String::from_utf8_lossy(&info.name).into_owned(),
            width: u32::from(crtc.width),
            height: u32::from(crtc.height),
            refresh_hz,
            primary: primary == Some(*output),
        });
    }
    for (index, monitor) in monitors.iter_mut().enumerate() {
        monitor.index = u32::try_from(index).unwrap_or(u32::MAX);
    }
    Ok(monitors)
}

/// The refresh rate of a RandR mode, in hertz.
///
/// The mode carries a dot clock and the total pixel counts of a frame. The
/// refresh is the dot clock divided by the pixels of a frame. A mode with a
/// double scan or an interlace flag halves the frame rate.
#[must_use]
pub fn refresh_from_mode(dot_clock_khz: u32, h_total: u32, v_total: u32, mode_flags: u64) -> u32 {
    if dot_clock_khz == 0 || h_total == 0 || v_total == 0 {
        return 0;
    }
    let pixels = u64::from(h_total) * u64::from(v_total);
    let mut refresh = u64::from(dot_clock_khz) * 1000 / pixels;
    // `double_scan` and `interlace` both halve the number of frames a second.
    let double_scan = 1u64 << 5;
    let interlace = 1u64 << 4;
    if mode_flags & (double_scan | interlace) != 0 {
        refresh /= 2;
    }
    u32::try_from(refresh).unwrap_or(u32::MAX)
}

/// The outputs of a display server. `x11_display` names the X display (`None` reads
/// `$DISPLAY`); it is ignored for Wayland.
pub fn list(
    source: MonitorSource,
    x11_display: Option<&str>,
) -> Result<Vec<MonitorInfo>, MonitorError> {
    match source {
        MonitorSource::Wayland => list_wayland(),
        MonitorSource::X11 => list_x11(x11_display),
    }
}

/// Find an output by name, or by index when the name is a number.
///
/// The host takes an `--output` flag that names a connector. A number is
/// accepted as well, so a script can pick the second output of a machine
/// without knowing its connector name.
pub fn find<'a>(monitors: &'a [MonitorInfo], wanted: &str) -> Option<&'a MonitorInfo> {
    monitors
        .iter()
        .find(|monitor| monitor.name == wanted)
        .or_else(|| {
            wanted
                .parse::<u32>()
                .ok()
                .and_then(|index| monitors.iter().find(|monitor| monitor.index == index))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_monitor_describes_itself_in_one_line() {
        let monitor = MonitorInfo {
            index: 0,
            name: "DP-1".to_string(),
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            primary: true,
        };
        assert_eq!(monitor.describe(), "DP-1 1920x1080@60");
    }

    #[test]
    fn the_refresh_follows_the_dot_clock() {
        // The usual 1920x1080 at 60 Hz: a dot clock of 148.5 MHz, which X
        // reports in kilohertz, over 2200 x 1125 pixels of a frame.
        assert_eq!(refresh_from_mode(148_500, 2200, 1125, 0), 60);
        // A mode with no dot clock has no refresh rate.
        assert_eq!(refresh_from_mode(0, 2200, 1125, 0), 0);
        assert_eq!(refresh_from_mode(148_500, 0, 1125, 0), 0);
    }

    #[test]
    fn an_interlaced_or_double_scanned_mode_halves_the_refresh() {
        let double_scan = 1u64 << 5;
        let interlace = 1u64 << 4;
        assert_eq!(refresh_from_mode(148_500, 2200, 1125, double_scan), 30);
        assert_eq!(refresh_from_mode(148_500, 2200, 1125, interlace), 30);
        // The flag must not disturb a progressive mode.
        assert_eq!(refresh_from_mode(148_500, 2200, 1125, 1 << 0), 60);
    }

    #[test]
    fn an_output_is_found_by_name_or_by_index() {
        let monitors = vec![
            MonitorInfo {
                index: 0,
                name: "eDP-1".to_string(),
                width: 1920,
                height: 1080,
                refresh_hz: 60,
                primary: true,
            },
            MonitorInfo {
                index: 1,
                name: "HDMI-A-1".to_string(),
                width: 2560,
                height: 1440,
                refresh_hz: 120,
                primary: false,
            },
        ];
        assert_eq!(find(&monitors, "HDMI-A-1").unwrap().index, 1);
        assert_eq!(find(&monitors, "1").unwrap().name, "HDMI-A-1");
        assert!(find(&monitors, "DP-9").is_none());
        assert!(find(&monitors, "7").is_none());
    }

    #[test]
    fn the_wayland_enumeration_finds_the_outputs_of_the_compositor() {
        // A compositor that is not running is a normal state for a host that
        // is only being asked about its X11 displays.
        match list_wayland() {
            Ok(monitors) => {
                for (index, monitor) in monitors.iter().enumerate() {
                    assert_eq!(monitor.index, u32::try_from(index).unwrap());
                    assert!(!monitor.name.is_empty(), "an output has a name");
                    assert!(monitor.width > 0, "an output has a size");
                    assert!(monitor.height > 0, "an output has a size");
                }
            }
            Err(reason) => {
                crate::host_log!(info
                    "SKIP the_wayland_enumeration_finds_the_outputs_of_the_compositor: {reason}"
                );
            }
        }
    }
}
