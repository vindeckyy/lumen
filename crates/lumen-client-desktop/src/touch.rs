//! The client's touchscreens and pens, read as evdev devices.
//!
//! A contact is a slot the kernel fills: the client sends the contact's phase and its position,
//! and the host injects them into a virtual touch device. A pen is one device with a position and
//! a pressure. The positions are a fraction of the device's own range, so a touch on the
//! client's screen lands where the finger is on the host whatever picture the client shows.
//!
//! The host's own virtual devices are skipped: a client on the host's machine must not send them
//! back.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use evdev::{AbsoluteAxisCode, Device, EventType, InputEvent, KeyCode};
use lumen_protocol::input::{InputAction, TouchPhase};
use lumen_transport::input_channel::InputSender;
use rustix::event::{PollFd, PollFlags, Timespec, poll};

/// How often the client looks for devices that were plugged in.
const SCAN: Duration = Duration::from_secs(2);
/// The range of every axis a device reports, by axis code.
type Ranges = HashMap<u16, (i32, i32)>;
/// The name the host's own virtual devices carry.
const VIRTUAL: &str = "Lumen virtual";

/// What kind of device it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A screen with contacts.
    Screen,
    /// A pen.
    Pen,
}

/// One touch device of this machine.
struct Touch {
    device: Device,
    ranges: Ranges,
    path: PathBuf,
    kind: Kind,
    /// The contact the kernel is filling, and whether it is down.
    slot: u8,
    down: bool,
    /// The position of the contact, in the device's own pixels.
    x: f64,
    y: f64,
    /// The pen's pressure, and whether the pen is near the surface.
    pressure: u16,
    in_range: bool,
}

/// Starts the touch thread. It sends through `sender`, the input socket's, and stops when
/// `running` goes false. The positions are scaled into the picture `ladder` is on.
pub fn spawn(sender: Arc<Mutex<InputSender>>, running: Arc<AtomicBool>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut devices: Vec<Touch> = Vec::new();
        let mut next_scan = Instant::now();
        while running.load(Ordering::SeqCst) {
            if Instant::now() >= next_scan {
                scan(&mut devices);
                next_scan = Instant::now() + SCAN;
            }
            let ready = wait_readable(&devices, Duration::from_millis(100));
            let mut gone = Vec::new();
            for (index, device) in devices.iter_mut().enumerate() {
                if !ready.contains(&index) {
                    continue;
                }
                // The events are collected first, because handling one needs the device again.
                let events: Vec<InputEvent> = match device.device.fetch_events() {
                    Ok(events) => events.collect(),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(_) => {
                        gone.push(index);
                        continue;
                    }
                };
                for event in events {
                    handle(event, device, &sender);
                }
            }
            for index in gone.into_iter().rev() {
                let device = devices.remove(index);
                eprintln!("lumen-client: touch device {} left", device.path.display());
            }
        }
    })
}

/// Sends what one event says about the device.
fn handle(event: InputEvent, device: &mut Touch, sender: &Mutex<InputSender>) {
    match event.event_type() {
        EventType::ABSOLUTE => {
            let value = f64::from(event.value());
            match event.code() {
                code if code == AbsoluteAxisCode::ABS_MT_SLOT.0 => {
                    device.slot = event.value().clamp(0, 255) as u8;
                }
                code if code == AbsoluteAxisCode::ABS_MT_TRACKING_ID.0 => {
                    if event.value() < 0 {
                        device.down = false;
                        send_touch(device, TouchPhase::Up, sender);
                    } else {
                        device.down = true;
                        send_touch(device, TouchPhase::Down, sender);
                    }
                }
                code if code == AbsoluteAxisCode::ABS_MT_POSITION_X.0 => {
                    device.x = value;
                    if device.down {
                        send_touch(device, TouchPhase::Motion, sender);
                    }
                }
                code if code == AbsoluteAxisCode::ABS_MT_POSITION_Y.0 => {
                    device.y = value;
                    if device.down {
                        send_touch(device, TouchPhase::Motion, sender);
                    }
                }
                code if code == AbsoluteAxisCode::ABS_X.0 => {
                    device.x = value;
                    send_pen(device, sender);
                }
                code if code == AbsoluteAxisCode::ABS_Y.0 => {
                    device.y = value;
                    send_pen(device, sender);
                }
                code if code == AbsoluteAxisCode::ABS_PRESSURE.0 => {
                    device.pressure = event.value().clamp(0, 4095) as u16;
                    send_pen(device, sender);
                }
                _ => {}
            }
        }
        EventType::KEY if event.code() == KeyCode::BTN_TOOL_PEN.0 => {
            device.in_range = event.value() != 0;
            send_pen(device, sender);
        }
        _ => {}
    }
}

/// Sends one contact of a screen.
fn send_touch(device: &Touch, phase: TouchPhase, sender: &Mutex<InputSender>) {
    let (x, y) = screen_fraction(device);
    send(
        sender,
        InputAction::Touch {
            slot: device.slot,
            phase,
            x,
            y,
        },
    );
}

/// Sends one sample of a pen.
fn send_pen(device: &Touch, sender: &Mutex<InputSender>) {
    if device.kind != Kind::Pen {
        return;
    }
    let (x, y) = screen_fraction(device);
    send(
        sender,
        InputAction::Pen {
            x,
            y,
            pressure: device.pressure,
            in_range: device.in_range,
        },
    );
}

/// The position as a fraction of the device's own range: 0 is its left edge and 1 its right.
/// A screen reports its contacts on the multitouch axes, a pen on `ABS_X` and `ABS_Y`.
fn screen_fraction(device: &Touch) -> (f64, f64) {
    let (x_axis, y_axis) = if device.kind == Kind::Pen {
        (AbsoluteAxisCode::ABS_X, AbsoluteAxisCode::ABS_Y)
    } else {
        (
            AbsoluteAxisCode::ABS_MT_POSITION_X,
            AbsoluteAxisCode::ABS_MT_POSITION_Y,
        )
    };
    (
        fraction(device.x, device.ranges.get(&x_axis.0)),
        fraction(device.y, device.ranges.get(&y_axis.0)),
    )
}

/// One device coordinate as a fraction of its range.
fn fraction(value: f64, range: Option<&(i32, i32)>) -> f64 {
    let Some(&(min, max)) = range else {
        return 0.0;
    };
    if max <= min {
        return 0.0;
    }
    ((value - f64::from(min)) / f64::from(max - min)).clamp(0.0, 1.0)
}

fn send(sender: &Mutex<InputSender>, action: InputAction) {
    if let Ok(mut sender) = sender.lock()
        && let Err(e) = sender.send(action, crate::receive::monotonic_us())
    {
        eprintln!("lumen-client: touch send failed: {e}");
    }
}

/// Opens the touch devices that are not tracked yet.
fn scan(devices: &mut Vec<Touch>) {
    let known: HashSet<PathBuf> = devices.iter().map(|d| d.path.clone()).collect();
    for (path, device) in evdev::enumerate() {
        if known.contains(&path) {
            continue;
        }
        let name = device.name().unwrap_or_default();
        if name.starts_with(VIRTUAL) {
            continue;
        }
        let Some(kind) = kind_of(&device) else {
            continue;
        };
        if device.set_nonblocking(true).is_err() {
            continue;
        }
        let Ok(ranges) = device
            .get_absinfo()
            .map(|axes| axes.map(|(code, info)| (code.0, (info.minimum(), info.maximum()))))
            .map(Iterator::collect::<Ranges>)
        else {
            continue;
        };
        eprintln!(
            "lumen-client: touch device {name} ({}) is read",
            path.display()
        );
        devices.push(Touch {
            device,
            ranges,
            path,
            kind,
            slot: 0,
            down: false,
            x: 0.0,
            y: 0.0,
            pressure: 0,
            in_range: false,
        });
    }
}

/// The kind of touch device, or `None` when the device is not one.
fn kind_of(device: &Device) -> Option<Kind> {
    let axes = device.supported_absolute_axes()?;
    let keys = device.supported_keys()?;
    if axes.contains(AbsoluteAxisCode::ABS_MT_TRACKING_ID)
        && axes.contains(AbsoluteAxisCode::ABS_MT_POSITION_X)
    {
        return Some(Kind::Screen);
    }
    if axes.contains(AbsoluteAxisCode::ABS_PRESSURE)
        && (keys.contains(KeyCode::BTN_TOOL_PEN) || keys.contains(KeyCode::BTN_STYLUS))
    {
        return Some(Kind::Pen);
    }
    None
}

/// The indices of the devices with events waiting, after at most `wait`.
fn wait_readable(devices: &[Touch], wait: Duration) -> Vec<usize> {
    if devices.is_empty() {
        thread::sleep(wait);
        return Vec::new();
    }
    let mut fds: Vec<PollFd<'_>> = devices
        .iter()
        .map(|d| PollFd::new(&d.device, PollFlags::IN))
        .collect();
    let timeout = Timespec {
        tv_sec: 0,
        tv_nsec: i64::from(wait.subsec_nanos()),
    };
    if poll(&mut fds, Some(&timeout)).is_err() {
        return Vec::new();
    }
    fds.iter()
        .enumerate()
        .filter(|(_, fd)| !fd.revents().is_empty())
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_coordinate_becomes_a_fraction_of_its_range() {
        let range = (0, 1080);
        assert_eq!(fraction(0.0, Some(&range)), 0.0);
        assert_eq!(fraction(1080.0, Some(&range)), 1.0);
        assert_eq!(fraction(540.0, Some(&range)), 0.5);
        assert_eq!(fraction(5000.0, Some(&range)), 1.0, "out of range clamps");
        assert_eq!(fraction(-5.0, Some(&range)), 0.0, "out of range clamps");
        assert_eq!(fraction(10.0, None), 0.0, "no range, no position");
        assert_eq!(fraction(10.0, Some(&(5, 5))), 0.0, "a flat range");
    }
}
