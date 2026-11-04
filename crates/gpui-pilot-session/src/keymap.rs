//! Characters and GPUI keystrokes as evdev key chords, looked up in the session's xkb keymap.

use std::os::fd::OwnedFd;

use gpui_pilot_protocol::Modifier;
use xkbcommon::xkb;

use crate::SessionError;

/// One key press: hold `mods` in order, tap `key`. Codes are evdev (xkb keycode - 8).
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chord {
    pub key: u32,
    pub mods: Vec<u32>,
}

/// A stretch of text typed through the keymap, or pasted because the keymap lacks it.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Run {
    Keys(Vec<Chord>),
    Paste(String),
}

/// The xkb keymap the session types through.
pub struct Keymap {
    keymap: xkb::Keymap,
}

/// GPUI's names for keys that are not a single character, with their xkb keysym names.
const NAMED_KEYS: &[(&str, &str)] = &[
    ("enter", "Return"),
    ("escape", "Escape"),
    ("tab", "Tab"),
    ("backspace", "BackSpace"),
    ("delete", "Delete"),
    ("insert", "Insert"),
    ("space", "space"),
    ("up", "Up"),
    ("down", "Down"),
    ("left", "Left"),
    ("right", "Right"),
    ("home", "Home"),
    ("end", "End"),
    ("pageup", "Prior"),
    ("pagedown", "Next"),
];

/// xkb modifier names and the key that sets each one.
const MODIFIER_KEYS: &[(&str, &str)] = &[
    ("Shift", "Shift_L"),
    ("Control", "Control_L"),
    ("Mod1", "Alt_L"),
    ("Mod4", "Super_L"),
    ("Mod5", "ISO_Level3_Shift"),
    ("LevelThree", "ISO_Level3_Shift"),
];

/// Offset between xkb keycodes and evdev codes.
const EVDEV_OFFSET: u32 = 8;

impl Keymap {
    /// Compiles the keymap for `layout` and `variant` with rules `evdev` and model `pc105`.
    /// Returns `None` when xkeyboard-config has no such layout.
    pub fn from_names(layout: &str, variant: &str) -> Option<Keymap> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "evdev",
            "pc105",
            layout,
            variant,
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )?;
        Some(Keymap { keymap })
    }

    /// Compiles a text v1 keymap of `size` bytes from `fd`, as a compositor sends it.
    pub fn from_fd(fd: OwnedFd, size: usize) -> std::io::Result<Keymap> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        // SAFETY: `fd` is owned and mapped read-only and private for `size` bytes, as the
        // wl_keyboard keymap contract requires.
        let keymap = unsafe {
            xkb::Keymap::new_from_fd(
                &context,
                fd,
                size,
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
        }?
        .ok_or_else(|| std::io::Error::other("the keymap does not compile"))?;
        Ok(Keymap { keymap })
    }

    /// The chord that types `c`, or `UnsupportedCharacter` when no key level produces it.
    pub fn char(&self, c: char) -> Result<Chord, SessionError> {
        let sym = xkb::Keysym::from_char(c);
        let km = &self.keymap;
        // Scan keys from the lowest code and levels from the lowest, taking the first level
        // reachable with modifiers we can press; a level needing Lock or NumLock is skipped.
        for kc in km.min_keycode().raw()..=km.max_keycode().raw() {
            let key = xkb::Keycode::new(kc);
            for level in 0..km.num_levels_for_key(key, 0) {
                if !km.key_get_syms_by_level(key, 0, level).contains(&sym) {
                    continue;
                }
                let mut masks = [0; 16];
                let n = km.key_get_mods_for_level(key, 0, level, &mut masks);
                if let Some(mods) = masks[..n.min(16)].iter().find_map(|&m| self.mask_keys(m)) {
                    return Ok(Chord {
                        key: kc - EVDEV_OFFSET,
                        mods,
                    });
                }
            }
        }
        Err(SessionError::UnsupportedCharacter(c))
    }

    /// The chord for a GPUI keystroke such as `ctrl-shift-a`, `alt-f5` or `ctrl--`.
    pub fn keystroke(&self, stroke: &str) -> Result<Chord, SessionError> {
        let invalid = || SessionError::InvalidKeystroke(stroke.to_string());
        // A trailing `-` after a separator names the minus key, as GPUI parses it.
        let (modifiers, key) = if stroke == "-" {
            ("", "-")
        } else if let Some(head) = stroke.strip_suffix("--") {
            (head, "-")
        } else {
            match stroke.rsplit_once('-') {
                Some((head, key)) if !head.is_empty() => (head, key),
                Some(_) => return Err(invalid()),
                None => ("", stroke),
            }
        };
        let mut mods = Vec::new();
        if !modifiers.is_empty() {
            for name in modifiers.split('-') {
                let sym = match name {
                    "ctrl" => "Control_L",
                    "alt" => "Alt_L",
                    "shift" => "Shift_L",
                    "cmd" | "super" | "win" | "platform" => "Super_L",
                    _ => return Err(invalid()),
                };
                let code = self.lowest_keycode(sym).ok_or_else(invalid)?;
                if !mods.contains(&code) {
                    mods.push(code);
                }
            }
        }
        let chord = self.key(key).ok_or_else(invalid)?;
        // The character's own modifiers follow the explicit ones, each pressed once.
        for code in chord.mods {
            if !mods.contains(&code) {
                mods.push(code);
            }
        }
        Ok(Chord {
            key: chord.key,
            mods,
        })
    }

    /// The chords for space-separated keystrokes, or the first invalid one.
    pub fn sequence(&self, keys: &str) -> Result<Vec<Chord>, SessionError> {
        keys.split_whitespace()
            .map(|stroke| self.keystroke(stroke))
            .collect()
    }

    /// The chords that type `text`, checked in full before any is returned.
    pub fn text(&self, text: &str) -> Result<Vec<Chord>, SessionError> {
        text.chars().map(|c| self.char(c)).collect()
    }

    /// `text` as alternating runs; consecutive characters of one kind share a run.
    pub fn runs(&self, text: &str) -> Vec<Run> {
        let mut runs = Vec::new();
        for c in text.chars() {
            // Extend the last run when it is of this character's kind, else open a new one.
            match (self.char(c), runs.last_mut()) {
                (Ok(chord), Some(Run::Keys(chords))) => chords.push(chord),
                (Ok(chord), _) => runs.push(Run::Keys(vec![chord])),
                (Err(_), Some(Run::Paste(pasted))) => pasted.push(c),
                (Err(_), _) => runs.push(Run::Paste(c.to_string())),
            }
        }
        runs
    }

    /// A keystroke's key part: a GPUI named key, `f1` to `f24`, or a single character.
    fn key(&self, name: &str) -> Option<Chord> {
        let named = NAMED_KEYS
            .iter()
            .find(|(gpui, _)| *gpui == name)
            .map(|(_, sym)| sym.to_string());
        let function = name
            .strip_prefix('f')
            .and_then(|n| n.parse::<u8>().ok())
            .filter(|n| (1..=24).contains(n) && name == format!("f{n}"))
            .map(|n| format!("F{n}"));
        if let Some(sym) = named.or(function) {
            let key = self.lowest_keycode(&sym)?;
            return Some(Chord {
                key,
                mods: Vec::new(),
            });
        }
        let mut chars = name.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => self.char(c).ok(),
            _ => None,
        }
    }

    /// The modifier keys that set every bit of `mask`, or `None` if a bit has no such key.
    fn mask_keys(&self, mask: xkb::ModMask) -> Option<Vec<u32>> {
        let mut keys = Vec::new();
        for bit in 0..xkb::ModMask::BITS {
            if mask & (1 << bit) == 0 {
                continue;
            }
            if bit >= self.keymap.num_mods() {
                return None;
            }
            let name = self.keymap.mod_get_name(bit);
            let (_, sym) = MODIFIER_KEYS
                .iter()
                .find(|(mod_name, _)| *mod_name == name)?;
            keys.push(self.lowest_keycode(sym)?);
        }
        Some(keys)
    }

    /// The evdev code of `modifier`'s left key, the one `keystroke` presses for it; `None` when
    /// the keymap has no such key.
    pub(crate) fn modifier(&self, modifier: Modifier) -> Option<u32> {
        let sym = match modifier {
            Modifier::Ctrl => "Control_L",
            Modifier::Alt => "Alt_L",
            Modifier::Shift => "Shift_L",
            Modifier::Super => "Super_L",
            _ => return None,
        };
        self.lowest_keycode(sym)
    }

    /// The lowest evdev code whose level 0 produces the keysym named `name`.
    fn lowest_keycode(&self, name: &str) -> Option<u32> {
        let sym = xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS);
        if sym.raw() == 0 {
            return None;
        }
        let km = &self.keymap;
        (km.min_keycode().raw()..=km.max_keycode().raw())
            .find(|&kc| {
                km.key_get_syms_by_level(xkb::Keycode::new(kc), 0, 0)
                    .contains(&sym)
            })
            .map(|kc| kc - EVDEV_OFFSET)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionError;

    fn keymap() -> Keymap {
        Keymap::from_names("us", "altgr-intl").expect("xkeyboard-config has us(altgr-intl)")
    }

    fn chord(key: u32, mods: &[u32]) -> Chord {
        Chord {
            key,
            mods: mods.to_vec(),
        }
    }

    #[test]
    fn ascii_needs_at_most_shift() {
        let k = keymap();
        assert_eq!(k.char('a').unwrap(), chord(30, &[]));
        assert_eq!(k.char('A').unwrap(), chord(30, &[42]));
        assert_eq!(k.char('!').unwrap(), chord(2, &[42]));
        assert_eq!(k.char(' ').unwrap(), chord(57, &[]));
        assert_eq!(
            k.char('\'').unwrap(),
            chord(40, &[]),
            "altgr-intl keeps the quote plain"
        );
    }

    #[test]
    fn accented_letters_go_through_altgr() {
        let e = keymap().char('é').unwrap();
        assert_eq!(e.key, 18);
        assert!(matches!(e.mods.as_slice(), [84] | [100]), "{e:?}");
    }

    #[test]
    fn a_character_outside_the_keymap_is_unsupported() {
        assert!(matches!(
            keymap().char('漢'),
            Err(SessionError::UnsupportedCharacter('漢'))
        ));
        let error = keymap().text("ok漢").unwrap_err();
        assert!(
            matches!(error, SessionError::UnsupportedCharacter('漢')),
            "{error}"
        );
        assert_eq!(
            keymap().text("Hi").unwrap(),
            vec![chord(35, &[42]), chord(23, &[])]
        );
    }

    #[test]
    fn text_splits_into_typed_and_pasted_runs() {
        let k = keymap();
        assert_eq!(k.runs(""), Vec::<Run>::new());
        assert_eq!(
            k.runs("Hi"),
            vec![Run::Keys(vec![chord(35, &[42]), chord(23, &[])])]
        );
        assert_eq!(
            k.runs("a漢字b"),
            vec![
                Run::Keys(vec![chord(30, &[])]),
                Run::Paste("漢字".into()),
                Run::Keys(vec![chord(48, &[])]),
            ]
        );
        assert_eq!(k.runs("✓"), vec![Run::Paste("✓".into())]);
    }

    #[test]
    fn gpui_keystrokes_map_to_chords() {
        let k = keymap();
        assert_eq!(k.keystroke("escape").unwrap(), chord(1, &[]));
        assert_eq!(k.keystroke("enter").unwrap(), chord(28, &[]));
        assert_eq!(k.keystroke("backspace").unwrap(), chord(14, &[]));
        assert_eq!(k.keystroke("ctrl-s").unwrap(), chord(31, &[29]));
        assert_eq!(k.keystroke("shift-tab").unwrap(), chord(15, &[42]));
        assert_eq!(k.keystroke("ctrl-shift-a").unwrap(), chord(30, &[29, 42]));
        assert_eq!(k.keystroke("alt-f5").unwrap(), chord(63, &[56]));
        assert_eq!(k.keystroke("cmd-up").unwrap(), chord(103, &[125]));
        assert_eq!(k.keystroke("pageup").unwrap(), chord(104, &[]));
        assert_eq!(
            k.keystroke("shift-!").unwrap(),
            chord(2, &[42]),
            "no duplicate shift"
        );
        assert_eq!(k.keystroke("-").unwrap(), chord(12, &[]));
        assert_eq!(k.keystroke("ctrl--").unwrap(), chord(12, &[29]));
    }

    #[test]
    fn modifiers_press_the_keys_keystroke_uses() {
        let k = keymap();
        for (modifier, stroke) in [
            (Modifier::Ctrl, "ctrl-a"),
            (Modifier::Alt, "alt-a"),
            (Modifier::Shift, "shift-a"),
            (Modifier::Super, "super-a"),
        ] {
            assert_eq!(
                k.modifier(modifier),
                Some(k.keystroke(stroke).unwrap().mods[0]),
                "{modifier:?}"
            );
        }
    }

    #[test]
    fn bad_keystrokes_are_invalid() {
        let k = keymap();
        for bad in ["", "ctrl-", "bogus", "hyper-a", "fn-a", "ctrl--x"] {
            assert!(
                matches!(k.keystroke(bad), Err(SessionError::InvalidKeystroke(ref s)) if s == bad),
                "{bad:?}"
            );
        }
        assert_eq!(
            k.sequence("ctrl-a backspace").unwrap(),
            vec![chord(30, &[29]), chord(14, &[])]
        );
        assert!(
            matches!(k.sequence("ctrl-a nope"), Err(SessionError::InvalidKeystroke(ref s)) if s == "nope")
        );
    }
}
