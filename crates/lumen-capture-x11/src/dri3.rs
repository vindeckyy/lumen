//! DRI3 capture: the root region is copied into a pixmap that the server imports
//! from a DMA-BUF we allocate, so the frame never touches host memory.
//!
//! The copy is fenced. After the copy the client triggers an X SYNC fence, and a
//! glamor server flushes its pending rendering when a fence is triggered. A round
//! trip after the trigger therefore means the copy has been submitted to the
//! kernel, and the buffer carries the kernel's implicit write fence: a GPU reader
//! on the same device, or a CPU reader that brackets its read with
//! `DMA_BUF_IOCTL_SYNC`, waits for the copy to finish.

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use drm_fourcc::DrmFourcc;
use gbm::{BufferObject, Device as GbmDevice, Modifier};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::{dri3, sync, xproto};
use x11rb::rust_connection::RustConnection;

use lumen_protocol::media::{DmabufImage, DmabufPlane};

use crate::Error;

/// Buffers the device copies into, in turn. A reader holds at most one frame while the next is
/// captured, and the driver orders a copy into a buffer after earlier reads of it.
const SLOTS: usize = 3;

/// One buffer the server copies into, kept for as long as the size does not change.
#[derive(Debug)]
struct Slot {
    /// Kept alive so the memory behind the exported fd stays allocated.
    _bo: BufferObject<()>,
    image: DmabufImage,
    pixmap: xproto::Pixmap,
}

/// A DRI3 device: a gbm device on the render node the server uses, plus a GC and a fence.
#[derive(Debug)]
pub struct Device {
    gbm: GbmDevice<OwnedFd>,
    node: Option<PathBuf>,
    gc: xproto::Gcontext,
    fence: sync::Fence,
    depth: u8,
    size: (u16, u16),
    slots: Vec<Slot>,
    next: usize,
}

impl Device {
    /// Open DRI3 on `root`. `None` when the server has no usable DRI3 or SYNC (extension
    /// missing, no render node, or the open request is refused), so the caller falls back.
    pub fn open(
        conn: &RustConnection,
        root: xproto::Window,
        depth: u8,
    ) -> Result<Option<Self>, Error> {
        for name in [dri3::X11_EXTENSION_NAME, sync::X11_EXTENSION_NAME] {
            if conn.extension_information(name)?.is_none() {
                return Ok(None);
            }
        }
        dri3::query_version(conn, 1, 2)?.reply()?;
        sync::initialize(conn, 3, 1)?.reply()?;
        let Ok(reply) = dri3::open(conn, root, 0)?.reply() else {
            return Ok(None);
        };
        let node = std::fs::read_link(format!("/proc/self/fd/{}", reply.device_fd.as_raw_fd()))
            .ok()
            .and_then(|path| render_node(&path));
        let Ok(gbm) = GbmDevice::new(reply.device_fd) else {
            return Ok(None);
        };
        let gc = conn.generate_id()?;
        // The root's own pixels are only its background: the windows on top are its children,
        // and a copy clips them out unless the GC includes inferiors (GetImage includes them).
        let gc_aux =
            xproto::CreateGCAux::new().subwindow_mode(xproto::SubwindowMode::INCLUDE_INFERIORS);
        xproto::create_gc(conn, gc, root, &gc_aux)?.check()?;
        let fence = conn.generate_id()?;
        sync::create_fence(conn, root, fence, false)?.check()?;
        Ok(Some(Self {
            gbm,
            node,
            gc,
            fence,
            depth,
            size: (0, 0),
            slots: Vec::new(),
            next: 0,
        }))
    }

    /// The render node the buffers are allocated on, when it is known.
    #[must_use]
    pub fn node(&self) -> Option<&Path> {
        self.node.as_deref()
    }

    /// Copy `region` of `root` into the next buffer and return it as an image. The copy has been
    /// submitted to the GPU when this returns (see the module comment).
    pub fn grab(
        &mut self,
        conn: &RustConnection,
        root: xproto::Window,
        origin: (i16, i16),
        size: (u16, u16),
    ) -> Result<DmabufImage, Error> {
        if self.size != size || self.slots.is_empty() {
            for slot in self.slots.drain(..) {
                xproto::free_pixmap(conn, slot.pixmap)?;
            }
            for _ in 0..SLOTS {
                let slot = self.slot(conn, root, size)?;
                self.slots.push(slot);
            }
            self.size = size;
        }
        let slot = &self.slots[self.next];
        self.next = (self.next + 1) % self.slots.len();
        let (w, h) = size;
        xproto::copy_area(
            conn,
            root,
            slot.pixmap,
            self.gc,
            origin.0,
            origin.1,
            0,
            0,
            w,
            h,
        )?;
        // Triggering the fence makes a glamor server flush the copy; resetting it makes it
        // ready for the next frame. check() is the round trip that waits for both.
        sync::trigger_fence(conn, self.fence)?;
        sync::reset_fence(conn, self.fence)?.check()?;
        let fd = slot.image.fd.try_clone()?;
        DmabufImage::new(
            fd,
            slot.image.fourcc,
            slot.image.modifier,
            slot.image.planes(),
        )
        .map_err(Error::Gbm)
    }

    /// Allocate one buffer of `size` and import it into the server as a pixmap.
    fn slot(
        &self,
        conn: &RustConnection,
        root: xproto::Window,
        size: (u16, u16),
    ) -> Result<Slot, Error> {
        let (w, h) = size;
        let fourcc = DrmFourcc::Xrgb8888;
        // An explicit LINEAR modifier list; the LINEAR usage flag yields DRM_FORMAT_MOD_INVALID on AMD and fails on NVIDIA.
        let bo = self.gbm.create_buffer_object_with_modifiers::<()>(
            u32::from(w),
            u32::from(h),
            fourcc,
            [Modifier::Linear].into_iter(),
        )?;
        let fd = bo.fd().map_err(|e| Error::Gbm(e.to_string()))?;
        let image = describe(&bo, fd, fourcc as u32)?;
        // describe() guarantees exactly one plane.
        let stride = image.planes()[0].stride;
        let stride16 = u16::try_from(stride)
            .map_err(|_| Error::Unsupported("row stride exceeds DRI3 limit"))?;
        let size_bytes = stride
            .checked_mul(u32::from(h))
            .ok_or(Error::Unsupported("DMA-BUF size overflows"))?;
        let pixmap = conn.generate_id()?;
        dri3::pixmap_from_buffer(
            conn,
            pixmap,
            root,
            size_bytes,
            w,
            h,
            stride16,
            self.depth,
            32,
            image.fd.try_clone()?,
        )?
        .check()?;
        Ok(Slot {
            _bo: bo,
            image,
            pixmap,
        })
    }
}

/// The render node of the DRM device at `path`: the path itself when it is a render node, else
/// the render node of the same device found through sysfs.
fn render_node(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    if name.starts_with("renderD") {
        return Some(path.to_path_buf());
    }
    std::fs::read_dir(format!("/sys/class/drm/{name}/device/drm"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name())
        .find_map(|entry| {
            let entry = entry.to_str()?;
            entry
                .starts_with("renderD")
                .then(|| Path::new("/dev/dri").join(entry))
        })
}

/// Describe a single-plane gbm buffer object as a [`DmabufImage`].
///
/// The modifier is the driver's value, passed through unchanged. `grab` asks for an explicit LINEAR list,
/// so the driver normally reports `DRM_FORMAT_MOD_LINEAR`. `DRM_FORMAT_MOD_INVALID` (`0x00ffffffffffffff`)
/// would mean an implicit layout, and is passed on as is. Single-plane only: `bo.fd()` exports one GEM
/// handle, which may not hold every plane.
fn describe(bo: &BufferObject<()>, fd: OwnedFd, fourcc: u32) -> Result<DmabufImage, Error> {
    let count = bo.plane_count();
    if count != 1 {
        return Err(Error::Gbm(format!(
            "buffer object has {count} planes; only single-plane RGB is supported"
        )));
    }
    let plane = DmabufPlane {
        offset: bo.offset(0),
        stride: bo.stride_for_plane(0),
    };
    DmabufImage::new(fd, fourcc, u64::from(bo.modifier()), &[plane]).map_err(Error::Gbm)
}
