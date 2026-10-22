//! Injection through the wlroots virtual input protocols (`zwlr_virtual_pointer_manager_v1` and
//! `zwp_virtual_keyboard_manager_v1`), as an ordinary client of `$WAYLAND_DISPLAY`.
//!
//! A headless wlroots compositor started with `WLR_LIBINPUT_NO_DEVICES=1` never adopts a uinput
//! device as a seat device, so uinput input does not reach it. These protocols reach it with no
//! access to `/dev/uinput` at all. The approach follows punktfunk's `pf-inject` wlroots injector
//! (MIT OR Apache-2.0, Copyright (c) 2026 unom - Enrico Bühler).
//!
//! Absolute motion is mapped onto the output the pointer was created for, so it lands on the
//! captured screen; without a named output it spans the whole layout. The keymap is the host's
//! default xkb keymap (`XKB_DEFAULT_*`). wlroots does not derive modifiers from a virtual
//! keyboard's keys, so the modifier keys are tracked here and the state is sent after each one.

use std::io::Write;
use std::os::fd::AsFd;
use std::time::Instant;

use lumen_input_wayland::{InputEvent, InputEventKind, PIXELS_PER_NOTCH};
use wayland_client::protocol::{wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};
use xkbcommon::xkb;

use crate::text::{KEY_LEFTSHIFT, for_each_tap};
use crate::{InputInjector, Residual};

#[derive(Default)]
struct Globals {
    pointer_mgr: Option<ZwlrVirtualPointerManagerV1>,
    keyboard_mgr: Option<ZwpVirtualKeyboardManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    /// Every output with the name it announced (`wl_output` v4), in advertisement order.
    outputs: Vec<(wl_output::WlOutput, Option<String>)>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for Globals {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        (): &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "zwlr_virtual_pointer_manager_v1" => {
                state.pointer_mgr = Some(registry.bind(name, version.min(2), qh, ()));
            }
            "zwp_virtual_keyboard_manager_v1" => {
                state.keyboard_mgr = Some(registry.bind(name, 1, qh, ()));
            }
            "wl_seat" if state.seat.is_none() => {
                state.seat = Some(registry.bind(name, version.min(7), qh, ()));
            }
            "wl_output" => {
                let index = state.outputs.len();
                let output = registry.bind(name, version.min(4), qh, index);
                state.outputs.push((output, None));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, usize> for Globals {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some(entry) = state.outputs.get_mut(*index)
        {
            entry.1 = Some(name);
        }
    }
}

/// These objects send no events this injector needs; `Dispatch` is still required to bind them.
macro_rules! ignore_events {
    ($($t:ty),* $(,)?) => {$(
        impl Dispatch<$t, ()> for Globals {
            fn event(_: &mut Self, _: &$t, _: <$t as Proxy>::Event, (): &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
ignore_events!(
    wl_seat::WlSeat,
    ZwlrVirtualPointerManagerV1,
    ZwlrVirtualPointerV1,
    ZwpVirtualKeyboardManagerV1,
    ZwpVirtualKeyboardV1,
);

/// A virtual pointer and keyboard on a wlroots compositor.
pub struct WlrInjector {
    conn: Connection,
    _queue: EventQueue<Globals>,
    pointer: ZwlrVirtualPointerV1,
    keyboard: ZwpVirtualKeyboardV1,
    /// The keymap the compositor maps. Dropping it early would not unmap it, but it stays with
    /// the keyboard it describes.
    _keymap: std::fs::File,
    /// The extent absolute positions are expressed in: the captured size.
    extent: (u32, u32),
    epoch: Instant,
    wheel_v: Residual,
    wheel_h: Residual,
    /// The keymap's modifier mask of each tracked modifier key.
    modifier_keys: Vec<(u32, u32)>,
    /// The keymap's Lock (Caps Lock) mask.
    lock_mask: u32,
    /// Evdev codes of the modifier keys held down.
    held: Vec<u32>,
    /// The locked modifiers.
    locked: u32,
}

/// Evdev modifier keys and the xkb modifier each one sets.
const MODIFIER_KEYS: [(u32, &str); 8] = [
    (42, xkb::MOD_NAME_SHIFT), // KEY_LEFTSHIFT
    (54, xkb::MOD_NAME_SHIFT), // KEY_RIGHTSHIFT
    (29, xkb::MOD_NAME_CTRL),  // KEY_LEFTCTRL
    (97, xkb::MOD_NAME_CTRL),  // KEY_RIGHTCTRL
    (56, xkb::MOD_NAME_ALT),   // KEY_LEFTALT
    (100, "Mod5"),             // KEY_RIGHTALT (AltGr on most layouts)
    (125, xkb::MOD_NAME_LOGO), // KEY_LEFTMETA
    (126, xkb::MOD_NAME_LOGO), // KEY_RIGHTMETA
];
/// `KEY_CAPSLOCK`.
const KEY_CAPSLOCK: u32 = 58;

/// The keymap's mask for modifier `name`, or zero when it has none.
fn mod_mask(keymap: &xkb::Keymap, name: &str) -> u32 {
    let index = keymap.mod_get_index(name);
    if index == xkb::MOD_INVALID {
        0
    } else {
        1 << index
    }
}

impl std::fmt::Debug for WlrInjector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WlrInjector")
            .field("extent", &self.extent)
            .finish_non_exhaustive()
    }
}

impl WlrInjector {
    /// Connects to `$WAYLAND_DISPLAY` and creates the virtual devices. Absolute positions arrive
    /// in pixels of a `width` x `height` capture of `output` (a connector name such as
    /// `HEADLESS-1`); without a matching output they span the whole layout.
    ///
    /// # Errors
    /// Returns an error when there is no Wayland display or the compositor lacks either protocol.
    pub fn connect(output: Option<&str>, width: u32, height: u32) -> Result<Self, String> {
        let conn = Connection::connect_to_env().map_err(|e| format!("wayland connect: {e}"))?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut globals = Globals::default();
        // Two round trips: the first binds the globals, the second delivers the output names.
        for _ in 0..2 {
            queue
                .roundtrip(&mut globals)
                .map_err(|e| format!("wayland roundtrip: {e}"))?;
        }
        let pointer_mgr = globals
            .pointer_mgr
            .take()
            .ok_or("the compositor has no zwlr_virtual_pointer_manager_v1")?;
        let keyboard_mgr = globals
            .keyboard_mgr
            .take()
            .ok_or("the compositor has no zwp_virtual_keyboard_manager_v1")?;
        let seat = globals.seat.take().ok_or("the compositor has no wl_seat")?;

        let target = output.and_then(|name| {
            globals
                .outputs
                .iter()
                .find(|(_, n)| n.as_deref() == Some(name))
                .map(|(o, _)| o)
        });
        let pointer = match target {
            Some(o) if pointer_mgr.version() >= 2 => {
                pointer_mgr.create_virtual_pointer_with_output(Some(&seat), Some(o), &qh, ())
            }
            _ => pointer_mgr.create_virtual_pointer(Some(&seat), &qh, ()),
        };
        let keyboard = keyboard_mgr.create_virtual_keyboard(&seat, &qh, ());

        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        // Empty names take the XKB_DEFAULT_* environment, then the built-in defaults.
        let keymap =
            xkb::Keymap::new_from_names(&context, "", "", "", "", None, xkb::COMPILE_NO_FLAGS)
                .ok_or("cannot compile the default xkb keymap")?;
        let text = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        let mut file = std::fs::File::from(
            rustix::fs::memfd_create("lumen-keymap", rustix::fs::MemfdFlags::CLOEXEC)
                .map_err(|e| format!("memfd_create: {e}"))?,
        );
        file.write_all(text.as_bytes())
            .and_then(|()| file.write_all(&[0]))
            .map_err(|e| format!("write keymap: {e}"))?;
        let size = u32::try_from(text.len() + 1).map_err(|_| "the keymap is too large")?;
        keyboard.keymap(wl_keyboard::KeymapFormat::XkbV1.into(), file.as_fd(), size);
        let modifier_keys = MODIFIER_KEYS
            .iter()
            .map(|&(code, name)| (code, mod_mask(&keymap, name)))
            .collect();
        let lock_mask = mod_mask(&keymap, xkb::MOD_NAME_CAPS);
        conn.flush().map_err(|e| format!("wayland flush: {e}"))?;
        Ok(Self {
            conn,
            _queue: queue,
            pointer,
            keyboard,
            _keymap: file,
            extent: (width.max(1), height.max(1)),
            epoch: Instant::now(),
            wheel_v: Residual::default(),
            wheel_h: Residual::default(),
            modifier_keys,
            lock_mask,
            held: Vec::new(),
            locked: 0,
        })
    }

    /// Milliseconds since the injector started, the protocols' event time.
    fn time(&self) -> u32 {
        // Wraps after 49 days, which is what a u32 millisecond clock does on the wire anyway.
        self.epoch.elapsed().as_millis() as u32
    }

    fn flush(&self) -> Result<(), String> {
        self.conn.flush().map_err(|e| format!("wayland flush: {e}"))
    }

    fn key(&mut self, keycode: u32, pressed: bool) -> Result<(), String> {
        let time = self.time();
        let state = if pressed {
            wl_keyboard::KeyState::Pressed
        } else {
            wl_keyboard::KeyState::Released
        };
        self.keyboard.key(time, keycode, state.into());
        if let Some(depressed) = self.modifiers_after(keycode, pressed) {
            self.keyboard.modifiers(depressed, 0, self.locked, 0);
        }
        self.flush()
    }

    /// Updates the tracked modifiers for a key and returns the depressed mask when it changed.
    fn modifiers_after(&mut self, keycode: u32, pressed: bool) -> Option<u32> {
        if keycode == KEY_CAPSLOCK {
            if pressed {
                self.locked ^= self.lock_mask;
            }
        } else if self.modifier_keys.iter().any(|(code, _)| *code == keycode) {
            self.held.retain(|code| *code != keycode);
            if pressed {
                self.held.push(keycode);
            }
        } else {
            return None;
        }
        Some(
            self.modifier_keys
                .iter()
                .filter(|(code, _)| self.held.contains(code))
                .fold(0, |mask, (_, bit)| mask | bit),
        )
    }

    fn motion(&mut self, dx: f64, dy: f64, x: f64, y: f64) -> Result<(), String> {
        let time = self.time();
        if x.is_finite() && y.is_finite() {
            let (w, h) = self.extent;
            // Rounded and clamped onto the extent, as the uinput absolute device does.
            let px = x.round().clamp(0.0, f64::from(w - 1)) as u32;
            let py = y.round().clamp(0.0, f64::from(h - 1)) as u32;
            self.pointer.motion_absolute(time, px, py, w, h);
        } else {
            self.pointer.motion(time, dx, dy);
        }
        self.pointer.frame();
        self.flush()
    }

    fn button(&mut self, button: u32, pressed: bool) -> Result<(), String> {
        let state = if pressed {
            wl_pointer::ButtonState::Pressed
        } else {
            wl_pointer::ButtonState::Released
        };
        self.pointer.button(self.time(), button, state);
        self.pointer.frame();
        self.flush()
    }

    fn scroll(&mut self, dx: f64, dy: f64) -> Result<(), String> {
        let time = self.time();
        self.pointer.axis_source(wl_pointer::AxisSource::Wheel);
        // Positive `dy` scrolls down, which is a positive wl_pointer vertical axis.
        for (axis, residual, delta) in [
            (wl_pointer::Axis::VerticalScroll, &mut self.wheel_v, dy),
            (wl_pointer::Axis::HorizontalScroll, &mut self.wheel_h, dx),
        ] {
            let notches = residual.take(delta / PIXELS_PER_NOTCH);
            if notches != 0 {
                let value = f64::from(notches) * PIXELS_PER_NOTCH;
                self.pointer.axis_discrete(time, axis, value, notches);
            }
        }
        self.pointer.frame();
        self.flush()
    }
}

impl InputInjector for WlrInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<(), String> {
        match event.kind {
            InputEventKind::Key { keycode, pressed } => self.key(keycode, pressed),
            InputEventKind::Motion { dx, dy, x, y } => self.motion(dx, dy, x, y),
            InputEventKind::Button { button, pressed } => self.button(button, pressed),
            InputEventKind::Scroll { dx, dy } => self.scroll(dx, dy),
        }
    }

    fn inject_text(&mut self, text: &str) -> Result<(), String> {
        for_each_tap(text, |code, shift| {
            if shift {
                self.key(KEY_LEFTSHIFT, true)?;
            }
            self.key(code, true)?;
            self.key(code, false)?;
            if shift {
                self.key(KEY_LEFTSHIFT, false)?;
            }
            Ok(())
        })
    }
}
