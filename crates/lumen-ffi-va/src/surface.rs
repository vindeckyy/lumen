//! VA surfaces: GPU-side pictures, allocated or imported from DMA-BUF.

use std::{
    fmt,
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
    sync::Arc,
};

use lumen_protocol::media::{DmabufImage, PixelFormat};

use crate::{
    display::{DisplayInner, VaDisplay},
    error::{VaError, VaResult, check, fit},
    format, sys,
};

/// A VA surface (`VASurfaceID`), destroyed on drop.
pub struct OwnedVaSurface {
    pub(crate) display: Arc<DisplayInner>,
    pub(crate) id: sys::VASurfaceID,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// One plane of a DMA-BUF layout: its byte offset and row pitch within the object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmabufPlane {
    pub offset: u32,
    pub pitch: u32,
}

/// A single-object DMA-BUF to import as a surface (see [`OwnedVaSurface::import_dmabuf`]).
#[derive(Debug, Clone, Copy)]
pub struct DmabufImport<'a> {
    /// Only `Nv12`, `P010` and `Bgrx` are accepted.
    pub format: PixelFormat,
    pub width: u32,
    pub height: u32,
    /// The DMA-BUF file descriptor. libva takes its own reference, so the caller keeps it.
    pub fd: BorrowedFd<'a>,
    /// Size of the DMA-BUF object in bytes.
    pub size: u32,
    /// DRM format modifier; `0` means linear.
    pub modifier: u64,
    /// One to four planes, all inside `fd`.
    pub planes: &'a [DmabufPlane],
}

/// A surface exported as one DMA-BUF object (see [`OwnedVaSurface::export_dmabuf`]).
#[derive(Debug)]
pub struct DmabufExport {
    pub width: u32,
    pub height: u32,
    /// DRM fourcc of the composed layer, for example `DRM_FORMAT_NV12`.
    pub drm_format: u32,
    /// DRM format modifier; `0` means linear.
    pub modifier: u64,
    /// Size of the DMA-BUF object in bytes.
    pub size: u32,
    /// The DMA-BUF file descriptor. This struct owns it and closes it on drop.
    pub fd: OwnedFd,
    pub planes: Vec<DmabufPlane>,
}

impl DmabufExport {
    /// The export as a [`DmabufImage`], keeping the driver's plane offsets, pitches and modifier.
    ///
    /// # Errors
    /// Returns a description when the export has no planes or more than a frame holds.
    pub fn into_image(self) -> Result<DmabufImage, String> {
        let planes: Vec<lumen_protocol::media::DmabufPlane> = self
            .planes
            .iter()
            .map(|plane| lumen_protocol::media::DmabufPlane {
                offset: plane.offset,
                stride: plane.pitch,
            })
            .collect();
        DmabufImage::new(self.fd, self.drm_format, self.modifier, &planes)
    }
}

impl OwnedVaSurface {
    /// The raw `VASurfaceID`.
    #[must_use]
    pub fn id(&self) -> sys::VASurfaceID {
        self.id
    }

    /// Allocates one surface of `format`.
    ///
    /// # Errors
    /// [`VaError::Status`] if libva cannot allocate it.
    pub fn create(
        display: &VaDisplay,
        format: PixelFormat,
        width: u32,
        height: u32,
    ) -> VaResult<Self> {
        let mut surfaces = Self::create_batch(display, format, width, height, 1)?;
        Ok(surfaces
            .pop()
            .expect("create_batch(1) returns exactly one surface"))
    }

    /// Allocates `count` surfaces of `format` in one call (`vaCreateSurfaces`).
    ///
    /// # Errors
    /// [`VaError::Status`] if libva cannot allocate them.
    pub fn create_batch(
        display: &VaDisplay,
        format: PixelFormat,
        width: u32,
        height: u32,
        count: usize,
    ) -> VaResult<Vec<Self>> {
        let lib = display.inner.lib();
        let (rt, code) = (format::rt_format(format), format::fourcc(format));
        let n = fit(count)?;
        let mut ids = vec![sys::VA_INVALID_SURFACE; count];
        let mut attribs = [pixel_format_attrib(code)];
        display.inner.call(|core, dpy| {
            // SAFETY: `ids` has `n` writable entries for libva to fill. `attribs` holds one valid
            // pixel-format attribute, read during the call.
            let status = unsafe {
                core.vaCreateSurfaces(
                    dpy,
                    rt,
                    width,
                    height,
                    ids.as_mut_ptr(),
                    n,
                    attribs.as_mut_ptr(),
                    1,
                )
            };
            check(lib, "vaCreateSurfaces", status)
        })?;
        Ok(ids
            .into_iter()
            .map(|id| Self {
                display: Arc::clone(&display.inner),
                id,
                width,
                height,
            })
            .collect())
    }

    /// Imports a single-object DMA-BUF as a surface
    /// (`VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2`).
    ///
    /// # Errors
    /// [`VaError::UnsupportedFormat`] for formats other than NV12, P010 and BGRX,
    /// [`VaError::Invalid`] for a bad plane count, and [`VaError::Status`] if the driver
    /// rejects the layout.
    pub fn import_dmabuf(display: &VaDisplay, import: DmabufImport<'_>) -> VaResult<Self> {
        let code = match import.format {
            PixelFormat::Nv12 | PixelFormat::P010 | PixelFormat::Bgrx => {
                format::fourcc(import.format)
            }
            other => return Err(VaError::UnsupportedFormat(other)),
        };
        if import.planes.is_empty() || import.planes.len() > 4 {
            return Err(VaError::Invalid("DMA-BUF import needs 1 to 4 planes"));
        }

        let mut layer = sys::_VADRMPRIMESurfaceDescriptor__bindgen_ty_2 {
            // The DRM layer names the format by DRM fourcc. For BGRX that differs from the VA one.
            drm_format: import.format.drm_fourcc(),
            num_planes: fit(import.planes.len())?,
            ..sys::_VADRMPRIMESurfaceDescriptor__bindgen_ty_2::default()
        };
        for (i, plane) in import.planes.iter().enumerate() {
            layer.offset[i] = plane.offset;
            layer.pitch[i] = plane.pitch;
        }
        let mut desc = sys::VADRMPRIMESurfaceDescriptor {
            fourcc: code,
            width: import.width,
            height: import.height,
            num_objects: 1,
            num_layers: 1,
            ..sys::VADRMPRIMESurfaceDescriptor::default()
        };
        desc.objects[0] = sys::_VADRMPRIMESurfaceDescriptor__bindgen_ty_1 {
            fd: import.fd.as_raw_fd(),
            size: import.size,
            drm_format_modifier: import.modifier,
        };
        desc.layers[0] = layer;

        let lib = display.inner.lib();
        let rt = format::rt_format(import.format);
        let mut attribs = [
            sys::VASurfaceAttrib {
                type_: sys::VASurfaceAttribMemoryType,
                flags: sys::VA_SURFACE_ATTRIB_SETTABLE,
                value: integer_value(sys::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2.cast_signed()),
            },
            sys::VASurfaceAttrib {
                type_: sys::VASurfaceAttribExternalBufferDescriptor,
                flags: sys::VA_SURFACE_ATTRIB_SETTABLE,
                value: sys::VAGenericValue {
                    type_: sys::VAGenericValueTypePointer,
                    value: sys::_VAGenericValue__bindgen_ty_1 {
                        p: (&raw mut desc).cast(),
                    },
                },
            },
        ];
        let mut id = sys::VA_INVALID_SURFACE;
        display.inner.call(|core, dpy| {
            // SAFETY: `desc` and `attribs` live until the call returns. libva reads the
            // descriptor and duplicates the object fd it keeps. That fd is `import.fd`, which is
            // a `BorrowedFd` and so stays open for the call. `id` is a valid out-pointer.
            let status = unsafe {
                core.vaCreateSurfaces(
                    dpy,
                    rt,
                    import.width,
                    import.height,
                    &mut id,
                    1,
                    attribs.as_mut_ptr(),
                    2,
                )
            };
            check(lib, "vaCreateSurfaces(DRM_PRIME_2)", status)
        })?;
        Ok(Self {
            display: Arc::clone(&display.inner),
            id,
            width: import.width,
            height: import.height,
        })
    }

    /// Imports a frame that lives in one DMA-BUF object, such as a decoded or captured picture.
    ///
    /// The object size is read from the file, which reports its size as its end offset.
    ///
    /// # Errors
    /// As [`Self::import_dmabuf`], and [`VaError::Io`] if the size cannot be read.
    pub fn import_frame(
        display: &VaDisplay,
        image: &DmabufImage,
        format: PixelFormat,
        width: u32,
        height: u32,
    ) -> VaResult<Self> {
        let mut file = std::fs::File::from(image.fd.try_clone()?);
        let size = std::io::Seek::seek(&mut file, std::io::SeekFrom::End(0))?;
        let planes: Vec<DmabufPlane> = image
            .planes()
            .iter()
            .map(|plane| DmabufPlane {
                offset: plane.offset,
                pitch: plane.stride,
            })
            .collect();
        Self::import_dmabuf(
            display,
            DmabufImport {
                format,
                width,
                height,
                fd: image.fd.as_fd(),
                size: fit(size)?,
                modifier: image.modifier,
                planes: &planes,
            },
        )
    }

    /// Exports the surface as one DMA-BUF object with composed planes
    /// (`vaExportSurfaceHandle`, `DRM_PRIME_2`, read-only, composed layers).
    ///
    /// # Errors
    /// [`VaError::Status`] if libva refuses the export, and [`VaError::Invalid`] if the surface
    /// spans several objects or layers. Those fds are closed before returning.
    pub fn export_dmabuf(&self) -> VaResult<DmabufExport> {
        let lib = self.display.lib();
        let mut desc = sys::VADRMPRIMESurfaceDescriptor::default();
        self.display.call(|core, dpy| {
            // SAFETY: `desc` is a valid, writable descriptor. On success libva fills it and hands
            // ownership of each object fd to the caller. The fds are wrapped below before any
            // other fallible step.
            let status = unsafe {
                core.vaExportSurfaceHandle(
                    dpy,
                    self.id,
                    sys::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                    sys::VA_EXPORT_SURFACE_READ_ONLY | sys::VA_EXPORT_SURFACE_COMPOSED_LAYERS,
                    (&raw mut desc).cast(),
                )
            };
            check(lib, "vaExportSurfaceHandle", status)
        })?;

        // Take ownership of every object fd now. Anything rejected below then closes on drop
        // instead of leaking. `None` marks an fd that libva reported as invalid.
        let object_count = (desc.num_objects as usize).min(desc.objects.len());
        let mut objects: Vec<Option<OwnedFd>> = desc.objects[..object_count]
            .iter()
            .map(|object| {
                (object.fd >= 0).then(|| {
                    // SAFETY: libva transferred ownership of this non-negative fd to us on a
                    // successful export. Nothing else closes it, so this is the only owner.
                    unsafe { OwnedFd::from_raw_fd(object.fd) }
                })
            })
            .collect();

        let layer = desc.layers[0];
        let plane_count = (layer.num_planes as usize).min(layer.object_index.len());
        let single_object = desc.num_objects == 1
            && desc.num_layers == 1
            && plane_count > 0
            && layer.object_index[..plane_count]
                .iter()
                .all(|&index| index == 0);
        let Some(fd) = objects
            .first_mut()
            .and_then(Option::take)
            .filter(|_| single_object)
        else {
            return Err(VaError::Invalid(
                "exported surface spans several DMA-BUF objects or layers",
            ));
        };

        Ok(DmabufExport {
            width: desc.width,
            height: desc.height,
            drm_format: layer.drm_format,
            modifier: desc.objects[0].drm_format_modifier,
            size: desc.objects[0].size,
            fd,
            planes: (0..plane_count)
                .map(|i| DmabufPlane {
                    offset: layer.offset[i],
                    pitch: layer.pitch[i],
                })
                .collect(),
        })
    }

    /// Blocks until all work queued on this surface has finished (`vaSyncSurface`).
    ///
    /// # Errors
    /// [`VaError::Status`] if libva fails.
    pub fn sync(&self) -> VaResult<()> {
        let lib = self.display.lib();
        self.display.call(|core, dpy| {
            // SAFETY: `self.id` is a live surface of `dpy`, kept alive by `self.display`.
            let status = unsafe { core.vaSyncSurface(dpy, self.id) };
            check(lib, "vaSyncSurface", status)
        })
    }
}

impl fmt::Debug for OwnedVaSurface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedVaSurface")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Drop for OwnedVaSurface {
    fn drop(&mut self) {
        self.display.call(|core, dpy| {
            // SAFETY: `self.id` was created on `dpy` and is destroyed exactly once, here. The
            // display is kept alive by `self.display`. The status is dropped because `Drop`
            // cannot report it.
            let _ = unsafe { core.vaDestroySurfaces(dpy, &mut self.id, 1) };
        });
    }
}

/// A placeholder attribute, used only as storage for libva to overwrite.
pub(crate) fn blank_attrib() -> sys::VASurfaceAttrib {
    sys::VASurfaceAttrib {
        type_: 0,
        flags: 0,
        value: integer_value(0),
    }
}

fn integer_value(value: i32) -> sys::VAGenericValue {
    sys::VAGenericValue {
        type_: sys::VAGenericValueTypeInteger,
        value: sys::_VAGenericValue__bindgen_ty_1 { i: value },
    }
}

fn pixel_format_attrib(fourcc: u32) -> sys::VASurfaceAttrib {
    sys::VASurfaceAttrib {
        type_: sys::VASurfaceAttribPixelFormat,
        flags: sys::VA_SURFACE_ATTRIB_SETTABLE,
        value: integer_value(fourcc.cast_signed()),
    }
}
