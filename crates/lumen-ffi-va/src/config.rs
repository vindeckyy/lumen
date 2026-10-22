//! VA configs: a profile and entrypoint with fixed attributes.

use std::{fmt, sync::Arc};

use crate::{
    display::DisplayInner,
    error::{VaResult, check},
    sys,
};

/// A VA config (`VAConfigID`), destroyed on drop.
pub struct VaConfig {
    pub(crate) display: Arc<DisplayInner>,
    pub(crate) id: sys::VAConfigID,
}

impl VaConfig {
    /// The raw `VAConfigID`.
    #[must_use]
    pub fn id(&self) -> sys::VAConfigID {
        self.id
    }

    /// Reads back profile, entrypoint and attributes (`vaQueryConfigAttributes`).
    ///
    /// # Errors
    /// [`crate::VaError::Status`] if libva fails.
    pub fn attributes(
        &self,
    ) -> VaResult<(sys::VAProfile, sys::VAEntrypoint, Vec<sys::VAConfigAttrib>)> {
        let lib = self.display.lib();
        self.display.call(|core, dpy| {
            // SAFETY: `dpy` is initialised and the lock is held. The call takes no out-pointers.
            let capacity = unsafe { core.vaMaxNumConfigAttributes(dpy) };
            let mut attribs = vec![
                sys::VAConfigAttrib { type_: 0, value: 0 };
                usize::try_from(capacity).unwrap_or(0)
            ];
            let (mut profile, mut entrypoint, mut count) = (0, 0, 0);
            // SAFETY: the out-pointers are valid. `attribs` holds `vaMaxNumConfigAttributes`
            // entries, the capacity libva requires. `self.id` names a config of this display,
            // which the `Arc` keeps alive.
            let status = unsafe {
                core.vaQueryConfigAttributes(
                    dpy,
                    self.id,
                    &mut profile,
                    &mut entrypoint,
                    attribs.as_mut_ptr(),
                    &mut count,
                )
            };
            check(lib, "vaQueryConfigAttributes", status)?;
            attribs.truncate(usize::try_from(count).unwrap_or(0));
            Ok((profile, entrypoint, attribs))
        })
    }

    /// Lists the surface attributes this config accepts (`vaQuerySurfaceAttributes`).
    ///
    /// # Errors
    /// [`crate::VaError::Status`] if libva fails.
    pub fn surface_attributes(&self) -> VaResult<Vec<sys::VASurfaceAttrib>> {
        let lib = self.display.lib();
        self.display.call(|core, dpy| {
            let mut count = 0;
            // SAFETY: a NULL list asks libva only for the attribute count. `count` is a valid
            // out-pointer.
            let status = unsafe {
                core.vaQuerySurfaceAttributes(dpy, self.id, std::ptr::null_mut(), &mut count)
            };
            check(lib, "vaQuerySurfaceAttributes", status)?;
            let mut attribs = vec![crate::surface::blank_attrib(); count as usize];
            // SAFETY: `attribs` has `count` writable entries, the size libva just reported.
            // `count` is a valid out-pointer.
            let status = unsafe {
                core.vaQuerySurfaceAttributes(dpy, self.id, attribs.as_mut_ptr(), &mut count)
            };
            check(lib, "vaQuerySurfaceAttributes", status)?;
            attribs.truncate(count as usize);
            Ok(attribs)
        })
    }
}

impl fmt::Debug for VaConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VaConfig")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Drop for VaConfig {
    fn drop(&mut self) {
        self.display.call(|core, dpy| {
            // SAFETY: `self.id` was created on `dpy` and is destroyed exactly once, here. The
            // display is still alive because `self.display` holds it. The status is dropped
            // because `Drop` cannot report it.
            let _ = unsafe { core.vaDestroyConfig(dpy, self.id) };
        });
    }
}
