//! The client's input path: X11 events leave as sealed datagrams for the host's input socket.
//!
//! It runs only when there is an X display to read. Under Xwayland, XI2 reports events only while
//! an X window has focus, so input aimed at native Wayland windows is not captured here.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use lumen_crypto::PacketCipher;
use lumen_input_x11::{RawConfig, RawEventSource};
use lumen_transport::input_channel::InputSender;

use crate::receive::monotonic_us;

/// How long one poll waits for an event before the thread looks at the running flag again.
const POLL: Duration = Duration::from_millis(20);

/// Starts the input thread: X events in, sealed input datagrams to `peer` out. Returns `None`,
/// after saying why on the standard error, when the X display or the socket is not available.
#[must_use]
pub fn spawn(
    local: SocketAddr,
    peer: SocketAddr,
    cipher: PacketCipher,
    running: Arc<AtomicBool>,
) -> Option<thread::JoinHandle<()>> {
    if std::env::var_os("DISPLAY").is_none() {
        eprintln!("lumen-client: input capture unavailable: no X display");
        return None;
    }
    let mut source = match RawEventSource::open(&RawConfig::default()) {
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
