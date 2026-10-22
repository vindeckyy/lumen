//! Buffers the compositor copies frames into: a `wl_shm` slot ring and gbm DMA-BUFs.

use std::fs::File;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::PathBuf;

use drm_fourcc::DrmFourcc;
use gbm::{BufferObject, Device, Modifier};
use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use wayland_client::{
    QueueHandle,
    protocol::{
        wl_buffer::WlBuffer,
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
    },
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::Flags as ParamsFlags, zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};

use lumen_protocol::media::{DmabufImage, DmabufPlane};
use wayland_client::WEnum;

use crate::Error;
use crate::screencopy::{DmabufSpec, ShmSpec, State};

/// Number of `wl_shm` slots. Frames rotate through them so a slot is not
/// rewritten by the compositor while it is being read back.
pub const SHM_SLOTS: usize = 3;

/// Bytes per pixel for every format this crate accepts.
pub const BYTES_PER_PIXEL: usize = 4;

/// DRM fourcc of `wl_shm` XRGB8888 and ARGB8888 (wl_shm enum values are not fourccs).
pub fn shm_fourcc(format: wl_shm::Format) -> u32 {
    match format {
        wl_shm::Format::Argb8888 => DrmFourcc::Argb8888 as u32,
        wl_shm::Format::Xrgb8888 => DrmFourcc::Xrgb8888 as u32,
        other => other as u32,
    }
}
/// The known `wl_shm` format, or the raw value the compositor sent.
pub fn shm_format(format: WEnum<wl_shm::Format>) -> Result<wl_shm::Format, Error> {
    match format {
        WEnum::Value(v) => Ok(v),
        WEnum::Unknown(raw) => Err(Error::UnsupportedFormat(raw)),
    }
}

/// A `wl_shm` pool of [`SHM_SLOTS`] equal frame slots backed by a memfd.
#[derive(Debug)]
pub struct ShmPool {
    memfd: OwnedFd,
    pool: WlShmPool,
    buffers: Vec<WlBuffer>,
    spec: ShmSpec,
    next_slot: usize,
}

impl ShmPool {
    /// Allocate the pool for frames of `spec`.
    pub fn new(shm: &WlShm, spec: ShmSpec, qh: &QueueHandle<State>) -> Result<Self, Error> {
        let format = shm_format(spec.format)?;
        let stride = u64::from(spec.stride);
        let frame_bytes = stride * u64::from(spec.height);
        let total = frame_bytes * SHM_SLOTS as u64;
        let too_big = || Error::InvalidConfig("frame too large for wl_shm");

        let memfd = memfd_create(c"lumen-screencopy", MemfdFlags::CLOEXEC)?;
        ftruncate(&memfd, total)?;
        let pool = shm.create_pool(
            memfd.as_fd(),
            i32::try_from(total).map_err(|_| too_big())?,
            qh,
            (),
        );
        let buffers = (0..SHM_SLOTS)
            .map(|slot| {
                let offset = i32::try_from(slot as u64 * frame_bytes).map_err(|_| too_big())?;
                Ok(pool.create_buffer(
                    offset,
                    i32::try_from(spec.width).map_err(|_| too_big())?,
                    i32::try_from(spec.height).map_err(|_| too_big())?,
                    i32::try_from(spec.stride).map_err(|_| too_big())?,
                    format,
                    qh,
                    (),
                ))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        Ok(Self {
            memfd,
            pool,
            buffers,
            spec,
            next_slot: 0,
        })
    }

    /// Whether this pool can hold frames of `spec` without reallocation.
    pub fn fits(&self, spec: ShmSpec) -> bool {
        self.spec == spec
    }

    /// Claim the next slot in rotation.
    pub fn next_slot(&mut self) -> usize {
        let slot = self.next_slot;
        self.next_slot = (self.next_slot + 1) % SHM_SLOTS;
        slot
    }

    /// The `wl_buffer` for `slot`.
    pub fn buffer(&self, slot: usize) -> &WlBuffer {
        &self.buffers[slot]
    }

    /// Read `slot` back as tightly packed rows, with `y_invert` applied.
    pub fn read(&self, slot: usize, y_invert: bool) -> Result<Vec<u8>, Error> {
        let stride = self.spec.stride as usize;
        let row = self.spec.width as usize * BYTES_PER_PIXEL;
        let rows = self.spec.height as usize;
        let mut data = vec![0u8; stride * rows];
        let offset = slot as u64 * (stride * rows) as u64;
        pread_exact(&self.memfd, &mut data, offset)?;
        if stride != row {
            for y in 1..rows {
                data.copy_within(y * stride..y * stride + row, y * row);
            }
            data.truncate(row * rows);
        }
        if y_invert {
            flip_rows(&mut data, row);
        }
        Ok(data)
    }

    /// Destroy the compositor-side objects. The memfd closes on drop.
    pub fn destroy(self) {
        for b in &self.buffers {
            b.destroy();
        }
        self.pool.destroy();
    }
}

fn pread_exact(fd: &OwnedFd, buf: &mut [u8], offset: u64) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let n = rustix::io::pread(fd, &mut buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        done += n;
    }
    Ok(())
}

fn flip_rows(data: &mut [u8], row: usize) {
    let rows = data.len() / row;
    for y in 0..rows / 2 {
        let (top, bottom) = data.split_at_mut((rows - 1 - y) * row);
        top[y * row..(y + 1) * row].swap_with_slice(&mut bottom[..row]);
    }
}

/// One DMA-BUF frame target: the gbm buffer, its wl_buffer, and the image the frame will carry.
#[derive(Debug)]
pub struct DmabufBuffer {
    pub buffer: WlBuffer,
    /// Kept alive until the copy completes. The fd exported in `image` keeps the memory alive after.
    _bo: BufferObject<()>,
    pub image: DmabufImage,
    pub width: u32,
    pub height: u32,
}

/// Describe a single-plane gbm buffer object as a [`DmabufImage`].
///
/// Screencopy formats are single-plane RGB, so anything else is refused. A multi-plane buffer object
/// can spread its planes over several GEM handles, and `bo.fd()` exports only one of them.
///
/// The modifier is the driver's value, passed through unchanged; nothing is substituted. `allocate` asks
/// for an explicit LINEAR list, so the driver normally reports `DRM_FORMAT_MOD_LINEAR`. If it reports
/// `DRM_FORMAT_MOD_INVALID` instead, the layout is implicit and the consumer must import it that way.
fn describe(bo: &BufferObject<()>, fd: OwnedFd, fourcc: u32) -> Result<DmabufImage, Error> {
    let count = bo.plane_count();
    if count != 1 {
        return Err(Error::Dmabuf(format!(
            "buffer object has {count} planes; only single-plane RGB is supported"
        )));
    }
    let plane = DmabufPlane {
        offset: bo.offset(0),
        stride: bo.stride_for_plane(0),
    };
    let modifier = u64::from(bo.modifier());
    DmabufImage::new(fd, fourcc, modifier, &[plane]).map_err(Error::Dmabuf)
}

/// gbm device plus the linux-dmabuf factory used to wrap allocations as `wl_buffer`s.
#[derive(Debug)]
pub struct Dmabuf {
    device: Device<File>,
    factory: ZwpLinuxDmabufV1,
}

impl Dmabuf {
    /// Open the render node matching `main_device`, or the first one if unknown.
    pub fn open(main_device: Option<u64>, factory: ZwpLinuxDmabufV1) -> Result<Self, Error> {
        let path = render_node(main_device)?;
        let device = Device::new(File::options().read(true).write(true).open(path)?)?;
        Ok(Self { device, factory })
    }

    /// Allocate a linear buffer for `spec`, wrapped as a `wl_buffer`.
    ///
    /// Allocation is per frame. The consumer owns the exported fd and the compositor writes into the
    /// buffer asynchronously, so no release point is known that would allow reusing it.
    pub fn allocate(
        &self,
        spec: DmabufSpec,
        qh: &QueueHandle<State>,
    ) -> Result<DmabufBuffer, Error> {
        let fourcc =
            DrmFourcc::try_from(spec.fourcc).map_err(|_| Error::UnsupportedFormat(spec.fourcc))?;
        let (w, h) = (spec.width, spec.height);
        // An explicit LINEAR modifier list. The LINEAR usage flag does not reach the same result:
        // on AMD it yields DRM_FORMAT_MOD_INVALID, and on NVIDIA it fails with EINVAL.
        let bo = self.device.create_buffer_object_with_modifiers::<()>(
            w,
            h,
            fourcc,
            [Modifier::Linear].into_iter(),
        )?;
        let fd = bo.fd().map_err(|e| Error::Gbm(e.to_string()))?;
        let image = describe(&bo, fd, spec.fourcc)?;

        // The compositor needs every plane attached, each with the same fd and modifier.
        let modifier = image.modifier;
        let params = self.factory.create_params(qh, ());
        for (index, plane) in (0u32..).zip(image.planes()) {
            params.add(
                image.fd.as_fd(),
                index,
                plane.offset,
                plane.stride,
                (modifier >> 32) as u32,
                modifier as u32,
            );
        }
        let buffer = params.create_immed(
            i32::try_from(w).map_err(|_| Error::InvalidConfig("frame too large"))?,
            i32::try_from(h).map_err(|_| Error::InvalidConfig("frame too large"))?,
            spec.fourcc,
            ParamsFlags::empty(),
            qh,
            (),
        );
        params.destroy();
        Ok(DmabufBuffer {
            buffer,
            _bo: bo,
            image,
            width: w,
            height: h,
        })
    }

    /// Destroy the factory proxy. The gbm device closes on drop.
    pub fn destroy(self) {
        self.factory.destroy();
    }
}

fn render_node(main_device: Option<u64>) -> Result<PathBuf, Error> {
    let mut nodes: Vec<PathBuf> = std::fs::read_dir("/dev/dri")?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("renderD"))
        })
        .collect();
    nodes.sort();
    match main_device {
        Some(dev) => nodes
            .into_iter()
            .find(|p| rustix::fs::stat(p.as_path()).is_ok_and(|s| s.st_rdev == dev))
            .ok_or(Error::NoRenderNode),
        None => nodes.into_iter().next().ok_or(Error::NoRenderNode),
    }
}

#[cfg(test)]
mod tests {
    use super::flip_rows;

    #[test]
    fn flip_rows_reverses_row_order() {
        let mut data = vec![1, 1, 2, 2, 3, 3];
        flip_rows(&mut data, 2);
        assert_eq!(data, vec![3, 3, 2, 2, 1, 1]);
    }

    #[test]
    fn flip_rows_keeps_odd_middle_row() {
        let mut data = vec![1, 2, 3, 4, 5, 6];
        flip_rows(&mut data, 2);
        assert_eq!(data, vec![5, 6, 3, 4, 1, 2]);
    }
}
