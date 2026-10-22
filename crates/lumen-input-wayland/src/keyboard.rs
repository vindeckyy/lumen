//! Checks evdev keycodes against the compositor's XKB keymap.
//!
//! Wayland and EIS both deliver evdev scancodes, which do not depend on layout. Keys are therefore
//! sent unchanged. xkbcommon is used only to learn which keys the active keymap defines, so keys
//! outside it are dropped instead of forwarded.

use std::fs::File;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;

use xkbcommon::xkb;

/// XKB keycodes are evdev codes plus this offset.
const EVDEV_TO_XKB_OFFSET: u32 = 8;

/// Set of evdev keycodes that the keymap defines.
#[derive(Debug)]
pub(crate) struct KeyFilter {
    defined: Vec<bool>,
}

impl KeyFilter {
    /// Compiles the keymap text of `size` bytes in `fd` and records which keys it defines.
    pub(crate) fn from_fd(fd: OwnedFd, size: usize) -> Result<Self, String> {
        let text = read_keymap_text(fd, size)?;
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_string(
            &ctx,
            text,
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or("xkbcommon could not compile the compositor keymap")?;

        let max_xkb = keymap.max_keycode().raw();
        let mut defined = vec![false; max_xkb.saturating_sub(EVDEV_TO_XKB_OFFSET) as usize + 1];
        for xkb_code in keymap.min_keycode().raw()..=max_xkb {
            let Some(evdev) = xkb_code.checked_sub(EVDEV_TO_XKB_OFFSET) else {
                continue;
            };
            if keymap.key_get_name(xkb::Keycode::new(xkb_code)).is_some() {
                defined[evdev as usize] = true;
            }
        }
        Ok(Self { defined })
    }

    /// True when the keymap defines evdev keycode `evdev`.
    pub(crate) fn contains(&self, evdev: u32) -> bool {
        self.defined.get(evdev as usize).copied().unwrap_or(false)
    }
}

/// Reads the keymap text with a positional read, so the descriptor is never mapped. A trailing
/// NUL, which Wayland counts in `size`, is removed.
fn read_keymap_text(fd: OwnedFd, size: usize) -> Result<String, String> {
    let file = File::from(fd);
    let mut buf = vec![0u8; size];
    file.read_exact_at(&mut buf, 0)
        .map_err(|e| format!("reading keymap fd: {e}"))?;
    while buf.last() == Some(&0) {
        buf.pop();
    }
    String::from_utf8(buf).map_err(|e| format!("keymap is not UTF-8: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Writes `text` to an unlinked temp file and returns it as an fd.
    fn keymap_fd(text: &str) -> OwnedFd {
        let path = std::env::temp_dir().join(format!("lumen-keymap-{}", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("open temp keymap");
        std::fs::remove_file(&path).expect("unlink temp keymap");
        file.write_all(text.as_bytes()).expect("write keymap");
        OwnedFd::from(file)
    }

    #[test]
    fn filter_keeps_only_keys_the_keymap_defines() {
        // Minimal keymap with two keys: A (evdev 30, xkb 38) and B (evdev 48, xkb 56).
        let text = r#"xkb_keymap {
            xkb_keycodes "test" {
                minimum = 8;
                maximum = 255;
                <AC01> = 38;
                <AB01> = 56;
            };
            xkb_types "test" { type "ONE_LEVEL" { modifiers = none; level_name[Level1] = "Any"; }; };
            xkb_compatibility "test" { };
            xkb_symbols "test" {
                key <AC01> { [ a ] };
                key <AB01> { [ b ] };
            };
        };"#;
        let filter = KeyFilter::from_fd(keymap_fd(text), text.len()).expect("test keymap compiles");
        assert!(filter.contains(30), "KEY_A is defined");
        assert!(filter.contains(48), "KEY_B is defined");
        assert!(!filter.contains(1), "KEY_ESC is not defined in this keymap");
        assert!(!filter.contains(100_000), "out of range is rejected");
    }
}
