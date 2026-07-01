//! Which `TERM` ghost advertises to session children, and the terminfo entry
//! it ships to back that promise.
//!
//! ghost's emulator implements the kitty feature profile (kitty keyboard
//! protocol on both sides, kitty graphics), and applications gate those
//! features on the TERM *name* rather than probing — Claude Code, for one,
//! only enables its kitty-keyboard / synchronized-output path under a TERM it
//! recognizes. So ghost advertises `xterm-kitty` — but a TERM the local curses
//! database cannot resolve breaks every terminfo consumer, so ghost *provides*
//! the entry rather than hoping the host has one: a copy precompiled into the
//! macOS `.app` bundle (`Resources/terminfo`, see `cargo xtask bundle`), or
//! one compiled on first use from the embedded source (`assets/`) into the
//! data dir with the system `tic`. Children are pointed at the provided
//! database via `TERMINFO_DIRS`. Ghost's own copy is preferred over a
//! system-installed one so sessions behave identically on every host; with no
//! copy providable (no bundle, no `tic`) we fall back to the system database
//! probe and, failing that, to plain `xterm-256color`. A non-empty
//! `GHOST_TERM` overrides the whole decision.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Advertised when ghost can provide the entry or the host can resolve it.
pub const PREFERRED: &str = "xterm-kitty";
/// Safe fallback present in every curses installation.
pub const FALLBACK: &str = "xterm-256color";

/// The terminfo source ghost provisions — see the file's provenance header
/// (derived from ncurses' MIT-licensed `kitty` entry, not kitty's own GPLv3
/// one, trimmed to what ghost's emulator actually implements).
const ENTRY_SOURCE: &str = include_str!("../assets/xterm-kitty.terminfo");

/// The terminal identity a session child is given.
pub struct SessionTerm {
    /// The `TERM` value.
    pub term: String,
    /// `TERMINFO_DIRS` for the child when ghost provides the entry itself:
    /// ghost's database first, any pre-existing value, then a trailing empty
    /// entry so the compiled-in default list still resolves every other TERM.
    pub terminfo_dirs: Option<OsString>,
}

/// Decide the `TERM` (and terminfo database) for a session child.
pub fn session_term() -> SessionTerm {
    if let Ok(term) = std::env::var("GHOST_TERM")
        && !term.is_empty()
    {
        return SessionTerm {
            term,
            terminfo_dirs: None,
        };
    }
    if let Some(dir) = provided_dir() {
        return SessionTerm {
            term: PREFERRED.to_string(),
            terminfo_dirs: Some(child_terminfo_dirs(&dir)),
        };
    }
    let term = if available(PREFERRED) {
        PREFERRED
    } else {
        FALLBACK
    };
    SessionTerm {
        term: term.to_string(),
        terminfo_dirs: None,
    }
}

/// The `TERMINFO_DIRS` value handing `dir` to a child without hiding anything:
/// pre-existing entries stay, and the trailing empty entry means "the
/// compiled-in default list" to ncurses.
fn child_terminfo_dirs(dir: &Path) -> OsString {
    let mut v = OsString::from(dir);
    if let Some(prev) = std::env::var_os("TERMINFO_DIRS")
        && !prev.is_empty()
    {
        v.push(":");
        v.push(prev);
    }
    v.push(":");
    v
}

/// The terminfo database ghost itself provides, if any: the copy packaged
/// next to the executable, or one provisioned into the data dir.
fn provided_dir() -> Option<PathBuf> {
    if let Some(dir) = packaged_dir()
        && available_in(std::slice::from_ref(&dir), PREFERRED)
    {
        return Some(dir);
    }
    provision(&crate::paths::data_dir().join("terminfo"))
}

/// Where a packaged install keeps the precompiled database: `Resources/
/// terminfo` in the macOS bundle (the executable lives in `Contents/MacOS`).
/// Harmlessly absent everywhere else.
fn packaged_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.parent()?.join("Resources").join("terminfo"))
}

/// Compile the embedded entry into `dir` with the system `tic`, returning the
/// dir once the compiled entry is resolvable there. The source is stamped
/// alongside (`xterm-kitty.src`) so an upgraded ghost recompiles and an
/// unchanged one costs a read and a stat. Concurrent spawns may race the
/// compile; they write identical content, so last-wins is fine.
fn provision(dir: &Path) -> Option<PathBuf> {
    let dirbuf = dir.to_path_buf();
    let stamp = dir.join("xterm-kitty.src");
    if std::fs::read_to_string(&stamp).is_ok_and(|s| s == ENTRY_SOURCE)
        && available_in(std::slice::from_ref(&dirbuf), PREFERRED)
    {
        return Some(dirbuf);
    }
    std::fs::create_dir_all(dir).ok()?;
    // tic wants the source on disk; pid-suffixed so concurrent spawns don't
    // truncate each other's copy mid-read.
    let src = dir.join(format!(".xterm-kitty.src.{}", std::process::id()));
    std::fs::write(&src, ENTRY_SOURCE).ok()?;
    let compiled = std::process::Command::new("tic")
        .args(["-x", "-o"])
        .arg(dir)
        .arg(&src)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    let _ = std::fs::remove_file(&src);
    if !compiled {
        return None;
    }
    mirror_layouts(dir);
    if !available_in(std::slice::from_ref(&dirbuf), PREFERRED) {
        return None;
    }
    // Stamp only after success, so a failed compile retries on the next spawn.
    let _ = std::fs::write(&stamp, ENTRY_SOURCE);
    Some(dirbuf)
}

/// `tic` writes one layout — `x/name` on Linux, hex `78/name` where ncurses
/// is built for case-insensitive filesystems (macOS) — but the child's curses
/// may expect either. Cheap insurance: copy every compiled entry into its
/// sibling layout.
fn mirror_layouts(dir: &Path) {
    let Ok(subdirs) = std::fs::read_dir(dir) else {
        return;
    };
    for sub in subdirs.flatten() {
        let name = sub.file_name();
        let Some(name) = name.to_str() else { continue };
        let sibling = match name.chars().collect::<Vec<_>>()[..] {
            [c] if c.is_ascii() => format!("{:02x}", c as u32),
            [_, _] => match u32::from_str_radix(name, 16).ok().and_then(char::from_u32) {
                Some(c) => c.to_string(),
                None => continue,
            },
            _ => continue,
        };
        let Ok(entries) = std::fs::read_dir(sub.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let to = dir.join(&sibling).join(entry.file_name());
            if !to.exists() && std::fs::create_dir_all(dir.join(&sibling)).is_ok() {
                let _ = std::fs::copy(entry.path(), &to);
            }
        }
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

    #[test]
    fn provisions_the_embedded_entry_and_recompiles_on_change() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("terminfo");

        // First use compiles (tic is present on every supported dev platform).
        let got = provision(&dir).expect("tic must be able to compile the embedded entry");
        assert_eq!(got, dir);
        assert!(available_in(std::slice::from_ref(&dir), PREFERRED));
        assert_eq!(
            std::fs::read_to_string(dir.join("xterm-kitty.src")).unwrap(),
            ENTRY_SOURCE,
            "stamp records the compiled source"
        );

        // Unchanged source: a cheap hit, still usable.
        assert_eq!(provision(&dir), Some(dir.clone()));

        // A stale stamp (an older ghost's source) forces a recompile.
        std::fs::write(dir.join("xterm-kitty.src"), "stale").unwrap();
        assert_eq!(provision(&dir), Some(dir.clone()));
        assert_eq!(
            std::fs::read_to_string(dir.join("xterm-kitty.src")).unwrap(),
            ENTRY_SOURCE
        );
    }

    #[test]
    fn provisioned_entry_exists_in_both_terminfo_layouts() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("terminfo");
        provision(&dir).expect("tic must be able to compile the embedded entry");
        // Whichever layout the local tic wrote, the mirror fills in the other:
        // char dirs (Linux) and hex dirs (macOS ncurses).
        assert!(dir.join("x").join("xterm-kitty").is_file());
        assert!(dir.join("78").join("xterm-kitty").is_file());
    }
}
