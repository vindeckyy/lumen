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
    /// The X11 window id the renderer publishes; it changes when the picture changes size.
    X11(Arc<AtomicU32>),
    /// The renderer's Wayland connection.
    Wayland(wayland_client::Connection),
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
                while running.load(Ordering::SeqCst) {
                    // The renderer recreates its window when the picture changes size.
                    if let Err(e) = source.set_window(window.load(Ordering::Acquire)) {
                        eprintln!("lumen-client: input capture stopped: {e}");
                        break;
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
                        Ok(Some(action)) => forward(&sender, &control, action),
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
        Window::Wayland(conn) => {
            let mut source = match WaylandSource::connect(&conn, None) {
                Ok(source) => source,
                Err(e) => {
                    eprintln!("lumen-client: input capture unavailable: {e}");
                    return None;
                }
            };
            Some(thread::spawn(move || {
                while running.load(Ordering::SeqCst) && !source.is_closed() {
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
                        Some(event) => forward(&sender, &control, to_action(event.kind)),
                        None => thread::sleep(WAYLAND_IDLE),
                    }
                }
                let _ = pads.join();
                let _ = touch.join();
            }))
        }
    }
}

/// The evdev code of F11, the key that toggles fullscreen.
const KEY_F11: u32 = 87;

/// Sends a viewer-window action to the host, unless it is a local shortcut.
///
/// F11 toggles the window's fullscreen state and never reaches the host, press or release.
fn forward(sender: &Mutex<InputSender>, window: &crate::WindowControl, action: InputAction) {
    if let Some(fullscreen) = shortcut(action) {
        if fullscreen {
            window.fullscreen.fetch_xor(true, Ordering::Relaxed);
        }
        return;
    }
    send(sender, action);
}

/// `Some(toggle)` when `action` is the fullscreen key: `true` on its press.
fn shortcut(action: InputAction) -> Option<bool> {
    match action {
        InputAction::Key { keycode, pressed } if keycode == KEY_F11 => Some(pressed),
        _ => None,
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

    #[test]
    fn f11_is_the_only_local_shortcut() {
        let key = |keycode, pressed| InputAction::Key { keycode, pressed };
        assert_eq!(shortcut(key(KEY_F11, true)), Some(true));
        assert_eq!(
            shortcut(key(KEY_F11, false)),
            Some(false),
            "the release is swallowed too"
        );
        assert_eq!(shortcut(key(KEY_F11 + 1, true)), None);
        assert_eq!(
            shortcut(InputAction::Button {
                button: 0x110,
                pressed: true
            }),
            None
        );
    }
}
