//! Host injection through a libei sender. The EIS socket comes from the RemoteDesktop portal's
//! `ConnectToEIS`, which is the sender side: it emulates input rather than capturing it.

use std::fmt;
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use ashpd::desktop::remote_desktop::{
    ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions,
};
use ashpd::desktop::{CreateSessionOptions, Session};
use lumen_ffi_libei::{Capabilities, Context, Device, Event};
use lumen_input_wayland::{InputEvent, InputEventKind, readable};
use tokio::runtime::Runtime;

use crate::InputInjector;
use crate::text::{KEY_LEFTSHIFT, for_each_tap};

/// How long to wait for the server to resume a device after the socket is attached.
const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// A device the server has offered, with its emulation state.
struct Emulated {
    device: Device,
    resumed: bool,
    emulating: bool,
}

/// Injects through an EIS sender connection.
pub struct EiInjector {
    ctx: Context,
    devices: Vec<Emulated>,
    sequence: u32,
    client_width: f64,
    client_height: f64,
    /// The portal session and the runtime that drives it. Ending the session ends emulation.
    _runtime: Runtime,
    _session: Session<RemoteDesktop>,
}

impl fmt::Debug for EiInjector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EiInjector")
            .field("devices", &self.devices.len())
            .field("client_width", &self.client_width)
            .field("client_height", &self.client_height)
            .finish_non_exhaustive()
    }
}

impl EiInjector {
    /// Opens a RemoteDesktop session with keyboard and pointer devices and attaches its EIS socket.
    /// The first run may show a consent dialog. Waits up to five seconds for a device to resume.
    pub fn open_remote_desktop(client_width: u32, client_height: u32) -> Result<Self, String> {
        if client_width == 0 || client_height == 0 {
            return Err("client resolution must be non-zero".to_owned());
        }
        let (runtime, session, fd) = connect_remote_desktop()?;
        let mut ctx = Context::new_sender(c"lumen-host").map_err(|e| e.to_string())?;
        ctx.setup_fd(fd).map_err(|e| e.to_string())?;
        let mut injector = Self {
            ctx,
            devices: Vec::new(),
            sequence: 0,
            client_width: f64::from(client_width),
            client_height: f64::from(client_height),
            _runtime: runtime,
            _session: session,
        };
        injector.wait_for_device()?;
        Ok(injector)
    }

    fn wait_for_device(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            self.pump()?;
            if self.devices.iter().any(|d| d.resumed) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "EIS server resumed no device within {} s; check that the portal granted keyboard and pointer",
                    READY_TIMEOUT.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Reads socket data and applies device lifecycle events.
    fn pump(&mut self) -> Result<(), String> {
        if self.ctx.as_fd().is_some_and(readable) {
            self.ctx.dispatch();
        }
        while let Some(event) = self.ctx.next_event() {
            match event {
                Event::Disconnected => return Err("EIS server disconnected".to_owned()),
                Event::DeviceAdded(device) => self.devices.push(Emulated {
                    device,
                    resumed: false,
                    emulating: false,
                }),
                Event::DeviceRemoved(id) => self.devices.retain(|d| d.device.id() != id),
                Event::DeviceResumed(id) => {
                    if let Some(d) = self.devices.iter_mut().find(|d| d.device.id() == id) {
                        d.resumed = true;
                    }
                }
                Event::DevicePaused(id) => {
                    if let Some(d) = self.devices.iter_mut().find(|d| d.device.id() == id) {
                        d.resumed = false;
                        d.emulating = false;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Index of a resumed device with capability `cap`, with emulation started on it.
    fn ready(&mut self, cap: Capabilities) -> Result<usize, String> {
        self.pump()?;
        let index = self
            .devices
            .iter()
            .position(|d| d.resumed && d.device.info().capabilities.contains(cap))
            .ok_or_else(|| format!("no resumed EIS device with capability {cap:?}"))?;
        if !self.devices[index].emulating {
            self.sequence = self.sequence.wrapping_add(1);
            self.devices[index].device.start_emulating(self.sequence);
            self.devices[index].emulating = true;
        }
        Ok(index)
    }

    /// Runs `emit` on a device with `cap`, then closes the frame and flushes it.
    fn send(
        &mut self,
        cap: Capabilities,
        emit: impl FnOnce(&Device) -> Result<(), String>,
    ) -> Result<(), String> {
        let index = self.ready(cap)?;
        let device = &self.devices[index].device;
        emit(device)?;
        device.frame(self.ctx.now());
        self.ctx.dispatch();
        Ok(())
    }

    fn keyboard(&mut self, keycode: u32, pressed: bool) -> Result<(), String> {
        self.send(Capabilities::KEYBOARD, move |d| {
            d.keyboard_key(keycode, pressed);
            Ok(())
        })
    }

    fn tap(&mut self, keycode: u32, shift: bool) -> Result<(), String> {
        if shift {
            self.keyboard(KEY_LEFTSHIFT, true)?;
        }
        self.keyboard(keycode, true)?;
        self.keyboard(keycode, false)?;
        if shift {
            self.keyboard(KEY_LEFTSHIFT, false)?;
        }
        Ok(())
    }

    /// Maps client-surface coordinates onto the first region of an absolute device.
    fn absolute(&mut self, x: f64, y: f64) -> Result<(), String> {
        let (cw, ch) = (self.client_width, self.client_height);
        self.send(Capabilities::POINTER_ABSOLUTE, move |d| {
            let region = d
                .info()
                .regions
                .first()
                .ok_or("absolute EIS device has no region")?;
            let px = f64::from(region.x) + x * f64::from(region.width) / cw;
            let py = f64::from(region.y) + y * f64::from(region.height) / ch;
            d.pointer_motion_absolute(px, py);
            Ok(())
        })
    }
}

impl InputInjector for EiInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<(), String> {
        match event.kind {
            InputEventKind::Key { keycode, pressed } => self.keyboard(keycode, pressed),
            // Absolute events carry finite coordinates. Relative events carry NaN there.
            InputEventKind::Motion { dx, dy, x, y } => {
                if x.is_finite() && y.is_finite() {
                    self.absolute(x, y)
                } else {
                    self.send(Capabilities::POINTER, move |d| {
                        d.pointer_motion(dx, dy);
                        Ok(())
                    })
                }
            }
            InputEventKind::Button { button, pressed } => {
                self.send(Capabilities::BUTTON, move |d| {
                    d.button(button, pressed);
                    Ok(())
                })
            }
            InputEventKind::Scroll { dx, dy } => self.send(Capabilities::SCROLL, move |d| {
                d.scroll_delta(dx, dy);
                Ok(())
            }),
        }
    }

    fn inject_text(&mut self, text: &str) -> Result<(), String> {
        for_each_tap(text, |code, shift| self.tap(code, shift))
    }
}

/// Opens the RemoteDesktop session and returns its EIS socket. The runtime and session are
/// returned too, because the portal ends emulation when the session closes.
fn connect_remote_desktop() -> Result<(Runtime, Session<RemoteDesktop>, OwnedFd), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;
    let (session, fd) = runtime.block_on(async {
        let remote = RemoteDesktop::new().await.map_err(|e| e.to_string())?;
        let session = remote
            .create_session(CreateSessionOptions::default())
            .await
            .map_err(|e| e.to_string())?;
        remote
            .select_devices(
                &session,
                SelectDevicesOptions::default()
                    .set_devices(DeviceType::Keyboard | DeviceType::Pointer),
            )
            .await
            .map_err(|e| e.to_string())?
            .response()
            .map_err(|e| e.to_string())?;
        remote
            .start(&session, None, StartOptions::default())
            .await
            .map_err(|e| e.to_string())?
            .response()
            .map_err(|e| e.to_string())?;
        let fd = remote
            .connect_to_eis(&session, ConnectToEISOptions::default())
            .await
            .map_err(|e| e.to_string())?;
        Ok::<_, String>((session, fd))
    })?;
    Ok((runtime, session, fd))
}
