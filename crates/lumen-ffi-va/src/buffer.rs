//! VA buffers: parameter blocks for a context, and coded output buffers.

use std::{fmt, ptr, sync::Arc};

use crate::{
    display::DisplayInner,
    error::{VaError, VaResult, check},
    sys,
};

/// A VA buffer (`VABufferID`), destroyed on drop.
pub struct OwnedVaBuffer {
    pub(crate) display: Arc<DisplayInner>,
    pub(crate) id: sys::VABufferID,
}

impl OwnedVaBuffer {
    /// The raw `VABufferID`.
    #[must_use]
    pub fn id(&self) -> sys::VABufferID {
        self.id
    }

    /// Copies the coded bitstream out of a coded buffer.
    ///
    /// Maps the buffer, walks its `VACodedBufferSegment` list, copies the bytes and unmaps,
    /// also when the walk fails.
    ///
    /// # Errors
    /// [`VaError::Status`] if mapping fails, and [`VaError::Invalid`] for a segment that
    /// claims bytes but has no data pointer.
    pub fn coded_bytes(&self) -> VaResult<Vec<u8>> {
        let lib = self.display.lib();
        self.display.call(|core, dpy| {
            let mut mapped = ptr::null_mut();
            // SAFETY: `mapped` is a valid out-pointer. On success libva returns the first
            // `VACodedBufferSegment` of this buffer, valid until `vaUnmapBuffer` below.
            let status = unsafe { core.vaMapBuffer(dpy, self.id, &mut mapped) };
            check(lib, "vaMapBuffer", status)?;

            // SAFETY: `mapped` came from `vaMapBuffer` above and stays mapped until the
            // `vaUnmapBuffer` call below, after this read returns.
            let bytes =
                unsafe { read_segments(mapped.cast::<sys::VACodedBufferSegment>().cast_const()) };

            // SAFETY: `self.id` was mapped above and is unmapped exactly once, here.
            let unmapped = check(lib, "vaUnmapBuffer", unsafe {
                core.vaUnmapBuffer(dpy, self.id)
            });
            let data = bytes?;
            unmapped?;
            Ok(data)
        })
    }
}

/// Concatenates the data of every segment in a libva coded-buffer list.
///
/// # Safety
/// `head` must be NULL, or the first node of a `VACodedBufferSegment` list that is mapped and
/// unchanged for the whole call.
unsafe fn read_segments(head: *const sys::VACodedBufferSegment) -> VaResult<Vec<u8>> {
    let mut out = Vec::new();
    let mut seg = head;
    while !seg.is_null() {
        // SAFETY: `seg` is non-NULL and, by this function's contract, a node of the mapped list.
        let node = unsafe { &*seg };
        if node.size > 0 {
            if node.buf.is_null() {
                return Err(VaError::Invalid(
                    "coded segment has bytes but no data pointer",
                ));
            }
            // SAFETY: per the `VACodedBufferSegment` contract, `buf` points to `size` bytes of
            // coded data inside the mapping, which is still live.
            let data =
                unsafe { std::slice::from_raw_parts(node.buf.cast::<u8>(), node.size as usize) };
            out.extend_from_slice(data);
        }
        seg = node.next.cast_const().cast();
    }
    Ok(out)
}

impl fmt::Debug for OwnedVaBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedVaBuffer")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Drop for OwnedVaBuffer {
    fn drop(&mut self) {
        self.display.call(|core, dpy| {
            // SAFETY: `self.id` was created on `dpy` and is destroyed exactly once, here. The
            // display is kept alive by `self.display`. The status is dropped because `Drop`
            // cannot report it.
            let _ = unsafe { core.vaDestroyBuffer(dpy, self.id) };
        });
    }
}
