//! Host injection through an EI sender, spoken by the pure-Rust `reis` crate. The EIS socket
//! comes from `$LIBEI_SOCKET` when it is set, and otherwise from the RemoteDesktop portal's
//! `ConnectToEIS`, which is the sender side: it emulates input rather than capturing it.

use std::fmt;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use ashpd::desktop::remote_desktop::{
    ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions,
};
use ashpd::desktop::{CreateSessionOptions, Session};
use lumen_input_wayland::ei_peer::{EiDevice, EiPeer};
use lumen_input_wayland::{InputEvent, InputEventKind};
use reis::ei;
use tokio::runtime::Runtime;

use crate::InputInjector;
use crate::text::{KEY_LEFTSHIFT, for_each_tap};

/// How long to wait for the handshake and for the server to resume a device.
const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// The portal session and the runtime that drives it. Ending the session ends emulation.
type Portal = (Runtime, Session<RemoteDesktop>);

/// Injects through an EIS sender connection.
pub struct EiInjector {
    peer: EiPeer,
    sequence: u32,
    epoch: Instant,
    client_width: f64,
    client_height: f64,
    _portal: Option<Portal>,
}

impl fmt::Debug for EiInjector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EiInjector")
            .field("devices", &self.peer.devices.len())
            .field("client_width", &self.client_width)
            .field("client_height", &self.client_height)
            .finish_non_exhaustive()
    }
}

impl EiInjector {
    /// Connects to `$LIBEI_SOCKET`, or opens a RemoteDesktop session with keyboard and pointer
    /// devices and attaches its EIS socket. The portal may show a consent dialog the first time.
    /// Waits up to five seconds for a device to resume.
    ///
    /// # Errors
    /// Returns an error when no EIS server answers, the handshake fails, or no device resumes.
    pub fn open_remote_desktop(client_width: u32, client_height: u32) -> Result<Self, String> {
        if client_width == 0 || client_height == 0 {
            return Err("client resolution must be non-zero".to_owned());
        }
        let (context, portal) = if let Some(context) =
            ei::Context::connect_to_env().map_err(|e| format!("LIBEI_SOCKET: {e}"))?
        {
            (context, None)
        } else {
            let (runtime, session, stream) = connect_remote_desktop()?;
            let context = ei::Context::new(stream).map_err(|e| format!("EI context: {e}"))?;
            (context, Some((runtime, session)))
        };
        let peer = EiPeer::connect(
            context,
            "lumen-host",
            ei::handshake::ContextType::Sender,
            READY_TIMEOUT,
        )?;
        let mut injector = Self {
            peer,
            sequence: 0,
            epoch: Instant::now(),
            client_width: f64::from(client_width),
            client_height: f64::from(client_height),
            _portal: portal,
        };
        injector.wait_for_device()?;
        Ok(injector)
    }

    fn wait_for_device(&mut self) -> Result<(), String> {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            self.peer.dispatch(|_, _| {})?;
            if self.peer.devices.iter().any(|d| d.resumed) {
                return Ok(());
            }
            self.peer.wait(deadline).map_err(|_| {
                format!(
                    "EIS server resumed no device within {} s; check that the portal granted keyboard and pointer",
                    READY_TIMEOUT.as_secs()
                )
            })?;
        }
    }

    /// Runs `emit` on a resumed device that has interface `T`, starting emulation on it first,
    /// then closes the frame and flushes it.
    fn send<T: ei::Interface>(
        &mut self,
        emit: impl FnOnce(&EiDevice, T) -> Result<(), String>,
    ) -> Result<(), String> {
        self.peer.dispatch(|_, _| {})?;
        let serial = self.peer.last_serial;
        let device = self
            .peer
            .devices
            .iter_mut()
            .find(|d| d.resumed && d.interface::<T>().is_some())
            .ok_or_else(|| format!("no resumed EIS device with {}", T::NAME))?;
        if !device.emulating {
            self.sequence = self.sequence.wrapping_add(1);
            device.device.start_emulating(serial, self.sequence);
            device.emulating = true;
        }
        let interface = device
            .interface::<T>()
            .ok_or_else(|| format!("EIS device lacks {}", T::NAME))?;
        emit(device, interface)?;
        let now_us = u64::try_from(self.epoch.elapsed().as_micros()).unwrap_or(u64::MAX);
        device.device.frame(serial, now_us);
        self.peer.flush()
    }

    fn keyboard(&mut self, keycode: u32, pressed: bool) -> Result<(), String> {
        let state = if pressed {
            ei::keyboard::KeyState::Press
        } else {
            ei::keyboard::KeyState::Released
        };
        self.send(|_, k: ei::Keyboard| {
            k.key(keycode, state);
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
        self.send(|d, p: ei::PointerAbsolute| {
            let &(rx, ry, rw, rh) = d
                .regions
                .first()
                .ok_or("absolute EIS device has no region")?;
            let px = f64::from(rx) + x * f64::from(rw) / cw;
            let py = f64::from(ry) + y * f64::from(rh) / ch;
            p.motion_absolute(px as f32, py as f32);
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
                    self.send(|_, p: ei::Pointer| {
                        p.motion_relative(dx as f32, dy as f32);
                        Ok(())
                    })
                }
            }
            InputEventKind::Button { button, pressed } => {
                let state = if pressed {
                    ei::button::ButtonState::Press
                } else {
                    ei::button::ButtonState::Released
                };
                self.send(|_, b: ei::Button| {
                    b.button(button, state);
                    Ok(())
                })
            }
            InputEventKind::Scroll { dx, dy } => self.send(|_, s: ei::Scroll| {
                s.scroll(dx as f32, dy as f32);
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
fn connect_remote_desktop() -> Result<(Runtime, Session<RemoteDesktop>, UnixStream), String> {
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
    Ok((runtime, session, UnixStream::from(fd)))
}
