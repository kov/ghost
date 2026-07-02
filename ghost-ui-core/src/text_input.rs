//! A single-line text-entry model: a buffer plus a caret, honouring the
//! navigation and editing chords users expect of any text field — char and
//! word motion, char and word deletion, line jumps. Pure and shared, so every
//! inline edit (the fleet's rename today) behaves identically.

use crate::input::{Key, Mods, NamedKey};

/// Word characters are alphanumerics; everything else separates. This treats
/// `-` in a name like "brave-otter" as a boundary, matching what native text
/// fields and readline do.
fn is_word(c: char) -> bool {
    c.is_alphanumeric()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextInput {
    text: String,
    /// Byte offset of the caret in `text`, always on a char boundary.
    cursor: usize,
}

impl TextInput {
    /// Start editing `text` with the caret at its end.
    pub fn new(text: String) -> Self {
        let cursor = text.len();
        Self { text, cursor }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn into_text(self) -> String {
        self.text
    }

    /// The text split at the caret, for rendering a cursor between the halves.
    pub fn halves(&self) -> (&str, &str) {
        self.text.split_at(self.cursor)
    }

    /// Insert text (typed, pasted, or IME-committed) at the caret.
    pub fn insert(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    /// Apply a navigation/editing key, returning whether it was handled (so
    /// the caller knows to redraw). Word motion and deletion answer to Alt
    /// (macOS Option) and to Ctrl (the Linux convention); Super-arrows and
    /// Super-Backspace mirror macOS's line-wise Cmd chords.
    pub fn key(&mut self, key: &Key, mods: Mods) -> bool {
        let word = mods.alt || mods.ctrl;
        match key {
            Key::Named(NamedKey::ArrowLeft) if mods.sup => self.cursor = 0,
            Key::Named(NamedKey::ArrowLeft) if word => self.cursor = self.prev_word(),
            Key::Named(NamedKey::ArrowLeft) => self.cursor = self.prev_char(),
            Key::Named(NamedKey::ArrowRight) if mods.sup => self.cursor = self.text.len(),
            Key::Named(NamedKey::ArrowRight) if word => self.cursor = self.next_word(),
            Key::Named(NamedKey::ArrowRight) => self.cursor = self.next_char(),
            Key::Named(NamedKey::Home) => self.cursor = 0,
            Key::Named(NamedKey::End) => self.cursor = self.text.len(),
            Key::Named(NamedKey::Backspace) if mods.sup => {
                self.text.drain(..self.cursor);
                self.cursor = 0;
            }
            Key::Named(NamedKey::Backspace) if word => {
                let to = self.prev_word();
                self.text.drain(to..self.cursor);
                self.cursor = to;
            }
            Key::Named(NamedKey::Backspace) => {
                let to = self.prev_char();
                self.text.drain(to..self.cursor);
                self.cursor = to;
            }
            Key::Named(NamedKey::Delete) if word => {
                let to = self.next_word();
                self.text.drain(self.cursor..to);
            }
            Key::Named(NamedKey::Delete) => {
                let to = self.next_char();
                self.text.drain(self.cursor..to);
            }
            _ => return false,
        }
        true
    }

    /// Caret position one char left; the start when already there.
    fn prev_char(&self) -> usize {
        self.text[..self.cursor]
            .chars()
            .next_back()
            .map_or(0, |c| self.cursor - c.len_utf8())
    }

    /// Caret position one char right; the end when already there.
    fn next_char(&self) -> usize {
        self.text[self.cursor..]
            .chars()
            .next()
            .map_or(self.text.len(), |c| self.cursor + c.len_utf8())
    }

    /// Start of the word before the caret: back over separators, then over
    /// the word itself.
    fn prev_word(&self) -> usize {
        self.text[..self.cursor]
            .trim_end_matches(|c| !is_word(c))
            .trim_end_matches(is_word)
            .len()
    }

    /// End of the word after the caret: forward over separators, then over
    /// the word itself.
    fn next_word(&self) -> usize {
        let rest = self.text[self.cursor..]
            .trim_start_matches(|c| !is_word(c))
            .trim_start_matches(is_word);
        self.text.len() - rest.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(k: NamedKey) -> Key {
        Key::Named(k)
    }

    #[test]
    fn insert_starts_at_the_end_and_follows_the_caret() {
        let mut t = TextInput::new("ab".into());
        t.insert("c");
        assert_eq!(t.text(), "abc");
        t.key(&named(NamedKey::ArrowLeft), Mods::NONE);
        t.key(&named(NamedKey::ArrowLeft), Mods::NONE);
        t.insert("X");
        assert_eq!(t.text(), "aXbc");
        assert_eq!(t.halves(), ("aX", "bc"));
    }

    #[test]
    fn char_motion_and_deletion_respect_utf8_boundaries() {
        let mut t = TextInput::new("héllo".into());
        t.key(&named(NamedKey::ArrowLeft), Mods::NONE);
        t.key(&named(NamedKey::ArrowLeft), Mods::NONE);
        t.key(&named(NamedKey::ArrowLeft), Mods::NONE);
        t.key(&named(NamedKey::Backspace), Mods::NONE); // deletes é
        assert_eq!(t.text(), "hllo");
        t.key(&named(NamedKey::Delete), Mods::NONE); // deletes l forward
        assert_eq!(t.text(), "hlo");
        // Motion clamps at both ends instead of panicking.
        for _ in 0..10 {
            t.key(&named(NamedKey::ArrowLeft), Mods::NONE);
        }
        assert_eq!(t.halves().0, "");
        for _ in 0..10 {
            t.key(&named(NamedKey::ArrowRight), Mods::NONE);
        }
        assert_eq!(t.halves().1, "");
    }

    #[test]
    fn word_motion_answers_to_alt_and_ctrl() {
        for mods in [Mods::ALT, Mods::CTRL] {
            let mut t = TextInput::new("brave-otter two".into());
            t.key(&named(NamedKey::ArrowLeft), mods);
            assert_eq!(t.halves().0, "brave-otter ", "back over 'two'");
            t.key(&named(NamedKey::ArrowLeft), mods);
            assert_eq!(t.halves().0, "brave-", "'-' separates words");
            t.key(&named(NamedKey::ArrowRight), mods);
            assert_eq!(t.halves().0, "brave-otter", "forward to a word end");
        }
    }

    #[test]
    fn word_deletion_removes_the_word_and_its_trailing_separators() {
        let mut t = TextInput::new("build box".into());
        t.key(&named(NamedKey::Backspace), Mods::ALT);
        assert_eq!(t.text(), "build ", "Alt-Backspace eats one word");
        let mut t = TextInput::new("build box".into());
        t.key(&named(NamedKey::Home), Mods::NONE);
        t.key(&named(NamedKey::Delete), Mods::ALT);
        assert_eq!(t.text(), " box", "Alt-Delete eats one word forward");
    }

    #[test]
    fn line_chords_jump_and_delete_to_the_edges() {
        let mut t = TextInput::new("one two".into());
        t.key(&named(NamedKey::Home), Mods::NONE);
        assert_eq!(t.halves().0, "");
        t.key(&named(NamedKey::End), Mods::NONE);
        assert_eq!(t.halves().1, "");
        t.key(&named(NamedKey::ArrowLeft), Mods::SUPER);
        assert_eq!(t.halves().0, "", "Cmd-Left jumps to the start");
        t.key(&named(NamedKey::ArrowRight), Mods::SUPER);
        assert_eq!(t.halves().1, "", "Cmd-Right jumps to the end");
        t.key(&named(NamedKey::ArrowLeft), Mods::ALT);
        t.key(&named(NamedKey::Backspace), Mods::SUPER);
        assert_eq!(t.text(), "two", "Cmd-Backspace deletes to the start");
    }

    #[test]
    fn unrelated_keys_are_not_handled() {
        let mut t = TextInput::new("abc".into());
        assert!(!t.key(&named(NamedKey::Enter), Mods::NONE));
        assert!(!t.key(&named(NamedKey::Escape), Mods::NONE));
        assert!(!t.key(&Key::Char("x".into()), Mods::NONE));
        assert_eq!(t.text(), "abc");
    }
}
