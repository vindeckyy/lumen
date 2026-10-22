//! Keycodes, keysyms and the evdev codes the protocol carries.
//!
//! An X11 server hands out keycodes that are evdev codes plus eight. The
//! protocol carries evdev codes, because they do not depend on the layout, so
//! every keycode is shifted by that offset on the way out.
//!
//! The keysym table comes from the X server, one row per keycode and one
//! column per group and level. The names come from xkbcommon, which knows the
//! whole keysym registry, so a key can be named without a keymap.

use lumen_protocol::input::InputAction;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as _;
use xkbcommon::xkb::{self, Keycode, Keysym};

/// XKB keycodes are evdev codes plus this offset.
pub const EVDEV_TO_XKB_OFFSET: u32 = 8;

/// The lowest keycode an X11 server hands out.
pub const MIN_X11_KEYCODE: u8 = 8;

/// The highest keycode an X11 server hands out.
pub const MAX_X11_KEYCODE: u8 = 255;

/// The number of keycodes an X11 server may hand out.
pub const X11_KEYCODE_COUNT: u8 = MAX_X11_KEYCODE - MIN_X11_KEYCODE + 1;

/// Why a keymap could not be read.
#[derive(Debug, thiserror::Error)]
pub enum KeymapError {
    /// The X server refused the request.
    #[error("the keyboard mapping request failed: {0}")]
    Connection(String),
    /// The reply did not hold a usable table.
    #[error("the keyboard mapping reply is malformed: {0}")]
    Malformed(&'static str),
    /// The XKB keymap text did not compile.
    #[error("the xkb keymap does not compile: {0}")]
    Xkb(String),
}

/// The keyboard mapping of an X11 display.
///
/// The mapping holds the keysym of every keycode. The keysyms themselves are
/// layout dependent, so they are only useful for naming a key; the evdev code
/// of a keycode does not depend on the layout at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Keymap {
    /// The keysyms of every keycode, indexed by the keycode itself.
    keysyms: Vec<Vec<u32>>,
}

impl Keymap {
    /// An empty keymap. No keycode resolves to a keysym.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Builds a keymap from a raw keysym table.
    ///
    /// `keysyms` holds `keysyms_per_keycode` entries for every keycode, in
    /// keycode order, starting at `first_keycode`. The first column of a
    /// keycode is the unshifted symbol, the second the shifted one.
    ///
    /// # Errors
    /// Returns [`KeymapError::Malformed`] when the table is empty or does not
    /// hold a whole number of keycodes.
    pub fn from_keysyms(
        first_keycode: u8,
        keysyms_per_keycode: u8,
        keysyms: &[u32],
    ) -> Result<Self, KeymapError> {
        if keysyms_per_keycode == 0 {
            return Err(KeymapError::Malformed(
                "the server reports no keysym per keycode",
            ));
        }
        let per_keycode = usize::from(keysyms_per_keycode);
        if !keysyms.len().is_multiple_of(per_keycode) {
            return Err(KeymapError::Malformed(
                "the keysym table is not a whole number of keycodes",
            ));
        }
        let mut table: Vec<Vec<u32>> = vec![Vec::new(); usize::from(first_keycode)];
        for chunk in keysyms.chunks(per_keycode) {
            table.push(chunk.to_vec());
        }
        Ok(Self { keysyms: table })
    }

    /// Compiles an XKB keymap and reads the keysym of every keycode out of it.
    ///
    /// The X server does not hand out the keymap text, so this is the path for
    /// a caller that has the text from somewhere else. xkbcommon compiles it.
    ///
    /// # Errors
    /// Returns [`KeymapError::Xkb`] when xkbcommon cannot compile the text.
    pub fn from_xkb_text(text: &str) -> Result<Self, KeymapError> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_string(
            &context,
            text.to_owned(),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| KeymapError::Xkb("xkbcommon refused the keymap".to_owned()))?;

        let max = keymap.max_keycode().raw();
        let min = keymap.min_keycode().raw();
        let mut table: Vec<Vec<u32>> = vec![Vec::new(); usize::try_from(max).unwrap_or(0) + 1];
        for code in min..=max {
            let syms = keymap.key_get_syms_by_level(Keycode::new(code), 0, 0);
            let row: Vec<u32> = syms.iter().map(|sym| sym.raw()).collect();
            if let Some(slot) = table.get_mut(usize::try_from(code).unwrap_or(0)) {
                *slot = row;
            }
        }
        Ok(Self { keysyms: table })
    }

    /// Reads the keyboard mapping from an X server.
    ///
    /// # Errors
    /// Returns [`KeymapError::Connection`] when the server refuses the
    /// request, and [`KeymapError::Malformed`] when the reply is unusable.
    pub fn load(conn: &impl Connection) -> Result<Self, KeymapError> {
        let reply = conn
            .get_keyboard_mapping(MIN_X11_KEYCODE, X11_KEYCODE_COUNT)
            .map_err(|e| KeymapError::Connection(e.to_string()))?
            .reply()
            .map_err(|e| KeymapError::Connection(e.to_string()))?;
        Self::from_keysyms(MIN_X11_KEYCODE, reply.keysyms_per_keycode, &reply.keysyms)
    }

    /// The keysyms of one keycode, in group and level order.
    #[must_use]
    pub fn keysyms(&self, x11_keycode: u8) -> &[u32] {
        self.keysyms
            .get(usize::from(x11_keycode))
            .map_or(&[], Vec::as_slice)
    }

    /// The keysym of a keycode, which is the symbol the layout defines.
    ///
    /// The first keysym of the first group is the unshifted symbol.
    #[must_use]
    pub fn keysym(&self, x11_keycode: u8) -> Option<u32> {
        self.keysyms(x11_keycode)
            .iter()
            .copied()
            .find(|sym| *sym != 0)
    }

    /// The name of the keysym of a keycode, for example `a` or `Return`.
    ///
    /// The name comes from the keysym registry of xkbcommon, so it does not
    /// depend on the layout of the display.
    #[must_use]
    pub fn keysym_name(&self, x11_keycode: u8) -> Option<String> {
        let sym = self.keysym(x11_keycode)?;
        let name = xkb::keysym_get_name(Keysym::new(sym));
        if name.is_empty() { None } else { Some(name) }
    }

    /// The evdev code of a keycode.
    #[must_use]
    pub fn evdev_code(&self, x11_keycode: u8) -> Option<u32> {
        evdev_code_from_x11(x11_keycode)
    }

    /// The protocol action of a key press or a key release.
    #[must_use]
    pub fn key_action(&self, x11_keycode: u8, pressed: bool) -> Option<InputAction> {
        let keycode = evdev_code_from_x11(x11_keycode)?;
        Some(InputAction::Key { keycode, pressed })
    }

    /// Whether the keymap holds a keysym for the keycode.
    #[must_use]
    pub fn defines(&self, x11_keycode: u8) -> bool {
        self.keysym(x11_keycode).is_some()
    }
}

/// The evdev code of an X11 keycode.
///
/// X11 keycodes are evdev codes plus eight, so the code is the keycode minus
/// that offset. A keycode outside the range the server may use has no evdev
/// code.
#[must_use]
pub fn evdev_code_from_x11(x11_keycode: u8) -> Option<u32> {
    if !(MIN_X11_KEYCODE..=MAX_X11_KEYCODE).contains(&x11_keycode) {
        return None;
    }
    Some(u32::from(x11_keycode) - EVDEV_TO_XKB_OFFSET)
}

/// The X11 keycode of an evdev code.
#[must_use]
pub fn x11_keycode_from_evdev(evdev: u32) -> Option<u8> {
    let code = evdev.checked_add(EVDEV_TO_XKB_OFFSET)?;
    if code > u32::from(MAX_X11_KEYCODE) {
        return None;
    }
    u8::try_from(code).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A keymap text with two keys: A (xkb 38, evdev 30) and B (xkb 56, evdev 48).
    const TEST_KEYMAP: &str = r#"xkb_keymap {
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

    #[test]
    fn x11_keycodes_are_evdev_codes_plus_eight() {
        assert_eq!(evdev_code_from_x11(38), Some(30));
        assert_eq!(evdev_code_from_x11(9), Some(1));
        assert_eq!(evdev_code_from_x11(8), Some(0));
        assert_eq!(evdev_code_from_x11(255), Some(247));
    }

    #[test]
    fn keycodes_outside_the_server_range_have_no_evdev_code() {
        assert_eq!(evdev_code_from_x11(0), None);
        assert_eq!(evdev_code_from_x11(7), None);
        assert_eq!(x11_keycode_from_evdev(248), None);
        assert_eq!(x11_keycode_from_evdev(u32::MAX), None);
    }

    #[test]
    fn the_evdev_code_and_the_x11_keycode_round_trip() {
        for evdev in [0u32, 1, 30, 48, 247] {
            let x11 = x11_keycode_from_evdev(evdev).expect("an x11 keycode");
            assert_eq!(evdev_code_from_x11(x11), Some(evdev));
        }
    }

    #[test]
    fn a_keymap_from_a_raw_table_names_its_keys() {
        // Two keycodes, two keysyms each: the unshifted symbol and the shifted one.
        let keysyms = [0x0061, 0x0041, 0x0062, 0x0042];
        let map = Keymap::from_keysyms(38, 2, &keysyms).expect("a keymap");
        assert_eq!(map.keysym(38), Some(0x0061));
        assert_eq!(map.keysym(39), Some(0x0062));
        assert_eq!(map.keysym_name(38).as_deref(), Some("a"));
        assert_eq!(map.keysym_name(39).as_deref(), Some("b"));
        assert_eq!(map.evdev_code(38), Some(30));
        assert!(map.defines(38));
        assert!(!map.defines(40), "an empty keycode has no keysym");
    }

    #[test]
    fn a_raw_table_that_is_not_a_whole_number_of_keycodes_is_refused() {
        let err = Keymap::from_keysyms(8, 2, &[0x0061, 0x0041, 0x0062]).expect_err("refused");
        assert!(matches!(err, KeymapError::Malformed(_)));
    }

    #[test]
    fn a_raw_table_without_a_keysym_per_keycode_is_refused() {
        let err = Keymap::from_keysyms(8, 0, &[]).expect_err("refused");
        assert!(matches!(err, KeymapError::Malformed(_)));
    }

    #[test]
    fn a_keymap_compiled_by_xkbcommon_names_its_keys() {
        let map = Keymap::from_xkb_text(TEST_KEYMAP).expect("the test keymap compiles");
        assert_eq!(map.keysym(38), Some(0x0061));
        assert_eq!(map.keysym_name(38).as_deref(), Some("a"));
        assert_eq!(map.keysym_name(56).as_deref(), Some("b"));
        assert_eq!(map.evdev_code(38), Some(30));
        assert_eq!(map.evdev_code(56), Some(48));
        assert!(!map.defines(60), "the keymap defines no other key");
    }

    #[test]
    fn a_keymap_that_does_not_compile_is_refused() {
        let err = Keymap::from_xkb_text("this is not a keymap").expect_err("refused");
        assert!(matches!(err, KeymapError::Xkb(_)));
    }

    #[test]
    fn a_key_press_becomes_a_protocol_action() {
        let map = Keymap::from_xkb_text(TEST_KEYMAP).expect("the test keymap compiles");
        let action = map.key_action(38, true).expect("an action");
        assert_eq!(
            action,
            InputAction::Key {
                keycode: 30,
                pressed: true
            }
        );
        assert_eq!(
            map.key_action(56, false),
            Some(InputAction::Key {
                keycode: 48,
                pressed: false
            })
        );
    }
}
