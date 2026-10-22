//! CPU read access to DMA-BUF memory.
//!
//! The mapping and the `DMA_BUF_IOCTL_SYNC` calls live here because this is the only crate
//! allowed to contain `unsafe`.

use std::{
    ffi::c_void,
    io,
    os::fd::{AsRawFd, BorrowedFd},
    ptr, slice,
};

use rustix::mm::{MapFlags, ProtFlags};

use crate::error::{VaError, VaResult};

/// `DMA_BUF_SYNC_READ`: the CPU reads the buffer.
const DMA_BUF_SYNC_READ: u64 = 1 << 0;
/// `DMA_BUF_SYNC_START`: begin CPU access, waiting for pending GPU writes.
const DMA_BUF_SYNC_START: u64 = 0;
/// `DMA_BUF_SYNC_END`: end CPU access, making CPU-side state visible to the device.
const DMA_BUF_SYNC_END: u64 = 1 << 2;

mod sync_ioctl {
    // DMA_BUF_IOCTL_SYNC = _IOW('b', 0, struct dma_buf_sync), and that struct is one u64 of flags.
    nix::ioctl_write_ptr!(dma_buf_sync, b'b', 0, u64);
}

/// Maps `len` bytes of a DMA-BUF read-only and passes them to `f`.
///
/// The read is bracketed by `DMA_BUF_IOCTL_SYNC` so the CPU sees the device's completed writes.
/// The mapping is removed before this returns, even if `f` panics.
///
/// # Errors
/// [`VaError::Invalid`] for a zero length, and [`VaError::Io`] if `mmap` or a sync ioctl fails.
pub fn read_dmabuf<R>(fd: BorrowedFd<'_>, len: usize, f: impl FnOnce(&[u8]) -> R) -> VaResult<R> {
    if len == 0 {
        return Err(VaError::Invalid("DMA-BUF read length is zero"));
    }
    // SAFETY: this maps a new region at a kernel-chosen address (NULL hint) with PROT_READ and
    // MAP_SHARED, covering `len` bytes from offset 0 of `fd`. `fd` is open for the call because it
    // is a `BorrowedFd`. The region is owned by `Mapping`, which unmaps it on every exit path.
    let ptr = unsafe {
        rustix::mm::mmap(
            ptr::null_mut(),
            len,
            ProtFlags::READ,
            MapFlags::SHARED,
            fd,
            0,
        )
    }
    .map_err(io::Error::from)?;
    let mapping = Mapping { ptr, len };

    dma_buf_sync(fd, DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ)?;
    // SAFETY: `ptr` covers `len` readable bytes for as long as `mapping` lives, which is past this
    // borrow. The mapping is read-only and no other reference writes to it through this process.
    let bytes = unsafe { slice::from_raw_parts(mapping.ptr.cast::<u8>(), mapping.len) };
    let result = f(bytes);
    let ended = dma_buf_sync(fd, DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ);
    ended.map(|()| result)
}

fn dma_buf_sync(fd: BorrowedFd<'_>, flags: u64) -> VaResult<()> {
    // SAFETY: `fd` is an open DMA-BUF file descriptor. The kernel reads one `u64` of flags from
    // `&flags`, which lives for the duration of the call.
    unsafe { sync_ioctl::dma_buf_sync(fd.as_raw_fd(), &raw const flags) }
        .map(|_| ())
        .map_err(|errno| VaError::Io(io::Error::from(errno)))
}

/// An active `mmap` region, unmapped on drop.
struct Mapping {
    ptr: *mut c_void,
    len: usize,
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `ptr` and `len` describe the region that `read_dmabuf` mapped, and this is its
        // only unmap. The status is dropped because `Drop` cannot report it.
        let _ = unsafe { rustix::mm::munmap(self.ptr, self.len) };
    }
}
