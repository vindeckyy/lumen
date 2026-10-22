//! The client's gamepads: every evdev gamepad on this machine, read as a whole state.
//!
//! A pad's state leaves on every change and again every [`REPEAT`], on the input socket, so a
//! lost datagram costs one repeat at most. Pads are found by the kernel gamepad layout
//! (`Documentation/input/gamepad.rst`): `BTN_SOUTH` marks a gamepad, sticks are `ABS_X`/`ABS_Y`
//! and `ABS_RX`/`ABS_RY`, analog triggers `ABS_Z`/`ABS_RZ`, the d-pad `ABS_HAT0X`/`ABS_HAT0Y` or
//! `BTN_DPAD_*`. A pad that unplugs is removed on the host too; new pads are looked for every
//! [`SCAN`]. The host's own virtual pads carry [`PAD_MAC_PREFIX`] and are skipped, so a client on
//! the host's machine does not feed them back.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use evdev::{
    AbsoluteAxisCode, Device, FFEffect, FFEffectData, FFEffectKind, FFReplay, FFTrigger, KeyCode,
};
use lumen_input_inject::PAD_MAC_PREFIX;
use lumen_protocol::input::{InputAction, MAX_PADS, pad};
use lumen_transport::input_channel::InputSender;
use rustix::event::{PollFd, PollFlags, Timespec, poll};

/// How often an unchanged state is sent again.
const REPEAT: Duration = Duration::from_millis(100);
/// How often the client looks for pads that were plugged in.
const SCAN: Duration = Duration::from_secs(2);
/// How long the pad thread waits for a pad event before it looks at the rumble slot again.
const RUMBLE_POLL: Duration = Duration::from_millis(10);

/// What the host's game wants of the pads' motors, by pad slot: the strong and the weak motor.
pub type RumbleSlot = Arc<Mutex<HashMap<u8, (u8, u8)>>>;

/// The range of every axis a device reports, by axis code. evdev's cached state keeps only
/// the values, so the ranges are read once, when the pad is opened.
type Ranges = HashMap<u16, (i32, i32)>;

/// One gamepad of this machine and the slot it drives on the host.
struct Pad {
    device: Device,
    ranges: Ranges,
    path: PathBuf,
    slot: u8,
    last: InputAction,
    /// The rumble effect the pad plays, once the host asked for one.
    effect: Option<FFEffect>,
    /// The motors the pad plays now, so the same request is not sent twice.
    motors: (u8, u8),
}

/// Starts the gamepad thread. It sends through `sender`, the input socket's, and stops when
/// `running` goes false. What the host's game wants of the pad's motors comes from `rumble`.
pub fn spawn(
    sender: Arc<Mutex<InputSender>>,
    running: Arc<AtomicBool>,
    rumble: RumbleSlot,
    window: Arc<crate::WindowControl>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut pads: Vec<Pad> = Vec::new();
        let mut next_scan = Instant::now();
        let mut next_repeat = Instant::now() + REPEAT;
        let mut rumble_failed = false;
        while running.load(Ordering::SeqCst) {
            if Instant::now() >= next_scan {
                scan(&mut pads);
                next_scan = Instant::now() + SCAN;
            }
            let wait = next_repeat.saturating_duration_since(Instant::now());
            // The host's game may ask for the motors at any time, so the wait is short when a
            // pad is there to play them.
            let wait = if pads.is_empty() {
                wait
            } else {
                wait.min(RUMBLE_POLL)
            };
            let ready = wait_readable(&pads, wait);
            let mut gone = Vec::new();
            for (index, pad) in pads.iter_mut().enumerate() {
                if !ready.contains(&index) {
                    continue;
                }
                // Draining the events updates the cached state the pad is read from.
                match pad.device.fetch_events() {
                    Ok(events) => events.for_each(drop),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(_) => {
                        gone.push(index);
                        continue;
                    }
                }
                let state = read_state(&pad.device, &pad.ranges, pad.slot);
                let state = apply_chord(state, &pad.last, &window);
                if state != pad.last {
                    pad.last = state;
                    send(&sender, state);
                }
            }
            for index in gone.into_iter().rev() {
                let pad = pads.remove(index);
                eprintln!("lumen-client: gamepad {} left", pad.path.display());
                send(&sender, InputAction::GamepadRemoved { pad: pad.slot });
            }
            if let Ok(slot) = rumble.lock() {
                for pad in &mut pads {
                    if let Some(&(strong, weak)) = slot.get(&pad.slot)
                        && let Err(e) = set_rumble(pad, strong, weak)
                        && !rumble_failed
                    {
                        rumble_failed = true;
                        eprintln!("lumen-client: gamepad {} cannot rumble: {e}", pad.slot);
                    }
                }
            }
            if Instant::now() >= next_repeat {
                for pad in &pads {
                    send(&sender, pad.last);
                }
                next_repeat = Instant::now() + REPEAT;
            }
        }
    })
}

/// The chord of a pad's buttons, with the chord's own buttons off the wire.
///
/// The Guide and the A are the client's chord: the host's game never sees them.
fn apply_chord(
    state: InputAction,
    previous: &InputAction,
    window: &crate::WindowControl,
) -> InputAction {
    let InputAction::Gamepad {
        pad,
        buttons,
        lx,
        ly,
        rx,
        ry,
        lt,
        rt,
    } = state
    else {
        return state;
    };
    let previous_buttons = match previous {
        InputAction::Gamepad { buttons, .. } => *buttons,
        _ => 0,
    };
    let (masked, chord) = crate::chords::pad_chord(buttons, previous_buttons);
    if let Some(chord) = chord {
        crate::input::act_on(window, chord);
    }
    InputAction::Gamepad {
        pad,
        buttons: masked,
        lx,
        ly,
        rx,
        ry,
        lt,
        rt,
    }
}

/// Plays `strong` and `weak` on the pad's motors. The effect is uploaded once and then updated,
/// so the pad keeps playing it until the host asks for something else.
fn set_rumble(pad: &mut Pad, strong: u8, weak: u8) -> std::io::Result<()> {
    if pad.motors == (strong, weak) {
        return Ok(());
    }
    pad.motors = (strong, weak);
    let data = rumble_effect(strong, weak);
    if let Some(effect) = pad.effect.as_mut() {
        return effect.update(data);
    }
    let mut effect = pad.device.upload_ff_effect(data)?;
    effect.play(1)?;
    pad.effect = Some(effect);
    Ok(())
}

/// The effect that plays `strong` and `weak` on the pad's motors. The kernel's magnitudes are
/// 16-bit, the host's are 8-bit.
fn rumble_effect(strong: u8, weak: u8) -> FFEffectData {
    FFEffectData {
        direction: 0,
        trigger: FFTrigger::default(),
        // A length of zero plays the effect until it is stopped.
        replay: FFReplay {
            length: 0,
            delay: 0,
        },
        kind: FFEffectKind::Rumble {
            strong_magnitude: u16::from(strong) * 257,
            weak_magnitude: u16::from(weak) * 257,
        },
    }
}

fn send(sender: &Mutex<InputSender>, action: InputAction) {
    if let Ok(mut sender) = sender.lock()
        && let Err(e) = sender.send(action, crate::receive::monotonic_us())
    {
        eprintln!("lumen-client: gamepad send failed: {e}");
    }
}

/// Opens the gamepads that are not tracked yet, while a host slot is free.
fn scan(pads: &mut Vec<Pad>) {
    let known: HashSet<PathBuf> = pads.iter().map(|p| p.path.clone()).collect();
    for (path, device) in evdev::enumerate() {
        if known.contains(&path) || !is_gamepad(&device) {
            continue;
        }
        let Some(slot) = (0..MAX_PADS).find(|s| pads.iter().all(|p| p.slot != *s)) else {
            return;
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
            "lumen-client: gamepad {} ({}) drives host pad {slot}",
            device.name().unwrap_or("unnamed"),
            path.display()
        );
        let last = read_state(&device, &ranges, slot);
        pads.push(Pad {
            device,
            ranges,
            path,
            slot,
            last,
            effect: None,
            motors: (0, 0),
        });
    }
}

fn is_gamepad(device: &Device) -> bool {
    let south = device
        .supported_keys()
        .is_some_and(|keys| keys.contains(KeyCode::BTN_SOUTH));
    let ours = device
        .unique_name()
        .is_some_and(|uniq| uniq.starts_with(PAD_MAC_PREFIX));
    south && !ours
}

/// Indices of the pads with events waiting, after at most `wait`.
fn wait_readable(pads: &[Pad], wait: Duration) -> Vec<usize> {
    if pads.is_empty() {
        thread::sleep(wait);
        return Vec::new();
    }
    let mut fds: Vec<PollFd<'_>> = pads
        .iter()
        .map(|p| PollFd::new(&p.device, PollFlags::IN))
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

/// The wire state of `device`, from the state evdev keeps for it.
fn read_state(device: &Device, ranges: &Ranges, slot: u8) -> InputAction {
    let state = device.cached_state();
    let keys = state.key_vals();
    let pressed = |code: KeyCode| keys.is_some_and(|k| k.contains(code));
    let abs = state.abs_vals();
    // The value and range of an axis the device has.
    let axis = |code: AbsoluteAxisCode| {
        let (lo, hi) = *ranges.get(&code.0)?;
        let value = abs?.get(usize::from(code.0))?.value;
        Some((value, lo, hi))
    };
    let stick = |code| axis(code).map_or(0, |(v, lo, hi)| scale_stick(v, lo, hi));
    let trigger = |code, digital| match axis(code) {
        Some((v, lo, hi)) => scale_trigger(v, lo, hi),
        None if pressed(digital) => u8::MAX,
        None => 0,
    };
    let hat = |code| axis(code).map_or(0, |(v, _, _)| v.signum());
    let (hat_x, hat_y) = (
        hat(AbsoluteAxisCode::ABS_HAT0X),
        hat(AbsoluteAxisCode::ABS_HAT0Y),
    );
    let mut buttons = 0u16;
    for (on, bit) in [
        (pressed(KeyCode::BTN_SOUTH), pad::A),
        (pressed(KeyCode::BTN_EAST), pad::B),
        (pressed(KeyCode::BTN_WEST), pad::X),
        (pressed(KeyCode::BTN_NORTH), pad::Y),
        (pressed(KeyCode::BTN_TL), pad::LEFT_SHOULDER),
        (pressed(KeyCode::BTN_TR), pad::RIGHT_SHOULDER),
        (pressed(KeyCode::BTN_SELECT), pad::BACK),
        (pressed(KeyCode::BTN_START), pad::START),
        (pressed(KeyCode::BTN_MODE), pad::GUIDE),
        (pressed(KeyCode::BTN_THUMBL), pad::LEFT_STICK),
        (pressed(KeyCode::BTN_THUMBR), pad::RIGHT_STICK),
        (pressed(KeyCode::BTN_DPAD_UP) || hat_y < 0, pad::DPAD_UP),
        (pressed(KeyCode::BTN_DPAD_DOWN) || hat_y > 0, pad::DPAD_DOWN),
        (pressed(KeyCode::BTN_DPAD_LEFT) || hat_x < 0, pad::DPAD_LEFT),
        (
            pressed(KeyCode::BTN_DPAD_RIGHT) || hat_x > 0,
            pad::DPAD_RIGHT,
        ),
    ] {
        if on {
            buttons |= bit;
        }
    }
    InputAction::Gamepad {
        pad: slot,
        buttons,
        lx: stick(AbsoluteAxisCode::ABS_X),
        // evdev puts down at positive; the wire puts up at positive.
        ly: stick(AbsoluteAxisCode::ABS_Y).saturating_neg(),
        rx: stick(AbsoluteAxisCode::ABS_RX),
        ry: stick(AbsoluteAxisCode::ABS_RY).saturating_neg(),
        lt: trigger(AbsoluteAxisCode::ABS_Z, KeyCode::BTN_TL2),
        rt: trigger(AbsoluteAxisCode::ABS_RZ, KeyCode::BTN_TR2),
    }
}

/// A stick value in `lo..=hi` as -32768 to 32767.
fn scale_stick(value: i32, lo: i32, hi: i32) -> i16 {
    if hi <= lo {
        return 0;
    }
    let span = i64::from(hi) - i64::from(lo);
    let offset = i64::from(value.clamp(lo, hi)) - i64::from(lo);
    i16::try_from(offset * 65535 / span - 32768).unwrap_or(0)
}

/// A trigger value in `lo..=hi` as 0 to 255.
fn scale_trigger(value: i32, lo: i32, hi: i32) -> u8 {
    if hi <= lo {
        return 0;
    }
    let span = i64::from(hi) - i64::from(lo);
    let offset = i64::from(value.clamp(lo, hi)) - i64::from(lo);
    u8::try_from(offset * 255 / span).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rumble_magnitudes_fill_the_kernel_range() {
        let evdev::FFEffectKind::Rumble {
            strong_magnitude,
            weak_magnitude,
        } = rumble_effect(200, 40).kind
        else {
            panic!("the effect is a rumble");
        };
        assert_eq!(strong_magnitude, 51400, "200 of 255 fills the range");
        assert_eq!(weak_magnitude, 10280, "40 of 255 fills the range");
        let evdev::FFEffectKind::Rumble {
            strong_magnitude,
            weak_magnitude,
        } = rumble_effect(0, 0).kind
        else {
            panic!("the effect is a rumble");
        };
        assert_eq!((strong_magnitude, weak_magnitude), (0, 0), "no rumble");
    }

    #[test]
    fn sticks_cover_the_wire_range_from_any_device_range() {
        assert_eq!(scale_stick(-32768, -32768, 32767), -32768);
        assert_eq!(scale_stick(32767, -32768, 32767), 32767);
        assert_eq!(scale_stick(0, 0, 255), -32768);
        assert_eq!(scale_stick(255, 0, 255), 32767);
        assert!(
            scale_stick(128, 0, 255).abs() < 256,
            "the middle stays near the middle"
        );
        assert_eq!(scale_stick(999, 0, 255), 32767, "out-of-range values clamp");
        assert_eq!(
            scale_stick(5, 3, 3),
            0,
            "a degenerate range reads as centred"
        );
    }

    #[test]
    fn triggers_cover_0_to_255() {
        assert_eq!(scale_trigger(0, 0, 1023), 0);
        assert_eq!(scale_trigger(1023, 0, 1023), 255);
        assert_eq!(scale_trigger(-5, 0, 255), 0);
    }
}
