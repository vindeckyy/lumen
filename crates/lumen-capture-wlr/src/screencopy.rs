//! Wayland connection state and event dispatch for screencopy.

use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
    globals::{GlobalList, GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer::WlBuffer,
        wl_output::{self, WlOutput},
        wl_registry::WlRegistry,
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
    },
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
    zwp_linux_dmabuf_feedback_v1::{self, ZwpLinuxDmabufFeedbackV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use crate::Error;

/// One output as announced by the compositor.
#[derive(Debug, Clone)]
pub struct OutputEntry {
    pub wl: WlOutput,
    pub name: Option<String>,
    /// Size of the current mode in physical pixels; zero until announced.
    pub width: u32,
    pub height: u32,
    /// Refresh of the current mode in millihertz; zero when unknown.
    pub refresh_mhz: u32,
}

/// `wl_shm` buffer geometry announced for one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShmSpec {
    pub format: WEnum<wl_shm::Format>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

/// DMA-BUF buffer geometry announced for one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmabufSpec {
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
}

/// Terminal state of a screencopy frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Copy finished; `capture_ts_us` is taken on `CLOCK_MONOTONIC` when the event was dispatched.
    Ready {
        capture_ts_us: u64,
    },
    Failed,
}

/// Events received for the screencopy frame currently in flight.
#[derive(Debug, Default, Clone, Copy)]
pub struct FrameEvents {
    pub shm: Option<ShmSpec>,
    pub dmabuf: Option<DmabufSpec>,
    pub buffer_done: bool,
    pub y_invert: bool,
    pub outcome: Option<Outcome>,
}

/// Dispatch target for every Wayland object this crate binds.
#[derive(Debug, Default)]
pub struct State {
    pub outputs: Vec<OutputEntry>,
    /// Bound `zwlr_screencopy_manager_v1` and its version.
    pub screencopy: Option<(ZwlrScreencopyManagerV1, u32)>,
    pub shm: Option<WlShm>,
    pub dmabuf: Option<ZwpLinuxDmabufV1>,
    /// `dev_t` of the device the compositor renders on, from dmabuf feedback.
    pub main_device: Option<u64>,
    pub frame: FrameEvents,
}

/// A connected event queue with the globals this crate needs already bound.
pub struct Session {
    pub conn: Connection,
    pub queue: wayland_client::EventQueue<State>,
    pub state: State,
}

/// Connect to the compositor, bind globals and wait for output metadata.
///
/// `$WAYLAND_DISPLAY` is tried first. A compositor that restarts often comes up on another
/// socket, so when the named one is gone the sockets in `$XDG_RUNTIME_DIR` are tried as well,
/// newest first.
pub fn connect() -> Result<Session, Error> {
    let conn = connect_any()?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let mut state = State::default();
    bind_globals(&globals, &qh, &mut state);
    queue.roundtrip(&mut state)?;
    Ok(Session { conn, queue, state })
}

/// The connection to the compositor: `$WAYLAND_DISPLAY` first, then every Wayland socket in
/// `$XDG_RUNTIME_DIR`, newest first.
///
/// A compositor that restarts often comes up on a different socket name, so a host that only
/// knows the old name cannot reconnect (O-29a).
fn connect_any() -> Result<Connection, Error> {
    match Connection::connect_to_env() {
        Ok(conn) => Ok(conn),
        Err(first) => {
            let candidates = wayland_sockets();
            if candidates.is_empty() {
                return Err(Error::Connect(first));
            }
            let runtime = std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_default();
            for name in candidates {
                let path = std::path::Path::new(&runtime).join(&name);
                let Ok(stream) = std::os::unix::net::UnixStream::connect(&path) else {
                    continue;
                };
                if let Ok(conn) = Connection::from_socket(stream) {
                    return Ok(conn);
                }
            }
            Err(Error::Connect(first))
        }
    }
}

/// The Wayland sockets in `$XDG_RUNTIME_DIR`, newest first.
fn wayland_sockets() -> Vec<String> {
    let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(std::path::Path::new(&dir)) else {
        return Vec::new();
    };
    let mut found: Vec<(std::time::SystemTime, String)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // The lock file of a socket is not a socket.
        if !name.starts_with("wayland-")
            || std::path::Path::new(name)
                .extension()
                .is_some_and(|e| e == "lock")
        {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        found.push((modified, name.to_owned()));
    }
    found.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    found.into_iter().map(|(_, name)| name).collect()
}

fn bind_globals(globals: &GlobalList, qh: &QueueHandle<State>, state: &mut State) {
    let registry: &WlRegistry = globals.registry();
    for g in globals.contents().clone_list() {
        match g.interface.as_str() {
            "wl_output" => {
                let index = state.outputs.len();
                let wl: WlOutput = registry.bind(g.name, g.version.min(4), qh, index);
                state.outputs.push(OutputEntry {
                    wl,
                    name: None,
                    width: 0,
                    height: 0,
                    refresh_mhz: 0,
                });
            }
            "zwlr_screencopy_manager_v1" => {
                let version = g.version.min(3);
                let m: ZwlrScreencopyManagerV1 = registry.bind(g.name, version, qh, ());
                state.screencopy = Some((m, version));
            }
            "wl_shm" => state.shm = Some(registry.bind(g.name, 1, qh, ())),
            "zwp_linux_dmabuf_v1" => {
                let version = g.version.min(4);
                let d: ZwpLinuxDmabufV1 = registry.bind(g.name, version, qh, ());
                if version >= 4 {
                    // Feedback names the render device; without it the first render node is used.
                    let _: ZwpLinuxDmabufFeedbackV1 = d.get_default_feedback(qh, ());
                }
                state.dmabuf = Some(d);
            }
            _ => {}
        }
    }
}

/// Current `CLOCK_MONOTONIC` time in microseconds.
pub fn monotonic_us() -> u64 {
    let ts = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let secs = u64::try_from(ts.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(ts.tv_nsec).unwrap_or(0);
    secs * 1_000_000 + nanos / 1_000
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlOutput, usize> for State {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(out) = state.outputs.get_mut(*index) else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => out.name = Some(name),
            wl_output::Event::Mode {
                flags,
                width,
                height,
                refresh,
            } => {
                if let WEnum::Value(mode) = flags
                    && mode.contains(wl_output::Mode::Current)
                {
                    out.width = u32::try_from(width).unwrap_or(0);
                    out.height = u32::try_from(height).unwrap_or(0);
                    out.refresh_mhz = u32::try_from(refresh).unwrap_or(0);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let ev = &mut state.frame;
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                ev.shm = Some(ShmSpec {
                    format,
                    width,
                    height,
                    stride,
                });
            }
            zwlr_screencopy_frame_v1::Event::LinuxDmabuf {
                format,
                width,
                height,
            } => {
                ev.dmabuf = Some(DmabufSpec {
                    fourcc: format,
                    width,
                    height,
                });
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => ev.buffer_done = true,
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                ev.y_invert = matches!(flags, WEnum::Value(f) if f.contains(zwlr_screencopy_frame_v1::Flags::YInvert));
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                ev.outcome = Some(Outcome::Ready {
                    capture_ts_us: monotonic_us(),
                });
            }
            zwlr_screencopy_frame_v1::Event::Failed => ev.outcome = Some(Outcome::Failed),
            _ => {}
        }
    }
}

impl Dispatch<ZwpLinuxDmabufFeedbackV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpLinuxDmabufFeedbackV1,
        event: zwp_linux_dmabuf_feedback_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_linux_dmabuf_feedback_v1::Event::MainDevice { device } = event
            && let Ok(bytes) = <[u8; 8]>::try_from(device.as_slice())
        {
            state.main_device = Some(u64::from_ne_bytes(bytes));
        }
    }
}

wayland_client::delegate_noop!(State: ignore ZwlrScreencopyManagerV1);
wayland_client::delegate_noop!(State: ignore WlShm);
wayland_client::delegate_noop!(State: ignore WlShmPool);
wayland_client::delegate_noop!(State: ignore WlBuffer);
wayland_client::delegate_noop!(State: ignore ZwpLinuxDmabufV1);
wayland_client::delegate_noop!(State: ignore ZwpLinuxBufferParamsV1);
