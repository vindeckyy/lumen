//! Characters to evdev keycodes on a US keyboard.
//!
//! shortcut: text injection assumes the host types with a US-positional layout. A host with
//! another layout gets the wrong glyphs. Upgrade: resolve keysyms through the host keymap.

/// `KEY_LEFTSHIFT`.
pub(crate) const KEY_LEFTSHIFT: u32 = 42;

/// Returns the evdev keycode for `c` on a US layout, and whether Shift must be held.
pub(crate) fn us_key(c: char) -> Option<(u32, bool)> {
    let plain = |code: u32| Some((code, false));
    let shifted = |code: u32| Some((code, true));
    match c {
        'a' => plain(30),
        'b' => plain(48),
        'c' => plain(46),
        'd' => plain(32),
        'e' => plain(18),
        'f' => plain(33),
        'g' => plain(34),
        'h' => plain(35),
        'i' => plain(23),
        'j' => plain(36),
        'k' => plain(37),
        'l' => plain(38),
        'm' => plain(50),
        'n' => plain(49),
        'o' => plain(24),
        'p' => plain(25),
        'q' => plain(16),
        'r' => plain(19),
        's' => plain(31),
        't' => plain(20),
        'u' => plain(22),
        'v' => plain(47),
        'w' => plain(17),
        'x' => plain(45),
        'y' => plain(21),
        'z' => plain(44),
        'A'..='Z' => shifted(us_key(c.to_ascii_lowercase())?.0),
        '1' => plain(2),
        '2' => plain(3),
        '3' => plain(4),
        '4' => plain(5),
        '5' => plain(6),
        '6' => plain(7),
        '7' => plain(8),
        '8' => plain(9),
        '9' => plain(10),
        '0' => plain(11),
        '!' => shifted(2),
        '@' => shifted(3),
        '#' => shifted(4),
        '$' => shifted(5),
        '%' => shifted(6),
        '^' => shifted(7),
        '&' => shifted(8),
        '*' => shifted(9),
        '(' => shifted(10),
        ')' => shifted(11),
        ' ' => plain(57),
        '\n' => plain(28),
        '\t' => plain(15),
        '-' => plain(12),
        '_' => shifted(12),
        '=' => plain(13),
        '+' => shifted(13),
        '[' => plain(26),
        '{' => shifted(26),
        ']' => plain(27),
        '}' => shifted(27),
        ';' => plain(39),
        ':' => shifted(39),
        '\'' => plain(40),
        '"' => shifted(40),
        '`' => plain(41),
        '~' => shifted(41),
        '\\' => plain(43),
        '|' => shifted(43),
        ',' => plain(51),
        '<' => shifted(51),
        '.' => plain(52),
        '>' => shifted(52),
        '/' => plain(53),
        '?' => shifted(53),
        _ => None,
    }
}

/// Calls `tap(keycode, shift)` once per character of `text`. Fails on the first character that has
/// no US keycode, so nothing after it is typed.
pub(crate) fn for_each_tap(
    text: &str,
    mut tap: impl FnMut(u32, bool) -> Result<(), String>,
) -> Result<(), String> {
    for c in text.chars() {
        let (code, shift) = us_key(c).ok_or_else(|| format!("no US-layout key for {c:?}"))?;
        tap(code, shift)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_digits_and_shifted_symbols() {
        assert_eq!(us_key('a'), Some((30, false)));
        assert_eq!(us_key('A'), Some((30, true)));
        assert_eq!(us_key('1'), Some((2, false)));
        assert_eq!(us_key('!'), Some((2, true)));
        assert_eq!(us_key(' '), Some((57, false)));
    }

    #[test]
    fn unknown_characters_are_rejected() {
        assert_eq!(us_key('é'), None);
        assert!(for_each_tap("ab€", |_, _| Ok(())).is_err());
    }

    #[test]
    fn taps_stop_at_first_unmappable_character() {
        let mut taps = Vec::new();
        let result = for_each_tap("a€b", |code, shift| {
            taps.push((code, shift));
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(taps, vec![(30, false)]);
    }
}
