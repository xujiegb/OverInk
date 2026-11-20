#![forbid(unsafe_code)]

//! Safe physical-key injection for the focused SDL canvas on Windows/macOS.
//! SDL_TextInput remains active: the system IME owns preedit and candidates.
//! Do not use Unicode text injection for ASCII keys; it bypasses IME composition.
//!
//! macOS requires Accessibility permission, requested by enigo's safe API.
//! On Windows the virtual-key events are sent through the system SendInput API
//! by the dependency. OverInk itself has no native pointers or FFI here.
use enigo::{Direction, Enigo, Key, Keyboard, Settings};
use std::cell::RefCell;

#[cfg(not(test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhysicalKey {
    Char(char),
    Space,
    Backspace,
    Return,
    Left,
    Right,
}

thread_local! {
    static INJECTOR: RefCell<Option<Enigo>> = const { RefCell::new(None) };
}

fn with_injector<T>(action: impl FnOnce(&mut Enigo) -> Result<T, String>) -> Result<T, String> {
    INJECTOR.with(|slot| {
        let mut guard = slot.borrow_mut();
        if guard.is_none() {
            *guard = Some(
                Enigo::new(&Settings::default())
                    .map_err(|e| format!("Could not initialize system key input: {e}"))?,
            );
        }
        match guard.as_mut() {
            Some(injector) => action(injector),
            None => Err("System keyboard injection is not available".into()),
        }
    })
}

// The physical QWERTY key, not the Unicode character, is what causes
// Windows TSF or macOS input methods to open and update candidate windows.
// Symbol variants are expressed as shift+physical-key rather than committing
// a precomposed character. A locale-specific non-ASCII key may still use the
// regular egui insert path when no native key is available.
fn base_key(ch: char) -> (char, bool) {
    let shifted = match ch {
        '!' => Some('1'),
        '@' => Some('2'),
        '#' => Some('3'),
        '$' => Some('4'),
        '%' => Some('5'),
        '^' => Some('6'),
        '&' => Some('7'),
        '*' => Some('8'),
        '(' => Some('9'),
        ')' => Some('0'),
        '_' => Some('-'),
        '+' => Some('='),
        '{' => Some('['),
        '}' => Some(']'),
        '|' => Some('\\'),
        ':' => Some(';'),
        '"' => Some('\''),
        '<' => Some(','),
        '>' => Some('.'),
        '?' => Some('/'),
        '~' => Some('`'),
        _ => None,
    };
    if let Some(key) = shifted {
        (key, true)
    } else {
        (ch.to_ascii_lowercase(), ch.is_ascii_uppercase())
    }
}

#[cfg(target_os = "windows")]
fn physical_code(ch: char) -> Option<u32> {
    match ch {
        'a'..='z' => Some(u32::from(ch) - u32::from('a') + 0x41),
        '0'..='9' => Some(u32::from(ch) - u32::from('0') + 0x30),
        ' ' => Some(0x20),
        '-' => Some(0xBD),
        '=' => Some(0xBB),
        '[' => Some(0xDB),
        ']' => Some(0xDD),
        '\\' => Some(0xDC),
        ';' => Some(0xBA),
        '\'' => Some(0xDE),
        ',' => Some(0xBC),
        '.' => Some(0xBE),
        '/' => Some(0xBF),
        '`' => Some(0xC0),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn physical_code(ch: char) -> Option<u32> {
    match ch {
        'a' => Some(0),
        's' => Some(1),
        'd' => Some(2),
        'f' => Some(3),
        'h' => Some(4),
        'g' => Some(5),
        'z' => Some(6),
        'x' => Some(7),
        'c' => Some(8),
        'v' => Some(9),
        'b' => Some(11),
        'q' => Some(12),
        'w' => Some(13),
        'e' => Some(14),
        'r' => Some(15),
        'y' => Some(16),
        't' => Some(17),
        '1' => Some(18),
        '2' => Some(19),
        '3' => Some(20),
        '4' => Some(21),
        '6' => Some(22),
        '5' => Some(23),
        '=' => Some(24),
        '9' => Some(25),
        '7' => Some(26),
        '-' => Some(27),
        '8' => Some(28),
        '0' => Some(29),
        ']' => Some(30),
        'o' => Some(31),
        'u' => Some(32),
        '[' => Some(33),
        'i' => Some(34),
        'p' => Some(35),
        'l' => Some(37),
        'j' => Some(38),
        '\'' => Some(39),
        'k' => Some(40),
        ';' => Some(41),
        '\\' => Some(42),
        ',' => Some(43),
        '/' => Some(44),
        'n' => Some(45),
        'm' => Some(46),
        '.' => Some(47),
        '`' => Some(50),
        ' ' => Some(49),
        _ => None,
    }
}

pub fn supports_character(ch: char) -> bool {
    let (ch, _) = base_key(ch);
    physical_code(ch).is_some()
}

fn send_with_modifier(keyboard: &mut Enigo, key: Key, modifier: Option<Key>) -> Result<(), String> {
    if let Some(modifier) = modifier {
        keyboard
            .key(modifier, Direction::Press)
            .map_err(|e| e.to_string())?;
        // Always release after the click, even if the click returns an error:
        // never leave Shift/Control/Win logically held.
        let click = keyboard.key(key, Direction::Click);
        let release = keyboard.key(modifier, Direction::Release);
        click.map_err(|e| e.to_string())?;
        release.map_err(|e| e.to_string())
    } else {
        keyboard
            .key(key, Direction::Click)
            .map_err(|e| e.to_string())
    }
}

#[cfg(not(test))]
pub fn send_key(action: PhysicalKey) -> Result<(), String> {
    with_injector(|keyboard| {
        let (key, modifier) = match action {
            PhysicalKey::Char(ch) => {
                let (base, shift) = base_key(ch);
                let Some(code) = physical_code(base) else {
                    return Err(format!("No physical key maps to {ch:?}"));
                };
                (Key::Other(code), shift.then_some(Key::Shift))
            }
            PhysicalKey::Space => (Key::Space, None),
            PhysicalKey::Backspace => (Key::Backspace, None),
            PhysicalKey::Return => (Key::Return, None),
            PhysicalKey::Left => (Key::LeftArrow, None),
            PhysicalKey::Right => (Key::RightArrow, None),
        };
        send_with_modifier(keyboard, key, modifier)
    })
}

/// Invoke each OS's established input-source shortcut without pretending
/// that a UTF-8 insert event changes the input language.
/// Windows: Win+Space. macOS: Control+Space (user-configurable in Settings).
pub fn switch_source() -> Result<(), String> {
    with_injector(|keyboard| {
        #[cfg(target_os = "windows")]
        let modifier = Key::Meta;
        #[cfg(target_os = "macos")]
        let modifier = Key::Control;
        send_with_modifier(keyboard, Key::Space, Some(modifier))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qwerty_keys_are_physical_and_shift_works() {
        assert_eq!(base_key('A'), ('a', true));
        assert_eq!(base_key('a'), ('a', false));
        assert_eq!(base_key('?'), ('/', true));
        assert_eq!(base_key('!'), ('1', true));
        for key in ['a', 'z', '0', '9', '/', ';', '!', '?'] {
            assert!(supports_character(key), "Missing physical mapping: {key}");
        }
        assert!(!supports_character('你'));
        assert!(!supports_character('🙂'));
    }
}
