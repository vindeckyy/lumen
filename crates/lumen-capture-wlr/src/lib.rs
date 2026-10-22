//! Wayland capture through wlroots screencopy.
//!
//! Each frame is copied into a gbm DMA-BUF when the compositor offers
//! `linux_dmabuf` and a render node opens. Otherwise it is copied into a
//! `wl_shm` slot and read back with `pread`. `copy_with_damage` is used when
//! the output refreshes at least 1.5x as fast as the stream.
//!
//! [`WlrCapture::poll_frame`] never blocks. It returns `None` until a frame is
//! ready and admissible under the requested rate.
#![forbid(unsafe_code)]

mod buffers;
mod damage;
mod readback;
mod screencopy;

use std::fmt;
use std::io;
use std::time::Instant;

use buffers::{Dmabuf, DmabufBuffer, ShmPool, shm_format, shm_fourcc};
use damage::Pacer;
use drm_fourcc::DrmFourcc;
use lumen_protocol::media::{FrameBuffer, FrameSource, PixelFormat, VideoFrame};
use screencopy::{FrameEvents, Outcome, OutputEntry, Session, ShmSpec, State};
use wayland_client::{QueueHandle, backend::WaylandError, protocol::wl_output::WlOutput};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

pub use readback::read_pixels;

/// An output as announced by the compositor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputInfo {
    /// Connector name such as `DP-1`. `None` when the compositor predates `wl_output.name`.
    pub name: Option<String>,
    /// Current mode size in physical pixels.
    pub width: u32,
    pub height: u32,
    /// Current refresh rate in millihertz; zero when unknown.
    pub refresh_mhz: u32,
}

impl From<&OutputEntry> for OutputInfo {
    fn from(o: &OutputEntry) -> Self {
        Self {
            name: o.name.clone(),
            width: o.width,
            height: o.height,
            refresh_mhz: o.refresh_mhz,
        }
    }
}

/// Which output to capture and at what rate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamConfig {
    /// Output connector name. When `None`, the output whose mode is `width`x`height` is used.
    pub output: Option<String>,
    /// Required output size; frames are captured at this size.
    pub width: u32,
    pub height: u32,
    /// Stream rate. Frames arriving faster are dropped.
    pub fps: u32,
    /// Capture through `wl_shm` instead of DMA-BUF, so frames arrive as CPU bytes.
    ///
    /// DMA-BUF is the default. The CPU path exists for hosts whose capture
    /// buffer cannot be mapped, and to compare the two paths.
    pub prefer_cpu: bool,
}

/// Errors from connecting to, or capturing from, a compositor.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot connect to Wayland display: {0}")]
    Connect(#[from] wayland_client::ConnectError),
    #[error("Wayland globals: {0}")]
    Globals(#[from] wayland_client::globals::GlobalError),
    #[error("Wayland dispatch: {0}")]
    Dispatch(#[from] wayland_client::DispatchError),
    #[error("Wayland connection: {0}")]
    Wayland(#[from] WaylandError),
    #[error("system call: {0}")]
    Errno(#[from] rustix::io::Errno),
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
    #[error("compositor does not offer zwlr_screencopy_manager_v1")]
    NoScreencopy,
    #[error("compositor does not offer wl_shm")]
    NoShm,
    #[error("no DRM render node for the compositor's device")]
    NoRenderNode,
    #[error("no output named {0}")]
    OutputNotFound(String),
    #[error("no output with a {width}x{height} mode")]
    NoOutputForSize { width: u32, height: u32 },
    #[error("output is {}x{}, requested {}x{}", .actual.0, .actual.1, .requested.0, .requested.1)]
    OutputSizeMismatch {
        requested: (u32, u32),
        actual: (u32, u32),
    },
    #[error("unsupported buffer format {0:#010x}")]
    UnsupportedFormat(u32),
    #[error("compositor announced no buffer for the frame")]
    NoBufferSpec,
    #[error("screencopy frame failed; compositor copy returned failed")]
    FrameFailed,
    #[error("gbm: {0}")]
    Gbm(String),
    #[error("DMA-BUF image: {0}")]
    Dmabuf(String),
    #[error("invalid configuration: {0}")]
    InvalidConfig(&'static str),
}

/// Outputs with a current mode, in compositor announcement order.
///
/// # Errors
/// Fails when no Wayland display is reachable or the registry cannot be read.
pub fn list_outputs() -> Result<Vec<OutputInfo>, Error> {
    let session = screencopy::connect()?;
    Ok(session
        .state
        .outputs
        .iter()
        .filter(|o| o.width > 0)
        .map(OutputInfo::from)
        .collect())
}

/// `DRM_FORMAT_MOD_INVALID`: the driver chose no explicit modifier, so the layout is implicit.
///
/// A captured [`DmabufImage`](lumen_protocol::media::DmabufImage) carries the modifier the driver
/// reported, unchanged. This crate never substitutes a guessed value. A consumer that receives this
/// value must import the image without a modifier, so the driver applies its own implicit layout.
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// Screencopy capture of one output.
pub struct WlrCapture {
    session: Session,
    qh: QueueHandle<State>,
    manager: ZwlrScreencopyManagerV1,
    manager_version: u32,
    output: WlOutput,
    width: u32,
    height: u32,
    copy_with_damage: bool,
    dmabuf: Option<Dmabuf>,
    shm: Option<ShmPool>,
    pacer: Pacer,
    inflight: Option<InFlight>,
    next_index: u64,
    error: Option<Error>,
}

impl fmt::Debug for WlrCapture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WlrCapture")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("copy_with_damage", &self.copy_with_damage)
            .field("dmabuf", &self.dmabuf.is_some())
            .field("next_index", &self.next_index)
            .finish_non_exhaustive()
    }
}

/// A screencopy request awaiting `ready` or `failed`.
struct InFlight {
    frame: ZwlrScreencopyFrameV1,
    /// Set once the copy has been requested.
    target: Option<Target>,
}

/// Where the compositor is copying the in-flight frame.
enum Target {
    Shm { slot: usize, spec: ShmSpec },
    Dmabuf(DmabufBuffer),
}

impl WlrCapture {
    /// Connect to the compositor and start capturing the output named by `config`.
    ///
    /// DMA-BUF is used when the compositor offers it and a render node opens;
    /// otherwise frames arrive through `wl_shm`.
    ///
    /// # Errors
    /// Fails on a missing display, missing screencopy support, or when no
    /// output matches `config`.
    pub fn open(config: &StreamConfig) -> Result<Self, Error> {
        if config.width == 0 || config.height == 0 || config.fps == 0 {
            return Err(Error::InvalidConfig(
                "width, height and fps must be non-zero",
            ));
        }
        let session = screencopy::connect()?;
        let (manager, manager_version) = session
            .state
            .screencopy
            .clone()
            .ok_or(Error::NoScreencopy)?;
        let output = select_output(&session.state.outputs, config)?.clone();
        let dmabuf = if config.prefer_cpu {
            None
        } else {
            session
                .state
                .dmabuf
                .clone()
                .and_then(|factory| Dmabuf::open(session.state.main_device, factory).ok())
        };
        Ok(Self {
            qh: session.queue.handle(),
            manager,
            manager_version,
            output: output.wl,
            width: output.width,
            height: output.height,
            copy_with_damage: damage::use_damage_copy(
                output.refresh_mhz,
                config.fps,
                manager_version,
            ),
            dmabuf,
            shm: None,
            pacer: Pacer::new(config.fps),
            inflight: None,
            next_index: 0,
            error: None,
            session,
        })
    }

    /// Captured frame size in pixels.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Whether frames are copied into DMA-BUFs (otherwise `wl_shm`).
    #[must_use]
    pub fn uses_dmabuf(&self) -> bool {
        self.dmabuf.is_some()
    }

    /// Whether `copy_with_damage` is requested (output refresh at least 1.5x the stream rate).
    #[must_use]
    pub fn uses_damage_copy(&self) -> bool {
        self.copy_with_damage
    }

    /// The most recent error from [`FrameSource::poll_frame`], if any. Clears it.
    pub fn take_error(&mut self) -> Option<Error> {
        self.error.take()
    }

    fn advance(&mut self) -> Result<Option<VideoFrame>, Error> {
        self.pump()?;
        if self.inflight.is_none() {
            self.request_frame();
        }
        let ev = self.session.state.frame;
        match ev.outcome {
            Some(Outcome::Ready { capture_ts_us }) => {
                return self.finish(capture_ts_us, ev.y_invert);
            }
            Some(Outcome::Failed) => {
                self.abandon();
                return Err(Error::FrameFailed);
            }
            None => {}
        }
        let copying = self.inflight.as_ref().is_some_and(|f| f.target.is_none());
        if copying && (ev.buffer_done || (self.manager_version < 3 && ev.shm.is_some())) {
            self.start_copy(ev)?;
        }
        Ok(None)
    }

    /// Dispatch everything the compositor has sent, without blocking.
    fn pump(&mut self) -> Result<(), Error> {
        self.session
            .queue
            .dispatch_pending(&mut self.session.state)?;
        match self.session.conn.flush() {
            Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {}
            other => other?,
        }
        if let Some(guard) = self.session.queue.prepare_read() {
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }
        self.session
            .queue
            .dispatch_pending(&mut self.session.state)?;
        Ok(())
    }

    fn request_frame(&mut self) {
        self.session.state.frame = FrameEvents::default();
        let frame = self.manager.capture_output(1, &self.output, &self.qh, ());
        self.inflight = Some(InFlight {
            frame,
            target: None,
        });
    }

    fn start_copy(&mut self, ev: FrameEvents) -> Result<(), Error> {
        let dmabuf = match (&self.dmabuf, ev.dmabuf) {
            (Some(d), Some(spec)) if pixel_format(spec.fourcc).is_some() => {
                match d.allocate(spec, &self.qh) {
                    Ok(buffer) => Some(buffer),
                    Err(e) => {
                        // This frame falls back to wl_shm; the cause stays available through take_error().
                        self.error = Some(e);
                        None
                    }
                }
            }
            _ => None,
        };
        let target = match dmabuf {
            Some(buf) => Target::Dmabuf(buf),
            None => self.shm_target(ev)?,
        };
        let buffer = match &target {
            Target::Shm { slot, .. } => self.shm.as_ref().ok_or(Error::NoShm)?.buffer(*slot),
            Target::Dmabuf(d) => &d.buffer,
        };
        let Some(inflight) = self.inflight.as_mut() else {
            return Ok(());
        };
        if self.copy_with_damage {
            inflight.frame.copy_with_damage(buffer);
        } else {
            inflight.frame.copy(buffer);
        }
        inflight.target = Some(target);
        Ok(())
    }

    fn shm_target(&mut self, ev: FrameEvents) -> Result<Target, Error> {
        let spec = ev.shm.ok_or(Error::NoBufferSpec)?;
        if !self.shm.as_ref().is_some_and(|p| p.fits(spec)) {
            let shm = self.session.state.shm.clone().ok_or(Error::NoShm)?;
            let fresh = ShmPool::new(&shm, spec, &self.qh)?;
            if let Some(old) = self.shm.replace(fresh) {
                old.destroy();
            }
        }
        let slot = self.shm.as_mut().ok_or(Error::NoShm)?.next_slot();
        Ok(Target::Shm { slot, spec })
    }

    fn finish(&mut self, capture_ts_us: u64, y_invert: bool) -> Result<Option<VideoFrame>, Error> {
        let Some(inflight) = self.inflight.take() else {
            return Ok(None);
        };
        inflight.frame.destroy();
        let Some(target) = inflight.target else {
            return Ok(None);
        };
        if !self.pacer.admit(Instant::now()) {
            if let Target::Dmabuf(d) = target {
                d.buffer.destroy();
            }
            return Ok(None);
        }
        match target {
            Target::Dmabuf(d) => {
                d.buffer.destroy();
                if y_invert {
                    // DMA-BUF cannot be flipped in place. Drop this frame and use wl_shm from now on.
                    self.disable_dmabuf();
                    return Ok(None);
                }
                let format =
                    pixel_format(d.image.fourcc).ok_or(Error::UnsupportedFormat(d.image.fourcc))?;
                let index = self.bump_index();
                Ok(Some(VideoFrame {
                    width: d.width,
                    height: d.height,
                    format,
                    buffer: FrameBuffer::Dmabuf(d.image),
                    capture_ts_us,
                    index,
                }))
            }
            Target::Shm { slot, spec } => {
                let wl_format = shm_format(spec.format)?;
                let fourcc = shm_fourcc(wl_format);
                let format = pixel_format(fourcc).ok_or(Error::UnsupportedFormat(fourcc))?;
                let data = self
                    .shm
                    .as_ref()
                    .ok_or(Error::NoShm)?
                    .read(slot, y_invert)?;
                let index = self.bump_index();
                Ok(Some(VideoFrame::cpu(
                    spec.width,
                    spec.height,
                    format,
                    data,
                    capture_ts_us,
                    index,
                )))
            }
        }
    }

    /// Drop the in-flight frame after a failed copy.
    fn abandon(&mut self) {
        if let Some(inflight) = self.inflight.take() {
            inflight.frame.destroy();
            if let Some(Target::Dmabuf(d)) = inflight.target {
                d.buffer.destroy();
                self.disable_dmabuf();
            }
        }
    }

    fn disable_dmabuf(&mut self) {
        if let Some(d) = self.dmabuf.take() {
            d.destroy();
        }
    }

    fn bump_index(&mut self) -> u64 {
        let index = self.next_index;
        self.next_index += 1;
        index
    }
}

impl FrameSource for WlrCapture {
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

fn select_output<'a>(
    outputs: &'a [OutputEntry],
    config: &StreamConfig,
) -> Result<&'a OutputEntry, Error> {
    let found = match &config.output {
        Some(name) => outputs
            .iter()
            .find(|o| o.name.as_deref() == Some(name.as_str()))
            .ok_or_else(|| Error::OutputNotFound(name.clone()))?,
        None => outputs
            .iter()
            .find(|o| (o.width, o.height) == (config.width, config.height))
            .ok_or(Error::NoOutputForSize {
                width: config.width,
                height: config.height,
            })?,
    };
    if (found.width, found.height) != (config.width, config.height) {
        return Err(Error::OutputSizeMismatch {
            requested: (config.width, config.height),
            actual: (found.width, found.height),
        });
    }
    Ok(found)
}

/// Map a DRM fourcc to the `PixelFormat` a frame reports.
///
/// The alpha byte of `ARGB8888` and `ABGR8888` is not carried: such frames report `Bgrx` or `Rgba`,
/// and the byte is padding. A DMA-BUF frame keeps its real fourcc in `DmabufImage::fourcc`; a consumer
/// that needs the exact layout must read that field, not `PixelFormat::drm_fourcc()`.
fn pixel_format(fourcc: u32) -> Option<PixelFormat> {
    if fourcc == DrmFourcc::Xrgb8888 as u32 || fourcc == DrmFourcc::Argb8888 as u32 {
        Some(PixelFormat::Bgrx)
    } else if fourcc == DrmFourcc::Xbgr8888 as u32 || fourcc == DrmFourcc::Abgr8888 as u32 {
        Some(PixelFormat::Rgba)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xrgb_maps_to_bgrx_and_xbgr_to_rgba() {
        assert_eq!(
            pixel_format(DrmFourcc::Xrgb8888 as u32),
            Some(PixelFormat::Bgrx)
        );
        assert_eq!(
            pixel_format(DrmFourcc::Argb8888 as u32),
            Some(PixelFormat::Bgrx)
        );
        assert_eq!(
            pixel_format(DrmFourcc::Xbgr8888 as u32),
            Some(PixelFormat::Rgba)
        );
        assert_eq!(pixel_format(DrmFourcc::Rgb565 as u32), None);
    }

    #[test]
    fn wl_shm_enum_values_map_to_fourcc() {
        use wayland_client::protocol::wl_shm::Format;
        assert_eq!(shm_fourcc(Format::Argb8888), DrmFourcc::Argb8888 as u32);
        assert_eq!(shm_fourcc(Format::Xrgb8888), DrmFourcc::Xrgb8888 as u32);
        assert_eq!(shm_fourcc(Format::Xbgr8888), DrmFourcc::Xbgr8888 as u32);
    }
}
