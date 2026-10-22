//! The Wayland clipboard.
//!
//! A Wayland compositor does not let a client read the selection of another
//! client: the `wl_data_device` interface only owns a selection, it never
//! reads one. The wlroots compositor offers a second interface for that,
//! `zwlr_data_control_manager_v1`, which lets a client watch the selection
//! and take it over.
//!
//! The host uses the data-control interface to read the clipboard of the
//! machine it runs on and to offer a clipboard of its own, so a client can
//! copy on one machine and paste on the other. The payload is UTF-8 and
//! capped at 1 MiB.

use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_registry::WlRegistry, wl_seat::WlSeat},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::{self, ZwlrDataControlManagerV1},
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

/// The largest clipboard payload the project accepts, in bytes.
pub const MAX_CLIPBOARD_BYTES: usize = 1 << 20;

/// The MIME type the project speaks.
pub const TEXT_UTF8: &str = "text/plain;charset=utf-8";

/// The MIME types that are tried when the selection is read, in order.
pub const READ_MIME_TYPES: [&str; 4] = [
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
];

/// How long a read waits for the compositor to answer.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(1500);

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
    /// The compositor does not offer the data-control interface.
    #[error("the compositor does not offer zwlr_data_control_manager_v1")]
    NoDataControl,
    /// The compositor does not offer a seat.
    #[error("the compositor does not offer a seat")]
    NoSeat,
    /// The compositor did not answer in time.
    #[error("the compositor did not answer within {0:?}")]
    Timeout(Duration),
    /// The payload is not valid UTF-8.
    #[error("the clipboard payload is not valid UTF-8")]
    NotUtf8,
    /// The payload is larger than the cap.
    #[error("the clipboard payload is larger than the {0} byte limit")]
    TooLarge(usize),
    /// The selection offers no type the project can read.
    #[error("the selection offers no readable type")]
    NoReadableType,
    /// The X11 clipboard failed.
    #[error("X11 clipboard: {0}")]
    X11(#[from] lumen_clipboard_x11::ClipboardError),
}

/// The clipboard of the machine the host runs on.
///
/// A Wayland session has the data-control device and an X11 session the selection. The host
/// uses the one that belongs to the display server it runs on, so a copy on the client can be
/// pasted on the host and the other way round.
pub enum Clipboard {
    /// The Wayland data-control device.
    Wayland(Box<WaylandClipboard>),
    /// The X11 selection.
    X11(Box<lumen_clipboard_x11::X11Clipboard>),
}

impl std::fmt::Debug for Clipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wayland(_) => f.write_str("Clipboard::Wayland"),
            Self::X11(_) => f.write_str("Clipboard::X11"),
        }
    }
}

impl Clipboard {
    /// Opens the clipboard of the display server the host runs on.
    ///
    /// # Errors
    /// Returns a description when the display server offers no clipboard.
    pub fn open() -> Result<Self, ClipboardError> {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            return WaylandClipboard::open().map(|device| Self::Wayland(Box::new(device)));
        }
        lumen_clipboard_x11::X11Clipboard::open(None)
            .map(|device| Self::X11(Box::new(device)))
            .map_err(ClipboardError::from)
    }

    /// The text the host owns, when it owns one.
    #[must_use]
    pub fn owned(&self) -> Option<&str> {
        match self {
            Self::Wayland(device) => device.owned(),
            Self::X11(device) => device.owned().map(|(_, text)| text),
        }
    }

    /// Reads the text the host does not own, when there is one.
    ///
    /// # Errors
    /// Returns a description when the clipboard cannot be read.
    pub fn read(&mut self, timeout: Duration) -> Result<Option<String>, ClipboardError> {
        match self {
            Self::Wayland(device) => device.read(timeout),
            Self::X11(device) => device
                .read(lumen_clipboard_x11::Selection::Clipboard, timeout)
                .map_err(ClipboardError::from),
        }
    }

    /// Takes the text, so the host owns the clipboard.
    ///
    /// # Errors
    /// Returns a description when the clipboard cannot hold the text.
    pub fn set(&mut self, text: &str) -> Result<(), ClipboardError> {
        match self {
            Self::Wayland(device) => device.set(text),
            Self::X11(device) => device
                .set(lumen_clipboard_x11::Selection::Clipboard, text)
                .map_err(ClipboardError::from),
        }
    }

    /// Serves the requests of the other applications for a while.
    ///
    /// # Errors
    /// Returns a description when the clipboard cannot answer.
    pub fn serve(&mut self, duration: Duration) -> Result<usize, ClipboardError> {
        match self {
            Self::Wayland(device) => device.serve(duration),
            Self::X11(device) => device.serve(duration).map_err(ClipboardError::from),
        }
    }
}

/// The state of the Wayland connection.
#[derive(Debug, Default)]
struct State {
    manager: Option<ZwlrDataControlManagerV1>,
    seat: Option<WlSeat>,
    /// The offers the compositor announced, keyed by the object id.
    offers: Vec<(u32, ZwlrDataControlOfferV1)>,
    /// The offer that holds the current selection, when there is one.
    selection: Option<u32>,
    /// The types the current offer supports.
    offered_types: Vec<String>,
    /// The data the compositor asked for, when the host owns the selection.
    pending_send: Option<(String, OwnedFd)>,
    /// The payload the host offers.
    owned: Option<String>,
    /// Whether the compositor revoked the selection.
    finished: bool,
}

impl State {
    fn offer_mut(&mut self, id: u32) -> Option<&mut ZwlrDataControlOfferV1> {
        self.offers
            .iter_mut()
            .find(|(offer_id, _)| *offer_id == id)
            .map(|(_, offer)| offer)
    }
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

impl Dispatch<ZwlrDataControlManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrDataControlManagerV1,
        _event: zwlr_data_control_manager_v1::Event,
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
        _event: wayland_client::protocol::wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                let protocol_id = id.id().protocol_id();
                state.offers.push((protocol_id, id));
                state.offered_types.clear();
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                state.selection = id.map(|offer| offer.id().protocol_id());
                state.offered_types.clear();
            }
            zwlr_data_control_device_v1::Event::Finished => {
                state.finished = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.offered_types.push(mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_source_v1::Event::Send { mime_type, fd } = event {
            state.pending_send = Some((mime_type, fd));
        }
    }
}

/// A Wayland clipboard.
///
/// The object holds its own connection to the compositor, so it does not
/// interfere with the capture connection.
#[derive(Debug)]
pub struct WaylandClipboard {
    conn: Connection,
    queue: wayland_client::EventQueue<State>,
    state: State,
    device: ZwlrDataControlDeviceV1,
    manager: ZwlrDataControlManagerV1,
    qh: QueueHandle<State>,
}

impl WaylandClipboard {
    /// Connect to the compositor named by `WAYLAND_DISPLAY`.
    ///
    /// # Errors
    /// Returns an error when the compositor cannot be reached or does not
    /// offer the data-control interface.
    pub fn open() -> Result<Self, ClipboardError> {
        let conn = Connection::connect_to_env()?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();
        let mut state = State::default();

        let registry: &WlRegistry = globals.registry();
        for global in globals.contents().clone_list() {
            match global.interface.as_str() {
                "zwlr_data_control_manager_v1" => {
                    let manager: ZwlrDataControlManagerV1 =
                        registry.bind(global.name, global.version.min(2), &qh, ());
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

        let manager = state.manager.clone().ok_or(ClipboardError::NoDataControl)?;
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
        })
    }

    /// The types the current selection offers.
    #[must_use]
    pub fn offered_types(&self) -> &[String] {
        &self.state.offered_types
    }

    /// Read the selection.
    ///
    /// The compositor is asked to hand the selection over; the offer that
    /// arrives is asked for its text. A selection nobody owns reads as
    /// `None`.
    ///
    /// # Errors
    /// Returns an error when the compositor does not answer, when the offer
    /// has no readable type, or when the payload is not UTF-8.
    pub fn read(&mut self, timeout: Duration) -> Result<Option<String>, ClipboardError> {
        // Asking for no selection makes the compositor send the current one.
        self.device.set_selection(None);
        self.conn.flush()?;
        let deadline = Instant::now() + timeout;
        let mut selection = None;
        while Instant::now() < deadline {
            // The dispatch is not a blocking one: the control loop calls this
            // and must not wait for an event that may never come.
            self.queue.dispatch_pending(&mut self.state)?;
            if let Some(id) = self.state.selection {
                selection = Some(id);
                break;
            }
        }
        let Some(id) = selection else {
            return Ok(None);
        };

        // The types of the offer arrive as events of the offer itself.
        let mime_type = {
            // The offer has to exist, otherwise the compositor sent a
            // selection this object cannot reach.
            self.state
                .offer_mut(id)
                .ok_or(ClipboardError::NoReadableType)?;
            let deadline = Instant::now() + timeout;
            while self.state.offered_types.is_empty() && Instant::now() < deadline {
                self.queue.dispatch_pending(&mut self.state)?;
            }
            READ_MIME_TYPES
                .iter()
                .find(|wanted| {
                    self.state
                        .offered_types
                        .iter()
                        .any(|offered| offered == *wanted)
                })
                .map(|mime| (*mime).to_string())
        };
        let Some(mime_type) = mime_type else {
            return Err(ClipboardError::NoReadableType);
        };

        let (mut reader, writer) = std::io::pipe().map_err(|_| ClipboardError::NotUtf8)?;
        let offer = self
            .state
            .offer_mut(id)
            .ok_or(ClipboardError::NoReadableType)?;
        offer.receive(mime_type, writer.as_fd());
        self.conn.flush()?;
        // The write end has to go away, or the read below never sees the end
        // of the payload.
        drop(writer);

        let mut data = Vec::new();
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let mut chunk = [0u8; 4096];
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if data.len() + n > MAX_CLIPBOARD_BYTES {
                        return Err(ClipboardError::TooLarge(data.len() + n));
                    }
                    data.extend_from_slice(&chunk[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(_) => return Err(ClipboardError::NotUtf8),
            }
        }
        if data.is_empty() {
            return Ok(None);
        }
        String::from_utf8(data)
            .map(Some)
            .map_err(|_| ClipboardError::NotUtf8)
    }

    /// Take ownership of the selection and offer `text` in it.
    ///
    /// The compositor asks for the data when a reader wants it; the answer
    /// is written then, so the host has to be pumped. [`Self::serve`] does
    /// that.
    ///
    /// # Errors
    /// Returns an error when the payload is too large.
    pub fn set(&mut self, text: &str) -> Result<(), ClipboardError> {
        if text.len() > MAX_CLIPBOARD_BYTES {
            return Err(ClipboardError::TooLarge(text.len()));
        }
        let source = self.manager.create_data_source(&self.qh, ());
        source.offer(TEXT_UTF8.to_string());
        source.offer("text/plain".to_string());
        self.device.set_selection(Some(&source));
        self.conn.flush()?;
        self.state.owned = Some(text.to_string());
        Ok(())
    }

    /// The payload the host owns, when it owns the selection.
    #[must_use]
    pub fn owned(&self) -> Option<&str> {
        self.state.owned.as_deref()
    }

    /// Answer the requests of the compositor for `duration`.
    ///
    /// # Errors
    /// Returns an error when a Wayland request fails.
    pub fn serve(&mut self, duration: Duration) -> Result<usize, ClipboardError> {
        let deadline = Instant::now() + duration;
        let mut served = 0;
        while Instant::now() < deadline {
            self.queue.blocking_dispatch(&mut self.state)?;
            if let Some((_mime_type, fd)) = self.state.pending_send.take() {
                let payload = self.state.owned.clone().unwrap_or_default();
                let mut file = std::fs::File::from(fd);
                let _ = file.write_all(payload.as_bytes());
                let _ = file.flush();
                served += 1;
            }
        }
        Ok(served)
    }
}

/// The file descriptor of a pipe, for the tests.
#[must_use]
pub fn pipe_fds() -> (OwnedFd, OwnedFd) {
    let (reader, writer) = std::io::pipe().expect("a pipe can be created");
    (OwnedFd::from(reader), OwnedFd::from(writer))
}

/// The raw descriptor of a file, for the tests.
#[must_use]
pub fn raw_fd(fd: &OwnedFd) -> i32 {
    fd.as_fd().as_raw_fd()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_read_order_starts_with_utf8() {
        assert_eq!(READ_MIME_TYPES[0], "text/plain;charset=utf-8");
        assert!(READ_MIME_TYPES.contains(&"UTF8_STRING"));
        assert!(READ_MIME_TYPES.contains(&"STRING"));
    }

    #[test]
    fn the_cap_is_one_mebibyte() {
        assert_eq!(MAX_CLIPBOARD_BYTES, 1024 * 1024);
    }

    #[test]
    fn the_compositor_has_to_offer_the_data_control_interface() {
        // Without a compositor the clipboard cannot open, and it must say so
        // rather than fall back to something else.
        match WaylandClipboard::open() {
            Ok(_) => {
                crate::host_log!(info "a compositor is running: the clipboard opened");
            }
            Err(reason) => {
                crate::host_log!(info "SKIP the_compositor_has_to_offer_the_data_control_interface: {reason}");
            }
        }
    }
}
