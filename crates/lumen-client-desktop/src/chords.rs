//! The key sequences the client answers itself.
//!
//! A chord is a key combination the client owns: it never reaches the host, and it acts on the
//! client instead. Ctrl+Alt+Shift with a letter is the chord prefix. F11 and Alt+Enter toggle
//! the fullscreen, the way a media player does.
//!
//! The chord's own key is swallowed on the press and on the release, so the host never sees it.
//! The modifiers are sent, so the host never sees them stuck.

use lumen_protocol::input::InputAction;

/// What a chord does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chord {
    /// Hold the pointer in the window, or let it go.
    Capture,
    /// Leave the session.
    Disconnect,
    /// Show or hide the heads-up display.
    Hud,
    /// Make the window fullscreen, or windowed again.
    Fullscreen,
}

// Evdev codes of the keys the chords use.
const KEY_Q: u32 = 16;
const KEY_D: u32 = 32;
const KEY_F: u32 = 33;
const KEY_H: u32 = 35;
const KEY_ENTER: u32 = 28;
const KEY_F11: u32 = 87;
const KEY_LEFTCTRL: u32 = 29;
const KEY_RIGHTCTRL: u32 = 97;
const KEY_LEFTALT: u32 = 56;
const KEY_RIGHTALT: u32 = 100;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_RIGHTSHIFT: u32 = 54;

/// The chords the detector holds.
#[derive(Debug, Default)]
pub struct Chords {
    /// The Ctrl key that is down.
    ctrl: bool,
    /// The Alt key that is down.
    alt: bool,
    /// The Shift key that is down.
    shift: bool,
    /// The chord's own key, which is down, and the chord it made, so the release is swallowed
    /// with the same answer.
    down: Option<(u32, Chord)>,
}

impl Chords {
    /// The chord the action makes, and whether it is the press.
    ///
    /// The answer is `None` for everything that is not a chord. A chord's release comes back
    /// with the same chord and `false`, so the caller can swallow it.
    pub fn take(&mut self, action: InputAction) -> Option<(Chord, bool)> {
        let InputAction::Key { keycode, pressed } = action else {
            return None;
        };
        // The release of a chord's key ends the chord, whatever it was.
        if !pressed && self.down.is_some_and(|(key, _)| key == keycode) {
            let (_, chord) = self.down.take().expect("the chord is there");
            return Some((chord, false));
        }
        if !pressed {
            self.forget(keycode);
            return None;
        }
        match keycode {
            KEY_LEFTCTRL | KEY_RIGHTCTRL => self.ctrl = true,
            KEY_LEFTALT | KEY_RIGHTALT => self.alt = true,
            KEY_LEFTSHIFT | KEY_RIGHTSHIFT => self.shift = true,
            _ => {}
        }
        let chord = self.chord_of(keycode);
        if let Some(chord) = chord {
            self.down = Some((keycode, chord));
        }
        chord.map(|chord| (chord, true))
    }

    /// The chord the key press makes, with the modifiers that are held.
    fn chord_of(&self, keycode: u32) -> Option<Chord> {
        if self.ctrl && self.alt && self.shift {
            return match keycode {
                KEY_Q => Some(Chord::Capture),
                KEY_D => Some(Chord::Disconnect),
                KEY_H => Some(Chord::Hud),
                _ => None,
            };
        }
        // F11 toggles the fullscreen, and so does Alt+Enter: some keyboard layers send a
        // media key for a plain F11.
        if keycode == KEY_F11 {
            return Some(Chord::Fullscreen);
        }
        if keycode == KEY_ENTER && self.alt {
            return Some(Chord::Fullscreen);
        }
        // Ctrl+Shift+F toggles the fullscreen the way it always did.
        if keycode == KEY_F && self.ctrl && self.shift {
            return Some(Chord::Fullscreen);
        }
        None
    }

    /// Forgets a modifier that came up.
    fn forget(&mut self, keycode: u32) {
        match keycode {
            KEY_LEFTCTRL | KEY_RIGHTCTRL => self.ctrl = false,
            KEY_LEFTALT | KEY_RIGHTALT => self.alt = false,
            KEY_LEFTSHIFT | KEY_RIGHTSHIFT => self.shift = false,
            _ => {}
        }
    }
}

/// The chord a pad's buttons make, and the buttons that go to the host.
///
/// The Guide button held with the A button is the client's own chord: the Guide and the A stay
/// off the wire, so the host's game never sees them. The chord fires when the A goes down.
#[must_use]
pub fn pad_chord(buttons: u16, previous: u16) -> (u16, Option<Chord>) {
    let guide = buttons & lumen_protocol::input::pad::GUIDE != 0;
    let a = buttons & lumen_protocol::input::pad::A != 0;
    if !(guide && a) {
        return (buttons, None);
    }
    let masked = buttons & !(lumen_protocol::input::pad::GUIDE | lumen_protocol::input::pad::A);
    let fired = previous & lumen_protocol::input::pad::A == 0;
    (masked, fired.then_some(Chord::Capture))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key press of a key.
    fn press(keycode: u32) -> InputAction {
        InputAction::Key {
            keycode,
            pressed: true,
        }
    }

    /// The key release of a key.
    fn release(keycode: u32) -> InputAction {
        InputAction::Key {
            keycode,
            pressed: false,
        }
    }

    /// A chord with its modifiers held.
    fn with_modifiers(chords: &mut Chords, keycode: u32) -> Option<(Chord, bool)> {
        chords.take(press(KEY_LEFTCTRL));
        chords.take(press(KEY_LEFTALT));
        chords.take(press(KEY_LEFTSHIFT));
        chords.take(press(keycode))
    }

    #[test]
    fn the_prefix_with_a_letter_is_a_chord() {
        let mut chords = Chords::default();
        assert_eq!(
            with_modifiers(&mut chords, KEY_Q),
            Some((Chord::Capture, true))
        );
        let mut chords = Chords::default();
        assert_eq!(
            with_modifiers(&mut chords, KEY_D),
            Some((Chord::Disconnect, true))
        );
        let mut chords = Chords::default();
        assert_eq!(with_modifiers(&mut chords, KEY_H), Some((Chord::Hud, true)));
    }

    #[test]
    fn a_letter_without_the_prefix_is_not_a_chord() {
        let mut chords = Chords::default();
        assert_eq!(chords.take(press(KEY_Q)), None);
    }

    #[test]
    fn the_prefix_with_another_letter_is_not_a_chord() {
        let mut chords = Chords::default();
        chords.take(press(KEY_LEFTCTRL));
        chords.take(press(KEY_LEFTALT));
        chords.take(press(KEY_LEFTSHIFT));
        assert_eq!(chords.take(press(KEY_F)), None);
    }

    #[test]
    fn f11_and_alt_enter_toggle_the_fullscreen() {
        let mut chords = Chords::default();
        assert_eq!(chords.take(press(KEY_F11)), Some((Chord::Fullscreen, true)));
        let mut chords = Chords::default();
        chords.take(press(KEY_LEFTALT));
        assert_eq!(
            chords.take(press(KEY_ENTER)),
            Some((Chord::Fullscreen, true))
        );
    }

    #[test]
    fn the_release_of_a_chord_key_is_swallowed() {
        let mut chords = Chords::default();
        assert_eq!(chords.take(press(KEY_F11)), Some((Chord::Fullscreen, true)));
        // The release is answered, so the caller can swallow it.
        assert_eq!(
            chords.take(release(KEY_F11)),
            Some((Chord::Fullscreen, false))
        );
        // A second release is nothing.
        assert_eq!(chords.take(release(KEY_F11)), None);
    }

    #[test]
    fn the_release_of_a_cord_names_the_cord_it_made() {
        let mut chords = Chords::default();
        assert_eq!(
            with_modifiers(&mut chords, KEY_Q),
            Some((Chord::Capture, true))
        );
        assert_eq!(chords.take(release(KEY_Q)), Some((Chord::Capture, false)));
    }

    #[test]
    fn the_pad_chord_fires_once_and_keeps_its_buttons_off_the_wire() {
        let guide = lumen_protocol::input::pad::GUIDE;
        let a = lumen_protocol::input::pad::A;
        let b = lumen_protocol::input::pad::B;
        // The Guide alone is nothing.
        assert_eq!(pad_chord(guide, 0), (guide, None));
        // The A alone is nothing.
        assert_eq!(pad_chord(a, 0), (a, None));
        // The Guide and the A are the chord: it fires and the buttons stay off the wire.
        assert_eq!(pad_chord(guide | a | b, 0), (b, Some(Chord::Capture)));
        // While it is held, it does not fire again.
        assert_eq!(pad_chord(guide | a | b, guide | a | b), (b, None));
    }

    #[test]
    fn a_modifier_that_comes_up_is_forgotten() {
        let mut chords = Chords::default();
        chords.take(press(KEY_LEFTCTRL));
        chords.take(press(KEY_LEFTALT));
        chords.take(press(KEY_LEFTSHIFT));
        chords.take(release(KEY_LEFTCTRL));
        // Without the Ctrl the letter is nothing.
        assert_eq!(chords.take(press(KEY_Q)), None);
    }
}
