//! X11 presentation.
//!
//! One window lives for the whole session. The picture is composited into a window-sized target
//! with XRender and presented with the Present extension, scaled and positioned by
//! [`crate::fit::place`], so a resolution change or a window resize moves the picture and never
//! destroys the window. A child window carries the HUD text.
//!
//! CPU frames are copied into MIT-SHM buffers and dmabuf frames become DRI3 pixmaps. Both are
//! composited into the target, so the picture reaches the screen scaled to the window instead of
//! at its own size. The server must offer RENDER and Present.

use std::fs::File;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use lumen_protocol::media::{
    DRM_FORMAT_MOD_INVALID, DRM_FORMAT_MOD_LINEAR, DmabufImage, FrameBuffer, FrameSink, KeepAlive,
    PixelFormat, VideoFrame,
};
use x11rb::NONE;
use x11rb::connection::Connection;
use x11rb::properties::WmSizeHints;
use x11rb::protocol::Event;
use x11rb::protocol::composite::{ConnectionExt as _, Redirect};
use x11rb::protocol::dpms::ConnectionExt as _;
use x11rb::protocol::dri3::ConnectionExt as _;
use x11rb::protocol::present::{ConnectionExt as _, EventMask};
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::render::{self, ConnectionExt as _, PictOp, Pictformat};
use x11rb::protocol::shm::{self as x_shm, ConnectionExt as _};
use x11rb::protocol::xproto::{
    self, AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, CreateGCAux,
    CreateWindowAux, EventMask as XEventMask, ImageFormat, PropMode, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::fit::{self, Placement, VideoFit};
use crate::format;
use crate::overlay;
use crate::shm;

/// Frames in flight: one is on the screen while the next is composited.
const PRESENT_SLOTS: usize = 2;
/// The window is never smaller than this.
const MIN_VIEW: (u16, u16) = (320, 180);
/// The HUD sits this far from the window's top-left corner.
const OVERLAY_OFFSET: i16 = 8;

/// Presents frames to an X server. The window lives for the whole session.
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
    /// Whether the server can turn its display's power management off.
    has_dpms: bool,
    /// Whether the display's power management is off because of this renderer.
    keep_awake: bool,
    /// The picture format of the window's visual, used for the composite targets.
    render_format: Pictformat,
    atoms: Atoms,
    title: String,
    window: Option<Window>,
    serial: u32,
    present_count: u64,
    last_present_ts_us: Option<u64>,
    /// The id of the current window, shared with readers on other threads (0 before the first).
    window_id: Arc<AtomicU32>,
    /// The user asked to close the window.
    close_requested: bool,
    /// Whether the user wants the window fullscreen.
    fullscreen: bool,
    /// How the picture is fitted into the window.
    fit: VideoFit,
    /// The HUD text; empty hides the HUD.
    overlay_lines: Vec<String>,
}

#[derive(Clone, Copy)]
struct Atoms {
    net_wm_state: xproto::Atom,
    fullscreen: xproto::Atom,
    bypass_compositor: xproto::Atom,
    wm_protocols: xproto::Atom,
    wm_delete_window: xproto::Atom,
    net_wm_name: xproto::Atom,
    utf8_string: xproto::Atom,
}

/// The window and what it shows.
struct Window {
    id: xproto::Window,
    /// The size the server reports for the window.
    view: (u32, u32),
    /// The picture size and format last presented.
    frame: Option<(u32, u32, PixelFormat)>,
    /// Where the picture sits in the window.
    placement: Option<Placement>,
    /// The frames in flight.
    slots: Vec<Slot>,
    /// The HUD child window.
    overlay: Option<Overlay>,
    /// Whether the server accepted the composite unredirect request.
    unredirected: bool,
}

/// One frame in flight.
struct Slot {
    /// The shared memory a CPU frame is written into.
    upload: Option<Upload>,
    /// The window-sized target that is presented.
    target: Option<Target>,
    /// The dmabuf picture imported for this frame, freed when the target goes idle.
    imported: Option<Imported>,
    /// Whether the server still reads this slot's target.
    busy: bool,
}

/// The shared memory and the picture of it, at the picture's size.
struct Upload {
    file: File,
    seg: x_shm::Seg,
    pixmap: xproto::Pixmap,
    picture: render::Picture,
    size: (u32, u32),
}

/// The window-sized target a frame is composited into and presented from.
struct Target {
    pixmap: xproto::Pixmap,
    picture: render::Picture,
    size: (u32, u32),
}

/// A dmabuf imported as a pixmap, kept until the server is done reading it.
struct Imported {
    pixmap: xproto::Pixmap,
    picture: render::Picture,
    /// The lease of the pool slot the pixmap aliases. It is held, not read: dropping it frees
    /// the buffer, which must not happen while the server still reads the pixmap.
    _lease: Option<KeepAlive>,
}

/// The HUD child window.
struct Overlay {
    id: xproto::Window,
    size: (u32, u32),
    visible: bool,
}

impl X11Renderer {
    /// Connects to `display`, or to `DISPLAY` when `None`.
    ///
    /// # Errors
    /// Fails when the display is unreachable, its root depth is not 32 bits per pixel, or the
    /// server lacks RENDER or Present.
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
        let has_dpms = conn
            .dpms_get_version(1, 0)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some();
        if !has_present {
            return Err(
                "X server lacks the Present extension; frames cannot be presented".to_owned(),
            );
        }
        if conn
            .render_query_version(1, 0)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_none()
        {
            return Err(
                "X server lacks the RENDER extension; the picture cannot be scaled".to_owned(),
            );
        }
        // The composite target uses the picture format of the window's visual. A 32 bit visual
        // keeps an alpha channel, which the frames do not carry.
        let formats = conn
            .render_query_pict_formats()
            .map_err(|e| format!("query picture formats: {e}"))?
            .reply()
            .map_err(|e| format!("query picture formats: {e}"))?;
        let render_format = formats
            .formats
            .iter()
            .find(|f| {
                f.depth == depth
                    && f.type_ == render::PictType::DIRECT
                    && f.direct.red_mask != 0
                    && (depth != 32 || f.direct.alpha_mask != 0)
            })
            .map(|f| f.id)
            .ok_or_else(|| format!("X server has no direct picture format for depth {depth}"))?;
        let atoms = Atoms {
            net_wm_state: intern(&conn, "_NET_WM_STATE")?,
            fullscreen: intern(&conn, "_NET_WM_STATE_FULLSCREEN")?,
            bypass_compositor: intern(&conn, "_NET_WM_BYPASS_COMPOSITOR")?,
            wm_protocols: intern(&conn, "WM_PROTOCOLS")?,
            wm_delete_window: intern(&conn, "WM_DELETE_WINDOW")?,
            net_wm_name: intern(&conn, "_NET_WM_NAME")?,
            utf8_string: intern(&conn, "UTF8_STRING")?,
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
            has_dpms,
            keep_awake: false,
            render_format,
            atoms,
            title: title.to_owned(),
            window: None,
            serial: 0,
            present_count: 0,
            last_present_ts_us: None,
            window_id: Arc::new(AtomicU32::new(0)),
            close_requested: false,
            fullscreen: false,
            fit: VideoFit::default(),
            overlay_lines: Vec::new(),
        })
    }

    /// Timestamp from the most recent `PresentCompleteNotify`, in microseconds on the server
    /// clock (CLOCK_MONOTONIC). `None` until the first completion arrives.
    #[must_use]
    pub fn last_present_ts_us(&self) -> Option<u64> {
        self.last_present_ts_us
    }

    /// The id of the window frames are presented in, or 0 before the first frame. The window
    /// lives for the whole session, so a reader keeps the id.
    #[must_use]
    pub fn window_id(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.window_id)
    }

    /// Number of frames presented so far.
    #[must_use]
    pub fn present_count(&self) -> u64 {
        self.present_count
    }

    /// `(DRM fourcc, modifier)` pairs this renderer imports: linear BGRX through DRI3 and
    /// Present, the only layout legacy `PixmapFromBuffer` describes. None without both.
    #[must_use]
    pub fn dmabuf_formats(&self) -> Vec<(u32, u64)> {
        if self.has_dri3 && self.has_present {
            vec![(PixelFormat::Bgrx.drm_fourcc(), DRM_FORMAT_MOD_LINEAR)]
        } else {
            Vec::new()
        }
    }

    /// Whether the current window was accepted for composite unredirection (full-screen only).
    #[must_use]
    pub fn is_unredirected(&self) -> bool {
        self.window.as_ref().is_some_and(|w| w.unredirected)
    }

    /// Keeps the screen awake while the session runs: the display's power management is turned
    /// off, so the screen does not blank.
    ///
    /// # Errors
    /// Returns a description when the request cannot be sent. A server without the DPMS
    /// extension keeps its own setting, and this is not an error.
    pub fn set_keep_awake(&mut self, on: bool) -> Result<(), String> {
        if !self.has_dpms {
            return Ok(());
        }
        let result = if on {
            self.conn.dpms_disable()
        } else {
            self.conn.dpms_enable()
        };
        result
            .map_err(|e| {
                format!(
                    "{} the display's power management: {e}",
                    if on { "turn off" } else { "turn on" }
                )
            })
            .and_then(|cookie| {
                cookie
                    .check()
                    .map_err(|e| format!("the display's power management: {e}"))
            })?;
        self.keep_awake = on;
        Ok(())
    }

    /// Whether the user asked to close the window.
    #[must_use]
    pub fn close_requested(&self) -> bool {
        self.close_requested
    }

    /// The size of the client's screen in pixels: the biggest RandR monitor, or the whole X
    /// screen when RandR lists none.
    #[must_use]
    pub fn display_size(&self) -> Option<(u32, u32)> {
        // shortcut: the biggest monitor stands for the screen, not the one the window is on.
        let biggest = self
            .conn
            .randr_get_monitors(self.root, true)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .and_then(|reply| {
                reply
                    .monitors
                    .iter()
                    .map(|m| (u32::from(m.width), u32::from(m.height)))
                    .max_by_key(|&(w, h)| u64::from(w) * u64::from(h))
            });
        let (w, h) = biggest.unwrap_or((self.root_size.0.into(), self.root_size.1.into()));
        (w > 0 && h > 0).then_some((w, h))
    }

    /// The window's size in pixels, when the window exists.
    #[must_use]
    pub fn window_pixels(&self) -> Option<(u32, u32)> {
        self.window.as_ref().map(|w| w.view)
    }

    /// Where the picture sits in the window. `None` before the first frame.
    #[must_use]
    pub fn placement(&self) -> Option<Placement> {
        self.window.as_ref().and_then(|w| w.placement)
    }

    /// Makes the window fullscreen, or windowed again, through the window manager
    /// (`_NET_WM_STATE`).
    ///
    /// # Errors
    /// Returns a description when the request cannot be sent.
    pub fn set_fullscreen(&mut self, fullscreen: bool) -> Result<(), String> {
        if self.fullscreen == fullscreen {
            return Ok(());
        }
        self.fullscreen = fullscreen;
        let Some(window) = &self.window else {
            return Ok(());
        };
        // EWMH: a mapped window asks the window manager with a client message to the root.
        let event = ClientMessageEvent::new(
            32,
            window.id,
            self.atoms.net_wm_state,
            [u32::from(fullscreen), self.atoms.fullscreen, 0, 1, 0],
        );
        self.conn
            .send_event(
                false,
                self.root,
                XEventMask::SUBSTRUCTURE_REDIRECT | XEventMask::SUBSTRUCTURE_NOTIFY,
                event,
            )
            .map_err(|e| format!("send _NET_WM_STATE: {e}"))?;
        self.conn
            .flush()
            .map_err(|e| format!("flush X connection: {e}"))
    }

    /// Sets how the picture is fitted into the window.
    ///
    /// # Errors
    /// Returns a description when the request cannot be sent.
    pub fn set_fit(&mut self, fit: VideoFit) -> Result<(), String> {
        self.fit = fit;
        self.place_picture();
        Ok(())
    }

    /// Sets the HUD text. Empty text hides the HUD.
    ///
    /// # Errors
    /// Returns a description when the request cannot be sent.
    pub fn set_overlay(&mut self, lines: &[String]) -> Result<(), String> {
        lines.clone_into(&mut self.overlay_lines);
        self.apply_overlay()
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
                if let Some(win) = self.window.as_mut()
                    && let Some(slot) = win
                        .slots
                        .iter_mut()
                        .find(|s| s.target.as_ref().is_some_and(|t| t.pixmap == idle.pixmap))
                {
                    slot.busy = false;
                    // The imported picture was read into the target, so the server is done
                    // with it: the lease of the pool slot goes with it.
                    if let Some(imported) = slot.imported.take() {
                        self.conn
                            .render_free_picture(imported.picture)
                            .map_err(|e| format!("free picture: {e}"))?;
                        self.conn
                            .free_pixmap(imported.pixmap)
                            .map_err(|e| format!("free pixmap: {e}"))?;
                    }
                }
            }
            Event::ConfigureNotify(configure) => {
                if let Some(win) = self.window.as_mut()
                    && configure.window == win.id
                {
                    let size = (u32::from(configure.width), u32::from(configure.height));
                    if size != win.view {
                        win.view = size;
                        // The targets are window-sized; they are rebuilt on the next frame.
                        self.place_picture();
                    }
                }
            }
            Event::ClientMessage(message)
                if message.type_ == self.atoms.wm_protocols
                    && message.data.as_data32()[0] == self.atoms.wm_delete_window =>
            {
                self.close_requested = true;
            }
            _ => {}
        }
        Ok(())
    }

    /// Places the picture in the window, after the picture or the window changed size.
    fn place_picture(&mut self) {
        let fit = self.fit;
        let Some(window) = self.window.as_mut() else {
            return;
        };
        let Some((width, height, _)) = window.frame else {
            return;
        };
        window.placement = fit::place(fit, (width, height), window.view);
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
        self.ensure_window((width, height))?;
        self.set_frame(width, height, format);
        let slot = self.free_slot()?;
        let placement = self
            .window
            .as_ref()
            .and_then(|w| w.placement)
            .ok_or("the picture has no placement in the window")?;
        // The upload holds the frame at its own size; the target is window-sized.
        let source = self.ensure_upload(slot, (width, height))?;
        let upload = self
            .window
            .as_ref()
            .and_then(|w| w.slots[slot].upload.as_ref())
            .ok_or("the upload target is missing after ensure_upload")?;
        // A 32 bit visual keeps an alpha channel the frames do not carry; the frame is opaque.
        let opaque = self.depth == 32;
        shm::write_pixels(&upload.file, 0, data, format == PixelFormat::Rgba, opaque)?;
        let (w, h) = dims_u16(width, height)?;
        self.conn
            .shm_put_image(
                upload.pixmap,
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
                upload.seg,
                0,
            )
            .map_err(|e| format!("MIT-SHM put_image: {e}"))?;
        self.compose_and_present(slot, source, &placement)
    }

    /// Imports `image` as a DRI3 pixmap and presents it.
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
        self.ensure_window((width, height))?;
        self.set_frame(width, height, format);
        let slot = self.free_slot()?;
        let placement = self
            .window
            .as_ref()
            .and_then(|w| w.placement)
            .ok_or("the picture has no placement in the window")?;
        let pixmap = self
            .conn
            .generate_id()
            .map_err(|e| format!("allocate X id: {e}"))?;
        // The request consumes an fd, so it gets a duplicate; the frame keeps its own.
        let owned = image
            .fd
            .try_clone()
            .map_err(|e| format!("duplicate dmabuf fd: {e}"))?;
        let window = self
            .window
            .as_ref()
            .ok_or("window missing after ensure_window")?;
        self.conn
            .dri3_pixmap_from_buffer(pixmap, window.id, size, w, h, stride, self.depth, 32, owned)
            .map_err(|e| format!("DRI3 pixmap from buffer: {e}"))?;
        let picture = self.create_picture(pixmap)?;
        if let Some(slot) = self.window.as_mut().and_then(|w| w.slots.get_mut(slot)) {
            slot.imported = Some(Imported {
                pixmap,
                picture,
                _lease: image.keepalive.clone(),
            });
        }
        self.compose_and_present(slot, picture, &placement)
    }

    /// Composites the source picture into the slot's target and presents the target.
    ///
    /// The transform maps the target rectangle onto the visible part of the frame, so the
    /// picture is scaled and cropped in one step.
    fn compose_and_present(
        &mut self,
        slot: usize,
        source: render::Picture,
        placement: &Placement,
    ) -> Result<(), String> {
        let target = self.ensure_target(slot)?;
        let (dst_x, dst_y, dst_w, dst_h) = (
            placement.dst[0],
            placement.dst[1],
            placement.dst[2],
            placement.dst[3],
        );
        let (src_x, src_y, src_w, src_h) = (
            placement.src[0],
            placement.src[1],
            placement.src[2],
            placement.src[3],
        );
        if dst_w <= 0 || dst_h <= 0 {
            return Err("the picture has no room in the window".to_owned());
        }
        // The transform maps the target rectangle onto the visible part of the picture: the
        // source offset is the translation and the scale is the inverse of the magnification.
        // The translation sits in matrix13 and matrix23; matrix31 and matrix32 belong to the
        // divisor row, and a value there collapses the whole transform.
        let transform = render::Transform {
            matrix11: fixed(src_w / f64::from(dst_w)),
            matrix12: 0,
            matrix13: fixed(src_x),
            matrix21: 0,
            matrix22: fixed(src_h / f64::from(dst_h)),
            matrix23: fixed(src_y),
            matrix31: 0,
            matrix32: 0,
            matrix33: 1 << 16,
        };
        self.conn
            .render_set_picture_transform(source, transform)
            .map_err(|e| format!("set picture transform: {e}"))?;
        self.conn
            .render_set_picture_filter(source, b"bilinear", &[])
            .map_err(|e| format!("set picture filter: {e}"))?;
        // The bars around the picture are black.
        let (tw, th) = {
            let target = self
                .window
                .as_ref()
                .and_then(|w| w.slots[slot].target.as_ref())
                .ok_or("the composite target is missing")?;
            target.size
        };
        let rects = [xproto::Rectangle {
            x: 0,
            y: 0,
            width: u16::try_from(tw).unwrap_or(u16::MAX),
            height: u16::try_from(th).unwrap_or(u16::MAX),
        }];
        self.conn
            .render_fill_rectangles(
                PictOp::CLEAR,
                target,
                render::Color {
                    red: 0,
                    green: 0,
                    blue: 0,
                    alpha: u16::MAX,
                },
                &rects,
            )
            .map_err(|e| format!("clear the composite target: {e}"))?;
        self.conn
            .render_composite(
                PictOp::SRC,
                source,
                u32::from(render::PictureEnum::NONE),
                target,
                0,
                0,
                0,
                0,
                dst_x as i16,
                dst_y as i16,
                dst_w as u16,
                dst_h as u16,
            )
            .map_err(|e| format!("composite the picture: {e}"))?;
        let pixmap = self
            .window
            .as_ref()
            .and_then(|w| w.slots[slot].target.as_ref())
            .map(|t| t.pixmap)
            .ok_or("the composite target is missing")?;
        self.serial = self.serial.wrapping_add(1);
        let serial = self.serial;
        let window = self
            .window
            .as_ref()
            .ok_or("window missing after ensure_window")?;
        self.conn
            .present_pixmap(
                window.id,
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
        if let Some(slot) = self.window.as_mut().and_then(|w| w.slots.get_mut(slot)) {
            slot.busy = true;
        }
        Ok(())
    }

    /// Records the picture's size and format, and places it in the window.
    fn set_frame(&mut self, width: u32, height: u32, format: PixelFormat) {
        if let Some(window) = self.window.as_mut() {
            window.frame = Some((width, height, format));
        }
        self.place_picture();
    }

    /// Creates the window at the first picture's size. The window lives until the renderer does.
    fn ensure_window(&mut self, frame: (u32, u32)) -> Result<(), String> {
        if self.window.is_some() {
            return Ok(());
        }
        let view = fit::initial_size(frame, self.display_size(), 1);
        let window = self.create_window(view)?;
        self.window = Some(window);
        self.window_id
            .store(self.window.as_ref().map_or(0, |w| w.id), Ordering::Release);
        self.apply_overlay()?;
        Ok(())
    }

    fn create_window(&mut self, view: (u32, u32)) -> Result<Window, String> {
        let (w, h) = dims_u16(view.0, view.1)?;
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
                &CreateWindowAux::new().event_mask(XEventMask::STRUCTURE_NOTIFY),
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
        self.conn
            .change_property8(
                PropMode::REPLACE,
                id,
                self.atoms.net_wm_name,
                self.atoms.utf8_string,
                self.title.as_bytes(),
            )
            .map_err(|e| format!("set window title: {e}"))?;
        // WM_CLASS is the instance and the class, each ending in a NUL.
        let class = format!("{0}\0{0}\0", crate::APP_ID);
        self.conn
            .change_property8(
                PropMode::REPLACE,
                id,
                AtomEnum::WM_CLASS,
                AtomEnum::STRING,
                class.as_bytes(),
            )
            .map_err(|e| format!("set WM_CLASS: {e}"))?;
        // The close button asks instead of killing the connection.
        self.conn
            .change_property32(
                PropMode::REPLACE,
                id,
                self.atoms.wm_protocols,
                AtomEnum::ATOM,
                &[self.atoms.wm_delete_window],
            )
            .map_err(|e| format!("set WM_PROTOCOLS: {e}"))?;
        // The window may shrink to the minimum, but it does not have to: the picture is scaled.
        WmSizeHints {
            min_size: Some((i32::from(MIN_VIEW.0), i32::from(MIN_VIEW.1))),
            ..WmSizeHints::new()
        }
        .set_normal_hints(&self.conn, id)
        .map_err(|e| format!("set size hints: {e}"))?;
        let fullscreen = self.fullscreen || (w, h) == self.root_size;
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
        self.conn
            .present_select_input(
                self.conn
                    .generate_id()
                    .map_err(|e| format!("allocate X id: {e}"))?,
                id,
                EventMask::COMPLETE_NOTIFY | EventMask::IDLE_NOTIFY,
            )
            .map_err(|e| format!("Present select input: {e}"))?;
        let mut slots = Vec::with_capacity(PRESENT_SLOTS);
        for _ in 0..PRESENT_SLOTS {
            slots.push(Slot {
                upload: None,
                target: None,
                imported: None,
                busy: false,
            });
        }
        Ok(Window {
            id,
            view,
            frame: None,
            placement: None,
            slots,
            overlay: None,
            unredirected,
        })
    }

    /// The shared memory a CPU frame is written into, rebuilt when the picture's size changes.
    fn ensure_upload(&mut self, slot: usize, size: (u32, u32)) -> Result<render::Picture, String> {
        let window = self
            .window
            .as_ref()
            .ok_or("window missing before the upload")?;
        let stale = window.slots[slot]
            .upload
            .as_ref()
            .is_none_or(|u| u.size != size);
        if stale {
            let old = self
                .window
                .as_mut()
                .and_then(|w| w.slots[slot].upload.take());
            if let Some(old) = old {
                self.conn
                    .render_free_picture(old.picture)
                    .map_err(|e| format!("free picture: {e}"))?;
                self.conn
                    .free_pixmap(old.pixmap)
                    .map_err(|e| format!("free pixmap: {e}"))?;
                self.conn
                    .shm_detach(old.seg)
                    .map_err(|e| format!("MIT-SHM detach: {e}"))?;
            }
            let (w, h) = dims_u16(size.0, size.1)?;
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
            let pixmap = self
                .conn
                .generate_id()
                .map_err(|e| format!("allocate X id: {e}"))?;
            self.conn
                .create_pixmap(self.depth, pixmap, self.root, w, h)
                .map_err(|e| format!("create pixmap: {e}"))?;
            let picture = self.create_picture(pixmap)?;
            if let Some(slot) = self.window.as_mut().and_then(|w| w.slots.get_mut(slot)) {
                slot.upload = Some(Upload {
                    file,
                    seg,
                    pixmap,
                    picture,
                    size,
                });
            }
        }
        self.window
            .as_ref()
            .and_then(|w| w.slots[slot].upload.as_ref())
            .map(|u| u.picture)
            .ok_or_else(|| "the upload target is missing after ensure_upload".to_owned())
    }

    /// The window-sized target a frame is composited into, rebuilt when the window changes size.
    fn ensure_target(&mut self, slot: usize) -> Result<render::Picture, String> {
        let view = self
            .window
            .as_ref()
            .ok_or("window missing before the target")?
            .view;
        let stale = self
            .window
            .as_ref()
            .is_none_or(|w| w.slots[slot].target.as_ref().is_none_or(|t| t.size != view));
        if stale {
            let old = self
                .window
                .as_mut()
                .and_then(|w| w.slots[slot].target.take());
            if let Some(old) = old {
                self.conn
                    .render_free_picture(old.picture)
                    .map_err(|e| format!("free picture: {e}"))?;
                self.conn
                    .free_pixmap(old.pixmap)
                    .map_err(|e| format!("free pixmap: {e}"))?;
            }
            let (w, h) = dims_u16(view.0, view.1)?;
            let pixmap = self
                .conn
                .generate_id()
                .map_err(|e| format!("allocate X id: {e}"))?;
            self.conn
                .create_pixmap(self.depth, pixmap, self.root, w, h)
                .map_err(|e| format!("create pixmap: {e}"))?;
            let picture = self.create_picture(pixmap)?;
            if let Some(slot) = self.window.as_mut().and_then(|w| w.slots.get_mut(slot)) {
                slot.target = Some(Target {
                    pixmap,
                    picture,
                    size: view,
                });
            }
        }
        self.window
            .as_ref()
            .and_then(|w| w.slots[slot].target.as_ref())
            .map(|t| t.picture)
            .ok_or_else(|| "the composite target is missing after ensure_target".to_owned())
    }

    /// A picture of a drawable in the visual's picture format.
    fn create_picture(&self, drawable: xproto::Drawable) -> Result<render::Picture, String> {
        let picture = self
            .conn
            .generate_id()
            .map_err(|e| format!("allocate X id: {e}"))?;
        self.conn
            .render_create_picture(
                picture,
                drawable,
                self.render_format,
                &render::CreatePictureAux::new(),
            )
            .map_err(|e| format!("create picture: {e}"))?;
        Ok(picture)
    }

    /// Draws the HUD text into the child window. Empty text hides it.
    fn apply_overlay(&mut self) -> Result<(), String> {
        let image = overlay::rasterize(&self.overlay_lines, 1);
        let Some(window) = self.window.as_mut() else {
            return Ok(());
        };
        let Some(image) = image else {
            if let Some(overlay) = window.overlay.as_mut()
                && overlay.visible
            {
                self.conn
                    .unmap_window(overlay.id)
                    .map_err(|e| format!("unmap the overlay window: {e}"))?;
                overlay.visible = false;
            }
            return Ok(());
        };
        let (w, h) = dims_u16(image.width, image.height)?;
        match window.overlay.as_mut() {
            Some(overlay) if overlay.size == (image.width, image.height) => {}
            Some(overlay) => {
                self.conn
                    .configure_window(
                        overlay.id,
                        &ConfigureWindowAux::new()
                            .width(u32::from(w))
                            .height(u32::from(h)),
                    )
                    .map_err(|e| format!("resize the overlay window: {e}"))?;
                overlay.size = (image.width, image.height);
            }
            None => {
                let id = self
                    .conn
                    .generate_id()
                    .map_err(|e| format!("allocate X id: {e}"))?;
                self.conn
                    .create_window(
                        self.depth,
                        id,
                        window.id,
                        OVERLAY_OFFSET,
                        OVERLAY_OFFSET,
                        w,
                        h,
                        0,
                        WindowClass::INPUT_OUTPUT,
                        self.visual,
                        &CreateWindowAux::new().background_pixel(0),
                    )
                    .map_err(|e| format!("create the overlay window: {e}"))?;
                window.overlay = Some(Overlay {
                    id,
                    size: (image.width, image.height),
                    visible: false,
                });
            }
        }
        let overlay = window
            .overlay
            .as_ref()
            .ok_or("the overlay window is missing")?;
        // The image goes through shared memory: a large overlay does not fit in one request.
        let len = u64::from(image.width) * u64::from(image.height) * 4;
        let file = shm::create(len)?;
        shm::write_pixels(&file, 0, &image.pixels, false, false)?;
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
        self.conn
            .shm_put_image(
                overlay.id,
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
                seg,
                0,
            )
            .map_err(|e| format!("MIT-SHM put_image: {e}"))?;
        self.conn
            .shm_detach(seg)
            .map_err(|e| format!("MIT-SHM detach: {e}"))?;
        if !overlay.visible {
            self.conn
                .map_window(overlay.id)
                .map_err(|e| format!("map the overlay window: {e}"))?;
            if let Some(overlay) = self.window.as_mut().and_then(|w| w.overlay.as_mut()) {
                overlay.visible = true;
            }
        }
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

impl Drop for X11Renderer {
    fn drop(&mut self) {
        // The display's power management belongs to the session, not to the client: it goes
        // back on when the client goes.
        if self.keep_awake {
            let _ = self.set_keep_awake(false);
        }
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

/// A factor as the 16.16 fixed point the RENDER transform uses.
fn fixed(value: f64) -> render::Fixed {
    (value * 65536.0).round() as i32
}
