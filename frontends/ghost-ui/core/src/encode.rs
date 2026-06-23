//! Pure key -> terminal-bytes encoder (legacy / xterm-default scheme).
//!
//! Given a logical key + modifier state (and the terminal's cursor-key mode),
//! produce the bytes ghost sends down the PTY. The scheme is classic xterm
//! "legacy": printable text verbatim, C0 control bytes for Ctrl+letter, an ESC
//! prefix for Alt (`metaSendsEscape`), and CSI/SS3 sequences for navigation/
//! function keys with the usual `1;<mod>` modifier parameter. DECCKM
//! (cursor-key application mode) switches unmodified cursor keys from CSI to
//! SS3, matching what apps like vim expect.

use crate::input::{Key, Mods, NamedKey};

/// Encode a *pressed* key into the bytes a terminal would transmit, or `None`
/// when the key produces nothing on its own: modifiers in isolation, dead keys
/// (left to IME), and unidentified keys. `app_cursor` is the terminal's DECCKM
/// state (`Vt::cursor_key_app_mode`); `modify_other_keys` is the xterm
/// modifyOtherKeys level (`Vt::modify_other_keys`, 0 = off).
pub fn encode(key: &Key, mods: Mods, app_cursor: bool, modify_other_keys: u8) -> Option<Vec<u8>> {
    // When the app negotiated modifyOtherKeys, that scheme wins for the keys it
    // covers; everything else falls through to the legacy encoder below.
    if let Some(bytes) = modify_other_keys_encode(key, mods, modify_other_keys) {
        return Some(bytes);
    }
    match key {
        Key::Named(named) => encode_named(*named, mods, app_cursor),
        Key::Char(s) => Some(encode_char(s, mods.ctrl, mods.alt)),
        // Key::Dead / Key::Unidentified: nothing on their own.
        _ => None,
    }
}

/// modifyOtherKeys (xterm XTMODKEYS resource 4): when `level` is non-zero, keys
/// carrying a *qualifying* modifier (Ctrl/Alt/Super — a lone Shift never
/// qualifies) are reported as `CSI 27 ; <mod> ; <code> ~` so apps can tell apart
/// combos the legacy scheme collapses (Ctrl+I vs Tab, Ctrl+M vs Enter, …).
/// `<code>` is the key's *unshifted* codepoint; case/shift lives in the modifier
/// parameter. Level 1 reports only keys with no unambiguous legacy byte; level 2
/// also reports the well-known C0 keys (Ctrl+letter, Tab, Enter, Esc, Backspace,
/// Space). Returns `None` to fall through to the legacy encoder.
fn modify_other_keys_encode(key: &Key, mods: Mods, level: u8) -> Option<Vec<u8>> {
    if level == 0 || !(mods.ctrl || mods.alt || mods.sup) {
        return None;
    }
    let (code, well_known) = match key {
        Key::Char(s) => {
            let mut chars = s.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None; // composed / multi-char input is not a single key
            }
            // The well-known C0 cases xterm leaves on their legacy byte at
            // level 1: Ctrl+letter (^A..^Z) and Ctrl+Space (NUL). Shift doesn't
            // change this (Ctrl+R and Ctrl+Shift+R collapse to the same byte at
            // level 1; only level 2 disambiguates them). Alt/Super, however,
            // have no unambiguous legacy byte, so they're reported even at
            // level 1.
            let well_known =
                mods.ctrl && !mods.alt && !mods.sup && (c.is_ascii_alphabetic() || c == ' ');
            (u32::from(c.to_ascii_lowercase()), well_known)
        }
        Key::Named(named) => {
            let code = match named {
                NamedKey::Enter => 13,
                NamedKey::Tab => 9,
                NamedKey::Escape => 27,
                NamedKey::Backspace => 127,
                NamedKey::Space => 32,
                // Navigation / function keys carry their modifier in their own
                // CSI/SS3 form; they are not part of modifyOtherKeys.
                _ => return None,
            };
            (code, true) // the C0 named keys are all "well-known"
        }
        _ => return None,
    };
    // Level 1 leaves the well-known keys on their legacy bytes; level 2 reports
    // them too. (Shift+Tab never reaches here — a lone Shift isn't qualifying —
    // so it keeps its dedicated back-tab CSI Z.)
    if level == 1 && well_known {
        return None;
    }
    Some(format!("\x1b[27;{m};{code}~", m = modifier_param(mods)).into_bytes())
}

/// Printable text. Ctrl maps an ASCII char to its C0 control byte; Alt prefixes
/// the result with ESC. Everything else is sent as its UTF-8 bytes.
fn encode_char(s: &str, ctrl: bool, alt: bool) -> Vec<u8> {
    let mut out = Vec::new();
    // Ctrl with a non-mappable key falls through and emits the text as-is.
    if ctrl && let Some(b) = ctrl_byte(s) {
        if alt {
            out.push(0x1b);
        }
        out.push(b);
        return out;
    }
    if alt {
        out.push(0x1b);
    }
    out.extend_from_slice(s.as_bytes());
    out
}

/// The C0 control byte for a single ASCII character held with Ctrl, if any.
fn ctrl_byte(s: &str) -> Option<u8> {
    let mut chars = s.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None; // more than one char: not a control combo
    }
    let b = match c.to_ascii_lowercase() {
        l @ 'a'..='z' => (l as u8) - b'a' + 1, // ^A=0x01 .. ^Z=0x1a
        ' ' | '@' => 0x00,                     // Ctrl+Space / Ctrl+@ -> NUL
        '[' => 0x1b,
        '\\' => 0x1c,
        ']' => 0x1d,
        '^' => 0x1e,
        '_' => 0x1f,
        '?' => 0x7f,
        _ => return None,
    };
    Some(b)
}

/// Encode the named (non-text) keys: the C0 keys plus the CSI/SS3 family.
fn encode_named(key: NamedKey, mods: Mods, app_cursor: bool) -> Option<Vec<u8>> {
    use NamedKey::*;

    // The spacebar may arrive as a named key on some platforms; route it
    // through the text encoder so Ctrl+Space -> NUL and Alt+Space -> ESC SP.
    if key == Space {
        return Some(encode_char(" ", mods.ctrl, mods.alt));
    }

    // Plain C0 keys. Alt still prefixes ESC (e.g. Alt+Enter).
    let simple: Option<&[u8]> = match key {
        Enter => Some(b"\r"),
        Tab if mods.shift => Some(b"\x1b[Z"),
        Tab => Some(b"\t"),
        Backspace => Some(b"\x7f"),
        Escape => Some(b"\x1b"),
        _ => None,
    };
    if let Some(bytes) = simple {
        let mut out = Vec::new();
        if mods.alt {
            out.push(0x1b);
        }
        out.extend_from_slice(bytes);
        return Some(out);
    }

    Some(encode_csi(named_csi(key)?, mods, app_cursor))
}

/// The CSI/SS3 shape of a navigation or function key.
enum Csi {
    /// `ESC [ <letter>` form (cursor keys, Home/End) — `ESC O <letter>` in
    /// cursor-key application mode when unmodified.
    Letter(u8),
    /// `ESC [ <num> ~` form (Insert/Delete/PageUp/PageDown, F5+).
    Tilde(u32),
    /// `ESC O <letter>` SS3 form when unmodified (F1-F4).
    Ss3(u8),
}

fn named_csi(key: NamedKey) -> Option<Csi> {
    use NamedKey::*;
    Some(match key {
        ArrowUp => Csi::Letter(b'A'),
        ArrowDown => Csi::Letter(b'B'),
        ArrowRight => Csi::Letter(b'C'),
        ArrowLeft => Csi::Letter(b'D'),
        Home => Csi::Letter(b'H'),
        End => Csi::Letter(b'F'),
        Insert => Csi::Tilde(2),
        Delete => Csi::Tilde(3),
        PageUp => Csi::Tilde(5),
        PageDown => Csi::Tilde(6),
        F1 => Csi::Ss3(b'P'),
        F2 => Csi::Ss3(b'Q'),
        F3 => Csi::Ss3(b'R'),
        F4 => Csi::Ss3(b'S'),
        F5 => Csi::Tilde(15),
        F6 => Csi::Tilde(17),
        F7 => Csi::Tilde(18),
        F8 => Csi::Tilde(19),
        F9 => Csi::Tilde(20),
        F10 => Csi::Tilde(21),
        F11 => Csi::Tilde(23),
        F12 => Csi::Tilde(24),
        _ => return None,
    })
}

/// The xterm modifier parameter: 1 + Shift(1) + Alt(2) + Ctrl(4) + Super(8).
fn modifier_param(mods: Mods) -> u32 {
    1 + (mods.shift as u32) + 2 * (mods.alt as u32) + 4 * (mods.ctrl as u32) + 8 * (mods.sup as u32)
}

fn encode_csi(csi: Csi, mods: Mods, app_cursor: bool) -> Vec<u8> {
    let m = modifier_param(mods);
    let modified = m != 1;
    match csi {
        Csi::Letter(f) => {
            let mut out = if modified {
                format!("\x1b[1;{m}").into_bytes()
            } else if app_cursor {
                // DECCKM: unmodified cursor keys use SS3 in application mode.
                b"\x1bO".to_vec()
            } else {
                b"\x1b[".to_vec()
            };
            out.push(f);
            out
        }
        Csi::Tilde(n) => if modified {
            format!("\x1b[{n};{m}~")
        } else {
            format!("\x1b[{n}~")
        }
        .into_bytes(),
        Csi::Ss3(f) => {
            // Modified F1-F4 switch from SS3 to the CSI `1;<mod>` form.
            let mut out = if modified {
                format!("\x1b[1;{m}").into_bytes()
            } else {
                b"\x1bO".to_vec()
            };
            out.push(f);
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{Key, Mods, NamedKey};

    fn ch(s: &str) -> Key {
        Key::Char(s.into())
    }
    fn named(k: NamedKey) -> Key {
        Key::Named(k)
    }
    fn none() -> Mods {
        Mods::NONE
    }

    #[test]
    fn plain_text_is_verbatim() {
        assert_eq!(encode(&ch("a"), none(), false, 0), Some(b"a".to_vec()));
        assert_eq!(encode(&ch("A"), Mods::SHIFT, false, 0), Some(b"A".to_vec()));
    }

    #[test]
    fn accented_char_is_utf8() {
        assert_eq!(
            encode(&ch("à"), none(), false, 0),
            Some("à".as_bytes().to_vec())
        );
    }

    #[test]
    fn ctrl_letters_map_to_c0() {
        assert_eq!(encode(&ch("a"), Mods::CTRL, false, 0), Some(vec![0x01]));
        assert_eq!(encode(&ch("c"), Mods::CTRL, false, 0), Some(vec![0x03]));
        assert_eq!(
            encode(&ch("C"), Mods::CTRL | Mods::SHIFT, false, 0),
            Some(vec![0x03])
        );
    }

    #[test]
    fn ctrl_symbols() {
        assert_eq!(encode(&ch("["), Mods::CTRL, false, 0), Some(vec![0x1b]));
        assert_eq!(encode(&ch("\\"), Mods::CTRL, false, 0), Some(vec![0x1c]));
        assert_eq!(encode(&ch("_"), Mods::CTRL, false, 0), Some(vec![0x1f]));
    }

    #[test]
    fn ctrl_space_is_nul_either_shape() {
        assert_eq!(encode(&ch(" "), Mods::CTRL, false, 0), Some(vec![0x00]));
        assert_eq!(
            encode(&named(NamedKey::Space), Mods::CTRL, false, 0),
            Some(vec![0x00])
        );
    }

    #[test]
    fn alt_prefixes_with_escape() {
        assert_eq!(
            encode(&ch("a"), Mods::ALT, false, 0),
            Some(vec![0x1b, b'a'])
        );
        assert_eq!(
            encode(&ch("a"), Mods::ALT | Mods::CTRL, false, 0),
            Some(vec![0x1b, 0x01])
        );
    }

    #[test]
    fn c0_named_keys() {
        assert_eq!(
            encode(&named(NamedKey::Enter), none(), false, 0),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::Tab), none(), false, 0),
            Some(b"\t".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::Tab), Mods::SHIFT, false, 0),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::Backspace), none(), false, 0),
            Some(vec![0x7f])
        );
        assert_eq!(
            encode(&named(NamedKey::Escape), none(), false, 0),
            Some(vec![0x1b])
        );
        assert_eq!(
            encode(&named(NamedKey::Enter), Mods::ALT, false, 0),
            Some(vec![0x1b, b'\r'])
        );
    }

    #[test]
    fn arrows_plain_and_modified() {
        assert_eq!(
            encode(&named(NamedKey::ArrowUp), none(), false, 0),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::ArrowRight), Mods::CTRL, false, 0),
            Some(b"\x1b[1;5C".to_vec())
        );
        assert_eq!(
            encode(
                &named(NamedKey::ArrowLeft),
                Mods::SHIFT | Mods::ALT,
                false,
                0
            ),
            Some(b"\x1b[1;4D".to_vec())
        );
    }

    #[test]
    fn arrows_use_ss3_in_application_cursor_mode() {
        // DECCKM on: unmodified cursor keys switch CSI -> SS3.
        assert_eq!(
            encode(&named(NamedKey::ArrowUp), none(), true, 0),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::Home), none(), true, 0),
            Some(b"\x1bOH".to_vec())
        );
        // ...but a modifier forces the CSI `1;<mod>` form even in app mode.
        assert_eq!(
            encode(&named(NamedKey::ArrowUp), Mods::CTRL, true, 0),
            Some(b"\x1b[1;5A".to_vec())
        );
    }

    #[test]
    fn function_keys() {
        assert_eq!(
            encode(&named(NamedKey::F1), none(), false, 0),
            Some(b"\x1bOP".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::F1), Mods::CTRL, false, 0),
            Some(b"\x1b[1;5P".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::F5), none(), false, 0),
            Some(b"\x1b[15~".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::F12), none(), false, 0),
            Some(b"\x1b[24~".to_vec())
        );
    }

    #[test]
    fn nav_tilde_keys() {
        assert_eq!(
            encode(&named(NamedKey::Delete), none(), false, 0),
            Some(b"\x1b[3~".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::PageUp), none(), false, 0),
            Some(b"\x1b[5~".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::Home), none(), false, 0),
            Some(b"\x1b[H".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::End), Mods::CTRL, false, 0),
            Some(b"\x1b[1;5F".to_vec())
        );
    }

    #[test]
    fn dead_and_unidentified_yield_nothing() {
        assert_eq!(encode(&Key::Dead, none(), false, 0), None);
        assert_eq!(encode(&Key::Unidentified, none(), false, 0), None);
    }

    #[test]
    fn modify_other_keys_off_is_legacy() {
        // Level 0 is exactly the legacy encoder.
        assert_eq!(encode(&ch("i"), Mods::CTRL, false, 0), Some(vec![0x09]));
        assert_eq!(
            encode(&named(NamedKey::Enter), Mods::CTRL, false, 0),
            Some(b"\r".to_vec())
        );
    }

    #[test]
    fn modify_other_keys_level_2_disambiguates_ctrl_and_c0_keys() {
        // Ctrl+I (a Char "i") reports as CSI 27;5;105~ instead of 0x09, so the
        // app can tell it apart from a real Tab.
        assert_eq!(
            encode(&ch("i"), Mods::CTRL, false, 2),
            Some(b"\x1b[27;5;105~".to_vec())
        );
        // The well-known named C0 keys, when modified, report at level 2 too.
        assert_eq!(
            encode(&named(NamedKey::Tab), Mods::CTRL, false, 2),
            Some(b"\x1b[27;5;9~".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::Enter), Mods::CTRL, false, 2),
            Some(b"\x1b[27;5;13~".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::Escape), Mods::CTRL, false, 2),
            Some(b"\x1b[27;5;27~".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::Backspace), Mods::CTRL, false, 2),
            Some(b"\x1b[27;5;127~".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::Space), Mods::CTRL, false, 2),
            Some(b"\x1b[27;5;32~".to_vec())
        );
        // Shift lives in the modifier; the code stays the unshifted lowercase
        // letter. And Alt+letter is disambiguated from ESC-then-letter.
        assert_eq!(
            encode(&ch("I"), Mods::CTRL | Mods::SHIFT, false, 2),
            Some(b"\x1b[27;6;105~".to_vec())
        );
        assert_eq!(
            encode(&ch("a"), Mods::ALT, false, 2),
            Some(b"\x1b[27;3;97~".to_vec())
        );
    }

    #[test]
    fn modify_other_keys_leaves_plain_text_and_lone_shift_alone() {
        // No qualifying modifier: plain and shifted text stay verbatim at level 2.
        assert_eq!(encode(&ch("a"), none(), false, 2), Some(b"a".to_vec()));
        assert_eq!(encode(&ch("A"), Mods::SHIFT, false, 2), Some(b"A".to_vec()));
        // A lone Shift never qualifies, so Shift+Tab keeps its back-tab (CSI Z).
        assert_eq!(
            encode(&named(NamedKey::Tab), Mods::SHIFT, false, 2),
            Some(b"\x1b[Z".to_vec())
        );
        // Navigation keys keep their own `1;<mod>` form, not modifyOtherKeys.
        assert_eq!(
            encode(&named(NamedKey::ArrowUp), Mods::CTRL, false, 2),
            Some(b"\x1b[1;5A".to_vec())
        );
    }

    #[test]
    fn modify_other_keys_level_1_spares_well_known_keys() {
        // Level 1 leaves the well-known C0 keys on their legacy bytes — including
        // Ctrl+Shift+letter, which collapses to the same byte as Ctrl+letter
        // (xterm only disambiguates it at level 2).
        assert_eq!(encode(&ch("i"), Mods::CTRL, false, 1), Some(vec![0x09]));
        assert_eq!(
            encode(&ch("I"), Mods::CTRL | Mods::SHIFT, false, 1),
            Some(vec![0x09])
        );
        assert_eq!(
            encode(&named(NamedKey::Tab), Mods::CTRL, false, 1),
            Some(b"\t".to_vec())
        );
        // …but it still disambiguates keys with no clean legacy byte (Alt+letter
        // would otherwise be indistinguishable from ESC-then-letter).
        assert_eq!(
            encode(&ch("a"), Mods::ALT, false, 1),
            Some(b"\x1b[27;3;97~".to_vec())
        );
    }
}
