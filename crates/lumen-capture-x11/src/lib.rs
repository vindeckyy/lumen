//! X11 host capture with a DRI3 → MIT-SHM → GetImage ladder.
//!
//! Captures are gated by XDamage on the root window and limited to the
//! configured frame rate. The cursor is not composited into frames; ask for it
//! with [`X11Capture::cursor`].
//!
//! [`X11Capture::poll_frame`] never blocks. It returns `None` until the root
//! has changed and the rate allows another frame.
#![forbid(unsafe_code)]

mod dri3;
mod shm;

use std::fmt;
use std::io;
use std::time::{Duration, Instant};

use lumen_protocol::media::{FrameBuffer, FrameSource, PixelFormat, VideoFrame};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::errors::{ConnectError, ConnectionError, ReplyError, ReplyOrIdError};
use x11rb::protocol::{Event, damage, xfixes, xproto};
use x11rb::rust_connection::RustConnection;

/// The longest an unchanged screen goes without a frame.
const IDLE_REFRESH: Duration = Duration::from_secs(1);

/// A rectangle in root-window pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// One rung of the capture ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// The root region is copied on the GPU into a DMA-BUF. The copy is fenced: when a frame
    /// is handed out the copy has been submitted, so readers wait for it. See `dri3.rs`.
    Dri3,
    /// MIT-SHM `GetImage` into a shared memfd.
    Shm,
    /// Plain `GetImage`, always available.
    GetImage,
}

/// How to open a capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureConfig {
    /// X display such as `:77`. `None` reads `$DISPLAY`.
    pub display: Option<String>,
    /// Capture rectangle in root coordinates. `None` captures the whole root window.
    pub region: Option<Rect>,
    /// Maximum frame rate; frames are never emitted faster.
    pub fps: u32,
    /// Use exactly this rung and fail if it is unavailable. `None` walks DRI3, MIT-SHM, then
    /// GetImage.
    pub backend: Option<Backend>,
}

/// The cursor as XFixes reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorImage {
    /// Cursor position in root coordinates.
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    /// Hotspot within the image.
    pub xhot: u16,
    pub yhot: u16,
    /// Changes whenever the cursor image changes.
    pub serial: u32,
    /// Premultiplied ARGB, `width * height` pixels, row-major.
    pub argb: Vec<u32>,
}

/// Errors from connecting to or capturing from an X server.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot connect to X display: {0}")]
    Connect(#[from] ConnectError),
    #[error("X connection: {0}")]
    Connection(#[from] ConnectionError),
    #[error("X request: {0}")]
    Reply(#[from] ReplyError),
    #[error("X resource id: {0}")]
    Id(#[from] ReplyOrIdError),
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
    #[error("system call: {0}")]
    Errno(#[from] rustix::io::Errno),
    #[error("gbm: {0}")]
    Gbm(String),
    #[error("unsupported X server: {0}")]
    Unsupported(&'static str),
    #[error("invalid configuration: {0}")]
    InvalidConfig(&'static str),
}

/// X11 capture of the root window or a sub-rectangle of it.
pub struct X11Capture {
    conn: RustConnection,
    root: xproto::Window,
    root_size: (u16, u16),
    region: Rect,
    /// `region` as X protocol values, validated against the root at open.
    origin: (i16, i16),
    size: (u16, u16),
    backend: Backend,
    forced: bool,
    shm: Option<shm::Segment>,
    dri3: Option<dri3::Device>,
    damage: Option<damage::Damage>,
    xfixes: bool,
    dirty: bool,
    interval: Duration,
    last_capture: Option<Instant>,
    next_index: u64,
    error: Option<Error>,
}

impl fmt::Debug for X11Capture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("X11Capture")
            .field("root", &self.root)
            .field("region", &self.region)
            .field("backend", &self.backend)
            .field("next_index", &self.next_index)
            .finish_non_exhaustive()
    }
}

impl X11Capture {
    /// Connect to the display and prepare the chosen capture rung.
    ///
    /// # Errors
    /// Fails when the display cannot be opened, the root visual is not 32-bit
    /// BGRX, the region is outside the root window, or a forced rung is unavailable.
    pub fn open(config: &CaptureConfig) -> Result<Self, Error> {
        if config.fps == 0 {
            return Err(Error::InvalidConfig("fps must be non-zero"));
        }
        let (conn, screen_num) = x11rb::connect(config.display.as_deref())?;
        let (root, root_w, root_h, root_depth) = {
            let screen = conn
                .setup()
                .roots
                .get(screen_num)
                .ok_or(Error::Unsupported("screen number out of range"))?;
            verify_bgrx(&conn, screen)?;
            (
                screen.root,
                screen.width_in_pixels,
                screen.height_in_pixels,
                screen.root_depth,
            )
        };
        let region = config.region.unwrap_or(Rect {
            x: 0,
            y: 0,
            width: u32::from(root_w),
            height: u32::from(root_h),
        });
        let (origin, size) = validate_region(region, (root_w, root_h))?;
        let damage = setup_damage(&conn, root)?;
        let xfixes = setup_xfixes(&conn)?;
        let (backend, shm, dri3) = ladder(&conn, root, root_depth, size, config.backend)?;
        Ok(Self {
            conn,
            root,
            root_size: (root_w, root_h),
            region,
            origin,
            size,
            backend,
            forced: config.backend.is_some(),
            shm,
            dri3,
            damage,
            xfixes,
            dirty: true,
            interval: Duration::from_secs(1) / config.fps,
            last_capture: None,
            next_index: 0,
            error: None,
        })
    }

    /// Full root-window geometry, for sizing the encoder.
    #[must_use]
    pub fn root_geometry(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: u32::from(self.root_size.0),
            height: u32::from(self.root_size.1),
        }
    }

    /// The rectangle being captured.
    #[must_use]
    pub fn region(&self) -> Rect {
        self.region
    }

    /// The rung currently in use. Drops after a runtime failure unless the rung was forced.
    #[must_use]
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// The most recent error from [`FrameSource::poll_frame`] or a rung fallback. Clears it.
    pub fn take_error(&mut self) -> Option<Error> {
        self.error.take()
    }

    /// Change the maximum frame rate (zero is treated as one).
    pub fn set_fps(&mut self, fps: u32) {
        self.interval = Duration::from_secs(1) / fps.max(1);
    }

    /// The render node the frames are allocated on, when the capture hands out DMA-BUFs. A
    /// consumer that imports the frames must open this node.
    #[must_use]
    pub fn render_node(&self) -> Option<&std::path::Path> {
        match self.backend {
            Backend::Dri3 => self.dri3.as_ref().and_then(dri3::Device::node),
            Backend::Shm | Backend::GetImage => None,
        }
    }

    /// The cursor image and position, via XFixes.
    ///
    /// # Errors
    /// Fails when XFixes is missing or the request fails.
    pub fn cursor(&self) -> Result<CursorImage, Error> {
        if !self.xfixes {
            return Err(Error::Unsupported("XFixes extension missing"));
        }
        let r = xfixes::get_cursor_image(&self.conn)?.reply()?;
        Ok(CursorImage {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
            xhot: r.xhot,
            yhot: r.yhot,
            serial: r.cursor_serial,
            argb: r.cursor_image,
        })
    }

    fn advance(&mut self) -> Result<Option<VideoFrame>, Error> {
        while let Some(event) = self.conn.poll_for_event()? {
            if matches!(event, Event::DamageNotify(_)) {
                self.dirty = true;
            }
        }
        if self.damage.is_none() {
            // Without XDamage every rate tick is a potential change.
            self.dirty = true;
        }
        let now = Instant::now();
        // An idle screen still sends a frame now and then: a client that joins or loses a frame
        // while nothing changes would otherwise wait for a picture that never comes.
        if self.last_capture.is_some_and(|t| now >= t + IDLE_REFRESH) {
            self.dirty = true;
        }
        // A tenth of the interval absorbs the caller's wake-up jitter, which would otherwise
        // stretch every period past the rate (the Wayland pacer does the same).
        let due = self.interval * 9 / 10;
        if !self.dirty || self.last_capture.is_some_and(|t| now < t + due) {
            return Ok(None);
        }
        let capture_ts_us = monotonic_us();
        // Re-arm damage before reading pixels so changes during the read raise a new event.
        if let Some(d) = self.damage {
            damage::subtract(&self.conn, d, x11rb::NONE, x11rb::NONE)?.check()?;
        }
        self.dirty = false;
        self.last_capture = Some(now);
        let buffer = match self.grab() {
            Ok(buffer) => buffer,
            Err(e) => {
                self.dirty = true;
                return Err(e);
            }
        };
        let index = self.next_index;
        self.next_index += 1;
        Ok(Some(VideoFrame {
            width: u32::from(self.size.0),
            height: u32::from(self.size.1),
            format: PixelFormat::Bgrx,
            buffer,
            capture_ts_us,
            index,
        }))
    }

    /// Capture with the current rung, stepping down on failure unless forced.
    fn grab(&mut self) -> Result<FrameBuffer, Error> {
        loop {
            let result = match self.backend {
                Backend::Dri3 => self.grab_dri3(),
                Backend::Shm => self.grab_shm(),
                Backend::GetImage => self.grab_get_image(),
            };
            match result {
                Ok(buffer) => return Ok(buffer),
                Err(e) if self.forced || self.backend == Backend::GetImage => return Err(e),
                Err(e) => self.downgrade(e),
            }
        }
    }

    /// Step to the next rung after `cause`. The cause stays available through `take_error`.
    fn downgrade(&mut self, cause: Error) {
        self.error = Some(cause);
        self.backend = match self.backend {
            Backend::Dri3 => {
                self.dri3 = None;
                if self.shm.is_some() {
                    Backend::Shm
                } else {
                    Backend::GetImage
                }
            }
            Backend::Shm | Backend::GetImage => {
                self.shm = None;
                Backend::GetImage
            }
        };
    }

    fn grab_dri3(&mut self) -> Result<FrameBuffer, Error> {
        let dev = self
            .dri3
            .as_mut()
            .ok_or(Error::Unsupported("DRI3 device missing"))?;
        Ok(FrameBuffer::Dmabuf(dev.grab(
            &self.conn,
            self.root,
            self.origin,
            self.size,
        )?))
    }

    fn grab_shm(&self) -> Result<FrameBuffer, Error> {
        let seg = self
            .shm
            .as_ref()
            .ok_or(Error::Unsupported("MIT-SHM segment missing"))?;
        Ok(FrameBuffer::Cpu {
            data: seg.grab(&self.conn, self.root, self.origin, self.size)?,
        })
    }

    fn grab_get_image(&self) -> Result<FrameBuffer, Error> {
        let (w, h) = self.size;
        let reply = xproto::get_image(
            &self.conn,
            xproto::ImageFormat::Z_PIXMAP,
            self.root,
            self.origin.0,
            self.origin.1,
            w,
            h,
            u32::MAX,
        )?
        .reply()?;
        if reply.data.len() != usize::from(w) * usize::from(h) * 4 {
            return Err(Error::Unsupported("GetImage returned an unexpected length"));
        }
        Ok(FrameBuffer::Cpu { data: reply.data })
    }
}

impl FrameSource for X11Capture {
    fn poll_frame(&mut self) -> Option<VideoFrame> {
        match self.advance() {
            Ok(frame) => frame,
            Err(e) => {
                self.error = Some(e);
                None
            }
        }
    }
}

/// Pick the rung to use. The default tries DRI3, then MIT-SHM, then GetImage; DRI3 keeps the
/// MIT-SHM segment as its fallback, so a DRI3 failure later steps down to MIT-SHM.
fn ladder(
    conn: &RustConnection,
    root: xproto::Window,
    depth: u8,
    size: (u16, u16),
    forced: Option<Backend>,
) -> Result<(Backend, Option<shm::Segment>, Option<dri3::Device>), Error> {
    if forced == Some(Backend::Dri3) {
        let dev = dri3::Device::open(conn, root, depth)?
            .ok_or(Error::Unsupported("DRI3 requested but unavailable"))?;
        return Ok((Backend::Dri3, None, Some(dev)));
    }
    let dri3 = if forced.is_none() {
        // A server that refuses DRI3 part-way is not an error for the default ladder.
        dri3::Device::open(conn, root, depth).ok().flatten()
    } else {
        None
    };
    let shm = if matches!(forced, None | Some(Backend::Shm)) {
        let len = usize::from(size.0) * usize::from(size.1) * 4;
        shm::Segment::attach(conn, len)?
    } else {
        None
    };
    if forced == Some(Backend::Shm) && shm.is_none() {
        return Err(Error::Unsupported("MIT-SHM requested but unavailable"));
    }
    let backend = match (&dri3, &shm) {
        (Some(_), _) => Backend::Dri3,
        (None, Some(_)) => Backend::Shm,
        (None, None) => Backend::GetImage,
    };
    Ok((backend, shm, dri3))
}

fn verify_bgrx(conn: &RustConnection, screen: &xproto::Screen) -> Result<(), Error> {
    let setup = conn.setup();
    if setup.image_byte_order != xproto::ImageOrder::LSB_FIRST {
        return Err(Error::Unsupported("big-endian X server"));
    }
    let bits = setup
        .pixmap_formats
        .iter()
        .find(|f| f.depth == screen.root_depth)
        .map(|f| f.bits_per_pixel);
    if bits != Some(32) {
        return Err(Error::Unsupported("root depth is not 32 bits per pixel"));
    }
    let visual = screen
        .allowed_depths
        .iter()
        .flat_map(|d| d.visuals.iter())
        .find(|v| v.visual_id == screen.root_visual)
        .ok_or(Error::Unsupported("root visual not listed"))?;
    if (visual.red_mask, visual.green_mask, visual.blue_mask) != (0xff_0000, 0xff_00, 0xff) {
        return Err(Error::Unsupported("root visual is not BGRX"));
    }
    Ok(())
}

/// Capture origin and size as X protocol values.
type Placement = ((i16, i16), (u16, u16));

fn validate_region(region: Rect, root: (u16, u16)) -> Result<Placement, Error> {
    let bad =
        || Error::InvalidConfig("capture region must be non-empty and inside the root window");
    let x = u16::try_from(region.x).map_err(|_| bad())?;
    let y = u16::try_from(region.y).map_err(|_| bad())?;
    let w = u16::try_from(region.width).map_err(|_| bad())?;
    let h = u16::try_from(region.height).map_err(|_| bad())?;
    if w == 0
        || h == 0
        || u32::from(x) + u32::from(w) > u32::from(root.0)
        || u32::from(y) + u32::from(h) > u32::from(root.1)
    {
        return Err(bad());
    }
    let origin = (
        i16::try_from(x).map_err(|_| bad())?,
        i16::try_from(y).map_err(|_| bad())?,
    );
    Ok((origin, (w, h)))
}

fn setup_damage(
    conn: &RustConnection,
    root: xproto::Window,
) -> Result<Option<damage::Damage>, Error> {
    if conn
        .extension_information(damage::X11_EXTENSION_NAME)?
        .is_none()
    {
        return Ok(None);
    }
    damage::query_version(conn, 1, 1)?.reply()?;
    let id = conn.generate_id()?;
    damage::create(conn, id, root, damage::ReportLevel::NON_EMPTY)?.check()?;
    Ok(Some(id))
}

fn setup_xfixes(conn: &RustConnection) -> Result<bool, Error> {
    if conn
        .extension_information(xfixes::X11_EXTENSION_NAME)?
        .is_none()
    {
        return Ok(false);
    }
    xfixes::query_version(conn, 5, 0)?.reply()?;
    Ok(true)
}

/// Current `CLOCK_MONOTONIC` time in microseconds.
fn monotonic_us() -> u64 {
    let ts = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let secs = u64::try_from(ts.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(ts.tv_nsec).unwrap_or(0);
    secs * 1_000_000 + nanos / 1_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_must_fit_inside_root() {
        let root = (800, 600);
        let rect = |x, y, width, height| Rect {
            x,
            y,
            width,
            height,
        };
        assert!(validate_region(rect(0, 0, 800, 600), root).is_ok());
        assert!(validate_region(rect(700, 0, 200, 10), root).is_err());
        assert!(validate_region(rect(0, 0, 0, 10), root).is_err());
        assert!(validate_region(rect(-1, 0, 10, 10), root).is_err());
    }
}
