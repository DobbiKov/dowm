//! Hotkey chords: `alt-shift-h`, `cmd-ctrl-alt-shift-space`.
//!
//! A chord is zero or more modifiers followed by exactly one key, joined by `-`. Key
//! names (see [`keys`](super::keys)) never contain a `-`, so the split is unambiguous.

use std::fmt;

use super::keys::KeyPreset;

/// The four modifiers a binding can require, as a set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Mods(u8);

impl Mods {
    pub const NONE: Mods = Mods(0);
    pub const CMD: Mods = Mods(1);
    pub const ALT: Mods = Mods(2);
    pub const CTRL: Mods = Mods(4);
    pub const SHIFT: Mods = Mods(8);

    /// Modifier names in the canonical order [`Hotkey`]'s `Display` writes them.
    const NAMED: [(&'static str, Mods); 4] = [
        ("cmd", Mods::CMD),
        ("alt", Mods::ALT),
        ("ctrl", Mods::CTRL),
        ("shift", Mods::SHIFT),
    ];

    pub fn from_name(name: &str) -> Option<Mods> {
        Self::NAMED.iter().find(|&&(n, _)| n == name).map(|&(_, m)| m)
    }

    pub fn contains(self, other: Mods) -> bool { self.0 & other.0 == other.0 }

    pub fn is_empty(self) -> bool { self.0 == 0 }

    /// The modifiers set in a CGEvent's flags. Anything else in there (caps lock, fn,
    /// the numeric-pad bit, left/right device bits) is not a binding modifier.
    pub fn from_cg_flags(flags: u64) -> Mods {
        const SHIFT: u64 = 0x0002_0000;
        const CTRL: u64 = 0x0004_0000;
        const ALT: u64 = 0x0008_0000;
        const CMD: u64 = 0x0010_0000;
        let mut mods = Mods::NONE;
        for (bit, m) in [(SHIFT, Mods::SHIFT), (CTRL, Mods::CTRL), (ALT, Mods::ALT), (CMD, Mods::CMD)] {
            if flags & bit != 0 {
                mods = mods | m;
            }
        }
        mods
    }
}

impl std::ops::BitOr for Mods {
    type Output = Mods;

    fn bitor(self, rhs: Mods) -> Mods { Mods(self.0 | rhs.0) }
}

/// A modifier set and a physical key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hotkey {
    pub mods: Mods,
    pub keycode: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HotkeyError {
    Empty,
    /// An empty segment: `alt--h`, `-h`, `alt-`.
    EmptySegment,
    UnknownModifier(String),
    DuplicateModifier(String),
    /// The chord ends in a modifier (or is only modifiers): no key to press.
    NoKey(String),
    UnknownKey(String),
}

impl fmt::Display for HotkeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "empty hotkey"),
            Self::EmptySegment => write!(f, "empty modifier or key between `-`"),
            Self::UnknownModifier(m) => {
                write!(f, "unknown modifier `{m}` (expected cmd, alt, ctrl or shift)")
            }
            Self::DuplicateModifier(m) => write!(f, "modifier `{m}` appears twice"),
            Self::NoKey(m) => write!(f, "the chord ends in the modifier `{m}`; it needs a key"),
            Self::UnknownKey(k) => write!(f, "unknown key `{k}`"),
        }
    }
}

impl std::error::Error for HotkeyError {}

impl Hotkey {
    /// Parse a chord, resolving the key name under `preset`.
    pub fn parse(text: &str, preset: KeyPreset) -> Result<Hotkey, HotkeyError> {
        if text.is_empty() {
            return Err(HotkeyError::Empty);
        }
        let parts: Vec<&str> = text.split('-').collect();
        if parts.iter().any(|p| p.is_empty()) {
            return Err(HotkeyError::EmptySegment);
        }
        let (key, modifiers) = parts.split_last().ok_or(HotkeyError::Empty)?;
        let mut mods = Mods::NONE;
        for name in modifiers {
            let m = Mods::from_name(name)
                .ok_or_else(|| HotkeyError::UnknownModifier((*name).to_owned()))?;
            if mods.contains(m) {
                return Err(HotkeyError::DuplicateModifier((*name).to_owned()));
            }
            mods = mods | m;
        }
        if Mods::from_name(key).is_some() {
            return Err(HotkeyError::NoKey((*key).to_owned()));
        }
        let keycode =
            preset.keycode(key).ok_or_else(|| HotkeyError::UnknownKey((*key).to_owned()))?;
        Ok(Hotkey { mods, keycode })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::keys::{KEYS, keycode};

    fn parse(text: &str) -> Result<Hotkey, HotkeyError> { Hotkey::parse(text, KeyPreset::Qwerty) }

    #[test]
    fn every_modifier_parses_alone_with_a_key() {
        for (name, mods) in [("cmd", Mods::CMD), ("alt", Mods::ALT), ("ctrl", Mods::CTRL), ("shift", Mods::SHIFT)] {
            assert_eq!(parse(&format!("{name}-h")), Ok(Hotkey { mods, keycode: 0x04 }), "{name}");
        }
    }

    #[test]
    fn a_bare_key_has_no_modifiers() {
        assert_eq!(parse("esc"), Ok(Hotkey { mods: Mods::NONE, keycode: 0x35 }));
        assert!(Mods::NONE.is_empty());
    }

    #[test]
    fn every_key_name_parses_bare_and_with_a_modifier() {
        for &(name, code) in KEYS {
            assert_eq!(parse(name), Ok(Hotkey { mods: Mods::NONE, keycode: code }), "{name}");
            assert_eq!(parse(&format!("alt-shift-{name}")), Ok(Hotkey { mods: Mods::ALT | Mods::SHIFT, keycode: code }), "{name}");
        }
    }

    #[test]
    fn modifier_order_does_not_matter() {
        let all = Mods::CMD | Mods::ALT | Mods::CTRL | Mods::SHIFT;
        let expected = Hotkey { mods: all, keycode: keycode("space").unwrap() };
        assert_eq!(parse("cmd-ctrl-alt-shift-space"), Ok(expected));
        assert_eq!(parse("shift-alt-ctrl-cmd-space"), Ok(expected));
        assert_eq!(parse("alt-cmd-shift-ctrl-space"), Ok(expected));
    }

    #[test]
    fn the_aerospace_example_chords() {
        assert_eq!(parse("alt-shift-semicolon").unwrap().keycode, 0x29);
        assert_eq!(parse("alt-slash").unwrap().keycode, 0x2C);
        assert_eq!(parse("alt-tab").unwrap(), Hotkey { mods: Mods::ALT, keycode: 0x30 });
    }

    #[test]
    fn rejects_empty_and_malformed_chords() {
        assert_eq!(parse(""), Err(HotkeyError::Empty));
        for bad in ["-", "alt-", "-h", "alt--h", "alt-shift-", "--"] {
            assert_eq!(parse(bad), Err(HotkeyError::EmptySegment), "{bad:?}");
        }
    }

    #[test]
    fn rejects_unknown_and_duplicate_modifiers() {
        assert_eq!(parse("meta-h"), Err(HotkeyError::UnknownModifier("meta".into())));
        assert_eq!(parse("alt-opt-h"), Err(HotkeyError::UnknownModifier("opt".into())));
        assert_eq!(parse("Alt-h"), Err(HotkeyError::UnknownModifier("Alt".into())));
        assert_eq!(parse("alt-alt-h"), Err(HotkeyError::DuplicateModifier("alt".into())));
        assert_eq!(parse("shift-alt-shift-h"), Err(HotkeyError::DuplicateModifier("shift".into())));
    }

    #[test]
    fn rejects_modifier_only_chords() {
        assert_eq!(parse("alt"), Err(HotkeyError::NoKey("alt".into())));
        assert_eq!(parse("cmd-ctrl-alt-shift"), Err(HotkeyError::NoKey("shift".into())));
    }

    #[test]
    fn rejects_unknown_keys() {
        assert_eq!(parse("alt-H"), Err(HotkeyError::UnknownKey("H".into())));
        assert_eq!(parse("alt-return"), Err(HotkeyError::UnknownKey("return".into())));
        assert_eq!(parse("alt-f21"), Err(HotkeyError::UnknownKey("f21".into())));
        assert_eq!(parse("alt-shift+h"), Err(HotkeyError::UnknownKey("shift+h".into())));
    }

    #[test]
    fn the_preset_decides_which_physical_key_a_name_means() {
        let qwerty = Hotkey::parse("alt-s", KeyPreset::Qwerty).unwrap();
        let dvorak = Hotkey::parse("alt-s", KeyPreset::Dvorak).unwrap();
        assert_eq!(qwerty.keycode, 0x01);
        assert_eq!(dvorak.keycode, 0x29);
        assert_eq!(qwerty.mods, dvorak.mods);
    }

    #[test]
    fn cg_flags_map_to_the_four_modifiers_and_ignore_the_rest() {
        assert_eq!(Mods::from_cg_flags(0), Mods::NONE);
        assert_eq!(Mods::from_cg_flags(0x0002_0000), Mods::SHIFT);
        assert_eq!(Mods::from_cg_flags(0x0004_0000), Mods::CTRL);
        assert_eq!(Mods::from_cg_flags(0x0008_0000), Mods::ALT);
        assert_eq!(Mods::from_cg_flags(0x0010_0000), Mods::CMD);
        // caps lock (0x10000), numeric pad (0x200000), fn (0x800000), left-alt device bit (0x20).
        let noise = 0x0001_0000 | 0x0020_0000 | 0x0080_0000 | 0x20;
        assert_eq!(Mods::from_cg_flags(noise | 0x0008_0000), Mods::ALT);
        assert_eq!(Mods::from_cg_flags(noise), Mods::NONE);
    }
}
