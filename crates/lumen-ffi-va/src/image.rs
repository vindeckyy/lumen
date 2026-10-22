//! VA images: CPU-visible views of surface memory.

use std::{fmt, ptr, slice, sync::Arc};

use lumen_protocol::media::PixelFormat;

use crate::{
    display::DisplayInner,
    error::{VaError, VaResult, check, fit},
    format,
    surface::OwnedVaSurface,
    sys,
};

/// A `VAImage` (`vaDeriveImage` or `vaCreateImage`), destroyed on drop.
pub struct OwnedVaImage {
    pub(crate) display: Arc<DisplayInner>,
    image: sys::VAImage,
}

impl OwnedVaSurface {
    /// Derives a CPU-mappable image that aliases this surface's memory (`vaDeriveImage`).
    ///
    /// The bytes are the surface's raw memory layout. On AMD that is tiled, so decoded pictures
    /// read back scrambled. Use [`OwnedVaSurface::get_image`] for pictures. Not every driver
    /// supports derivation. Those return [`VaError::Status`]. Do not render to the surface while
    /// the image is mapped.
    ///
    /// # Errors
    /// [`VaError::Status`] if libva cannot derive the image.
    pub fn derive_image(&self) -> VaResult<OwnedVaImage> {
        let lib = self.display.lib();
        let mut image = sys::VAImage::default();
        self.display.call(|core, dpy| {
            // SAFETY: `image` is a valid, writable `VAImage`. On success libva fills it with
            // handles to objects it created on `dpy`, which `OwnedVaImage` destroys on drop.
            let status = unsafe { core.vaDeriveImage(dpy, self.id, &mut image) };
            check(lib, "vaDeriveImage", status)
        })?;
        Ok(OwnedVaImage {
            display: Arc::clone(&self.display),
            image,
        })
    }

    /// Copies this surface into a linear NV12 image (`vaCreateImage` + `vaGetImage`).
    ///
    /// The driver de-tiles the picture during the copy, so the bytes are the decoded picture.
    /// This matches what `ffmpeg` does for VA-API downloads. The call waits for pending work on
    /// the surface first.
    ///
    /// # Errors
    /// [`VaError::UnsupportedFormat`] if the driver offers no NV12 image format, and
    /// [`VaError::Status`] if libva fails.
    pub fn get_image(&self) -> VaResult<OwnedVaImage> {
        self.get_image_as(PixelFormat::Nv12)
    }

    /// Copies this surface into a linear image of `format`, as [`Self::get_image`] does for NV12.
    ///
    /// # Errors
    /// [`VaError::UnsupportedFormat`] if the driver offers no image format with the fourcc of
    /// `format`, and [`VaError::Status`] if libva fails.
    pub fn get_image_as(&self, format: PixelFormat) -> VaResult<OwnedVaImage> {
        let lib = self.display.lib();
        let width = fit(self.width)?;
        let height = fit(self.height)?;
        let wanted = format::fourcc(format);

        let mut image_format = self.display.call(|core, dpy| {
            // SAFETY: `dpy` is initialised and the lock is held. The call takes no out-pointers.
            let capacity = unsafe { core.vaMaxNumImageFormats(dpy) };
            let mut formats =
                vec![sys::VAImageFormat::default(); usize::try_from(capacity).unwrap_or(0)];
            let mut count = 0;
            // SAFETY: `formats` has `vaMaxNumImageFormats` entries, the capacity libva requires.
            // `count` is a valid out-pointer.
            let status = unsafe { core.vaQueryImageFormats(dpy, formats.as_mut_ptr(), &mut count) };
            check(lib, "vaQueryImageFormats", status)?;
            formats.truncate(usize::try_from(count).unwrap_or(0));
            formats
                .into_iter()
                .find(|candidate| candidate.fourcc == wanted)
                .ok_or(VaError::UnsupportedFormat(format))
        })?;

        let mut image = sys::VAImage::default();
        self.display.call(|core, dpy| {
            // SAFETY: `image_format` is a format the driver just reported, and `image` is a valid
            // out-pointer. libva checks the dimensions it is given.
            let status =
                unsafe { core.vaCreateImage(dpy, &mut image_format, width, height, &mut image) };
            check(lib, "vaCreateImage", status)
        })?;

        // Wrap the image now, so it is destroyed if the copy below fails.
        let owned = OwnedVaImage {
            display: Arc::clone(&self.display),
            image,
        };
        self.display.call(|core, dpy| {
            // SAFETY: `self.id` is a live surface of `dpy`, kept alive by `self.display`.
            let status = unsafe { core.vaSyncSurface(dpy, self.id) };
            check(lib, "vaSyncSurface", status)?;
            // SAFETY: `owned.image.image_id` is the image created on `dpy` above and not yet
            // destroyed. The surface is synced, so the copy reads finished pixels. The copy
            // covers the whole surface at offset (0, 0).
            let status = unsafe {
                core.vaGetImage(
                    dpy,
                    self.id,
                    0,
                    0,
                    self.width,
                    self.height,
                    owned.image.image_id,
                )
            };
            check(lib, "vaGetImage", status)
        })?;
        Ok(owned)
    }
}

impl OwnedVaImage {
    /// The `VAImage` description: `pitches`, `offsets`, `width`, `height`, `data_size`, `format`.
    ///
    /// Plane `i` starts at byte `offsets[i]` and has row stride `pitches[i]`.
    #[must_use]
    pub fn info(&self) -> &sys::VAImage {
        &self.image
    }

    /// Maps the image bytes for CPU access (`vaMapBuffer` on the image buffer). The mapping
    /// ends when the returned guard drops.
    ///
    /// # Errors
    /// [`VaError::Status`] if libva cannot map the buffer.
    pub fn map(&mut self) -> VaResult<MappedVaImage<'_>> {
        let lib = self.display.lib();
        let buffer = self.image.buf;
        let mut data = ptr::null_mut();
        self.display.call(|core, dpy| {
            // SAFETY: `buffer` is this image's data buffer on `dpy`. `data` is a valid
            // out-pointer. On success it covers `data_size` bytes until `vaUnmapBuffer`, which
            // `MappedVaImage::drop` calls.
            let status = unsafe { core.vaMapBuffer(dpy, buffer, &mut data) };
            check(lib, "vaMapBuffer", status)
        })?;
        Ok(MappedVaImage {
            image: self,
            data: data.cast(),
            len: self.image.data_size as usize,
        })
    }
}

impl fmt::Debug for OwnedVaImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedVaImage")
            .field("image_id", &self.image.image_id)
            .field("size", &(self.image.width, self.image.height))
            .finish_non_exhaustive()
    }
}

impl Drop for OwnedVaImage {
    fn drop(&mut self) {
        self.display.call(|core, dpy| {
            // SAFETY: `image_id` was created on `dpy` and is destroyed exactly once, here. The
            // display is kept alive by `self.display`. The status is dropped because `Drop` cannot
            // report it.
            let _ = unsafe { core.vaDestroyImage(dpy, self.image.image_id) };
        });
    }
}

/// A mapped image buffer. Unmapped on drop.
#[derive(Debug)]
pub struct MappedVaImage<'a> {
    image: &'a OwnedVaImage,
    data: *mut u8,
    len: usize,
}

impl MappedVaImage<'_> {
    /// The image bytes. Plane `i` starts at `info().offsets[i]` with stride `info().pitches[i]`.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `data` points to `len` bytes that libva keeps mapped while this guard lives.
        // The guard borrows the image, so the buffer cannot be unmapped or destroyed first.
        // Only shared references are handed out here, and writers need `&mut self`.
        unsafe { slice::from_raw_parts(self.data, self.len) }
    }

    /// The image bytes, writable.
    #[must_use]
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: same region as `bytes`. `&mut self` makes this the only live reference into the
        // mapping.
        unsafe { slice::from_raw_parts_mut(self.data, self.len) }
    }
}

impl Drop for MappedVaImage<'_> {
    fn drop(&mut self) {
        self.image.display.call(|core, dpy| {
            // SAFETY: the image buffer was mapped by `OwnedVaImage::map` and is unmapped exactly
            // once, here. The status is dropped because `Drop` cannot report it.
            let _ = unsafe { core.vaUnmapBuffer(dpy, self.image.image.buf) };
        });
    }
}
