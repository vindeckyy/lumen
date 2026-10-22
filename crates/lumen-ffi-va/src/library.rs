//! Runtime loading of libva and libva-drm.

use std::{fmt, sync::LazyLock};

use crate::{error::VaError, sys};

/// Entry points of `libva.so.2` and `libva-drm.so.2`, loaded once per process.
///
/// The libraries are never unloaded. libva keeps process-wide state, and unloading
/// it while another thread still uses a display would be unsound.
pub struct VaLibrary {
    pub(crate) core: sys::VaCore,
    pub(crate) drm: sys::VaDrm,
}

static LIBRARY: LazyLock<Result<VaLibrary, String>> = LazyLock::new(VaLibrary::open);

impl VaLibrary {
    /// Returns the process-wide libva handle, loading it on first use.
    ///
    /// # Errors
    /// [`VaError::Unavailable`] if a library is absent or lacks a required symbol.
    /// The outcome is cached, so every call reports the same result.
    pub fn load() -> Result<&'static Self, VaError> {
        match &*LIBRARY {
            Ok(library) => Ok(library),
            Err(reason) => Err(VaError::Unavailable(reason.clone())),
        }
    }

    fn open() -> Result<Self, String> {
        // SAFETY: loading a shared object runs its initialisers. `libva.so.2` is the system
        // VA-API library that these bindings describe. `VaCore::new` resolves every symbol
        // before it returns, so no entry point can be called while missing.
        let core =
            unsafe { sys::VaCore::new("libva.so.2") }.map_err(|e| format!("libva.so.2: {e}"))?;
        // SAFETY: the same contract applies to `libva-drm.so.2`, which exports `vaGetDisplayDRM`.
        let drm = unsafe { sys::VaDrm::new("libva-drm.so.2") }
            .map_err(|e| format!("libva-drm.so.2: {e}"))?;
        Ok(Self { core, drm })
    }
}

impl fmt::Debug for VaLibrary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VaLibrary").finish_non_exhaustive()
    }
}
