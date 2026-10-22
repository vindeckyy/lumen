//! One EI client connection, sender or receiver, on the low-level `reis` protocol objects.
//!
//! reis's high-level event converter holds callbacks that are not `Send`, and both the host
//! injector and the client source move to a thread of their own. This keeps the little state
//! both need instead: the handshake, the last serial, the seats and the devices with their
//! interfaces, regions and resume state. Input events go to the caller unchanged.

use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use reis::{PendingRequestResult, ei};

use crate::readable;

/// Every capability Lumen uses: keyboard, relative and absolute pointer, button and scroll.
const INTERFACES: [&str; 5] = [
    "ei_keyboard",
    "ei_pointer",
    "ei_pointer_absolute",
    "ei_button",
    "ei_scroll",
];

/// A device the server announced.
#[derive(Debug)]
pub struct EiDevice {
    /// The protocol object.
    pub device: ei::Device,
    /// The interface objects the server attached, such as `ei_keyboard`.
    interfaces: Vec<reis::Object>,
    /// Regions of an absolute device: `(x, y, width, height)` in logical pixels.
    pub regions: Vec<(u32, u32, u32, u32)>,
    /// Whether the server has resumed the device, so it may emulate or deliver.
    pub resumed: bool,
    /// Whether this sender has started emulating on it.
    pub emulating: bool,
}

impl EiDevice {
    /// The device's interface `T`, when the server attached one.
    #[must_use]
    pub fn interface<T: ei::Interface>(&self) -> Option<T> {
        self.interfaces
            .iter()
            .find(|o| o.interface() == T::NAME)
            .and_then(|o| o.clone().downcast())
    }
}

/// One handshaken EI connection.
#[derive(Debug)]
pub struct EiPeer {
    context: ei::Context,
    /// The last serial the server sent, which every sender request must quote.
    pub last_serial: u32,
    /// Seats with the capability masks they announced.
    seats: Vec<(ei::Seat, u64)>,
    /// Every device the server announced and has not destroyed.
    pub devices: Vec<EiDevice>,
}

impl EiPeer {
    /// Runs the handshake on `context` as `name` with `kind`, waiting at most `timeout`
    /// for the server, which keeps a half-ready socket from hanging the caller.
    ///
    /// # Errors
    /// Returns an error on an I/O failure, a protocol violation, or a server that does not answer.
    pub fn connect(
        context: ei::Context,
        name: &str,
        kind: ei::handshake::ContextType,
        timeout: Duration,
    ) -> Result<Self, String> {
        let deadline = Instant::now() + timeout;
        let mut handshaker = reis::handshake::EiHandshaker::new(name, kind);
        let response = 'handshake: loop {
            wait_readable(&context, deadline)?;
            context.read().map_err(|e| format!("EI read: {e}"))?;
            while let Some(result) = context.pending_event() {
                let PendingRequestResult::Request(event) = result else {
                    return Err(format!("EI handshake: bad message {result:?}"));
                };
                if let Some(response) = handshaker
                    .handle_event(event)
                    .map_err(|e| format!("EI handshake: {e}"))?
                {
                    break 'handshake response;
                }
            }
        };
        Ok(Self {
            context,
            last_serial: response.serial,
            seats: Vec::new(),
            devices: Vec::new(),
        })
    }

    /// The protocol context, for requests and flushing.
    #[must_use]
    pub fn context(&self) -> &ei::Context {
        &self.context
    }

    /// Sends the queued requests.
    ///
    /// # Errors
    /// Returns the socket error.
    pub fn flush(&self) -> Result<(), String> {
        self.context.flush().map_err(|e| format!("EI flush: {e}"))
    }

    /// Waits until the socket is readable or `deadline` passes.
    ///
    /// # Errors
    /// Returns an error once the deadline passes.
    pub fn wait(&self, deadline: Instant) -> Result<(), String> {
        wait_readable(&self.context, deadline)
    }

    /// Reads what the socket holds without blocking, applies the connection, seat and device
    /// events, and hands every other event to `input`.
    ///
    /// # Errors
    /// Returns an error when the server disconnected or the socket failed.
    pub fn dispatch(&mut self, mut input: impl FnMut(&Self, ei::Event)) -> Result<(), String> {
        if readable(self.context.as_fd()) {
            match self.context.read() {
                Ok(0) => return Err("EIS server disconnected".to_owned()),
                Ok(_) => {}
                Err(e) => return Err(format!("EI read: {e}")),
            }
        }
        while let Some(result) = self.context.pending_event() {
            let PendingRequestResult::Request(event) = result else {
                continue;
            };
            match event {
                ei::Event::Connection(_, ei::connection::Event::Disconnected { .. }) => {
                    return Err("EIS server disconnected".to_owned());
                }
                ei::Event::Connection(_, ei::connection::Event::Ping { ping }) => ping.done(0),
                ei::Event::Connection(_, ei::connection::Event::Seat { seat }) => {
                    self.seats.push((seat, 0));
                }
                ei::Event::Seat(seat, event) => self.seat_event(&seat, event),
                ei::Event::Device(device, event) => self.device_event(&device, event),
                other => input(self, other),
            }
        }
        self.flush()
    }

    fn seat_event(&mut self, seat: &ei::Seat, event: ei::seat::Event) {
        let Some(entry) = self.seats.iter_mut().find(|(s, _)| s == seat) else {
            return;
        };
        match event {
            ei::seat::Event::Capability { mask, interface } => {
                if INTERFACES.contains(&interface.as_str()) {
                    entry.1 |= mask;
                }
            }
            ei::seat::Event::Done => seat.bind(entry.1),
            ei::seat::Event::Device { device } => self.devices.push(EiDevice {
                device,
                interfaces: Vec::new(),
                regions: Vec::new(),
                resumed: false,
                emulating: false,
            }),
            ei::seat::Event::Destroyed { serial } => {
                self.last_serial = serial;
                self.seats.retain(|(s, _)| s != seat);
            }
            _ => {}
        }
    }

    fn device_event(&mut self, device: &ei::Device, event: ei::device::Event) {
        let Some(index) = self.devices.iter().position(|d| &d.device == device) else {
            return;
        };
        let entry = &mut self.devices[index];
        match event {
            ei::device::Event::Interface { object } => entry.interfaces.push(object),
            ei::device::Event::Region {
                offset_x,
                offset_y,
                width,
                hight,
                ..
            } => entry.regions.push((offset_x, offset_y, width, hight)),
            ei::device::Event::Resumed { serial } => {
                self.last_serial = serial;
                entry.resumed = true;
            }
            ei::device::Event::Paused { serial } => {
                self.last_serial = serial;
                entry.resumed = false;
                entry.emulating = false;
            }
            ei::device::Event::Destroyed { serial } => {
                self.last_serial = serial;
                self.devices.remove(index);
            }
            ei::device::Event::StartEmulating { serial, .. }
            | ei::device::Event::StopEmulating { serial }
            | ei::device::Event::Frame { serial, .. } => self.last_serial = serial,
            _ => {}
        }
    }
}

/// Waits until `context`'s socket is readable or `deadline` passes.
fn wait_readable(context: &ei::Context, deadline: Instant) -> Result<(), String> {
    while !readable(context.as_fd()) {
        if Instant::now() >= deadline {
            return Err("EIS server did not answer in time".to_owned());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}
