//! Which `TERM` ghost advertises to session children.
//!
//! ghost's emulator implements the kitty feature profile (kitty keyboard
//! protocol on both sides, kitty graphics), and applications gate those
//! features on the TERM *name* rather than probing — Claude Code, for one,
//! only enables its kitty-keyboard / synchronized-output path under a TERM it
//! recognizes. So ghost prefers `xterm-kitty`, but only when that terminfo
//! entry is actually installed on the host: advertising a TERM the local
//! curses database cannot resolve breaks every terminfo consumer. Without the
//! entry we fall back to plain `xterm-256color`, and a non-empty `GHOST_TERM`
//! overrides the whole decision.

use std::path::{Path, PathBuf};

/// Advertised when the host terminfo database can resolve it.
pub const PREFERRED: &str = "xterm-kitty";
/// Safe fallback present in every curses installation.
pub const FALLBACK: &str = "xterm-256color";

/// The `TERM` value ghost sets for session children.
pub fn session_term() -> String {
    match std::env::var("GHOST_TERM") {
        Ok(term) if !term.is_empty() => term,
        _ if available(PREFERRED) => PREFERRED.to_string(),
        _ => FALLBACK.to_string(),
    }
}

/// Whether `name` resolves in the host's terminfo database, mirroring
/// ncurses's search order (TERMINFO, ~/.terminfo, TERMINFO_DIRS, defaults).
pub fn available(name: &str) -> bool {
    available_in(&search_dirs(), name)
}

fn search_dirs() -> Vec<PathBuf> {
    dirs_from(
        std::env::var("TERMINFO").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
        std::env::var("TERMINFO_DIRS").ok().as_deref(),
    )
}

/// ncurses search order: `$TERMINFO`, `$HOME/.terminfo`, each entry of the
/// colon-separated `$TERMINFO_DIRS` (an empty entry means the compiled-in
/// default), then the well-known system locations.
fn dirs_from(
    terminfo: Option<&str>,
    home: Option<&str>,
    terminfo_dirs: Option<&str>,
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(t) = terminfo.filter(|s| !s.is_empty()) {
        dirs.push(PathBuf::from(t));
    }
    if let Some(h) = home.filter(|s| !s.is_empty()) {
        dirs.push(Path::new(h).join(".terminfo"));
    }
    if let Some(list) = terminfo_dirs {
        for entry in list.split(':') {
            if entry.is_empty() {
                dirs.push(PathBuf::from("/usr/share/terminfo"));
            } else {
                dirs.push(PathBuf::from(entry));
            }
        }
    }
    for d in [
        "/etc/terminfo",
        "/lib/terminfo",
        "/usr/lib/terminfo",
        "/usr/share/terminfo",
        "/usr/local/share/terminfo",
    ] {
        dirs.push(PathBuf::from(d));
    }
    dirs
}

/// A compiled entry lives at `<dir>/<first-char>/<name>` (Linux) or
/// `<dir>/<first-char-hex>/<name>` (the layout macOS's ncurses uses).
fn available_in(dirs: &[PathBuf], name: &str) -> bool {
    let Some(first) = name.chars().next() else {
        return false;
    };
    dirs.iter().any(|dir| {
        dir.join(first.to_string()).join(name).is_file()
            || dir
                .join(format!("{:02x}", first as u32))
                .join(name)
                .is_file()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_entry_in_char_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("ti");
        std::fs::create_dir_all(db.join("x")).unwrap();
        std::fs::write(db.join("x").join("xterm-kitty"), b"").unwrap();
        let dirs = [db];
        assert!(available_in(&dirs, "xterm-kitty"));
        assert!(!available_in(&dirs, "xterm-ghostty"));
    }

    #[test]
    fn finds_entry_in_hex_subdir() {
        // macOS ncurses stores entries under the hex code of the first char:
        // 'x' == 0x78.
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("ti");
        std::fs::create_dir_all(db.join("78")).unwrap();
        std::fs::write(db.join("78").join("xterm-kitty"), b"").unwrap();
        assert!(available_in(&[db], "xterm-kitty"));
    }

    #[test]
    fn missing_dirs_and_names_are_not_found() {
        assert!(!available_in(
            &[PathBuf::from("/nonexistent-ti")],
            "xterm-kitty"
        ));
        assert!(!available_in(&[PathBuf::from("/tmp")], ""));
    }

    #[test]
    fn search_order_is_env_then_home_then_dirs_then_defaults() {
        let dirs = dirs_from(Some("/env/ti"), Some("/home/u"), Some("/a::/b"));
        let expect_prefix: Vec<PathBuf> = [
            "/env/ti",
            "/home/u/.terminfo",
            "/a",
            "/usr/share/terminfo", // empty TERMINFO_DIRS entry -> default
            "/b",
            "/etc/terminfo",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        assert_eq!(&dirs[..expect_prefix.len()], &expect_prefix[..]);
    }

    #[test]
    fn empty_env_values_are_skipped() {
        let dirs = dirs_from(Some(""), Some(""), None);
        assert_eq!(dirs[0], PathBuf::from("/etc/terminfo"));
    }
}
