//! Key names (as AeroSpace documents them) and their macOS virtual keycodes.
//!
//! Keycodes are the physical `kVK_*` codes of an ANSI keyboard (`Carbon/HIToolbox/
//! Events.h`), which is what a CGEvent key-down reports whatever the active input
//! source is. A [`KeyPreset`] decides which *physical* key a name refers to: under
//! `qwerty` the name `s` is the key labelled S, under `dvorak` it is the key that types
//! `s` on a Dvorak layout (physically where QWERTY has `;`).

use std::fmt;

/// The key-mapping preset (`key-mapping.preset`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KeyPreset {
    #[default]
    Qwerty,
    Dvorak,
    Colemak,
}

impl KeyPreset {
    pub const ALL: [KeyPreset; 3] = [KeyPreset::Qwerty, KeyPreset::Dvorak, KeyPreset::Colemak];

    pub fn name(self) -> &'static str {
        match self {
            KeyPreset::Qwerty => "qwerty",
            KeyPreset::Dvorak => "dvorak",
            KeyPreset::Colemak => "colemak",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.name() == name)
    }

    /// The keycode of the key that `name` refers to under this preset.
    pub fn keycode(self, name: &str) -> Option<u16> {
        let qwerty = keycode(name)?;
        let Some(ch) = typed_char(name) else { return Some(qwerty) };
        // The physical key carrying `ch` in this layout, named by its QWERTY label.
        let (layout_rows, qwerty_rows) = match self {
            KeyPreset::Qwerty => return Some(qwerty),
            KeyPreset::Dvorak => (DVORAK, QWERTY),
            KeyPreset::Colemak => (COLEMAK, QWERTY),
        };
        let position = layout_rows.chars().position(|c| c == ch)?;
        let label = qwerty_rows.chars().nth(position)?;
        keycode(&char_key_name(label))
    }
}

impl fmt::Display for KeyPreset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.name()) }
}

// The three letter/punctuation blocks of an ANSI keyboard, row by row, in the same
// physical order, so the nth char of each string sits on the same key.
const QWERTY: &str = "qwertyuiop[]asdfghjkl;'zxcvbnm,./-=";
const DVORAK: &str = "',.pyfgcrl/=aoeuidhtns-;qjkxbmwvz[]";
const COLEMAK: &str = "qwfpgjluy;[]arstdhneio'zxcvbkm,./-=";

/// The character a key name types, for the keys a layout preset moves.
fn typed_char(name: &str) -> Option<char> {
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return (c.is_ascii_lowercase()).then_some(c);
    }
    Some(match name {
        "minus" => '-',
        "equal" => '=',
        "period" => '.',
        "comma" => ',',
        "slash" => '/',
        "quote" => '\'',
        "semicolon" => ';',
        "leftSquareBracket" => '[',
        "rightSquareBracket" => ']',
        _ => return None,
    })
}

/// The key name of the character `c` (the inverse of [`typed_char`]).
fn char_key_name(c: char) -> String {
    match c {
        '-' => "minus",
        '=' => "equal",
        '.' => "period",
        ',' => "comma",
        '/' => "slash",
        '\'' => "quote",
        ';' => "semicolon",
        '[' => "leftSquareBracket",
        ']' => "rightSquareBracket",
        other => return other.to_string(),
    }
    .to_owned()
}

/// Every key name and its QWERTY keycode.
pub const KEYS: &[(&str, u16)] = &[
    ("a", 0x00), ("s", 0x01), ("d", 0x02), ("f", 0x03), ("h", 0x04), ("g", 0x05),
    ("z", 0x06), ("x", 0x07), ("c", 0x08), ("v", 0x09), ("b", 0x0B), ("q", 0x0C),
    ("w", 0x0D), ("e", 0x0E), ("r", 0x0F), ("y", 0x10), ("t", 0x11),
    ("1", 0x12), ("2", 0x13), ("3", 0x14), ("4", 0x15), ("6", 0x16), ("5", 0x17),
    ("equal", 0x18), ("9", 0x19), ("7", 0x1A), ("minus", 0x1B), ("8", 0x1C), ("0", 0x1D),
    ("rightSquareBracket", 0x1E), ("o", 0x1F), ("u", 0x20), ("leftSquareBracket", 0x21),
    ("i", 0x22), ("p", 0x23), ("enter", 0x24), ("l", 0x25), ("j", 0x26), ("quote", 0x27),
    ("k", 0x28), ("semicolon", 0x29), ("backslash", 0x2A), ("comma", 0x2B), ("slash", 0x2C),
    ("n", 0x2D), ("m", 0x2E), ("period", 0x2F), ("tab", 0x30), ("space", 0x31),
    ("backtick", 0x32), ("backspace", 0x33), ("esc", 0x35),
    ("keypadDecimalMark", 0x41), ("keypadMultiply", 0x43), ("keypadPlus", 0x45),
    ("keypadClear", 0x47), ("keypadDivide", 0x4B), ("keypadEnter", 0x4C),
    ("keypadMinus", 0x4E), ("keypadEqual", 0x51),
    ("keypad0", 0x52), ("keypad1", 0x53), ("keypad2", 0x54), ("keypad3", 0x55),
    ("keypad4", 0x56), ("keypad5", 0x57), ("keypad6", 0x58), ("keypad7", 0x59),
    ("keypad8", 0x5B), ("keypad9", 0x5C),
    ("f1", 0x7A), ("f2", 0x78), ("f3", 0x63), ("f4", 0x76), ("f5", 0x60), ("f6", 0x61),
    ("f7", 0x62), ("f8", 0x64), ("f9", 0x65), ("f10", 0x6D), ("f11", 0x67), ("f12", 0x6F),
    ("f13", 0x69), ("f14", 0x6B), ("f15", 0x71), ("f16", 0x6A), ("f17", 0x40),
    ("f18", 0x4F), ("f19", 0x50), ("f20", 0x5A),
    ("left", 0x7B), ("right", 0x7C), ("down", 0x7D), ("up", 0x7E),
];

/// The QWERTY keycode of a key name.
pub fn keycode(name: &str) -> Option<u16> {
    KEYS.iter().find(|&&(n, _)| n == name).map(|&(_, code)| code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    /// The oracle: AeroSpace's documented key list against `kVK_*` from Events.h,
    /// written out independently of `KEYS`.
    const EXPECTED: &[(&str, u16)] = &[
        // kVK_ANSI_A.. in header order
        ("a", 0x00), ("s", 0x01), ("d", 0x02), ("f", 0x03), ("h", 0x04), ("g", 0x05),
        ("z", 0x06), ("x", 0x07), ("c", 0x08), ("v", 0x09), ("b", 0x0B), ("q", 0x0C),
        ("w", 0x0D), ("e", 0x0E), ("r", 0x0F), ("y", 0x10), ("t", 0x11), ("o", 0x1F),
        ("u", 0x20), ("i", 0x22), ("p", 0x23), ("l", 0x25), ("j", 0x26), ("k", 0x28),
        ("n", 0x2D), ("m", 0x2E),
        // digits (kVK_ANSI_1 ..)
        ("1", 0x12), ("2", 0x13), ("3", 0x14), ("4", 0x15), ("5", 0x17), ("6", 0x16),
        ("7", 0x1A), ("8", 0x1C), ("9", 0x19), ("0", 0x1D),
        // keypad digits
        ("keypad0", 0x52), ("keypad1", 0x53), ("keypad2", 0x54), ("keypad3", 0x55),
        ("keypad4", 0x56), ("keypad5", 0x57), ("keypad6", 0x58), ("keypad7", 0x59),
        ("keypad8", 0x5B), ("keypad9", 0x5C),
        // function keys
        ("f1", 0x7A), ("f2", 0x78), ("f3", 0x63), ("f4", 0x76), ("f5", 0x60), ("f6", 0x61),
        ("f7", 0x62), ("f8", 0x64), ("f9", 0x65), ("f10", 0x6D), ("f11", 0x67),
        ("f12", 0x6F), ("f13", 0x69), ("f14", 0x6B), ("f15", 0x71), ("f16", 0x6A),
        ("f17", 0x40), ("f18", 0x4F), ("f19", 0x50), ("f20", 0x5A),
        // special keys
        ("minus", 0x1B), ("equal", 0x18), ("period", 0x2F), ("comma", 0x2B),
        ("slash", 0x2C), ("backslash", 0x2A), ("quote", 0x27), ("semicolon", 0x29),
        ("backtick", 0x32), ("leftSquareBracket", 0x21), ("rightSquareBracket", 0x1E),
        ("space", 0x31), ("enter", 0x24), ("esc", 0x35), ("backspace", 0x33), ("tab", 0x30),
        // keypad specials
        ("keypadClear", 0x47), ("keypadDecimalMark", 0x41), ("keypadDivide", 0x4B),
        ("keypadEnter", 0x4C), ("keypadEqual", 0x51), ("keypadMinus", 0x4E),
        ("keypadMultiply", 0x43), ("keypadPlus", 0x45),
        // arrows
        ("left", 0x7B), ("down", 0x7D), ("up", 0x7E), ("right", 0x7C),
    ];

    #[test]
    fn every_documented_key_maps_to_its_exact_keycode() {
        for &(name, code) in EXPECTED {
            assert_eq!(keycode(name), Some(code), "key {name}");
        }
    }

    #[test]
    fn the_table_holds_exactly_the_documented_keys() {
        let have: BTreeSet<&str> = KEYS.iter().map(|&(n, _)| n).collect();
        let want: BTreeSet<&str> = EXPECTED.iter().map(|&(n, _)| n).collect();
        assert_eq!(have, want);
        assert_eq!(KEYS.len(), EXPECTED.len(), "no key is listed twice");
        // letters, digits, keypad digits, F keys, special keys, keypad specials, arrows.
        assert_eq!(EXPECTED.len(), 26 + 10 + 10 + 20 + 16 + 8 + 4);
    }

    #[test]
    fn no_two_keys_share_a_keycode() {
        let mut seen: BTreeMap<u16, &str> = BTreeMap::new();
        for &(name, code) in KEYS {
            if let Some(other) = seen.insert(code, name) {
                panic!("{name} and {other} both map to {code:#x}");
            }
        }
    }

    #[test]
    fn unknown_and_wrongly_cased_names_have_no_keycode() {
        for name in ["", "A", "H", "Space", "ESC", "leftsquarebracket", "f21", "f0", "keypad10", "shift", "alt", " a"] {
            assert_eq!(keycode(name), None, "{name:?}");
        }
    }

    #[test]
    fn presets_parse_by_name_and_round_trip() {
        for preset in KeyPreset::ALL {
            assert_eq!(KeyPreset::from_name(preset.name()), Some(preset));
        }
        assert_eq!(KeyPreset::from_name("azerty"), None);
        assert_eq!(KeyPreset::default(), KeyPreset::Qwerty);
    }

    #[test]
    fn qwerty_is_the_identity() {
        for &(name, code) in EXPECTED {
            assert_eq!(KeyPreset::Qwerty.keycode(name), Some(code), "{name}");
        }
    }

    #[test]
    fn dvorak_names_the_key_that_types_the_character() {
        // Dvorak has `'` where QWERTY has `q`, `a` unchanged, `s` where `;` sits.
        assert_eq!(KeyPreset::Dvorak.keycode("quote"), keycode("q"));
        assert_eq!(KeyPreset::Dvorak.keycode("a"), keycode("a"));
        assert_eq!(KeyPreset::Dvorak.keycode("s"), keycode("semicolon"));
        assert_eq!(KeyPreset::Dvorak.keycode("h"), keycode("j"));
        assert_eq!(KeyPreset::Dvorak.keycode("n"), keycode("l"));
        assert_eq!(KeyPreset::Dvorak.keycode("z"), keycode("slash"));
        assert_eq!(KeyPreset::Dvorak.keycode("minus"), keycode("quote"));
        assert_eq!(KeyPreset::Dvorak.keycode("leftSquareBracket"), keycode("minus"));
    }

    #[test]
    fn colemak_names_the_key_that_types_the_character() {
        assert_eq!(KeyPreset::Colemak.keycode("f"), keycode("e"));
        assert_eq!(KeyPreset::Colemak.keycode("s"), keycode("d"));
        assert_eq!(KeyPreset::Colemak.keycode("n"), keycode("j"));
        assert_eq!(KeyPreset::Colemak.keycode("semicolon"), keycode("p"));
        assert_eq!(KeyPreset::Colemak.keycode("a"), keycode("a"));
    }

    #[test]
    fn layout_presets_are_permutations_and_leave_other_keys_alone() {
        for preset in [KeyPreset::Dvorak, KeyPreset::Colemak] {
            let codes: BTreeSet<u16> =
                KEYS.iter().map(|&(n, _)| preset.keycode(n).expect(n)).collect();
            assert_eq!(codes.len(), KEYS.len(), "{preset} maps two names to one key");
            for name in ["space", "enter", "esc", "tab", "left", "f5", "keypad3", "1", "backslash"] {
                assert_eq!(preset.keycode(name), keycode(name), "{preset} {name}");
            }
        }
        assert_eq!(KeyPreset::Dvorak.keycode("nope"), None);
    }

    #[test]
    fn the_layout_strings_cover_the_same_physical_keys() {
        for rows in [QWERTY, DVORAK, COLEMAK] {
            assert_eq!(rows.chars().count(), 35);
            assert_eq!(rows.chars().collect::<BTreeSet<_>>().len(), 35, "{rows}");
        }
        let sorted = |s: &str| {
            let mut v: Vec<char> = s.chars().collect();
            v.sort_unstable();
            v
        };
        assert_eq!(sorted(QWERTY), sorted(DVORAK));
        assert_eq!(sorted(QWERTY), sorted(COLEMAK));
    }
}
