//! Pure key -> terminal-bytes encoder (legacy / xterm-default scheme).
//!
//! Given a logical key + modifier state (and the terminal's cursor-key mode),
//! produce the bytes ghost sends down the PTY. The scheme is classic xterm
//! "legacy": printable text verbatim, C0 control bytes for Ctrl+letter, an ESC
//! prefix for Alt (`metaSendsEscape`), and CSI/SS3 sequences for navigation/
//! function keys with the usual `1;<mod>` modifier parameter. DECCKM
//! (cursor-key application mode) switches unmodified cursor keys from CSI to
//! SS3, matching what apps like vim expect.

use crate::input::{Key, KeyEventKind, Mods, NamedKey};

/// kitty keyboard progressive-enhancement flag bits (`Vt::kitty_keyboard_flags`).
const KITTY_DISAMBIGUATE: u8 = 0b00001;
const KITTY_REPORT_EVENT_TYPES: u8 = 0b00010;
const KITTY_REPORT_ALL_KEYS: u8 = 0b01000;
/// Flags whose presence routes a key through the kitty `CSI u` encoder rather
/// than the legacy / modifyOtherKeys schemes. (4 = report-alternate-keys and
/// 16 = report-associated-text are sub-field modifiers, not base modes.)
const KITTY_BASE_FLAGS: u8 = KITTY_DISAMBIGUATE | KITTY_REPORT_EVENT_TYPES | KITTY_REPORT_ALL_KEYS;

/// Encode a *pressed* key into the bytes a terminal would transmit, or `None`
/// when the key produces nothing on its own: modifiers in isolation, dead keys
/// (left to IME), and unidentified keys. `app_cursor` is the terminal's DECCKM
/// state (`Vt::cursor_key_app_mode`); `modify_other_keys` is the xterm
/// modifyOtherKeys level (`Vt::modify_other_keys`, 0 = off); `kitty_flags` is the
/// kitty keyboard protocol's active flags (`Vt::kitty_keyboard_flags`, 0 = off),
/// which supersede both of the older schemes when set.
pub fn encode(
    key: &Key,
    mods: Mods,
    app_cursor: bool,
    modify_other_keys: u8,
    kitty_flags: u8,
    kind: KeyEventKind,
) -> Option<Vec<u8>> {
    // The kitty keyboard protocol, when negotiated, replaces both older schemes
    // for every key (it produces the legacy bytes itself for the keys it leaves
    // alone, e.g. plain text and bare Enter/Tab/Backspace).
    if kitty_flags & KITTY_BASE_FLAGS != 0 {
        return kitty_encode(key, mods, kitty_flags, kind);
    }
    // Repeats fold into a press for the legacy schemes (auto-repeat re-sends the
    // byte); releases produce nothing.
    if matches!(kind, KeyEventKind::Release) {
        return None;
    }
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

/// kitty keyboard protocol encoder (the `CSI u` / progressive-enhancement
/// scheme). Implements the *disambiguate* flag (1) — and the key-selection of
/// *report-all-keys* (8) — in their base form: `CSI code[;mods] u` for text /
/// control keys, the legacy-shaped `CSI [1;mods]<letter>` and `CSI num[;mods]~`
/// forms for navigation / function keys. The event-type, alternate-key and
/// associated-text sub-fields (flags 2/4/16) are layered on in later steps.
///
/// Returns the bytes for the key (including the plain legacy byte for keys the
/// active flags leave alone) or `None` for keys that emit nothing (dead /
/// unidentified / unknown).
fn kitty_encode(key: &Key, mods: Mods, flags: u8, kind: KeyEventKind) -> Option<Vec<u8>> {
    let force_all = flags & KITTY_REPORT_ALL_KEYS != 0;
    // A modifier other than a lone Shift makes a text key "not text" (and so
    // disambiguated); Shift alone keeps producing its shifted glyph.
    let nonshift_mod = mods.ctrl || mods.alt || mods.sup;
    // The event-type sub-field for keys emitted as escape codes: present only
    // when report-event-types (flag 2) is on and the event is a repeat/release.
    // When it is absent, a repeat folds into a press and a release is suppressed.
    let event = event_subfield(flags, kind);
    match key {
        Key::Char(s) => {
            if (force_all || nonshift_mod)
                && let Some(code) = base_codepoint(s)
            {
                csi_u_or_suppressed(code, mods, event, kind)
            } else if matches!(kind, KeyEventKind::Release) {
                // A text key has no escape-code form here, so its release is not
                // reported (only flag 8 promotes text keys to escape codes).
                None
            } else {
                // Plain / shifted / composed text stays verbatim (legacy).
                Some(s.as_bytes().to_vec())
            }
        }
        Key::Named(named) => kitty_encode_named(*named, mods, force_all, nonshift_mod, event, kind),
        // Key::Dead / Key::Unidentified: nothing on their own.
        _ => None,
    }
}

/// The `:event-type` value for a key reported as an escape code: `Some(2)` for a
/// repeat, `Some(3)` for a release, `None` for a press or when event reporting is
/// off. A `None` on a repeat means "encode as a plain press" (auto-repeat).
fn event_subfield(flags: u8, kind: KeyEventKind) -> Option<u8> {
    if flags & KITTY_REPORT_EVENT_TYPES == 0 {
        return None;
    }
    match kind {
        KeyEventKind::Press => None,
        KeyEventKind::Repeat => Some(2),
        KeyEventKind::Release => Some(3),
    }
}

/// Emit the `CSI u` form, unless this is a release that isn't being reported
/// (releases require the event-types flag), in which case nothing is sent.
fn csi_u_or_suppressed(
    code: u32,
    mods: Mods,
    event: Option<u8>,
    kind: KeyEventKind,
) -> Option<Vec<u8>> {
    if matches!(kind, KeyEventKind::Release) && event.is_none() {
        None
    } else {
        Some(csi_u(code, mods, event))
    }
}

/// The kitty unicode-key-code for printable text: the key's *unshifted* (lower-
/// case) codepoint, so Ctrl+Shift+A and Ctrl+A share code 97 with Shift in the
/// modifier field. `None` for composed / multi-char input (no single key). The
/// true base-layout key (for non-letter shifted keys) arrives with flag 4.
fn base_codepoint(s: &str) -> Option<u32> {
    let mut chars = s.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some(u32::from(c.to_ascii_lowercase()))
}

fn kitty_encode_named(
    key: NamedKey,
    mods: Mods,
    force_all: bool,
    nonshift_mod: bool,
    event: Option<u8>,
    kind: KeyEventKind,
) -> Option<Vec<u8>> {
    use NamedKey::*;
    // For keys with no escape-code representation in the active flags, a release
    // is never reported and a repeat re-sends the legacy byte.
    let legacy_byte = |b: u8| {
        if matches!(kind, KeyEventKind::Release) {
            None
        } else {
            Some(vec![b])
        }
    };
    match key {
        // Esc always disambiguates (a real Esc vs. the start of a sequence).
        Escape => csi_u_or_suppressed(27, mods, event, kind),
        // The legacy exceptions: bare Enter/Tab/Backspace keep their byte so a
        // shell stays usable; ANY modifier (incl. Shift — there is no legacy
        // form for, e.g., Shift+Tab under kitty) flips them to CSI u.
        Enter | Tab | Backspace => {
            let code = match key {
                Enter => 13,
                Tab => 9,
                _ => 127,
            };
            if force_all || mods.shift || nonshift_mod {
                csi_u_or_suppressed(code, mods, event, kind)
            } else {
                legacy_byte(match key {
                    Enter => b'\r',
                    Tab => b'\t',
                    _ => 0x7f,
                })
            }
        }
        // Space generates text: bare (or Shift+Space) stays a literal space; a
        // non-Shift modifier disambiguates it (Ctrl+Space replaces legacy NUL).
        Space => {
            if force_all || nonshift_mod {
                csi_u_or_suppressed(32, mods, event, kind)
            } else {
                legacy_byte(b' ')
            }
        }
        // Navigation / cursor / F1-F4: the xterm letter form, CSI (never SS3)
        // under the kitty path. The leading `1;mods` appears only when modified.
        ArrowUp => kitty_letter(b'A', mods, event, kind),
        ArrowDown => kitty_letter(b'B', mods, event, kind),
        ArrowRight => kitty_letter(b'C', mods, event, kind),
        ArrowLeft => kitty_letter(b'D', mods, event, kind),
        Home => kitty_letter(b'H', mods, event, kind),
        End => kitty_letter(b'F', mods, event, kind),
        F1 => kitty_letter(b'P', mods, event, kind),
        F2 => kitty_letter(b'Q', mods, event, kind),
        F4 => kitty_letter(b'S', mods, event, kind),
        // Tilde-form keys (F3 has no letter form; F5+ and the nav block).
        F3 => kitty_tilde(13, mods, event, kind),
        Insert => kitty_tilde(2, mods, event, kind),
        Delete => kitty_tilde(3, mods, event, kind),
        PageUp => kitty_tilde(5, mods, event, kind),
        PageDown => kitty_tilde(6, mods, event, kind),
        F5 => kitty_tilde(15, mods, event, kind),
        F6 => kitty_tilde(17, mods, event, kind),
        F7 => kitty_tilde(18, mods, event, kind),
        F8 => kitty_tilde(19, mods, event, kind),
        F9 => kitty_tilde(20, mods, event, kind),
        F10 => kitty_tilde(21, mods, event, kind),
        F11 => kitty_tilde(23, mods, event, kind),
        F12 => kitty_tilde(24, mods, event, kind),
        Other => None,
    }
}

/// The `;<mods>[:<event>]` field shared by every kitty CSI form. Omitted entirely
/// when it would be the default (no modifiers, no event); but an event sub-field
/// forces the modifier value to appear (as `;1`) so the colon has a host.
fn mods_field(mods: Mods, event: Option<u8>) -> String {
    let m = modifier_param(mods);
    match event {
        Some(ev) => format!(";{m}:{ev}"),
        None if m == 1 => String::new(),
        None => format!(";{m}"),
    }
}

/// `CSI <code>[;<mods>[:<event>]] u`.
fn csi_u(code: u32, mods: Mods, event: Option<u8>) -> Vec<u8> {
    format!("\x1b[{code}{}u", mods_field(mods, event)).into_bytes()
}

/// `CSI <final>` bare, `CSI 1<field><final>` when the field is present (the
/// leading `1` key-number rides with the modifier field). Cursor keys / F1-F4.
/// Returns `None` for an unreported release.
fn kitty_letter(
    final_byte: u8,
    mods: Mods,
    event: Option<u8>,
    kind: KeyEventKind,
) -> Option<Vec<u8>> {
    if matches!(kind, KeyEventKind::Release) && event.is_none() {
        return None;
    }
    let field = mods_field(mods, event);
    let mut out = if field.is_empty() {
        b"\x1b[".to_vec()
    } else {
        format!("\x1b[1{field}").into_bytes()
    };
    out.push(final_byte);
    Some(out)
}

/// `CSI <num>[<field>] ~`. Returns `None` for an unreported release.
fn kitty_tilde(num: u32, mods: Mods, event: Option<u8>, kind: KeyEventKind) -> Option<Vec<u8>> {
    if matches!(kind, KeyEventKind::Release) && event.is_none() {
        return None;
    }
    Some(format!("\x1b[{num}{}~", mods_field(mods, event)).into_bytes())
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
    use crate::input::{Key, KeyEventKind, Mods, NamedKey};

    /// Legacy / modifyOtherKeys tests run with kitty off; this 4-arg wrapper keeps
    /// them readable (the real `super::encode` takes the kitty flags + event kind).
    fn encode(key: &Key, mods: Mods, app_cursor: bool, modify_other_keys: u8) -> Option<Vec<u8>> {
        super::encode(
            key,
            mods,
            app_cursor,
            modify_other_keys,
            0,
            KeyEventKind::Press,
        )
    }

    /// kitty-path encode of a key press: legacy + modifyOtherKeys off, kitty on.
    fn kitty(key: &Key, mods: Mods, flags: u8) -> Option<Vec<u8>> {
        super::encode(key, mods, false, 0, flags, KeyEventKind::Press)
    }

    /// kitty-path encode of a specific event kind (for the report-event-types flag).
    fn kitty_ev(key: &Key, mods: Mods, flags: u8, kind: KeyEventKind) -> Option<Vec<u8>> {
        super::encode(key, mods, false, 0, flags, kind)
    }

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

    // ---- kitty keyboard protocol: disambiguate (flag 1) ----

    const F1_DISAMBIGUATE: u8 = 1;

    #[test]
    fn kitty_disambiguate_keeps_plain_and_shifted_text_legacy() {
        // Text-producing keys stay verbatim — only the ambiguous control combos
        // move to CSI u under flag 1.
        assert_eq!(
            kitty(&ch("a"), none(), F1_DISAMBIGUATE),
            Some(b"a".to_vec())
        );
        assert_eq!(
            kitty(&ch("A"), Mods::SHIFT, F1_DISAMBIGUATE),
            Some(b"A".to_vec())
        );
        assert_eq!(
            kitty(&ch(" "), none(), F1_DISAMBIGUATE),
            Some(b" ".to_vec())
        );
        // Bare Enter / Tab / Backspace keep their legacy byte (so `reset` works).
        assert_eq!(
            kitty(&named(NamedKey::Enter), none(), F1_DISAMBIGUATE),
            Some(b"\r".to_vec())
        );
        assert_eq!(
            kitty(&named(NamedKey::Tab), none(), F1_DISAMBIGUATE),
            Some(b"\t".to_vec())
        );
        assert_eq!(
            kitty(&named(NamedKey::Backspace), none(), F1_DISAMBIGUATE),
            Some(vec![0x7f])
        );
    }

    #[test]
    fn kitty_disambiguate_reports_control_combos_as_csi_u() {
        // The unicode key code is always the unshifted codepoint; the modifier
        // bitfield is 1 + the bits, and is omitted when it is the default 1.
        assert_eq!(
            kitty(&ch("a"), Mods::CTRL, F1_DISAMBIGUATE),
            Some(b"\x1b[97;5u".to_vec())
        );
        // Ctrl+I is now distinct from a real Tab.
        assert_eq!(
            kitty(&ch("i"), Mods::CTRL, F1_DISAMBIGUATE),
            Some(b"\x1b[105;5u".to_vec())
        );
        assert_eq!(
            kitty(&ch("a"), Mods::ALT, F1_DISAMBIGUATE),
            Some(b"\x1b[97;3u".to_vec())
        );
        assert_eq!(
            kitty(&ch("a"), Mods::CTRL | Mods::ALT, F1_DISAMBIGUATE),
            Some(b"\x1b[97;7u".to_vec())
        );
        // Shift+Alt+a: base stays 97, the Shift lives in the modifier (4).
        assert_eq!(
            kitty(&ch("A"), Mods::SHIFT | Mods::ALT, F1_DISAMBIGUATE),
            Some(b"\x1b[97;4u".to_vec())
        );
        // Ctrl+Space replaces the legacy NUL.
        assert_eq!(
            kitty(&named(NamedKey::Space), Mods::CTRL, F1_DISAMBIGUATE),
            Some(b"\x1b[32;5u".to_vec())
        );
    }

    #[test]
    fn kitty_disambiguate_escape_and_modified_exceptions() {
        // Esc always reports as CSI u — the headline disambiguation.
        assert_eq!(
            kitty(&named(NamedKey::Escape), none(), F1_DISAMBIGUATE),
            Some(b"\x1b[27u".to_vec())
        );
        assert_eq!(
            kitty(&named(NamedKey::Escape), Mods::CTRL, F1_DISAMBIGUATE),
            Some(b"\x1b[27;5u".to_vec())
        );
        // Modified Enter/Tab/Backspace have no legacy form, so they disambiguate.
        assert_eq!(
            kitty(&named(NamedKey::Enter), Mods::CTRL, F1_DISAMBIGUATE),
            Some(b"\x1b[13;5u".to_vec())
        );
        assert_eq!(
            kitty(&named(NamedKey::Backspace), Mods::CTRL, F1_DISAMBIGUATE),
            Some(b"\x1b[127;5u".to_vec())
        );
        // Shift+Tab and Ctrl+Tab both go to CSI u under kitty (not legacy CSI Z).
        assert_eq!(
            kitty(&named(NamedKey::Tab), Mods::SHIFT, F1_DISAMBIGUATE),
            Some(b"\x1b[9;2u".to_vec())
        );
        assert_eq!(
            kitty(&named(NamedKey::Tab), Mods::CTRL, F1_DISAMBIGUATE),
            Some(b"\x1b[9;5u".to_vec())
        );
    }

    #[test]
    fn kitty_disambiguate_navigation_uses_csi_letter_and_tilde_forms() {
        // Cursor / F1-F4: CSI letter, leading `1;mods` only when modified, and
        // CSI even in app-cursor mode (the kitty path ignores DECCKM).
        assert_eq!(
            kitty(&named(NamedKey::ArrowUp), none(), F1_DISAMBIGUATE),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            super::encode(
                &named(NamedKey::ArrowUp),
                none(),
                true,
                0,
                F1_DISAMBIGUATE,
                KeyEventKind::Press
            ),
            Some(b"\x1b[A".to_vec()),
            "app-cursor mode does not switch the kitty path to SS3"
        );
        assert_eq!(
            kitty(&named(NamedKey::ArrowUp), Mods::CTRL, F1_DISAMBIGUATE),
            Some(b"\x1b[1;5A".to_vec())
        );
        assert_eq!(
            kitty(&named(NamedKey::F1), none(), F1_DISAMBIGUATE),
            Some(b"\x1b[P".to_vec())
        );
        assert_eq!(
            kitty(&named(NamedKey::F1), Mods::CTRL, F1_DISAMBIGUATE),
            Some(b"\x1b[1;5P".to_vec())
        );
        // Tilde-form keys: F3, F5+, and the nav block.
        assert_eq!(
            kitty(&named(NamedKey::F3), none(), F1_DISAMBIGUATE),
            Some(b"\x1b[13~".to_vec())
        );
        assert_eq!(
            kitty(&named(NamedKey::F5), none(), F1_DISAMBIGUATE),
            Some(b"\x1b[15~".to_vec())
        );
        assert_eq!(
            kitty(&named(NamedKey::PageUp), none(), F1_DISAMBIGUATE),
            Some(b"\x1b[5~".to_vec())
        );
        assert_eq!(
            kitty(&named(NamedKey::Delete), Mods::CTRL, F1_DISAMBIGUATE),
            Some(b"\x1b[3;5~".to_vec())
        );
    }

    #[test]
    fn kitty_report_all_keys_forces_text_and_bare_c0_to_csi_u() {
        // Flag 8 reports every key as an escape code (text suppressed): plain a
        // and bare Enter/Tab become CSI u, where flag 1 alone left them legacy.
        const ALL: u8 = 8;
        assert_eq!(kitty(&ch("a"), none(), ALL), Some(b"\x1b[97u".to_vec()));
        assert_eq!(
            kitty(&named(NamedKey::Enter), none(), ALL),
            Some(b"\x1b[13u".to_vec())
        );
        assert_eq!(
            kitty(&named(NamedKey::Tab), none(), ALL),
            Some(b"\x1b[9u".to_vec())
        );
    }

    // ---- kitty keyboard protocol: report event types (flag 2) ----

    const F1_F2: u8 = 1 | 2; // disambiguate + report-event-types

    #[test]
    fn kitty_event_types_mark_repeat_and_release_on_reported_keys() {
        // A press is unchanged (event type 1 is the default, omitted).
        assert_eq!(
            kitty_ev(&ch("a"), Mods::CTRL, F1_F2, KeyEventKind::Press),
            Some(b"\x1b[97;5u".to_vec())
        );
        // Repeat -> :2, Release -> :3, in the modifier field (forced present).
        assert_eq!(
            kitty_ev(&ch("a"), Mods::CTRL, F1_F2, KeyEventKind::Repeat),
            Some(b"\x1b[97;5:2u".to_vec())
        );
        assert_eq!(
            kitty_ev(&ch("a"), Mods::CTRL, F1_F2, KeyEventKind::Release),
            Some(b"\x1b[97;5:3u".to_vec())
        );
        // A no-modifier release still carries the (default) ;1 so the colon binds.
        assert_eq!(
            kitty_ev(
                &named(NamedKey::Escape),
                none(),
                F1_F2,
                KeyEventKind::Release
            ),
            Some(b"\x1b[27;1:3u".to_vec())
        );
        // Nav keys carry the event in their letter/tilde forms too.
        assert_eq!(
            kitty_ev(
                &named(NamedKey::ArrowUp),
                none(),
                F1_F2,
                KeyEventKind::Release
            ),
            Some(b"\x1b[1;1:3A".to_vec())
        );
        assert_eq!(
            kitty_ev(&named(NamedKey::F5), none(), F1_F2, KeyEventKind::Repeat),
            Some(b"\x1b[15;1:2~".to_vec())
        );
    }

    #[test]
    fn kitty_event_types_do_not_report_text_keys_or_releases_without_the_flag() {
        // Under flag 1 alone, a release is never reported and a repeat re-presses.
        assert_eq!(
            kitty_ev(&ch("a"), Mods::CTRL, F1_DISAMBIGUATE, KeyEventKind::Release),
            None
        );
        assert_eq!(
            kitty_ev(&ch("a"), Mods::CTRL, F1_DISAMBIGUATE, KeyEventKind::Repeat),
            Some(b"\x1b[97;5u".to_vec())
        );
        // Even with flag 2 on, a TEXT key (no escape-code form under flags 1|2) is
        // not reported on release, and a repeat just re-sends its text.
        assert_eq!(
            kitty_ev(&ch("a"), none(), F1_F2, KeyEventKind::Release),
            None
        );
        assert_eq!(
            kitty_ev(&ch("a"), none(), F1_F2, KeyEventKind::Repeat),
            Some(b"a".to_vec())
        );
        // Bare Enter is a legacy byte under flags 1|2, so its release isn't reported.
        assert_eq!(
            kitty_ev(
                &named(NamedKey::Enter),
                none(),
                F1_F2,
                KeyEventKind::Release
            ),
            None
        );
    }
}
