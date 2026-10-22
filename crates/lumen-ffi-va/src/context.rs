//! VA contexts: a config bound to render-target surfaces, plus the buffers and picture
//! calls that drive it.

use std::{ffi::c_void, fmt, ptr, sync::Arc};

use crate::{
    buffer::OwnedVaBuffer,
    config::VaConfig,
    display::{DisplayInner, same_display},
    error::{VaResult, check, fit},
    surface::OwnedVaSurface,
    sys,
};

/// A VA context (`VAContextID`), destroyed on drop.
pub struct VaContext {
    pub(crate) display: Arc<DisplayInner>,
    pub(crate) id: sys::VAContextID,
}

impl VaContext {
    /// Creates a progressive context for `config` with `targets` as its render targets
    /// (`vaCreateContext`).
    ///
    /// # Errors
    /// [`VaError::Invalid`] if a target belongs to another display, and [`VaError::Status`]
    /// if libva rejects the context.
    pub fn create(
        config: &VaConfig,
        width: u32,
        height: u32,
        targets: &[&OwnedVaSurface],
    ) -> VaResult<Self> {
        for target in targets {
            same_display(&config.display, &target.display)?;
        }
        let (w, h) = (fit(width)?, fit(height)?);
        let mut ids: Vec<sys::VASurfaceID> = targets.iter().map(|target| target.id).collect();
        let n = fit(ids.len())?;
        let display = Arc::clone(&config.display);
        let lib = display.lib();
        let id = display.call(|core, dpy| {
            let mut id = sys::VA_INVALID_ID;
            // SAFETY: `ids` is valid for `n` reads and writes, and libva reads it during the call.
            // `VA_PROGRESSIVE` fits in `c_int`. `id` is a valid out-pointer.
            let status = unsafe {
                core.vaCreateContext(
                    dpy,
                    config.id,
                    w,
                    h,
                    sys::VA_PROGRESSIVE.cast_signed(),
                    ids.as_mut_ptr(),
                    n,
                    &mut id,
                )
            };
            check(lib, "vaCreateContext", status).map(|()| id)
        })?;
        Ok(Self { display, id })
    }

    /// The raw `VAContextID`.
    #[must_use]
    pub fn id(&self) -> sys::VAContextID {
        self.id
    }

    /// Creates a parameter buffer holding one `T`, copied into libva during the call
    /// (`vaCreateBuffer`).
    ///
    /// `T` must be the `#[repr(C)]` struct libva expects for `kind`.
    ///
    /// # Errors
    /// [`VaError::Status`] if libva rejects the buffer.
    pub fn create_buffer<T: Copy>(
        &self,
        kind: sys::VABufferType,
        value: &T,
    ) -> VaResult<OwnedVaBuffer> {
        self.create_buffer_array(kind, std::slice::from_ref(value))
    }

    /// Creates a parameter buffer holding `values` back to back, copied into libva during the
    /// call. `T` must be the `#[repr(C)]` element type libva expects for `kind`.
    ///
    /// # Errors
    /// [`VaError::Invalid`] if the sizes do not fit libva, and [`VaError::Status`] if libva
    /// rejects the buffer.
    pub fn create_buffer_array<T: Copy>(
        &self,
        kind: sys::VABufferType,
        values: &[T],
    ) -> VaResult<OwnedVaBuffer> {
        let size = fit(std::mem::size_of::<T>())?;
        let count = fit(values.len())?;
        self.create_raw(kind, size, count, values.as_ptr().cast_mut().cast())
    }

    /// Creates a data buffer holding raw bytes, such as slice data (`vaCreateBuffer` with
    /// `size = bytes.len()` and `num_elements = 1`). libva copies `bytes` during the call.
    ///
    /// # Errors
    /// [`VaError::Invalid`] if the length does not fit libva, and [`VaError::Status`] if libva
    /// rejects the buffer.
    pub fn create_data_buffer(
        &self,
        kind: sys::VABufferType,
        bytes: &[u8],
    ) -> VaResult<OwnedVaBuffer> {
        let size = fit(bytes.len())?;
        self.create_raw(kind, size, 1, bytes.as_ptr().cast_mut().cast())
    }

    /// Creates an output buffer of `bytes` bytes that libva fills, such as a coded buffer
    /// (`VAEncCodedBufferType`).
    ///
    /// # Errors
    /// [`VaError::Status`] if libva cannot allocate it.
    pub fn create_empty_buffer(
        &self,
        kind: sys::VABufferType,
        bytes: u32,
    ) -> VaResult<OwnedVaBuffer> {
        self.create_raw(kind, bytes, 1, ptr::null_mut())
    }

    fn create_raw(
        &self,
        kind: sys::VABufferType,
        size: u32,
        count: u32,
        data: *mut c_void,
    ) -> VaResult<OwnedVaBuffer> {
        let lib = self.display.lib();
        let id = self.display.call(|core, dpy| {
            let mut id = sys::VA_INVALID_ID;
            // SAFETY: `data` is NULL, or points to `size * count` bytes that stay valid for the
            // call. libva copies them into its own storage before returning. `id` is a valid
            // out-pointer.
            let status =
                unsafe { core.vaCreateBuffer(dpy, self.id, kind, size, count, data, &mut id) };
            check(lib, "vaCreateBuffer", status).map(|()| id)
        })?;
        Ok(OwnedVaBuffer {
            display: Arc::clone(&self.display),
            id,
        })
    }

    /// Starts a picture that decodes or encodes into `target` (`vaBeginPicture`).
    ///
    /// # Errors
    /// [`VaError::Invalid`] if `target` belongs to another display, and [`VaError::Status`]
    /// if libva fails.
    pub fn begin_picture(&self, target: &OwnedVaSurface) -> VaResult<()> {
        same_display(&self.display, &target.display)?;
        let lib = self.display.lib();
        self.display.call(|core, dpy| {
            // SAFETY: `self.id` and `target.id` name live objects of `dpy`, which the `Arc`s keep
            // alive.
            let status = unsafe { core.vaBeginPicture(dpy, self.id, target.id) };
            check(lib, "vaBeginPicture", status)
        })
    }

    /// Submits parameter and slice buffers for the current picture (`vaRenderPicture`).
    ///
    /// # Errors
    /// [`VaError::Invalid`] if a buffer belongs to another display, and [`VaError::Status`]
    /// if libva fails.
    pub fn render_picture(&self, buffers: &[&OwnedVaBuffer]) -> VaResult<()> {
        for buffer in buffers {
            same_display(&self.display, &buffer.display)?;
        }
        let mut ids: Vec<sys::VABufferID> = buffers.iter().map(|buffer| buffer.id).collect();
        let n = fit(ids.len())?;
        let lib = self.display.lib();
        self.display.call(|core, dpy| {
            // SAFETY: `ids` holds `n` buffer ids of `dpy`, read during the call.
            let status = unsafe { core.vaRenderPicture(dpy, self.id, ids.as_mut_ptr(), n) };
            check(lib, "vaRenderPicture", status)
        })
    }

    /// Ends the current picture and starts its processing (`vaEndPicture`).
    ///
    /// # Errors
    /// [`VaError::Status`] if libva fails.
    pub fn end_picture(&self) -> VaResult<()> {
        let lib = self.display.lib();
        self.display.call(|core, dpy| {
            // SAFETY: `self.id` is a live context of `dpy`, kept alive by `self.display`.
            let status = unsafe { core.vaEndPicture(dpy, self.id) };
            check(lib, "vaEndPicture", status)
        })
    }
}

impl fmt::Debug for VaContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VaContext")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Drop for VaContext {
    fn drop(&mut self) {
        self.display.call(|core, dpy| {
            // SAFETY: `self.id` was created on `dpy` and is destroyed exactly once, here. The
            // display is kept alive by `self.display`. The status is dropped because `Drop`
            // cannot report it.
            let _ = unsafe { core.vaDestroyContext(dpy, self.id) };
        });
    }
}
