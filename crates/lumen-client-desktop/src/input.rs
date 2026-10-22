//! The client's input path: the keys and pointer of the viewer window, and the gamepads of this
//! machine, leave as sealed datagrams for the host's input socket.
//!
//! Only the viewer's own window is read. On X11 that is the window's events: keys while it has
//! the focus, the pointer while it is over it. On Wayland the seat is bound on the renderer's own
//! connection, and the compositor sends a client keyboard and pointer events only for its own
//! surfaces, with the focus. Keys still down when the focus leaves are released either way.
//! Positions are window pixels, which are the pixels of the picture the host serves, so the host
//! scales them to its screen. Gamepads are read by their own thread, see [`crate::gamepad`];
//! both threads seal with the one sender, so the cipher's counter never repeats.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use lumen_crypto::PacketCipher;
use lumen_host_service::control::ControlMessage;
use lumen_input_wayland::{InputEventKind, InputSource, WaylandSource};
use lumen_input_x11::WindowEventSource;
use lumen_protocol::input::InputAction;
use lumen_transport::input_channel::InputSender;

use crate::chords::{Chord, Chords};
use crate::receive::monotonic_us;

/// How long one poll waits for an event before the thread looks at the running flag and the
/// window again.
const POLL: Duration = Duration::from_millis(20);
/// How long the Wayland reader sleeps when the seat has nothing; the seat source never blocks.
const WAYLAND_IDLE: Duration = Duration::from_millis(1);

/// The host's newest cursor shape, a `ControlMessage::Cursor`, shared with the control stage.
pub type CursorSlot = Arc<Mutex<Option<ControlMessage>>>;

/// The shape in `slot` when its serial differs from `shown`, which it then becomes.
fn new_cursor(slot: &CursorSlot, shown: &mut Option<u32>) -> Option<ControlMessage> {
    let message = slot.lock().ok()?.clone()?;
    let ControlMessage::Cursor { serial, .. } = &message else {
        return None;
    };
    if *shown == Some(*serial) {
        return None;
    }
    *shown = Some(*serial);
    Some(message)
}

/// Where the viewer's input comes from.
#[derive(Debug)]
pub enum Window {
    /// The X11 window id the renderer publishes; it stays the same for the whole session.
    X11(Arc<AtomicU32>),
    /// The renderer's Wayland connection and the surface the pointer is locked on.
    Wayland(
        wayland_client::Connection,
        Option<wayland_client::protocol::wl_surface::WlSurface>,
    ),
}

/// Which input of the viewer the client sends to the host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InputMode {
    /// Nothing is sent: the client only watches.
    Off,
    /// The pointer of the viewer window.
    Pointer,
    /// The keyboard of the viewer window.
    Keyboard,
    /// The pointer and the keyboard.
    #[default]
    Full,
}

impl InputMode {
    /// The mode called `name`: `off`, `pointer`, `keyboard` or `full`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "off" => Some(Self::Off),
            "pointer" => Some(Self::Pointer),
            "keyboard" => Some(Self::Keyboard),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    /// The name [`Self::parse`] reads back.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Pointer => "pointer",
            Self::Keyboard => "keyboard",
            Self::Full => "full",
        }
    }

    /// Whether this mode sends anything to the host.
    #[must_use]
    pub fn sends_anything(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Whether this mode sends the pointer.
    #[must_use]
    pub fn sends_pointer(self) -> bool {
        matches!(self, Self::Pointer | Self::Full)
    }

    /// Whether this mode sends the keyboard.
    #[must_use]
    pub fn sends_keyboard(self) -> bool {
        matches!(self, Self::Keyboard | Self::Full)
    }

    /// Whether this mode sends the action.
    #[must_use]
    pub fn allows(self, action: &InputAction) -> bool {
        match action {
            InputAction::Key { .. } => self.sends_keyboard(),
            InputAction::MotionAbsolute { .. }
            | InputAction::MotionRelative { .. }
            | InputAction::Button { .. }
            | InputAction::Scroll { .. } => self.sends_pointer(),
            // The pads and the touch are their own devices: they are not the viewer's input.
            InputAction::Gamepad { .. }
            | InputAction::GamepadRemoved { .. }
            | InputAction::Touch { .. }
            | InputAction::Pen { .. } => true,
        }
    }
}

/// The state the client's input threads share.
#[derive(Clone, Debug)]
pub struct Shared {
    /// The host's cursor shape, for the input thread.
    pub cursor: CursorSlot,
    /// What the host's game wants of the pads' motors, for the gamepad thread.
    pub rumble: crate::gamepad::RumbleSlot,
    /// The viewer window, for the local shortcuts.
    pub window: Arc<crate::WindowControl>,
}

/// Starts the input thread: events of the viewer window in, sealed input datagrams to `peer` out.
/// Returns `None`, after saying why on the standard error, when input cannot run.
#[must_use]
pub fn spawn(
    local: SocketAddr,
    peer: SocketAddr,
    cipher: PacketCipher,
    running: Arc<AtomicBool>,
    window: Window,
    shared: Shared,
    mode: InputMode,
) -> Option<thread::JoinHandle<()>> {
    let mut shown = None;
    let sender = match InputSender::bind(local, peer, cipher) {
        Ok(sender) => Arc::new(Mutex::new(sender)),
        Err(e) => {
            eprintln!("lumen-client: input socket unavailable: {e}");
            return None;
        }
    };
    let pads = crate::gamepad::spawn(
        Arc::clone(&sender),
        Arc::clone(&running),
        shared.rumble.clone(),
        Arc::clone(&shared.window),
    );
    let touch = crate::touch::spawn(Arc::clone(&sender), Arc::clone(&running));
    let cursor = shared.cursor;
    let control = shared.window;
    match window {
        Window::X11(window) => {
            let mut source = match WindowEventSource::open(None) {
                Ok(source) => source,
                Err(e) => {
                    eprintln!("lumen-client: input capture unavailable: {e}");
                    return None;
                }
            };
            Some(thread::spawn(move || {
                let mut chords = Chords::default();
                let mut captured = false;
                while running.load(Ordering::SeqCst) {
                    // The renderer recreates its window when the picture changes size.
                    if let Err(e) = source.set_window(window.load(Ordering::Acquire)) {
                        eprintln!("lumen-client: input capture stopped: {e}");
                        break;
                    }
                    // The capture holds the pointer in the window.
                    let wanted = control.captured.load(Ordering::Relaxed);
                    if wanted != captured {
                        captured = wanted;
                        if let Err(e) = source.set_pointer_grabbed(captured) {
                            eprintln!("lumen-client: cannot hold the pointer: {e}");
                        }
                    }
                    if let Some(ControlMessage::Cursor {
                        hot_x,
                        hot_y,
                        width,
                        height,
                        argb,
                        ..
                    }) = new_cursor(&cursor, &mut shown)
                        && let Err(e) = source.set_cursor(hot_x, hot_y, width, height, &argb)
                    {
                        eprintln!("lumen-client: cannot show the host's cursor: {e}");
                    }
                    match source.poll_action(POLL) {
                        Ok(Some(action)) => forward(&sender, &control, &mut chords, mode, action),
                        Ok(None) => {}
                        Err(e) => {
                            eprintln!("lumen-client: input capture stopped: {e}");
                            break;
                        }
                    }
                }
                let _ = pads.join();
                let _ = touch.join();
            }))
        }
        Window::Wayland(conn, surface) => {
            let mut source = match WaylandSource::connect(&conn, surface.as_ref()) {
                Ok(source) => source,
                Err(e) => {
                    eprintln!("lumen-client: input capture unavailable: {e}");
                    return None;
                }
            };
            Some(thread::spawn(move || {
                let mut chords = Chords::default();
                let mut captured = false;
                while running.load(Ordering::SeqCst) && !source.is_closed() {
                    // The capture locks the pointer in the window.
                    let wanted = control.captured.load(Ordering::Relaxed);
                    if wanted != captured {
                        captured = wanted;
                        if let Err(e) = source.set_pointer_locked(captured) {
                            eprintln!("lumen-client: cannot lock the pointer: {e}");
                        }
                    }
                    if let Some(ControlMessage::Cursor {
                        hot_x,
                        hot_y,
                        width,
                        height,
                        argb,
                        ..
                    }) = new_cursor(&cursor, &mut shown)
                        && let Err(e) = source.set_cursor(hot_x, hot_y, width, height, &argb)
                    {
                        eprintln!("lumen-client: cannot show the host's cursor: {e}");
                    }
                    match source.poll_event() {
                        Some(event) => {
                            forward(&sender, &control, &mut chords, mode, to_action(event.kind));
                        }
                        None => thread::sleep(WAYLAND_IDLE),
                    }
                }
                let _ = pads.join();
                let _ = touch.join();
            }))
        }
    }
}

/// Sends a viewer-window action to the host, unless it is a chord or the mode forbids it.
///
/// A chord never reaches the host: the client answers it itself. The chord's key is swallowed
/// on the press and on the release; the modifiers are sent, so the host never sees them stuck.
fn forward(
    sender: &Mutex<InputSender>,
    window: &crate::WindowControl,
    chords: &mut Chords,
    mode: InputMode,
    action: InputAction,
) {
    if let Some((chord, pressed)) = chords.take(action) {
        if pressed {
            act_on(window, chord);
        }
        return;
    }
    if !mode.allows(&action) {
        return;
    }
    send(sender, action);
}

/// What the client does with a chord.
pub(crate) fn act_on(window: &crate::WindowControl, chord: Chord) {
    match chord {
        Chord::Fullscreen => {
            window.fullscreen.fetch_xor(true, Ordering::Relaxed);
        }
        Chord::Capture => {
            window.captured.fetch_xor(true, Ordering::Relaxed);
        }
        Chord::Disconnect => {
            window.disconnect.store(true, Ordering::SeqCst);
        }
        Chord::Hud => {
            window.hud.fetch_xor(true, Ordering::Relaxed);
        }
    }
}

fn send(sender: &Mutex<InputSender>, action: InputAction) {
    // The stamp is read as late as possible, so it marks the moment the event went out.
    if let Ok(mut sender) = sender.lock()
        && let Err(e) = sender.send(action, monotonic_us())
    {
        eprintln!("lumen-client: input send failed: {e}");
    }
}

/// The wire action of a seat event. Relative motion carries `NaN` positions.
fn to_action(kind: InputEventKind) -> InputAction {
    match kind {
        InputEventKind::Key { keycode, pressed } => InputAction::Key { keycode, pressed },
        InputEventKind::Motion { dx, dy, x, y } => {
            if x.is_finite() && y.is_finite() {
                InputAction::MotionAbsolute { x, y }
            } else {
                InputAction::MotionRelative { dx, dy }
            }
        }
        InputEventKind::Button { button, pressed } => InputAction::Button { button, pressed },
        InputEventKind::Scroll { dx, dy } => InputAction::Scroll { dx, dy },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key action of a key.
    fn key(keycode: u32, pressed: bool) -> InputAction {
        InputAction::Key { keycode, pressed }
    }

    #[test]
    fn the_pointer_only_mode_sends_no_keys() {
        let key = key(30, true);
        let motion = InputAction::MotionRelative { dx: 1.0, dy: 0.0 };
        let button = InputAction::Button {
            button: 0x110,
            pressed: true,
        };
        assert!(!InputMode::Pointer.allows(&key));
        assert!(InputMode::Pointer.allows(&motion));
        assert!(InputMode::Pointer.allows(&button));
        assert!(InputMode::Keyboard.allows(&key));
        assert!(!InputMode::Keyboard.allows(&motion));
        assert!(!InputMode::Keyboard.allows(&button));
        assert!(!InputMode::Off.allows(&key));
        assert!(!InputMode::Off.allows(&motion));
        assert!(InputMode::Full.allows(&key));
        assert!(InputMode::Full.allows(&motion));
    }

    #[test]
    fn the_modifiers_are_not_chords_and_go_to_the_host() {
        let mut chords = Chords::default();
        assert_eq!(
            chords.take(key(29, true)),
            None,
            "the Ctrl goes to the host"
        );
        assert_eq!(
            chords.take(key(42, true)),
            None,
            "the Shift goes to the host"
        );
    }

    #[test]
    fn a_chord_acts_on_the_window_and_is_swallowed() {
        let window = crate::WindowControl::default();
        let mut chords = Chords::default();
        // Ctrl+Alt+Shift+Q holds the pointer in the window.
        chords.take(key(29, true));
        chords.take(key(56, true));
        chords.take(key(42, true));
        assert_eq!(
            chords.take(key(16, true)),
            Some((Chord::Capture, true)),
            "the chord is answered"
        );
        act_on(&window, Chord::Capture);
        assert!(window.captured.load(Ordering::Relaxed));
        // The release is swallowed too.
        assert_eq!(chords.take(key(16, false)), Some((Chord::Capture, false)));
    }
}
