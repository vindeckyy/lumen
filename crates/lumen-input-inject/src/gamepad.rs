//! Virtual gamepads: one DualSense per client pad, made through `/dev/uhid`.
//!
//! The kernel's `hid-playstation` driver binds on the Sony vendor and product ids and the USB
//! report descriptor below, so games, SDL and Steam see an ordinary wired DualSense. The driver
//! asks for three feature reports while it probes (calibration `0x05`, pairing `0x09`, firmware
//! `0x20`) and waits for the answers, so every pad has a thread that answers the kernel. Input
//! report `0x01` carries the state.
//!
//! Opening `/dev/uhid` needs the `uhid` rule of `packaging/udev/60-lumen-input.rules`.
//!
//! The uhid event layout follows `linux/uhid.h` (`struct uhid_event` is packed). The report
//! descriptor and the feature blobs come from punktfunk (`pf-driver-proto/src/dualsense.rs`,
//! MIT OR Apache-2.0, Copyright (c) 2026 unom - Enrico Bühler), which took the descriptor from
//! inputtino's `ps5.hpp` (MIT).

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};
use std::time::Instant;

use lumen_protocol::input::{InputAction, MAX_PADS, pad};
use rustix::event::{PollFd, PollFlags, Timespec, poll};

const UHID_PATH: &str = "/dev/uhid";

/// How long a latched rumble may sit without the game driving the rumble plane before the
/// motors are cut. The kernel reports a rumble when the effect starts and does not report it
/// when the effect is erased, so a rumble that is never driven again would play forever. The
/// window is above SDL's ~2 s resend, so a rumble the game holds is not cut.
const RUMBLE_IDLE: std::time::Duration = std::time::Duration::from_millis(2500);
/// How often a held rumble is renewed for the client, so the client keeps playing it and
/// notices a link that died.
const RUMBLE_RENEW: std::time::Duration = std::time::Duration::from_millis(250);

const UHID_DESTROY: u32 = 1;
const UHID_GET_REPORT: u32 = 9;
const UHID_GET_REPORT_REPLY: u32 = 10;
const UHID_CREATE2: u32 = 11;
const UHID_INPUT2: u32 = 12;
const UHID_SET_REPORT: u32 = 13;
const UHID_SET_REPORT_REPLY: u32 = 14;

/// `sizeof(struct uhid_event)`: the type, then the union, whose largest member is
/// `uhid_create2_req` (name 128, phys 64, uniq 64, two `u16`, four `u32`, `rd_data` 4096).
const EVENT_LEN: usize = 4 + 4372;

const BUS_USB: u16 = 0x03;
const SONY: u32 = 0x054C;
const DUALSENSE: u32 = 0x0CE6;
const EIO: u16 = 5;

/// The start of every Lumen pad's MAC as `hid-playstation` writes it into the evdev `uniq`
/// (high octet first). A client on the same machine skips devices that carry it, so it never
/// sends a host pad back to the host. The MAC is locally administered, so it is no vendor's.
pub const PAD_MAC_PREFIX: &str = "02:4c:55:4d:00:";

/// USB DualSense report descriptor (inputtino `ps5.hpp`).
#[rustfmt::skip]
const RDESC: [u8; 273] = [
    0x05, 0x01, 0x09, 0x05, 0xA1, 0x01, 0x85, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x35,
    0x09, 0x33, 0x09, 0x34, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x06, 0x81, 0x02, 0x06,
    0x00, 0xFF, 0x09, 0x20, 0x95, 0x01, 0x81, 0x02, 0x05, 0x01, 0x09, 0x39, 0x15, 0x00, 0x25, 0x07,
    0x35, 0x00, 0x46, 0x3B, 0x01, 0x65, 0x14, 0x75, 0x04, 0x95, 0x01, 0x81, 0x42, 0x65, 0x00, 0x05,
    0x09, 0x19, 0x01, 0x29, 0x0F, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x0F, 0x81, 0x02, 0x06,
    0x00, 0xFF, 0x09, 0x21, 0x95, 0x0D, 0x81, 0x02, 0x06, 0x00, 0xFF, 0x09, 0x22, 0x15, 0x00, 0x26,
    0xFF, 0x00, 0x75, 0x08, 0x95, 0x34, 0x81, 0x02, 0x85, 0x02, 0x09, 0x23, 0x95, 0x2F, 0x91, 0x02,
    0x85, 0x05, 0x09, 0x33, 0x95, 0x28, 0xB1, 0x02, 0x85, 0x08, 0x09, 0x34, 0x95, 0x2F, 0xB1, 0x02,
    0x85, 0x09, 0x09, 0x24, 0x95, 0x13, 0xB1, 0x02, 0x85, 0x0A, 0x09, 0x25, 0x95, 0x1A, 0xB1, 0x02,
    0x85, 0x20, 0x09, 0x26, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x21, 0x09, 0x27, 0x95, 0x04, 0xB1, 0x02,
    0x85, 0x22, 0x09, 0x40, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x80, 0x09, 0x28, 0x95, 0x3F, 0xB1, 0x02,
    0x85, 0x81, 0x09, 0x29, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x82, 0x09, 0x2A, 0x95, 0x09, 0xB1, 0x02,
    0x85, 0x83, 0x09, 0x2B, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x84, 0x09, 0x2C, 0x95, 0x3F, 0xB1, 0x02,
    0x85, 0x85, 0x09, 0x2D, 0x95, 0x02, 0xB1, 0x02, 0x85, 0xA0, 0x09, 0x2E, 0x95, 0x01, 0xB1, 0x02,
    0x85, 0xE0, 0x09, 0x2F, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xF0, 0x09, 0x30, 0x95, 0x3F, 0xB1, 0x02,
    0x85, 0xF1, 0x09, 0x31, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xF2, 0x09, 0x32, 0x95, 0x0F, 0xB1, 0x02,
    0x85, 0xF4, 0x09, 0x35, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xF5, 0x09, 0x36, 0x95, 0x03, 0xB1, 0x02,
    0xC0,
];

/// Feature `0x05`, motion calibration: 20 LSB per degree per second and 10000 LSB per g.
#[rustfmt::skip]
const FEATURE_CALIBRATION: [u8; 41] = [
    0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x27, 0xF0, 0xD8, 0x10, 0x27, 0xF0, 0xD8, 0x10,
    0x27, 0xF0, 0xD8, 0xF4, 0x01, 0xF4, 0x01, 0x10, 0x27, 0xF0, 0xD8, 0x10, 0x27, 0xF0, 0xD8, 0x10,
    0x27, 0xF0, 0xD8, 0x0B, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// Feature `0x09`, pairing info: the MAC at bytes 1 to 6, low octet first. `02:4c:55:4d:00:00`
/// is locally administered; [`feature_report`] puts the pad index in the low octet.
#[rustfmt::skip]
const FEATURE_PAIRING: [u8; 20] = [
    0x09, 0x00, 0x00, 0x4D, 0x55, 0x4C, 0x02, 0x08, 0x25, 0x00, 0x1E, 0x00, 0xEE, 0x74, 0xD0, 0xBC,
    0x00, 0x00, 0x00, 0x00,
];

/// Feature `0x20`, firmware info, newer than Sony's shipping firmware so nothing offers an update.
#[rustfmt::skip]
const FEATURE_FIRMWARE: [u8; 64] = [
    0x20, 0x4A, 0x75, 0x6E, 0x20, 0x31, 0x39, 0x20, 0x32, 0x30, 0x32, 0x33, 0x31, 0x34, 0x3A, 0x34,
    0x37, 0x3A, 0x33, 0x34, 0x03, 0x00, 0x44, 0x00, 0x08, 0x02, 0x00, 0x01, 0x36, 0x00, 0x00, 0x01,
    0xC1, 0xC8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x99, 0x09, 0x00, 0x00,
    0x14, 0x00, 0x00, 0x00, 0x0B, 0x00, 0x01, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// The feature report the driver asked for, or `None` for one this pad does not have. The
/// pairing report gives every pad its own MAC, so SDL and Steam do not merge two pads.
fn feature_report(report: u8, index: u8) -> Option<Vec<u8>> {
    match report {
        0x05 => Some(FEATURE_CALIBRATION.to_vec()),
        0x09 => {
            let mut reply = FEATURE_PAIRING.to_vec();
            reply[1] = reply[1].wrapping_add(index);
            Some(reply)
        }
        0x20 => Some(FEATURE_FIRMWARE.to_vec()),
        _ => None,
    }
}

/// Input report `0x01` for one wire state. `seq` counts the reports of this pad.
fn input_report(action: &InputAction, seq: u8) -> [u8; 64] {
    let mut r = [0u8; 64];
    r[0] = 0x01;
    r[1..5].fill(0x80);
    r[8] = 0x08;
    // The accelerometer reads 1 g up: zero would read as free fall.
    r[24..26].copy_from_slice(&10_000i16.to_le_bytes());
    // IMU temperature, two lifted touch contacts, no trigger effect, charged on USB power.
    r[32] = 0x14;
    r[33] = 0x80;
    r[37] = 0x80;
    r[42] = 0x09;
    r[43] = 0x09;
    r[53] = 0x2A;
    r[54] = 0x18;
    r[7] = seq;
    let InputAction::Gamepad {
        buttons,
        lx,
        ly,
        rx,
        ry,
        lt,
        rt,
        ..
    } = *action
    else {
        return r;
    };
    // The wire's sticks put up at positive; the DualSense puts up at 0.
    let axis = |v: i16| u8::try_from((i32::from(v) + 32768) >> 8).unwrap_or(0x80);
    r[1] = axis(lx);
    r[2] = axis(ly.saturating_neg());
    r[3] = axis(rx);
    r[4] = axis(ry.saturating_neg());
    r[5] = lt;
    r[6] = rt;
    let on = |bit: u16| buttons & bit != 0;
    let mut face = 0u8;
    for (bit, value) in [
        (pad::X, 0x10),
        (pad::A, 0x20),
        (pad::B, 0x40),
        (pad::Y, 0x80),
    ] {
        if on(bit) {
            face |= value;
        }
    }
    r[8] = hat(buttons) | face;
    let mut shoulders = 0u8;
    for (pressed, value) in [
        (on(pad::LEFT_SHOULDER), 0x01),
        (on(pad::RIGHT_SHOULDER), 0x02),
        (lt > 0, 0x04),
        (rt > 0, 0x08),
        (on(pad::BACK), 0x10),
        (on(pad::START), 0x20),
        (on(pad::LEFT_STICK), 0x40),
        (on(pad::RIGHT_STICK), 0x80),
    ] {
        if pressed {
            shoulders |= value;
        }
    }
    r[9] = shoulders;
    r[10] = u8::from(on(pad::GUIDE));
    r
}

/// The hat value of the d-pad bits: 0 is up, then clockwise in eighths, 8 is centred.
/// Opposite directions cancel.
fn hat(buttons: u16) -> u8 {
    let up = buttons & pad::DPAD_UP != 0;
    let down = buttons & pad::DPAD_DOWN != 0;
    let left = buttons & pad::DPAD_LEFT != 0;
    let right = buttons & pad::DPAD_RIGHT != 0;
    match (up && !down, down && !up, left && !right, right && !left) {
        (true, _, false, false) => 0,
        (true, _, false, true) => 1,
        (false, false, false, true) => 2,
        (_, true, false, true) => 3,
        (_, true, false, false) => 4,
        (_, true, true, _) => 5,
        (false, false, true, _) => 6,
        (true, _, true, _) => 7,
        _ => 8,
    }
}

fn event(kind: u32) -> Box<[u8; EVENT_LEN]> {
    let mut ev = Box::new([0u8; EVENT_LEN]);
    ev[0..4].copy_from_slice(&kind.to_ne_bytes());
    ev
}

fn put_str(ev: &mut [u8], offset: usize, capacity: usize, text: &str) {
    // The kernel reads a C string: the last byte of the field stays zero.
    let len = text.len().min(capacity - 1);
    ev[offset..offset + len].copy_from_slice(&text.as_bytes()[..len]);
}

/// `UHID_CREATE2` for pad `index`.
fn create_event(index: u8) -> Box<[u8; EVENT_LEN]> {
    let mut ev = event(UHID_CREATE2);
    put_str(
        &mut ev[..],
        4,
        128,
        "Sony Interactive Entertainment DualSense Wireless Controller",
    );
    put_str(&mut ev[..], 132, 64, &format!("lumen-pad/{index}"));
    let rd_size = u16::try_from(RDESC.len()).unwrap_or(u16::MAX);
    ev[260..262].copy_from_slice(&rd_size.to_ne_bytes());
    ev[262..264].copy_from_slice(&BUS_USB.to_ne_bytes());
    ev[264..268].copy_from_slice(&SONY.to_ne_bytes());
    ev[268..272].copy_from_slice(&DUALSENSE.to_ne_bytes());
    ev[272..276].copy_from_slice(&0x0100u32.to_ne_bytes());
    ev[280..280 + RDESC.len()].copy_from_slice(&RDESC);
    ev
}

/// `UHID_INPUT2` carrying one input report.
fn input_event(report: &[u8; 64]) -> Box<[u8; EVENT_LEN]> {
    let mut ev = event(UHID_INPUT2);
    ev[4..6].copy_from_slice(&64u16.to_ne_bytes());
    ev[6..70].copy_from_slice(report);
    ev
}

/// The answer to a kernel request, or `None` when the request needs none.
fn answer(request: &[u8], index: u8) -> Option<Box<[u8; EVENT_LEN]>> {
    let kind = u32::from_ne_bytes(request.get(0..4)?.try_into().ok()?);
    // Both requests carry the id the reply must echo at bytes 4 to 8, then the report number.
    let id = request.get(4..8)?;
    match kind {
        UHID_GET_REPORT => {
            let mut ev = event(UHID_GET_REPORT_REPLY);
            ev[4..8].copy_from_slice(id);
            match feature_report(*request.get(8)?, index) {
                Some(data) => {
                    let len = u16::try_from(data.len()).unwrap_or(0);
                    ev[10..12].copy_from_slice(&len.to_ne_bytes());
                    ev[12..12 + data.len()].copy_from_slice(&data);
                }
                None => ev[8..10].copy_from_slice(&EIO.to_ne_bytes()),
            }
            Some(ev)
        }
        // The driver writes LEDs and trigger settings as feature reports; they are acknowledged.
        UHID_SET_REPORT => {
            let mut ev = event(UHID_SET_REPORT_REPLY);
            ev[4..8].copy_from_slice(id);
            Some(ev)
        }
        _ => None,
    }
}

/// What a game asked the pad's motors to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rumble {
    /// The pad index.
    pub pad: u8,
    /// The strong, low-frequency motor (the DualSense's left), 0 to 255.
    pub strong: u8,
    /// The weak, high-frequency motor (the right), 0 to 255.
    pub weak: u8,
}

/// The motors a request asks for, `(strong, weak)`, when it carries an output report `0x02` that
/// sets them. The report is the DualSense output report: the id, two flag bytes, the right
/// motor and the left motor. `hid-playstation` marks a rumble update with the haptics-select
/// bit of the first flag byte; a report that only sets the lightbar leaves it clear.
///
/// The request is either the kernel sending an output report (`UHID_OUTPUT`) or a writer
/// setting one (`UHID_SET_REPORT`, what a hidraw writer does).
fn rumble_of(request: &[u8]) -> Option<(u8, u8)> {
    const UHID_OUTPUT: u32 = 6;
    const HAPTICS_SELECT: u8 = 0x02;
    /// The uhid report type of an output report.
    const UHID_OUTPUT_REPORT: u8 = 1;
    let kind = u32::from_ne_bytes(request.get(0..4)?.try_into().ok()?);
    let data = match kind {
        // `uhid_output_req`: the data, then its size.
        UHID_OUTPUT => {
            let size = u16::from_ne_bytes(request.get(4 + 4096..4 + 4098)?.try_into().ok()?);
            request.get(4..4 + usize::from(size))?
        }
        // `uhid_set_report_req`: the id, the report number, the report type, then the data.
        UHID_SET_REPORT => {
            if *request.get(9)? != UHID_OUTPUT_REPORT {
                return None;
            }
            request.get(10..)?
        }
        _ => return None,
    };
    let (&id, flag0, right, left) = (data.first()?, data.get(1)?, data.get(3)?, data.get(4)?);
    (id == 0x02 && flag0 & HAPTICS_SELECT != 0).then_some((*left, *right))
}

/// One virtual DualSense. Dropping it removes the device.
struct VirtualPad {
    device: File,
    seq: u8,
    stop: Arc<AtomicBool>,
    service: Option<JoinHandle<()>>,
}

impl VirtualPad {
    fn create(index: u8, rumble: Sender<Rumble>) -> io::Result<Self> {
        let mut device = OpenOptions::new().read(true).write(true).open(UHID_PATH)?;
        device.write_all(&create_event(index)[..])?;
        let mut requests = device.try_clone()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let service = thread::Builder::new()
            .name(format!("lumen-pad-{index}"))
            .spawn(move || {
                let mut request = vec![0u8; EVENT_LEN];
                let tick = Timespec {
                    tv_sec: 0,
                    tv_nsec: 200_000_000,
                };
                let mut motors = (0, 0);
                // When the game last drove the rumble plane, and when the latched level was
                // last renewed for the client.
                let mut driven = Instant::now();
                let mut renewed = Instant::now();
                while !stopped.load(Ordering::Relaxed) {
                    let mut fds = [PollFd::new(&requests, PollFlags::IN)];
                    if !matches!(poll(&mut fds, Some(&tick)), Ok(n) if n > 0) {
                        // The tick is over. A rumble the game still drives is renewed for the
                        // client; one it stopped driving is cut.
                        if motors != (0, 0) {
                            if driven.elapsed() > RUMBLE_IDLE {
                                motors = (0, 0);
                                let _ = rumble.send(Rumble {
                                    pad: index,
                                    strong: 0,
                                    weak: 0,
                                });
                            } else if renewed.elapsed() >= RUMBLE_RENEW {
                                renewed = Instant::now();
                                let _ = rumble.send(Rumble {
                                    pad: index,
                                    strong: motors.0,
                                    weak: motors.1,
                                });
                            }
                        }
                        continue;
                    }
                    let Ok(len) = requests.read(&mut request) else {
                        return;
                    };
                    if let Some(now) = rumble_of(&request[..len]) {
                        // The game drove the rumble plane, so the level is still wanted.
                        driven = Instant::now();
                        if now != motors {
                            motors = now;
                            let _ = rumble.send(Rumble {
                                pad: index,
                                strong: now.0,
                                weak: now.1,
                            });
                        }
                    }
                    if let Some(reply) = answer(&request[..len], index)
                        && requests.write_all(&reply[..]).is_err()
                    {
                        return;
                    }
                }
            })?;
        Ok(Self {
            device,
            seq: 0,
            stop,
            service: Some(service),
        })
    }

    fn send(&mut self, action: &InputAction) -> io::Result<()> {
        let report = input_report(action, self.seq);
        self.seq = self.seq.wrapping_add(1);
        match self.device.write_all(&input_event(&report)[..]) {
            // The kernel refuses input until the driver finished probing; the next repeat lands.
            Err(e) if e.raw_os_error() == Some(rustix::io::Errno::INVAL.raw_os_error()) => Ok(()),
            other => other,
        }
    }
}

impl Drop for VirtualPad {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.device.write_all(&event(UHID_DESTROY)[..]);
        if let Some(service) = self.service.take() {
            let _ = service.join();
        }
    }
}

/// The virtual gamepads of one session, one per pad index the client uses.
pub struct Gamepads {
    pads: [Option<VirtualPad>; MAX_PADS as usize],
    rumble: Sender<Rumble>,
}

impl std::fmt::Debug for Gamepads {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let present: Vec<usize> = (0..self.pads.len())
            .filter(|&i| self.pads[i].is_some())
            .collect();
        f.debug_struct("Gamepads")
            .field("pads", &present)
            .finish_non_exhaustive()
    }
}

impl Gamepads {
    /// No pads yet. What games ask of the pads' motors goes to `rumble`.
    #[must_use]
    pub fn new(rumble: Sender<Rumble>) -> Self {
        Self {
            pads: Default::default(),
            rumble,
        }
    }

    /// Applies one gamepad action: the first state of a pad creates its device, a removal
    /// destroys it. Other actions are ignored.
    ///
    /// # Errors
    /// Returns a description when `/dev/uhid` cannot be opened or written.
    pub fn apply(&mut self, action: &InputAction) -> Result<(), String> {
        match *action {
            InputAction::Gamepad { pad, .. } => {
                let slot = self
                    .pads
                    .get_mut(usize::from(pad))
                    .ok_or("gamepad index out of range")?;
                if slot.is_none() {
                    *slot = Some(VirtualPad::create(pad, self.rumble.clone()).map_err(|e| {
                        format!(
                            "cannot create a virtual gamepad on {UHID_PATH}: {e} \
                             (install packaging/udev/60-lumen-input.rules)"
                        )
                    })?);
                }
                if let Some(device) = slot {
                    device
                        .send(action)
                        .map_err(|e| format!("gamepad {pad} input failed: {e}"))?;
                }
                Ok(())
            }
            InputAction::GamepadRemoved { pad } => {
                if let Some(slot) = self.pads.get_mut(usize::from(pad)) {
                    *slot = None;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(buttons: u16, ly: i16, lt: u8) -> InputAction {
        InputAction::Gamepad {
            pad: 0,
            buttons,
            lx: 0,
            ly,
            rx: i16::MIN,
            ry: i16::MAX,
            lt,
            rt: 0,
        }
    }

    #[test]
    fn sticks_centre_at_0x80_and_up_is_zero() {
        let r = input_report(&state(0, i16::MAX, 0), 0);
        assert_eq!(r[1], 0x80, "a centred stick");
        assert_eq!(r[2], 0x00, "left stick pushed up");
        assert_eq!(r[3], 0x00, "right stick fully left");
        assert_eq!(r[4], 0x00, "right stick pushed up");
        let r = input_report(&state(0, i16::MIN, 0), 0);
        assert_eq!(r[2], 0xFF, "left stick pulled down");
    }

    #[test]
    fn buttons_land_in_their_bits() {
        let r = input_report(&state(pad::A | pad::Y | pad::START | pad::GUIDE, 0, 200), 7);
        assert_eq!(r[7], 7, "the sequence byte");
        assert_eq!(r[8], 0x08 | 0x20 | 0x80, "centred hat, cross and triangle");
        assert_eq!(r[5], 200);
        assert_eq!(
            r[9],
            0x04 | 0x20,
            "a pulled trigger also sets its button, and options"
        );
        assert_eq!(r[10], 0x01, "the PS button");
    }

    #[test]
    fn the_hat_turns_clockwise_and_opposites_cancel() {
        use pad::{DPAD_DOWN as D, DPAD_LEFT as L, DPAD_RIGHT as R, DPAD_UP as U};
        let cases = [
            (0, 8),
            (U, 0),
            (U | R, 1),
            (R, 2),
            (D | R, 3),
            (D, 4),
            (D | L, 5),
            (L, 6),
            (U | L, 7),
            (U | D, 8),
            (L | R, 8),
            (U | D | R, 2),
        ];
        for (buttons, want) in cases {
            assert_eq!(hat(buttons), want, "buttons {buttons:#06x}");
        }
    }

    #[test]
    fn create_places_the_identity_where_the_kernel_reads_it() {
        let ev = create_event(2);
        assert_eq!(
            u32::from_ne_bytes(ev[0..4].try_into().unwrap()),
            UHID_CREATE2
        );
        assert!(ev[132..196].starts_with(b"lumen-pad/2\0"));
        assert_eq!(u16::from_ne_bytes([ev[260], ev[261]]), 273);
        assert_eq!(u32::from_ne_bytes(ev[264..268].try_into().unwrap()), SONY);
        assert_eq!(
            u32::from_ne_bytes(ev[268..272].try_into().unwrap()),
            DUALSENSE
        );
        assert_eq!(&ev[280..280 + 273], &RDESC[..]);
        assert_eq!(ev[4 + 127], 0, "the name stays a C string");
    }

    /// A `UHID_OUTPUT` event carrying `data`.
    fn output(data: &[u8]) -> Vec<u8> {
        let mut ev = vec![0u8; EVENT_LEN];
        ev[0..4].copy_from_slice(&6u32.to_ne_bytes());
        ev[4..4 + data.len()].copy_from_slice(data);
        let size = u16::try_from(data.len()).unwrap();
        ev[4100..4102].copy_from_slice(&size.to_ne_bytes());
        ev
    }

    /// A `UHID_SET_REPORT` event carrying an output report.
    fn set_report(data: &[u8]) -> Vec<u8> {
        let mut ev = vec![0u8; EVENT_LEN];
        ev[0..4].copy_from_slice(&UHID_SET_REPORT.to_ne_bytes());
        ev[8] = 0x02;
        ev[9] = 1;
        ev[10..10 + data.len()].copy_from_slice(data);
        ev
    }

    #[test]
    fn rumble_is_read_only_from_a_haptics_update() {
        // Report 0x02: flag0 with haptics select, flag1, right motor, left motor.
        let mut report = [0u8; 63];
        report[..5].copy_from_slice(&[0x02, 0x03, 0x00, 40, 200]);
        assert_eq!(
            rumble_of(&output(&report)),
            Some((200, 40)),
            "(strong, weak)"
        );
        report[1] = 0x01;
        assert_eq!(rumble_of(&output(&report)), None, "no haptics select");
        report[..2].copy_from_slice(&[0x31, 0x03]);
        assert_eq!(rumble_of(&output(&report)), None, "not report 0x02");
        let mut get = output(&[0x02, 0x03, 0, 40, 200]);
        get[0..4].copy_from_slice(&UHID_GET_REPORT.to_ne_bytes());
        assert_eq!(rumble_of(&get), None, "not an output event");
        assert_eq!(rumble_of(&output(&[0x02, 0x03, 0])), None, "too short");
    }

    #[test]
    fn a_writer_that_sets_the_output_report_is_read_too() {
        let mut report = [0u8; 63];
        report[..5].copy_from_slice(&[0x02, 0x03, 0x00, 40, 200]);
        assert_eq!(
            rumble_of(&set_report(&report)),
            Some((200, 40)),
            "(strong, weak)"
        );
        let mut feature = set_report(&report);
        feature[9] = 0;
        assert_eq!(
            rumble_of(&feature),
            None,
            "a feature report is not a rumble"
        );
    }

    #[test]
    fn feature_requests_get_their_report_or_an_error() {
        let mut request = vec![0u8; EVENT_LEN];
        request[0..4].copy_from_slice(&UHID_GET_REPORT.to_ne_bytes());
        request[4..8].copy_from_slice(&77u32.to_ne_bytes());
        for (report, len) in [(0x05u8, 41usize), (0x09, 20), (0x20, 64)] {
            request[8] = report;
            let reply = answer(&request, 1).expect("a reply");
            assert_eq!(
                u32::from_ne_bytes(reply[0..4].try_into().unwrap()),
                UHID_GET_REPORT_REPLY
            );
            assert_eq!(u32::from_ne_bytes(reply[4..8].try_into().unwrap()), 77);
            assert_eq!(u16::from_ne_bytes([reply[8], reply[9]]), 0, "no error");
            assert_eq!(usize::from(u16::from_ne_bytes([reply[10], reply[11]])), len);
            assert_eq!(reply[12], report, "the reply starts with its report id");
        }
        request[8] = 0x42;
        let reply = answer(&request, 1).expect("a reply");
        assert_eq!(u16::from_ne_bytes([reply[8], reply[9]]), EIO);
    }

    #[test]
    fn every_pad_has_its_own_mac_under_the_lumen_prefix() {
        // hid-playstation writes the MAC high octet first, lower-case, colon separated.
        let uniq = |index| {
            let mac = feature_report(0x09, index).unwrap();
            mac[1..7]
                .iter()
                .rev()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(":")
        };
        assert!(uniq(0).starts_with(PAD_MAC_PREFIX), "{}", uniq(0));
        assert!(uniq(3).starts_with(PAD_MAC_PREFIX), "{}", uniq(3));
        assert_ne!(uniq(0), uniq(1));
    }
}
