//! The environment knobs this crate reads.
//!
//! Every `LUMEN_*` read lives here, so the list of what can change the behaviour of the
//! library is in one place. The rule is punktfunk's (MIT OR Apache-2.0, Copyright (c) 2026 unom
//! - Enrico Bühler).

use std::path::PathBuf;

/// The DRM device the VA-API display opens: `LUMEN_VA_DEVICE`, or the default search.
#[must_use]
pub fn va_device() -> Option<PathBuf> {
    std::env::var_os("LUMEN_VA_DEVICE").map(PathBuf::from)
}
