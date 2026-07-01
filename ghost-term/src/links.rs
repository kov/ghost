//! Interning table for OSC 8 hyperlink URIs.
//!
//! Cells are packed `Copy` values, so they carry a small interned id (see
//! `Pen::link`) rather than the URI itself; this table owns the strings. Ids
//! are 1-based `NonZeroU16`s, so a cell pays two bytes and `Option` is free.
//! Identical URIs share an id (prompts tend to repeat the same link every
//! line), and the table is capped at `u16::MAX` distinct URIs per terminal —
//! past that, new links are dropped rather than evicting old ones, since ids
//! already stamped on cells must stay valid for the terminal's lifetime.

use std::collections::HashMap;
use std::num::NonZeroU16;

#[derive(Debug, Default)]
pub(crate) struct Links {
    by_uri: HashMap<String, NonZeroU16>,
    uris: Vec<String>,
}

impl Links {
    /// Intern `uri`, returning its id — `None` only once the table is full.
    pub(crate) fn intern(&mut self, uri: &str) -> Option<NonZeroU16> {
        if let Some(&id) = self.by_uri.get(uri) {
            return Some(id);
        }
        let id = NonZeroU16::new(u16::try_from(self.uris.len() + 1).ok()?)?;
        self.uris.push(uri.to_string());
        self.by_uri.insert(uri.to_string(), id);
        Some(id)
    }

    /// The URI behind an id, if it was interned here.
    pub(crate) fn get(&self, id: u16) -> Option<&str> {
        self.uris
            .get(usize::from(id.checked_sub(1)?))
            .map(String::as_str)
    }

    pub(crate) fn clear(&mut self) {
        self.by_uri.clear();
        self.uris.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interns_and_resolves() {
        let mut links = Links::default();
        let a = links.intern("https://a").unwrap();
        let b = links.intern("https://b").unwrap();
        assert_ne!(a, b);
        assert_eq!(links.intern("https://a"), Some(a), "same URI, same id");
        assert_eq!(links.get(a.get()), Some("https://a"));
        assert_eq!(links.get(b.get()), Some("https://b"));
        assert_eq!(links.get(0), None);
        assert_eq!(links.get(3), None);

        links.clear();
        assert_eq!(links.get(a.get()), None);
    }
}
