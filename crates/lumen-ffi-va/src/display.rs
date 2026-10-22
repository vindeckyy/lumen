//! VA display: one initialised libva device.

use std::{
    ffi::CStr,
    fmt,
    fs::{self, OpenOptions},
    os::fd::{AsRawFd, BorrowedFd, OwnedFd},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError, Weak},
};

use crate::{
    config::VaConfig,
    error::{VaError, VaResult, check, fit},
    library::VaLibrary,
    sys,
};

/// Every display initialised in this process, keyed by raw handle.
///
/// libva caches displays per DRM device, so opening one node twice yields one raw
/// handle. The weak entries make `vaInitialize` run once per handle, and let
/// `vaTerminate` run when the last user drops it.
static DISPLAYS: Mutex<Vec<(usize, Weak<DisplayInner>)>> = Mutex::new(Vec::new());

/// An initialised VA display. Cheap to clone.
///
/// Objects made from a display (configs, contexts, surfaces, buffers, images) keep
/// it alive, so `vaTerminate` never runs while one of them still exists.
#[derive(Clone, Debug)]
pub struct VaDisplay {
    pub(crate) inner: Arc<DisplayInner>,
}

pub(crate) struct DisplayInner {
    lib: &'static VaLibrary,
    raw: sys::VADisplay,
    version: (i32, i32),
    /// Serialises every libva call on `raw`.
    lock: Mutex<()>,
    /// The DRM fd behind `raw`. Dropped after `vaTerminate`, which still needs it.
    _drm_fd: OwnedFd,
}

// SAFETY: `raw` is an opaque libva handle, not Rust-owned memory. Every use goes through
// `call`, which holds `lock`, so libva never sees two calls on one display at once. The other
// fields are a `'static` reference, plain data, a mutex and an owned fd, all `Send + Sync`.
unsafe impl Send for DisplayInner {}
// SAFETY: see the `Send` impl; shared access is serialised by `lock`.
unsafe impl Sync for DisplayInner {}

impl DisplayInner {
    /// Runs `f` with the display lock held. Every libva call on this display goes through here.
    pub(crate) fn call<R>(&self, f: impl FnOnce(&sys::VaCore, sys::VADisplay) -> R) -> R {
        let _guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        f(&self.lib.core, self.raw)
    }

    pub(crate) fn lib(&self) -> &'static VaLibrary {
        self.lib
    }
}

impl fmt::Debug for DisplayInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VaDisplay")
            .field("raw", &self.raw)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

impl Drop for DisplayInner {
    fn drop(&mut self) {
        // SAFETY: `raw` was initialised by `VaDisplay::init`. No object that calls into it is
        // alive: each one holds an `Arc` to this struct, and this is the last `Arc`. `&mut self`
        // rules out concurrent calls. The status is dropped because `Drop` cannot report it.
        let _ = unsafe { self.lib.core.vaTerminate(self.raw) };
    }
}

/// Fails if `other` belongs to a different display than the one that owns `own`.
pub(crate) fn same_display(own: &Arc<DisplayInner>, other: &Arc<DisplayInner>) -> VaResult<()> {
    if Arc::ptr_eq(own, other) {
        Ok(())
    } else {
        Err(VaError::Invalid("object belongs to another VA display"))
    }
}

impl VaDisplay {
    /// Opens a DRM render node, such as `/dev/dri/renderD129`, and initialises VA-API on it.
    ///
    /// # Errors
    /// [`VaError::Unavailable`] without libva, [`VaError::Open`] if the node cannot be
    /// opened, and [`VaError::NoDisplay`] or [`VaError::Status`] if libva rejects it.
    pub fn open(render_node: &Path) -> VaResult<Self> {
        let lib = VaLibrary::load()?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(render_node)
            .map_err(|source| VaError::Open {
                path: render_node.to_owned(),
                source,
            })?;
        Self::init(lib, file.into())
    }

    /// Opens the first usable render node.
    ///
    /// Candidates, in order: `$LUMEN_VA_DEVICE` if set, `renderD129`, `renderD128`, then the
    /// other `renderD*` nodes sorted by name.
    ///
    /// # Errors
    /// [`VaError::Unavailable`] without libva, or [`VaError::NoDevice`] listing why each
    /// candidate failed.
    pub fn open_preferred() -> VaResult<Self> {
        VaLibrary::load()?;
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(path) = crate::knobs::va_device() {
            candidates.push(path);
        }
        candidates.push("/dev/dri/renderD129".into());
        candidates.push("/dev/dri/renderD128".into());
        let mut others: Vec<PathBuf> = fs::read_dir("/dev/dri")
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("renderD"))
            })
            .collect();
        others.sort();
        others.retain(|path| !candidates.contains(path));
        candidates.extend(others);

        let mut failures = Vec::new();
        for path in candidates {
            match Self::open(&path) {
                Ok(display) => return Ok(display),
                Err(e) => failures.push(format!("{}: {e}", path.display())),
            }
        }
        Err(VaError::NoDevice(failures.join("; ")))
    }

    /// Initialises VA-API on an already open DRM fd. The display keeps its own duplicate of
    /// the fd, so the caller may close theirs.
    ///
    /// # Errors
    /// As for [`VaDisplay::open`].
    pub fn open_drm(fd: BorrowedFd<'_>) -> VaResult<Self> {
        let lib = VaLibrary::load()?;
        Self::init(lib, fd.try_clone_to_owned()?)
    }

    fn init(lib: &'static VaLibrary, fd: OwnedFd) -> VaResult<Self> {
        let mut displays = DISPLAYS.lock().unwrap_or_else(PoisonError::into_inner);
        displays.retain(|(_, weak)| weak.strong_count() > 0);

        // SAFETY: `fd` is an open DRM fd owned by this function, so it stays open for the call.
        // `vaGetDisplayDRM` only reads the descriptor number. The returned handle is checked for
        // NULL below.
        let raw = unsafe { lib.drm.vaGetDisplayDRM(fd.as_raw_fd()) };
        if raw.is_null() {
            return Err(VaError::NoDisplay);
        }
        if let Some(live) = displays
            .iter()
            .find(|(handle, _)| *handle == raw as usize)
            .and_then(|(_, weak)| weak.upgrade())
        {
            // libva returned a display this process already initialised. `fd` closes here; the
            // live display keeps the fd it was created with.
            return Ok(Self { inner: live });
        }

        let (mut major, mut minor) = (0, 0);
        // SAFETY: `raw` is a non-NULL display from `vaGetDisplayDRM` that no object references
        // yet, so no lock is needed. `major` and `minor` are valid out-pointers for the call.
        let status = unsafe { lib.core.vaInitialize(raw, &mut major, &mut minor) };
        check(lib, "vaInitialize", status)?;

        let inner = Arc::new(DisplayInner {
            lib,
            raw,
            version: (major, minor),
            lock: Mutex::new(()),
            _drm_fd: fd,
        });
        displays.push((raw as usize, Arc::downgrade(&inner)));
        Ok(Self { inner })
    }

    /// The `(major, minor)` VA-API version negotiated by `vaInitialize`.
    #[must_use]
    pub fn version(&self) -> (i32, i32) {
        self.inner.version
    }

    /// The driver's vendor string.
    #[must_use]
    pub fn vendor(&self) -> String {
        self.inner.call(|core, dpy| {
            // SAFETY: `dpy` is initialised and the lock is held. libva returns a driver-owned,
            // NUL-terminated string that stays valid while the display lives. It is copied
            // before the lock is released.
            let ptr = unsafe { core.vaQueryVendorString(dpy) };
            if ptr.is_null() {
                return String::new();
            }
            // SAFETY: `ptr` is non-NULL and NUL-terminated, per the libva contract above.
            unsafe { CStr::from_ptr(ptr) }
                .to_string_lossy()
                .into_owned()
        })
    }

    /// Lists the profiles the driver supports (`vaQueryConfigProfiles`).
    ///
    /// # Errors
    /// [`VaError::Status`] if libva fails.
    pub fn profiles(&self) -> VaResult<Vec<sys::VAProfile>> {
        let lib = self.inner.lib();
        self.inner.call(|core, dpy| {
            // SAFETY: `dpy` is initialised and the lock is held. The call takes no out-pointers.
            let capacity = unsafe { core.vaMaxNumProfiles(dpy) };
            let mut list = vec![0; fit::<_, usize>(capacity).unwrap_or(0)];
            let mut count = 0;
            // SAFETY: `list` has `vaMaxNumProfiles` entries, the capacity libva requires.
            // `count` is a valid out-pointer.
            let status = unsafe { core.vaQueryConfigProfiles(dpy, list.as_mut_ptr(), &mut count) };
            check(lib, "vaQueryConfigProfiles", status)?;
            list.truncate(fit::<_, usize>(count).unwrap_or(0));
            Ok(list)
        })
    }

    /// Lists the entrypoints (decode, encode, ...) the driver offers for `profile`.
    ///
    /// # Errors
    /// [`VaError::Status`] if libva fails.
    pub fn entrypoints(&self, profile: sys::VAProfile) -> VaResult<Vec<sys::VAEntrypoint>> {
        let lib = self.inner.lib();
        self.inner.call(|core, dpy| {
            // SAFETY: `dpy` is initialised and the lock is held. The call takes no out-pointers.
            let capacity = unsafe { core.vaMaxNumEntrypoints(dpy) };
            let mut list = vec![0; fit::<_, usize>(capacity).unwrap_or(0)];
            let mut count = 0;
            // SAFETY: `list` has `vaMaxNumEntrypoints` entries, the capacity libva requires.
            // `count` is a valid out-pointer.
            let status = unsafe {
                core.vaQueryConfigEntrypoints(dpy, profile, list.as_mut_ptr(), &mut count)
            };
            check(lib, "vaQueryConfigEntrypoints", status)?;
            list.truncate(fit::<_, usize>(count).unwrap_or(0));
            Ok(list)
        })
    }

    /// Whether the driver offers `entrypoint` for `profile`. Query failures count as no.
    #[must_use]
    pub fn supports(&self, profile: sys::VAProfile, entrypoint: sys::VAEntrypoint) -> bool {
        self.entrypoints(profile)
            .is_ok_and(|offered| offered.contains(&entrypoint))
    }

    /// Reads the driver's value for each attribute in `attribs` (`vaGetConfigAttributes`).
    ///
    /// Set `type_` on input; libva fills in `value`.
    ///
    /// # Errors
    /// [`VaError::Status`] if libva fails.
    pub fn config_attributes(
        &self,
        profile: sys::VAProfile,
        entrypoint: sys::VAEntrypoint,
        attribs: &mut [sys::VAConfigAttrib],
    ) -> VaResult<()> {
        let lib = self.inner.lib();
        let count = fit(attribs.len())?;
        self.inner.call(|core, dpy| {
            // SAFETY: `attribs` is valid and writable for `count` entries. libva writes only the
            // `value` fields inside that range.
            let status = unsafe {
                core.vaGetConfigAttributes(dpy, profile, entrypoint, attribs.as_mut_ptr(), count)
            };
            check(lib, "vaGetConfigAttributes", status)
        })
    }

    /// Creates a config for `profile` and `entrypoint` with the given attributes
    /// (`vaCreateConfig`).
    ///
    /// # Errors
    /// [`VaError::Status`] if libva rejects the attributes.
    pub fn create_config(
        &self,
        profile: sys::VAProfile,
        entrypoint: sys::VAEntrypoint,
        attribs: &[sys::VAConfigAttrib],
    ) -> VaResult<VaConfig> {
        let lib = self.inner.lib();
        let count = fit(attribs.len())?;
        let id = self.inner.call(|core, dpy| {
            let mut id = sys::VA_INVALID_ID;
            // SAFETY: libva reads `count` entries from `attribs` during the call and keeps no
            // pointer to them. It does not write to the input list, so casting away const is
            // only needed to satisfy the C signature. `id` is a valid out-pointer.
            let status = unsafe {
                core.vaCreateConfig(
                    dpy,
                    profile,
                    entrypoint,
                    attribs.as_ptr().cast_mut(),
                    count,
                    &mut id,
                )
            };
            check(lib, "vaCreateConfig", status).map(|()| id)
        })?;
        Ok(VaConfig {
            display: Arc::clone(&self.inner),
            id,
        })
    }
}
