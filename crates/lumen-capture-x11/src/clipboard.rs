//! The X11 clipboard: the CLIPBOARD and PRIMARY selections.
//!
//! X11 has no single clipboard. A *selection* is a named piece of data that
//! one application offers and another asks for. Two selections matter for a
//! remote desktop:
//!
//! * `CLIPBOARD` holds what the user copied with the keyboard.
//! * `PRIMARY` holds what the user selected with the mouse.
//!
//! Reading a selection means asking the owner to convert it: the requestor
//! sends `ConvertSelection` and the owner answers with the data in a
//! property on the requestor's window, followed by a `SelectionNotify`
//! event. Writing a selection means taking ownership of it and answering the
//! `SelectionRequest` events of whoever asks.
//!
//! The payloads are UTF-8 and capped at 1 MiB, the same cap the control
//! channel uses.

use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::errors::{ConnectionError, ReplyError, ReplyOrIdError};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    self, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// The largest clipboard payload the project accepts, in bytes.
pub const MAX_CLIPBOARD_BYTES: usize = 1 << 20;

/// How long a read waits for the selection owner to answer.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(1500);

/// The "no atom" value, which the X protocol uses for "no property".
const NO_ATOM: xproto::Atom = 0;

/// The `CurrentTime` timestamp, which the X protocol defines as zero.
const CURRENT_TIME: u32 = 0;

/// The X selection to work with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Selection {
    /// `CLIPBOARD`: what the user copied.
    Clipboard,
    /// `PRIMARY`: what the user selected with the mouse.
    Primary,
}

impl Selection {
    /// The name of the X atom of the selection.
    #[must_use]
    pub fn atom_name(self) -> &'static str {
        match self {
            Self::Clipboard => "CLIPBOARD",
            Self::Primary => "PRIMARY",
        }
    }

    /// Every selection this module handles.
    #[must_use]
    pub fn all() -> [Selection; 2] {
        [Self::Clipboard, Self::Primary]
    }
}

/// The target types a clipboard payload can be requested as.
///
/// The list is in the order the reader tries them: UTF-8 first, because that
/// is what the project speaks, then the older Latin-1 `STRING` and `TEXT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// `UTF8_STRING`.
    Utf8String,
    /// `STRING`.
    String,
    /// `TEXT`.
    Text,
    /// `TARGETS`: the list of types the owner supports.
    Targets,
}

impl Target {
    /// The name of the X atom of the target.
    #[must_use]
    pub fn atom_name(self) -> &'static str {
        match self {
            Self::Utf8String => "UTF8_STRING",
            Self::String => "STRING",
            Self::Text => "TEXT",
            Self::Targets => "TARGETS",
        }
    }

    /// The targets a reader tries, in order.
    #[must_use]
    pub fn read_order() -> [Target; 3] {
        [Self::Utf8String, Self::String, Self::Text]
    }

    /// The targets an owner offers, in order.
    #[must_use]
    pub fn offer_order() -> [Target; 4] {
        [Self::Utf8String, Self::String, Self::Text, Self::Targets]
    }
}

/// Errors of the clipboard.
#[derive(Debug, thiserror::Error)]
pub enum ClipboardError {
    /// The X connection failed.
    #[error("X connection: {0}")]
    Connection(#[from] ConnectionError),
    /// The X display could not be reached.
    #[error("cannot connect to the X display: {0}")]
    Connect(#[from] x11rb::errors::ConnectError),
    /// An X request failed.
    #[error("X request: {0}")]
    Reply(#[from] ReplyError),
    /// An X resource id could not be allocated.
    #[error("X resource id: {0}")]
    Id(#[from] ReplyOrIdError),
    /// The owner did not answer in time.
    #[error("the selection owner did not answer within {0:?}")]
    Timeout(Duration),
    /// The payload is not valid UTF-8.
    #[error("the clipboard payload is not valid UTF-8")]
    NotUtf8,
    /// The payload is larger than the cap.
    #[error("the clipboard payload is larger than the {0} byte limit")]
    TooLarge(usize),
    /// The clipboard is empty.
    #[error("the clipboard is empty")]
    Empty,
    /// The display has no such screen.
    #[error("the X display has no screen {0}")]
    NoScreen(usize),
}

/// The atoms the clipboard needs.
#[derive(Debug, Clone, Copy)]
struct Atoms {
    clipboard: xproto::Atom,
    primary: xproto::Atom,
    utf8_string: xproto::Atom,
    string: xproto::Atom,
    text: xproto::Atom,
    targets: xproto::Atom,
    /// The property the reader receives the data in.
    payload: xproto::Atom,
    /// The property an owner falls back to when the requestor names none.
    fallback: xproto::Atom,
}

/// An X11 clipboard.
///
/// The object owns one small window, which is the window the selections are
/// read into and the window that owns a selection this object writes.
#[derive(Debug)]
pub struct X11Clipboard {
    conn: RustConnection,
    root: xproto::Window,
    window: xproto::Window,
    atoms: Atoms,
    /// The payload this object owns, when it took ownership of a selection.
    owned: Option<(Selection, String)>,
}

impl X11Clipboard {
    /// Connect to `display`, or to `DISPLAY` when `None`.
    ///
    /// # Errors
    /// Returns an error when the display cannot be reached or the window and
    /// the atoms cannot be created.
    pub fn open(display: Option<&str>) -> Result<Self, ClipboardError> {
        let (conn, screen_num) = x11rb::connect(display)?;
        let root = conn
            .setup()
            .roots
            .get(screen_num)
            .map(|screen| screen.root)
            .ok_or(ClipboardError::NoScreen(screen_num))?;
        let atoms = Atoms {
            clipboard: intern(&conn, "CLIPBOARD")?,
            primary: intern(&conn, "PRIMARY")?,
            utf8_string: intern(&conn, "UTF8_STRING")?,
            string: intern(&conn, "STRING")?,
            text: intern(&conn, "TEXT")?,
            targets: intern(&conn, "TARGETS")?,
            payload: intern(&conn, "LUMEN_CLIPBOARD")?,
            fallback: intern(&conn, "LUMEN_CLIPBOARD_FALLBACK")?,
        };
        let window = conn.generate_id()?;
        conn.create_window(
            0,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::COPY_FROM_PARENT,
            0,
            &CreateWindowAux::new(),
        )?
        .check()?;
        Ok(Self {
            conn,
            root,
            window,
            atoms,
            owned: None,
        })
    }

    /// The window the selections are read into and written from.
    #[must_use]
    pub fn window(&self) -> xproto::Window {
        self.window
    }

    /// The root window of the display.
    #[must_use]
    pub fn root(&self) -> xproto::Window {
        self.root
    }

    /// Read a selection.
    ///
    /// The owner is asked for UTF-8 first, then for `STRING` and `TEXT`. A
    /// selection nobody owns reads as `None`. A payload that is not UTF-8 is
    /// an error rather than a silent loss.
    ///
    /// # Errors
    /// Returns an error when the owner does not answer, when the payload is
    /// not UTF-8, or when it is larger than the cap.
    pub fn read(
        &mut self,
        selection: Selection,
        timeout: Duration,
    ) -> Result<Option<String>, ClipboardError> {
        for target in Target::read_order() {
            if let Some(text) = self.read_target(selection, target, timeout)? {
                return Ok(Some(text));
            }
        }
        Ok(None)
    }

    /// Read a selection as one target.
    ///
    /// # Errors
    /// Returns an error when the owner does not answer or the payload is not
    /// UTF-8.
    pub fn read_target(
        &mut self,
        selection: Selection,
        target: Target,
        timeout: Duration,
    ) -> Result<Option<String>, ClipboardError> {
        let owner = self
            .conn
            .get_selection_owner(self.selection_atom(selection))?
            .reply()?;
        if owner.owner == NO_ATOM {
            return Ok(None);
        }
        let target_atom = self.target_atom(target);
        self.conn
            .convert_selection(
                self.window,
                self.selection_atom(selection),
                target_atom,
                self.atoms.payload,
                CURRENT_TIME,
            )?
            .check()?;
        self.conn.flush()?;

        let deadline = Instant::now() + timeout;
        let mut answered = false;
        while Instant::now() < deadline {
            while let Some(event) = self.conn.poll_for_event()? {
                match event {
                    Event::SelectionNotify(notify) if notify.requestor == self.window => {
                        answered = true;
                        if notify.property == NO_ATOM {
                            // The owner refused this target.
                            return Ok(None);
                        }
                        break;
                    }
                    // This object may own the selection it is reading, for
                    // example when a clipboard manager reads back what it
                    // wrote. The request has to be answered, or the read
                    // never completes.
                    Event::SelectionRequest(request) => self.answer_request(&request)?,
                    _ => {}
                }
            }
            if answered {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        if !answered {
            return Err(ClipboardError::Timeout(timeout));
        }

        let reply = self
            .conn
            .get_property(
                true,
                self.window,
                self.atoms.payload,
                xproto::Atom::from(xproto::AtomEnum::ANY),
                0,
                u32::MAX,
            )?
            .reply()?;
        if reply.value.is_empty() {
            return Ok(None);
        }
        if reply.value.len() > MAX_CLIPBOARD_BYTES {
            return Err(ClipboardError::TooLarge(reply.value.len()));
        }
        let text = decode(&reply.value)?;
        Ok(Some(text))
    }

    /// Take ownership of a selection and offer `text` in it.
    ///
    /// The object answers the requests that arrive while it is pumped; see
    /// [`Self::serve`]. The selection keeps its content while this object
    /// lives.
    ///
    /// # Errors
    /// Returns an error when the payload is too large or the X request fails.
    pub fn set(&mut self, selection: Selection, text: &str) -> Result<(), ClipboardError> {
        if text.len() > MAX_CLIPBOARD_BYTES {
            return Err(ClipboardError::TooLarge(text.len()));
        }
        self.conn
            .set_selection_owner(self.window, self.selection_atom(selection), CURRENT_TIME)?
            .check()?;
        self.conn.flush()?;
        self.owned = Some((selection, text.to_string()));
        Ok(())
    }

    /// The payload this object owns, when it owns one.
    #[must_use]
    pub fn owned(&self) -> Option<(Selection, &str)> {
        self.owned.as_ref().map(|(s, t)| (*s, t.as_str()))
    }

    /// Answer the selection requests that arrive for `duration`.
    ///
    /// The host calls this after it took ownership of a selection, so the
    /// request that the local clipboard manager sends is answered.
    ///
    /// # Errors
    /// Returns an error when an X request fails.
    pub fn serve(&mut self, duration: Duration) -> Result<usize, ClipboardError> {
        let deadline = Instant::now() + duration;
        let mut served = 0usize;
        while Instant::now() < deadline {
            while let Some(event) = self.conn.poll_for_event()? {
                if let Event::SelectionRequest(request) = event {
                    self.answer_request(&request)?;
                    served += 1;
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(served)
    }

    /// Answer one selection request.
    fn answer_request(
        &mut self,
        request: &xproto::SelectionRequestEvent,
    ) -> Result<(), ClipboardError> {
        let Some((_, text)) = self.owned.clone() else {
            // Nothing is owned, so the request is refused.
            return self.notify(request, NO_ATOM);
        };
        let property = if request.property == NO_ATOM {
            self.atoms.fallback
        } else {
            request.property
        };
        if request.target == self.atoms.targets {
            let targets = [
                self.atoms.utf8_string,
                self.atoms.string,
                self.atoms.text,
                self.atoms.targets,
            ];
            self.conn
                .change_property32(
                    PropMode::REPLACE,
                    request.requestor,
                    property,
                    xproto::AtomEnum::ATOM,
                    &targets,
                )?
                .check()?;
        } else if request.target == self.atoms.utf8_string
            || request.target == self.atoms.string
            || request.target == self.atoms.text
        {
            self.conn
                .change_property8(
                    PropMode::REPLACE,
                    request.requestor,
                    property,
                    self.atoms.utf8_string,
                    text.as_bytes(),
                )?
                .check()?;
        } else {
            // An unknown target is refused.
            return self.notify(request, NO_ATOM);
        }
        self.conn.flush()?;
        self.notify(request, property)
    }

    /// Tell the requestor that the request is answered.
    fn notify(
        &mut self,
        request: &xproto::SelectionRequestEvent,
        property: xproto::Atom,
    ) -> Result<(), ClipboardError> {
        let event = xproto::SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: request.time,
            requestor: request.requestor,
            selection: request.selection,
            target: request.target,
            property,
        };
        self.conn
            .send_event(false, request.requestor, EventMask::NO_EVENT, event)?
            .check()?;
        self.conn.flush()?;
        Ok(())
    }

    fn selection_atom(&self, selection: Selection) -> xproto::Atom {
        match selection {
            Selection::Clipboard => self.atoms.clipboard,
            Selection::Primary => self.atoms.primary,
        }
    }

    fn target_atom(&self, target: Target) -> xproto::Atom {
        match target {
            Target::Utf8String => self.atoms.utf8_string,
            Target::String => self.atoms.string,
            Target::Text => self.atoms.text,
            Target::Targets => self.atoms.targets,
        }
    }
}

/// Decode a clipboard payload into text.
///
/// A payload that ends with a NUL byte loses it: X11 applications of the
/// `STRING` era terminate their text with one.
pub fn decode(bytes: &[u8]) -> Result<String, ClipboardError> {
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    String::from_utf8(bytes.to_vec()).map_err(|_| ClipboardError::NotUtf8)
}

/// Encode a clipboard payload the way X11 expects it.
///
/// # Errors
/// Returns an error when the payload is larger than the cap.
pub fn encode(text: &str) -> Result<Vec<u8>, ClipboardError> {
    if text.len() > MAX_CLIPBOARD_BYTES {
        return Err(ClipboardError::TooLarge(text.len()));
    }
    Ok(text.as_bytes().to_vec())
}

fn intern(conn: &RustConnection, name: &str) -> Result<xproto::Atom, ClipboardError> {
    let reply = conn.intern_atom(false, name.as_bytes())?.reply()?;
    Ok(reply.atom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_selection_names_are_the_x_atom_names() {
        assert_eq!(Selection::Clipboard.atom_name(), "CLIPBOARD");
        assert_eq!(Selection::Primary.atom_name(), "PRIMARY");
        assert_eq!(Selection::all().len(), 2);
    }

    #[test]
    fn utf8_is_the_first_target_tried() {
        assert_eq!(Target::read_order()[0], Target::Utf8String);
        assert_eq!(Target::read_order().len(), 3);
        assert_eq!(Target::offer_order().len(), 4);
        assert_eq!(Target::Targets.atom_name(), "TARGETS");
    }

    #[test]
    fn a_trailing_nul_is_dropped() {
        assert_eq!(decode(b"hello\0").unwrap(), "hello");
        assert_eq!(decode(b"hello").unwrap(), "hello");
        assert_eq!(decode(b"").unwrap(), "");
    }

    #[test]
    fn invalid_utf8_is_refused() {
        assert!(matches!(
            decode(&[0xff, 0xfe]),
            Err(ClipboardError::NotUtf8)
        ));
    }

    #[test]
    fn a_payload_over_the_cap_is_refused() {
        let big = "x".repeat(MAX_CLIPBOARD_BYTES + 1);
        assert!(matches!(encode(&big), Err(ClipboardError::TooLarge(_))));
        let ok = "x".repeat(MAX_CLIPBOARD_BYTES);
        assert_eq!(encode(&ok).unwrap().len(), MAX_CLIPBOARD_BYTES);
    }

    #[test]
    fn the_encoding_round_trips() {
        let text = "hello \u{1F600}";
        assert_eq!(decode(&encode(text).unwrap()).unwrap(), text);
    }
}
