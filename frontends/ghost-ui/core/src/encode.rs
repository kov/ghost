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
/// state (`Vt::cursor_key_app_mode`).
pub fn encode(key: &Key, mods: Mods, app_cursor: bool) -> Option<Vec<u8>> {
    match key {
        Key::Named(named) => encode_named(*named, mods, app_cursor),
        Key::Char(s) => Some(encode_char(s, mods.ctrl, mods.alt)),
        // Key::Dead / Key::Unidentified: nothing on their own.
        _ => None,
    }
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
        assert_eq!(encode(&ch("a"), none(), false), Some(b"a".to_vec()));
        assert_eq!(encode(&ch("A"), Mods::SHIFT, false), Some(b"A".to_vec()));
    }

    #[test]
    fn accented_char_is_utf8() {
        assert_eq!(
            encode(&ch("à"), none(), false),
            Some("à".as_bytes().to_vec())
        );
    }

    #[test]
    fn ctrl_letters_map_to_c0() {
        assert_eq!(encode(&ch("a"), Mods::CTRL, false), Some(vec![0x01]));
        assert_eq!(encode(&ch("c"), Mods::CTRL, false), Some(vec![0x03]));
        assert_eq!(
            encode(&ch("C"), Mods::CTRL | Mods::SHIFT, false),
            Some(vec![0x03])
        );
    }

    #[test]
    fn ctrl_symbols() {
        assert_eq!(encode(&ch("["), Mods::CTRL, false), Some(vec![0x1b]));
        assert_eq!(encode(&ch("\\"), Mods::CTRL, false), Some(vec![0x1c]));
        assert_eq!(encode(&ch("_"), Mods::CTRL, false), Some(vec![0x1f]));
    }

    #[test]
    fn ctrl_space_is_nul_either_shape() {
        assert_eq!(encode(&ch(" "), Mods::CTRL, false), Some(vec![0x00]));
        assert_eq!(
            encode(&named(NamedKey::Space), Mods::CTRL, false),
            Some(vec![0x00])
        );
    }

    #[test]
    fn alt_prefixes_with_escape() {
        assert_eq!(encode(&ch("a"), Mods::ALT, false), Some(vec![0x1b, b'a']));
        assert_eq!(
            encode(&ch("a"), Mods::ALT | Mods::CTRL, false),
            Some(vec![0x1b, 0x01])
        );
    }

    #[test]
    fn c0_named_keys() {
        assert_eq!(
            encode(&named(NamedKey::Enter), none(), false),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::Tab), none(), false),
            Some(b"\t".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::Tab), Mods::SHIFT, false),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::Backspace), none(), false),
            Some(vec![0x7f])
        );
        assert_eq!(
            encode(&named(NamedKey::Escape), none(), false),
            Some(vec![0x1b])
        );
        assert_eq!(
            encode(&named(NamedKey::Enter), Mods::ALT, false),
            Some(vec![0x1b, b'\r'])
        );
    }

    #[test]
    fn arrows_plain_and_modified() {
        assert_eq!(
            encode(&named(NamedKey::ArrowUp), none(), false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::ArrowRight), Mods::CTRL, false),
            Some(b"\x1b[1;5C".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::ArrowLeft), Mods::SHIFT | Mods::ALT, false),
            Some(b"\x1b[1;4D".to_vec())
        );
    }

    #[test]
    fn arrows_use_ss3_in_application_cursor_mode() {
        // DECCKM on: unmodified cursor keys switch CSI -> SS3.
        assert_eq!(
            encode(&named(NamedKey::ArrowUp), none(), true),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::Home), none(), true),
            Some(b"\x1bOH".to_vec())
        );
        // ...but a modifier forces the CSI `1;<mod>` form even in app mode.
        assert_eq!(
            encode(&named(NamedKey::ArrowUp), Mods::CTRL, true),
            Some(b"\x1b[1;5A".to_vec())
        );
    }

    #[test]
    fn function_keys() {
        assert_eq!(
            encode(&named(NamedKey::F1), none(), false),
            Some(b"\x1bOP".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::F1), Mods::CTRL, false),
            Some(b"\x1b[1;5P".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::F5), none(), false),
            Some(b"\x1b[15~".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::F12), none(), false),
            Some(b"\x1b[24~".to_vec())
        );
    }

    #[test]
    fn nav_tilde_keys() {
        assert_eq!(
            encode(&named(NamedKey::Delete), none(), false),
            Some(b"\x1b[3~".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::PageUp), none(), false),
            Some(b"\x1b[5~".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::Home), none(), false),
            Some(b"\x1b[H".to_vec())
        );
        assert_eq!(
            encode(&named(NamedKey::End), Mods::CTRL, false),
            Some(b"\x1b[1;5F".to_vec())
        );
    }

    #[test]
    fn dead_and_unidentified_yield_nothing() {
        assert_eq!(encode(&Key::Dead, none(), false), None);
        assert_eq!(encode(&Key::Unidentified, none(), false), None);
    }
}
