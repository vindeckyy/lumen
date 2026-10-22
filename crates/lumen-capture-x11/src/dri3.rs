//! DRI3 capture: the root region is copied into a pixmap that the server imports
//! from a DMA-BUF we allocate, so the frame never touches host memory.

use std::os::fd::OwnedFd;

use drm_fourcc::DrmFourcc;
use gbm::{BufferObject, Device as GbmDevice, Modifier};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::{dri3, xproto};
use x11rb::rust_connection::RustConnection;

use lumen_protocol::media::{DmabufImage, DmabufPlane};

use crate::Error;

/// A DRI3 device: a gbm device on the render node the server uses, plus a GC for copies.
#[derive(Debug)]
pub struct Device {
    gbm: GbmDevice<OwnedFd>,
    gc: xproto::Gcontext,
    depth: u8,
}

impl Device {
    /// Open DRI3 on `root`. `None` when the server has no usable DRI3 (extension missing,
    /// no render node, or the open request is refused), so the caller falls back.
    pub fn open(
        conn: &RustConnection,
        root: xproto::Window,
        depth: u8,
    ) -> Result<Option<Self>, Error> {
        if conn
            .extension_information(dri3::X11_EXTENSION_NAME)?
            .is_none()
        {
            return Ok(None);
        }
        dri3::query_version(conn, 1, 2)?.reply()?;
        let Ok(reply) = dri3::open(conn, root, 0)?.reply() else {
            return Ok(None);
        };
        let Ok(gbm) = GbmDevice::new(reply.device_fd) else {
            return Ok(None);
        };
        let gc = conn.generate_id()?;
        xproto::create_gc(conn, gc, root, &xproto::CreateGCAux::new())?.check()?;
        Ok(Some(Self { gbm, gc, depth }))
    }

    /// Copy `region` of `root` into a fresh DMA-BUF and return it as an image.
    ///
    /// Not fenced. The server's GPU copy can still be in flight when this returns; a CPU read
    /// then saw pre-copy pixels (zeros on first read, red after 500 ms, on Xwayland).
    /// Upgrade: wait on a DRI3 fence fd (xshmfence) before handing the frame out.
    pub fn grab(
        &self,
        conn: &RustConnection,
        root: xproto::Window,
        origin: (i16, i16),
        size: (u16, u16),
    ) -> Result<DmabufImage, Error> {
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
        // check() is a round-trip: the copy has executed, and any server error for it surfaces here.
        xproto::copy_area(conn, root, pixmap, self.gc, origin.0, origin.1, 0, 0, w, h)?.check()?;
        xproto::free_pixmap(conn, pixmap)?;
        Ok(image)
    }
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
