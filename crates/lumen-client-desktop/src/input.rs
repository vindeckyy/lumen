//! The client's input path: the keys and pointer of the viewer window leave as sealed datagrams
//! for the host's input socket.
//!
//! Only the viewer's own X11 window is read: keys while it has the focus, the pointer while it is
//! over it. Positions are window pixels, which are the pixels of the picture the host serves, so
//! the host scales them to its screen.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::Duration;

use lumen_crypto::PacketCipher;
use lumen_input_x11::WindowEventSource;
use lumen_transport::input_channel::InputSender;

use crate::receive::monotonic_us;

/// How long one poll waits for an event before the thread looks at the running flag and the
/// window again.
const POLL: Duration = Duration::from_millis(20);

/// Starts the input thread: events of the viewer window in, sealed input datagrams to `peer` out.
/// `window` is the X11 window id the renderer publishes; `None` (Wayland presenter) means no
/// input. Returns `None`, after saying why on the standard error, when input cannot run.
#[must_use]
pub fn spawn(
    local: SocketAddr,
    peer: SocketAddr,
    cipher: PacketCipher,
    running: Arc<AtomicBool>,
    window: Option<Arc<AtomicU32>>,
) -> Option<thread::JoinHandle<()>> {
    let Some(window) = window else {
        eprintln!("lumen-client: input capture needs the X11 presenter (--x11)");
        return None;
    };
    let mut source = match WindowEventSource::open(None) {
        Ok(source) => source,
        Err(e) => {
            eprintln!("lumen-client: input capture unavailable: {e}");
            return None;
        }
    };
    let mut sender = match InputSender::bind(local, peer, cipher) {
        Ok(sender) => sender,
        Err(e) => {
            eprintln!("lumen-client: input socket unavailable: {e}");
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
            match source.poll_action(POLL) {
                Ok(Some(action)) => {
                    // The stamp is read as late as possible, so it marks the moment the event went out.
                    if let Err(e) = sender.send(action, monotonic_us()) {
                        eprintln!("lumen-client: input send failed: {e}");
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    eprintln!("lumen-client: input capture stopped: {e}");
                    break;
                }
            }
        }
    }))
}
