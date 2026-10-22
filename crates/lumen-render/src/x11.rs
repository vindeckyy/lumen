//! X11 presentation.
//!
//! CPU frames are copied into MIT-SHM segments. With the Present extension each frame is
//! uploaded into a pixmap and queued with `present::pixmap`, and the pixmap is reused after
//! `PresentIdleNotify`. Without Present, `shm::put_image` draws straight into the window.
//! dmabuf frames (BGRX only) become DRI3 pixmaps and go through Present. A window that covers
//! the whole screen asks the compositor to unredirect it.

use std::fs::File;
use std::os::fd::OwnedFd;

use lumen_protocol::media::{
    DRM_FORMAT_MOD_INVALID, DRM_FORMAT_MOD_LINEAR, DmabufImage, FrameBuffer, FrameSink,
    PixelFormat, VideoFrame,
};
use x11rb::NONE;
use x11rb::connection::Connection;
use x11rb::properties::WmSizeHints;
use x11rb::protocol::Event;
use x11rb::protocol::composite::{ConnectionExt as _, Redirect};
use x11rb::protocol::dri3::ConnectionExt as _;
use x11rb::protocol::present::{ConnectionExt as _, EventMask};
use x11rb::protocol::shm::{self as x_shm, ConnectionExt as _};
use x11rb::protocol::xproto::{
    self, AtomEnum, ConnectionExt as _, CreateGCAux, CreateWindowAux, ImageFormat, PropMode,
    WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::format;
use crate::shm;

/// Frames in flight with Present: one is copied to the screen while the next is written.
const PRESENT_SLOTS: usize = 2;

/// Presents frames to an X server. The window is recreated when the frame size or format changes.
pub struct X11Renderer {
    conn: RustConnection,
    root: xproto::Window,
    root_size: (u16, u16),
    depth: u8,
    visual: xproto::Visualid,
    gc: xproto::Gcontext,
    has_shm: bool,
    has_present: bool,
    has_dri3: bool,
    has_composite: bool,
    atoms: Atoms,
    title: String,
    window: Option<Window>,
    serial: u32,
    present_count: u64,
    last_present_ts_us: Option<u64>,
}

#[derive(Clone, Copy)]
struct Atoms {
    net_wm_state: xproto::Atom,
    fullscreen: xproto::Atom,
    bypass_compositor: xproto::Atom,
}

struct Window {
    id: xproto::Window,
    width: u32,
    height: u32,
    format: PixelFormat,
    slots: Vec<Slot>,
    /// DRI3 pixmaps for dmabuf frames, freed on `PresentIdleNotify`.
    dmabuf_pixmaps: Vec<xproto::Pixmap>,
    /// Whether the server accepted the composite unredirect request.
    unredirected: bool,
}

/// One shared-memory upload target.
struct Slot {
    file: File,
    seg: x_shm::Seg,
    /// Intermediate pixmap; present when the Present extension is available.
    pixmap: Option<xproto::Pixmap>,
    busy: bool,
}

impl X11Renderer {
    /// Connects to `display`, or to `DISPLAY` when `None`.
    ///
    /// # Errors
    /// Fails when the display is unreachable or its root depth is not 32 bits per pixel.
    pub fn connect(display: Option<&str>, title: &str) -> Result<Self, String> {
        let (conn, screen_num) =
            RustConnection::connect(display).map_err(|e| format!("connect to X display: {e}"))?;
        let setup = conn.setup();
        let screen = setup
            .roots
            .get(screen_num)
            .ok_or("X server reports no such screen")?;
        if !setup
            .pixmap_formats
            .iter()
            .any(|f| f.depth == screen.root_depth && f.bits_per_pixel == 32)
        {
            return Err(format!(
                "X root depth {} is not 32 bits per pixel",
                screen.root_depth
            ));
        }
        let (root, depth, visual) = (screen.root, screen.root_depth, screen.root_visual);
        let root_size = (screen.width_in_pixels, screen.height_in_pixels);
        // Optional extensions: each flag records availability and the request is only issued when needed.
        let has_shm = conn
            .shm_query_version()
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some();
        let has_present = conn
            .present_query_version(1, 0)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some();
        let has_dri3 = conn
            .dri3_query_version(1, 0)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some();
        let has_composite = conn
            .composite_query_version(0, 2)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some();
        let atoms = Atoms {
            net_wm_state: intern(&conn, "_NET_WM_STATE")?,
            fullscreen: intern(&conn, "_NET_WM_STATE_FULLSCREEN")?,
            bypass_compositor: intern(&conn, "_NET_WM_BYPASS_COMPOSITOR")?,
        };
        let gc = conn
            .generate_id()
            .map_err(|e| format!("allocate X id: {e}"))?;
        conn.create_gc(gc, root, &CreateGCAux::new())
            .map_err(|e| format!("create GC: {e}"))?;
        Ok(Self {
            conn,
            root,
            root_size,
            depth,
            visual,
            gc,
            has_shm,
            has_present,
            has_dri3,
            has_composite,
            atoms,
            title: title.to_owned(),
            window: None,
            serial: 0,
            present_count: 0,
            last_present_ts_us: None,
        })
    }

    /// Timestamp from the most recent `PresentCompleteNotify`, in microseconds on the server
    /// clock (CLOCK_MONOTONIC). `None` until the first completion arrives.
    #[must_use]
    pub fn last_present_ts_us(&self) -> Option<u64> {
        self.last_present_ts_us
    }

    /// Number of frames presented so far.
    #[must_use]
    pub fn present_count(&self) -> u64 {
        self.present_count
    }

    /// Whether the current window was accepted for composite unredirection (full-screen only).
    #[must_use]
    pub fn is_unredirected(&self) -> bool {
        self.window.as_ref().is_some_and(|w| w.unredirected)
    }

    /// Handles queued server events without blocking.
    ///
    /// # Errors
    /// Returns the first X protocol error or connection failure reported by the server.
    pub fn poll_events(&mut self) -> Result<(), String> {
        while let Some(event) = self
            .conn
            .poll_for_event()
            .map_err(|e| format!("X connection: {e}"))?
        {
            self.handle_event(event)?;
        }
        Ok(())
    }

    fn handle_event(&mut self, event: Event) -> Result<(), String> {
        match event {
            Event::Error(e) => return Err(format!("X protocol error: {e:?}")),
            Event::PresentCompleteNotify(done) => self.last_present_ts_us = Some(done.ust),
            Event::PresentIdleNotify(idle) => {
                if let Some(win) = self.window.as_mut() {
                    if let Some(slot) = win.slots.iter_mut().find(|s| s.pixmap == Some(idle.pixmap))
                    {
                        slot.busy = false;
                    } else if let Some(pos) =
                        win.dmabuf_pixmaps.iter().position(|p| *p == idle.pixmap)
                    {
                        let pixmap = win.dmabuf_pixmaps.swap_remove(pos);
                        self.conn
                            .free_pixmap(pixmap)
                            .map_err(|e| format!("free pixmap: {e}"))?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn present_cpu(
        &mut self,
        width: u32,
        height: u32,
        format: PixelFormat,
        data: &[u8],
    ) -> Result<(), String> {
        format::check_cpu_frame(width, height, format, data.len())?;
        if !self.has_shm {
            return Err("X server lacks MIT-SHM; CPU frames cannot be presented".to_owned());
        }
        self.ensure_window(width, height, format)?;
        let slot = self.free_slot()?;
        self.serial = self.serial.wrapping_add(1);
        let serial = self.serial;
        let win = self
            .window
            .as_ref()
            .ok_or("window missing after ensure_window")?;
        let s = &win.slots[slot];
        shm::write_pixels(&s.file, 0, data, format == PixelFormat::Rgba)?;
        let (w, h) = dims_u16(width, height)?;
        if let Some(pixmap) = s.pixmap {
            // Ordering: the upload is read by the server before the Present request is processed.
            self.conn
                .shm_put_image(
                    pixmap,
                    self.gc,
                    w,
                    h,
                    0,
                    0,
                    w,
                    h,
                    0,
                    0,
                    self.depth,
                    u8::from(ImageFormat::Z_PIXMAP),
                    false,
                    s.seg,
                    0,
                )
                .map_err(|e| format!("MIT-SHM put_image: {e}"))?;
            self.conn
                .present_pixmap(
                    win.id,
                    pixmap,
                    serial,
                    NONE,
                    NONE,
                    0,
                    0,
                    NONE,
                    NONE,
                    NONE,
                    0,
                    0,
                    0,
                    0,
                    &[],
                )
                .map_err(|e| format!("Present pixmap: {e}"))?;
            self.conn
                .flush()
                .map_err(|e| format!("flush X requests: {e}"))?;
            if let Some(win) = self.window.as_mut() {
                win.slots[slot].busy = true;
            }
        } else {
            self.conn
                .shm_put_image(
                    win.id,
                    self.gc,
                    w,
                    h,
                    0,
                    0,
                    w,
                    h,
                    0,
                    0,
                    self.depth,
                    u8::from(ImageFormat::Z_PIXMAP),
                    false,
                    s.seg,
                    0,
                )
                .map_err(|e| format!("MIT-SHM put_image: {e}"))?;
            // Round trip: the server has read the segment before the next frame overwrites it.
            self.conn
                .get_input_focus()
                .map_err(|e| format!("X round trip: {e}"))?
                .reply()
                .map_err(|e| format!("X round trip: {e}"))?;
        }
        Ok(())
    }

    /// Imports `image` as a DRI3 pixmap and queues it with Present.
    ///
    /// Legacy `DRI3 PixmapFromBuffer` carries one plane, no offset and no modifier, so this path
    /// imports only linear and implicit-modifier buffers. An explicit tiled or compressed modifier
    /// is refused rather than imported with the wrong layout. There is no fallback.
    fn present_dmabuf(
        &mut self,
        width: u32,
        height: u32,
        format: PixelFormat,
        image: &DmabufImage,
    ) -> Result<(), String> {
        if !self.has_dri3 {
            return Err("X server lacks DRI3; dmabuf frames cannot be presented".to_owned());
        }
        if format != PixelFormat::Bgrx {
            return Err(format!(
                "X11 dmabuf presentation needs BGRX, got {format:?}; convert first"
            ));
        }
        if image.modifier != DRM_FORMAT_MOD_LINEAR && image.modifier != DRM_FORMAT_MOD_INVALID {
            return Err(format!(
                "X11 DRI3 import handles only linear and implicit modifiers; modifier {:#018x} is not imported",
                image.modifier
            ));
        }
        let buffer_size = format::check_dmabuf(width, height, format, image)?;
        let [plane] = image.planes() else {
            return Err("BGRX dmabuf must have exactly one plane".to_owned());
        };
        if plane.offset != 0 {
            return Err(format!(
                "DRI3 PixmapFromBuffer has no offset; dmabuf plane starts at byte {}",
                plane.offset
            ));
        }
        let stride = u16::try_from(plane.stride)
            .map_err(|_| format!("dmabuf stride {} exceeds the DRI3 limit", plane.stride))?;
        let size = u32::try_from(buffer_size)
            .map_err(|_| format!("dmabuf size {buffer_size} exceeds the DRI3 limit"))?;
        let (w, h) = dims_u16(width, height)?;
        self.ensure_window(width, height, format)?;
        self.serial = self.serial.wrapping_add(1);
        let serial = self.serial;
        let pixmap = self
            .conn
            .generate_id()
            .map_err(|e| format!("allocate X id: {e}"))?;
        // The request consumes an fd, so it gets a duplicate; the frame keeps its own.
        let owned = image
            .fd
            .try_clone()
            .map_err(|e| format!("duplicate dmabuf fd: {e}"))?;
        let win = self
            .window
            .as_mut()
            .ok_or("window missing after ensure_window")?;
        self.conn
            .dri3_pixmap_from_buffer(pixmap, win.id, size, w, h, stride, self.depth, 32, owned)
            .map_err(|e| format!("DRI3 pixmap from buffer: {e}"))?;
        self.conn
            .present_pixmap(
                win.id,
                pixmap,
                serial,
                NONE,
                NONE,
                0,
                0,
                NONE,
                NONE,
                NONE,
                0,
                0,
                0,
                0,
                &[],
            )
            .map_err(|e| format!("Present pixmap: {e}"))?;
        self.conn
            .flush()
            .map_err(|e| format!("flush X requests: {e}"))?;
        win.dmabuf_pixmaps.push(pixmap);
        Ok(())
    }

    /// Recreates the window when size or format differ from the current one.
    fn ensure_window(
        &mut self,
        width: u32,
        height: u32,
        format: PixelFormat,
    ) -> Result<(), String> {
        let current = self.window.as_ref().map(|w| {
            (
                crate::resize::SurfaceSize {
                    width: w.width,
                    height: w.height,
                },
                w.format,
            )
        });
        let action = crate::resize::decide(current, width, height, format);
        if action == crate::resize::ResizeAction::Keep {
            return Ok(());
        }
        if let Some(old) = self.window.take() {
            self.destroy_window(old)?;
        }
        self.window = Some(self.create_window(width, height, format)?);
        Ok(())
    }

    fn create_window(
        &mut self,
        width: u32,
        height: u32,
        format: PixelFormat,
    ) -> Result<Window, String> {
        let (w, h) = dims_u16(width, height)?;
        let id = self
            .conn
            .generate_id()
            .map_err(|e| format!("allocate X id: {e}"))?;
        self.conn
            .create_window(
                self.depth,
                id,
                self.root,
                0,
                0,
                w,
                h,
                0,
                WindowClass::INPUT_OUTPUT,
                self.visual,
                &CreateWindowAux::new(),
            )
            .map_err(|e| format!("create window: {e}"))?;
        self.conn
            .change_property8(
                PropMode::REPLACE,
                id,
                AtomEnum::WM_NAME,
                AtomEnum::STRING,
                self.title.as_bytes(),
            )
            .map_err(|e| format!("set window title: {e}"))?;
        // Fixed size: min equals max, so the window stays at the frame size.
        let size = (i32::from(w), i32::from(h));
        WmSizeHints {
            min_size: Some(size),
            max_size: Some(size),
            ..WmSizeHints::new()
        }
        .set_normal_hints(&self.conn, id)
        .map_err(|e| format!("set size hints: {e}"))?;
        let fullscreen = (w, h) == self.root_size;
        if fullscreen {
            // Ask the compositor not to composite this window (EWMH bypass hint) and mark it fullscreen.
            self.conn
                .change_property32(
                    PropMode::REPLACE,
                    id,
                    self.atoms.net_wm_state,
                    AtomEnum::ATOM,
                    &[self.atoms.fullscreen],
                )
                .map_err(|e| format!("set _NET_WM_STATE: {e}"))?;
            self.conn
                .change_property32(
                    PropMode::REPLACE,
                    id,
                    self.atoms.bypass_compositor,
                    AtomEnum::CARDINAL,
                    &[1],
                )
                .map_err(|e| format!("set _NET_WM_BYPASS_COMPOSITOR: {e}"))?;
        }
        self.conn
            .map_window(id)
            .map_err(|e| format!("map window: {e}"))?;
        let mut unredirected = false;
        if fullscreen && self.has_composite {
            // Best effort: a server rejects unredirect for a window no compositor redirects
            // (BadValue on Xvfb). The outcome is recorded and the presenter keeps running.
            unredirected = self
                .conn
                .composite_unredirect_window(id, Redirect::MANUAL)
                .map_err(|e| format!("composite unredirect: {e}"))?
                .check()
                .is_ok();
        }
        if self.has_present {
            self.conn
                .present_select_input(
                    self.conn
                        .generate_id()
                        .map_err(|e| format!("allocate X id: {e}"))?,
                    id,
                    EventMask::COMPLETE_NOTIFY | EventMask::IDLE_NOTIFY,
                )
                .map_err(|e| format!("Present select input: {e}"))?;
        }
        let count = if self.has_present { PRESENT_SLOTS } else { 1 };
        let mut slots = Vec::with_capacity(count);
        for _ in 0..count {
            slots.push(self.create_slot(id, w, h)?);
        }
        Ok(Window {
            id,
            width,
            height,
            format,
            slots,
            dmabuf_pixmaps: Vec::new(),
            unredirected,
        })
    }

    fn create_slot(&self, window: xproto::Window, w: u16, h: u16) -> Result<Slot, String> {
        let len = u64::from(w) * u64::from(h) * 4;
        let file = shm::create(len)?;
        let seg = self
            .conn
            .generate_id()
            .map_err(|e| format!("allocate X id: {e}"))?;
        let seg_fd = OwnedFd::from(
            file.try_clone()
                .map_err(|e| format!("duplicate shm fd: {e}"))?,
        );
        self.conn
            .shm_attach_fd(seg, seg_fd, false)
            .map_err(|e| format!("MIT-SHM attach: {e}"))?;
        let pixmap = if self.has_present {
            let pixmap = self
                .conn
                .generate_id()
                .map_err(|e| format!("allocate X id: {e}"))?;
            self.conn
                .create_pixmap(self.depth, pixmap, window, w, h)
                .map_err(|e| format!("create pixmap: {e}"))?;
            Some(pixmap)
        } else {
            None
        };
        Ok(Slot {
            file,
            seg,
            pixmap,
            busy: false,
        })
    }

    fn destroy_window(&self, win: Window) -> Result<(), String> {
        for slot in win.slots {
            self.conn
                .shm_detach(slot.seg)
                .map_err(|e| format!("MIT-SHM detach: {e}"))?;
            if let Some(pixmap) = slot.pixmap {
                self.conn
                    .free_pixmap(pixmap)
                    .map_err(|e| format!("free pixmap: {e}"))?;
            }
        }
        for pixmap in win.dmabuf_pixmaps {
            self.conn
                .free_pixmap(pixmap)
                .map_err(|e| format!("free pixmap: {e}"))?;
        }
        self.conn
            .destroy_window(win.id)
            .map_err(|e| format!("destroy window: {e}"))?;
        self.conn
            .flush()
            .map_err(|e| format!("flush X requests: {e}"))
    }

    /// Returns a slot whose previous frame the server no longer reads. Blocks on
    /// `PresentIdleNotify` when every slot is still queued.
    fn free_slot(&mut self) -> Result<usize, String> {
        loop {
            if let Some(win) = self.window.as_ref()
                && let Some(slot) = win.slots.iter().position(|s| !s.busy)
            {
                return Ok(slot);
            }
            let event = self
                .conn
                .wait_for_event()
                .map_err(|e| format!("wait for X event: {e}"))?;
            self.handle_event(event)?;
        }
    }
}

impl FrameSink for X11Renderer {
    fn present(&mut self, frame: &VideoFrame) -> Result<(), String> {
        self.poll_events()?;
        match &frame.buffer {
            FrameBuffer::Cpu { data } => {
                self.present_cpu(frame.width, frame.height, frame.format, data)?;
            }
            FrameBuffer::Dmabuf(image) => {
                self.present_dmabuf(frame.width, frame.height, frame.format, image)?;
            }
        }
        self.present_count += 1;
        Ok(())
    }
}

impl std::fmt::Debug for X11Renderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("X11Renderer")
            .field("title", &self.title)
            .field("present_count", &self.present_count)
            .field("has_present", &self.has_present)
            .field("has_dri3", &self.has_dri3)
            .finish_non_exhaustive()
    }
}

fn intern(conn: &RustConnection, name: &str) -> Result<xproto::Atom, String> {
    let reply = conn
        .intern_atom(false, name.as_bytes())
        .map_err(|e| format!("intern {name}: {e}"))?
        .reply()
        .map_err(|e| format!("intern {name}: {e}"))?;
    Ok(reply.atom)
}

fn dims_u16(width: u32, height: u32) -> Result<(u16, u16), String> {
    let w =
        u16::try_from(width).map_err(|_| format!("frame width {width} exceeds the X11 limit"))?;
    let h = u16::try_from(height)
        .map_err(|_| format!("frame height {height} exceeds the X11 limit"))?;
    Ok((w, h))
}
