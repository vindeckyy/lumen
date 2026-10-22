//! The Wayland clipboard of the client.
//!
//! The client owns a `wl_data_device`, so it can hold the selection of the
//! client machine. The host reads that selection through its data-control
//! interface, and the payload travels over the control channel as a
//! [`ControlMessage::Clipboard`].
//!
//! The `wl_data_device_manager_v1` interface is the modern one, but the
//! `wayland.xml` that ships with the pinned `wayland-client` does not carry
//! it yet, and neither does every compositor. The older
//! `wl_data_device_manager` is what the compositor on this machine offers,
//! so the client uses that one. The two are interchangeable for what the
//! client does: it owns a data source and hands it to the seat.
//!
//! The payload is UTF-8 and capped at 1 MiB, the same cap the host uses.

use std::io::Write;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_data_device::{self, WlDataDevice},
        wl_data_device_manager::WlDataDeviceManager,
        wl_data_source::{self, WlDataSource},
        wl_registry::WlRegistry,
        wl_seat::WlSeat,
    },
};

/// The largest clipboard payload the project accepts, in bytes.
pub const MAX_CLIPBOARD_BYTES: usize = 1 << 20;

/// The MIME type the project speaks.
pub const TEXT_UTF8: &str = "text/plain;charset=utf-8";

/// The MIME types the client offers, in order.
pub const OFFER_MIME_TYPES: [&str; 3] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"];

/// Errors of the clipboard.
#[derive(Debug, thiserror::Error)]
pub enum ClipboardError {
    /// The Wayland display could not be reached.
    #[error("cannot connect to the Wayland display: {0}")]
    Connect(#[from] wayland_client::ConnectError),
    /// The Wayland globals could not be read.
    #[error("Wayland globals: {0}")]
    Globals(#[from] wayland_client::globals::GlobalError),
    /// A Wayland request failed.
    #[error("Wayland request: {0}")]
    Dispatch(#[from] wayland_client::DispatchError),
    /// The Wayland connection failed.
    #[error("Wayland connection: {0}")]
    Wayland(#[from] wayland_client::backend::WaylandError),
    /// The compositor does not offer the data-device interface.
    #[error("the compositor does not offer wl_data_device_manager_v1")]
    NoDataDevice,
    /// The compositor does not offer a seat.
    #[error("the compositor does not offer a seat")]
    NoSeat,
    /// The payload is larger than the cap.
    #[error("the clipboard payload is larger than the {0} byte limit")]
    TooLarge(usize),
    /// The X11 clipboard failed.
    #[error("X11 clipboard: {0}")]
    X11(#[from] lumen_clipboard_x11::ClipboardError),
    /// The compositor's data-control device failed.
    #[error("the compositor's clipboard: {0}")]
    Control(#[from] lumen_host_service::clipboard::ClipboardError),
}

/// The clipboard of the machine the client runs on.
///
/// A Wayland session has the data device and an X11 session the selection. The client uses the
/// one that belongs to the display server it runs on, so a copy on the host can be pasted on
/// the client and the other way round.
pub enum Clipboard {
    /// The Wayland clipboard of this machine.
    Wayland {
        /// The device that holds the selection, so the peer can read it.
        hold: WaylandClipboard,
        /// The device that reads what the user copied here. It is missing when the compositor
        /// offers no data-control interface, and then the client only holds a selection.
        read: Option<lumen_host_service::clipboard::WaylandClipboard>,
    },
    /// The X11 selection of this machine.
    X11(lumen_clipboard_x11::X11Clipboard),
}

/// How often the client reads its own clipboard.
const READ_POLL: Duration = Duration::from_millis(100);

/// Reads the clipboard of this machine on a thread of its own.
///
/// The read waits for the display server to answer, so it cannot run in the control stage:
/// the stage has to keep the clock exchange going. The text is put in the slot, and the
/// control stage sends it to the host when it changed.
#[must_use]
pub fn spawn_reader(running: Arc<AtomicBool>) -> Arc<Mutex<Option<String>>> {
    let slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let shared = Arc::clone(&slot);
    thread::spawn(move || {
        let mut device = match Clipboard::open() {
            Ok(device) => Some(device),
            Err(reason) => {
                eprintln!("lumen-client: no clipboard available: {reason}");
                None
            }
        };
        while running.load(Ordering::SeqCst) {
            if let Some(device) = device.as_mut() {
                let text = device.read(Duration::from_millis(20)).ok().flatten();
                if let Some(text) = text {
                    let mut guard = match shared.lock() {
                        Ok(guard) => guard,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    if guard.as_deref() != Some(text.as_str()) {
                        *guard = Some(text);
                    }
                }
            }
            thread::sleep(READ_POLL);
        }
    });
    slot
}

impl std::fmt::Debug for Clipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wayland { .. } => f.write_str("Clipboard::Wayland"),
            Self::X11(_) => f.write_str("Clipboard::X11"),
        }
    }
}

impl Clipboard {
    /// Opens the clipboard of the display server the client runs on.
    ///
    /// # Errors
    /// Returns a description when the display server offers no clipboard.
    pub fn open() -> Result<Self, ClipboardError> {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            let hold = WaylandClipboard::open()?;
            let read = lumen_host_service::clipboard::WaylandClipboard::open().ok();
            return Ok(Self::Wayland { hold, read });
        }
        lumen_clipboard_x11::X11Clipboard::open(None)
            .map(Self::X11)
            .map_err(ClipboardError::from)
    }

    /// The text the client owns, when it owns one.
    #[must_use]
    pub fn owned(&self) -> Option<&str> {
        match self {
            Self::Wayland { hold, .. } => hold.owned(),
            Self::X11(device) => device.owned().map(|(_, text)| text),
        }
    }

    /// Whether the client may own the clipboard.
    #[must_use]
    pub fn may_own(&self) -> bool {
        match self {
            Self::Wayland { hold, .. } => hold.may_own(),
            Self::X11(_) => true,
        }
    }

    /// Reads the text the user copied here, when there is one.
    ///
    /// # Errors
    /// Returns a description when the clipboard cannot be read.
    pub fn read(&mut self, timeout: Duration) -> Result<Option<String>, ClipboardError> {
        match self {
            Self::Wayland { read, .. } => match read {
                Some(device) => device.read(timeout).map_err(ClipboardError::Control),
                None => Ok(None),
            },
            Self::X11(device) => device
                .read(lumen_clipboard_x11::Selection::Clipboard, timeout)
                .map_err(ClipboardError::from),
        }
    }

    /// Takes the text, so the client owns the clipboard.
    ///
    /// # Errors
    /// Returns a description when the clipboard cannot hold the text.
    pub fn set(&mut self, text: &str) -> Result<(), ClipboardError> {
        match self {
            Self::Wayland { hold, .. } => hold.set(text),
            Self::X11(device) => device
                .set(lumen_clipboard_x11::Selection::Clipboard, text)
                .map_err(ClipboardError::from),
        }
    }

    /// Gives the text up.
    ///
    /// # Errors
    /// Returns a description when the clipboard cannot be cleared.
    pub fn clear(&mut self) -> Result<(), ClipboardError> {
        match self {
            Self::Wayland { hold, .. } => hold.clear(),
            Self::X11(device) => device
                .set(lumen_clipboard_x11::Selection::Clipboard, "")
                .map_err(ClipboardError::from),
        }
    }

    /// Serves the requests of the other applications for a while.
    ///
    /// # Errors
    /// Returns a description when the clipboard cannot answer.
    pub fn serve(&mut self, duration: Duration) -> Result<usize, ClipboardError> {
        match self {
            Self::Wayland { hold, .. } => hold.serve(duration),
            Self::X11(device) => device.serve(duration).map_err(ClipboardError::from),
        }
    }
}

/// The state of the Wayland connection.
#[derive(Debug, Default)]
struct State {
    manager: Option<WlDataDeviceManager>,
    seat: Option<WlSeat>,
    /// The payload the compositor asked for.
    pending_send: Option<(String, OwnedFd)>,
    /// The payload the client offers.
    owned: Option<String>,
    /// The serial of the last focus event of the seat.
    ///
    /// The legacy `wl_data_device` interface needs the serial of the event
    /// that gave the client the focus, because only the focused client may
    /// own the selection.
    focus_serial: Option<u32>,
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegistry,
        _event: <WlRegistry as Proxy>::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlDataDeviceManager, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlDataDeviceManager,
        _event: <WlDataDeviceManager as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        _event: <WlSeat as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlDataDevice, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &WlDataDevice,
        event: wl_data_device::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_data_device::Event::Enter { serial, .. } = event {
            state.focus_serial = Some(serial);
        }
    }
}

impl Dispatch<WlDataSource, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &WlDataSource,
        event: wl_data_source::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Send { mime_type, fd } => {
                state.pending_send = Some((mime_type, fd));
            }
            wl_data_source::Event::Cancelled => {
                state.owned = None;
            }
            _ => {}
        }
    }
}

/// The clipboard of the client.
#[derive(Debug)]
pub struct WaylandClipboard {
    conn: Connection,
    queue: wayland_client::EventQueue<State>,
    state: State,
    device: WlDataDevice,
    manager: WlDataDeviceManager,
    qh: QueueHandle<State>,
    /// Whether the selection was already offered to the compositor.
    offered: bool,
}

impl WaylandClipboard {
    /// Connect to the compositor named by `WAYLAND_DISPLAY`.
    ///
    /// # Errors
    /// Returns an error when the compositor cannot be reached or does not
    /// offer the data-device interface.
    pub fn open() -> Result<Self, ClipboardError> {
        let conn = Connection::connect_to_env()?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();
        let mut state = State::default();

        let registry: &WlRegistry = globals.registry();
        for global in globals.contents().clone_list() {
            match global.interface.as_str() {
                "wl_data_device_manager" => {
                    let manager: WlDataDeviceManager =
                        registry.bind(global.name, global.version.min(3), &qh, ());
                    state.manager = Some(manager);
                }
                "wl_seat" => {
                    let seat: WlSeat = registry.bind(global.name, global.version.min(7), &qh, ());
                    state.seat = Some(seat);
                }
                _ => {}
            }
        }
        queue.roundtrip(&mut state)?;

        let manager = state.manager.clone().ok_or(ClipboardError::NoDataDevice)?;
        let seat = state.seat.clone().ok_or(ClipboardError::NoSeat)?;
        let device = manager.get_data_device(&seat, &qh, ());
        queue.roundtrip(&mut state)?;

        Ok(Self {
            conn,
            queue,
            state,
            device,
            manager,
            qh,
            offered: false,
        })
    }

    /// The payload the client offers, when it owns the selection.
    #[must_use]
    pub fn owned(&self) -> Option<&str> {
        self.state.owned.as_deref()
    }

    /// Whether the client may own the selection.
    ///
    /// The legacy `wl_data_device` interface only lets the client that has
    /// the focus own the selection, so the client has to wait for the focus
    /// event of the seat.
    #[must_use]
    pub fn may_own(&self) -> bool {
        self.state.focus_serial.is_some()
    }

    /// Take ownership of the selection and offer `text` in it.
    ///
    /// When the client does not have the focus yet, the text is kept and
    /// offered as soon as the focus arrives.
    ///
    /// # Errors
    /// Returns an error when the payload is too large or a Wayland request
    /// fails.
    pub fn set(&mut self, text: &str) -> Result<(), ClipboardError> {
        if text.len() > MAX_CLIPBOARD_BYTES {
            return Err(ClipboardError::TooLarge(text.len()));
        }
        self.state.owned = Some(text.to_string());
        self.apply_selection()
    }

    /// Give the selection up.
    ///
    /// # Errors
    /// Returns an error when a Wayland request fails.
    pub fn clear(&mut self) -> Result<(), ClipboardError> {
        self.state.owned = None;
        let Some(serial) = self.state.focus_serial else {
            return Ok(());
        };
        self.device.set_selection(None, serial);
        self.conn.flush()?;
        Ok(())
    }

    /// Offer the payload the client holds, when it may own the selection.
    fn apply_selection(&mut self) -> Result<(), ClipboardError> {
        let Some(serial) = self.state.focus_serial else {
            return Ok(());
        };
        let Some(text) = self.state.owned.clone() else {
            return Ok(());
        };
        let source = self.manager.create_data_source(&self.qh, ());
        for mime_type in OFFER_MIME_TYPES {
            source.offer(mime_type.to_string());
        }
        self.device.set_selection(Some(&source), serial);
        self.conn.flush()?;
        let _ = text;
        Ok(())
    }

    /// Answer the requests of the compositor for `duration`.
    ///
    /// # Errors
    /// Returns an error when a Wayland request fails.
    pub fn serve(&mut self, duration: Duration) -> Result<usize, ClipboardError> {
        let deadline = std::time::Instant::now() + duration;
        let mut served = 0;
        while std::time::Instant::now() < deadline {
            self.queue.blocking_dispatch(&mut self.state)?;
            if let Some((_mime_type, fd)) = self.state.pending_send.take() {
                let payload = self.state.owned.clone().unwrap_or_default();
                let mut file = std::fs::File::from(fd);
                let _ = file.write_all(payload.as_bytes());
                let _ = file.flush();
                served += 1;
            }
            // The client may have got the focus since the last round, so the
            // selection it wants to hold is offered now.
            if self.state.focus_serial.is_some() && !self.offered {
                self.apply_selection()?;
                self.offered = true;
            }
        }
        Ok(served)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cap_is_one_mebibyte() {
        assert_eq!(MAX_CLIPBOARD_BYTES, 1024 * 1024);
    }

    #[test]
    fn the_offer_starts_with_utf8() {
        assert_eq!(OFFER_MIME_TYPES[0], "text/plain;charset=utf-8");
        assert!(OFFER_MIME_TYPES.contains(&"UTF8_STRING"));
    }

    #[test]
    fn the_compositor_has_to_offer_the_data_device_interface() {
        match WaylandClipboard::open() {
            Ok(_) => {
                eprintln!("a compositor is running: the clipboard opened");
            }
            Err(reason) => {
                eprintln!("SKIP the_compositor_has_to_offer_the_data_device_interface: {reason}");
            }
        }
    }
}
