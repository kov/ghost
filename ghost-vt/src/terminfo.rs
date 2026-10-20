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
//! the compiled entry embedded in the binary (`assets/`), written into the data
//! dir on first use — no `tic` needed. Children are pointed at the provided
//! database via `TERMINFO_DIRS`.
//!
//! A session reached over plain ssh needs the entry on the *remote* side, where
//! ghost has nothing installed: [`remote_bootstrap`] is a remote command that
//! writes it into the remote user's cache and starts their login shell pointed
//! at it. Ghost's own copy is preferred over a
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
/// one, trimmed to what ghost's emulator actually implements). What ships is
/// [`ENTRY_COMPILED`]; the source is the record it is checked against.
#[cfg(test)]
const ENTRY_SOURCE: &str = include_str!("../assets/xterm-kitty.terminfo");

/// [`ENTRY_SOURCE`] compiled with `tic -x`, in the legacy format every ncurses
/// reads (a test holds the two together).
const ENTRY_COMPILED: &[u8] = include_bytes!("../assets/xterm-kitty.compiled");

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

/// Write the embedded compiled entry into `dir`, in both layouts curses may
/// look in (`x/` on Linux, hex `78/` where ncurses is built for
/// case-insensitive filesystems, as on macOS), returning the dir once it is
/// there. An entry already matching costs two reads; a stale one (an older
/// ghost's) is replaced. Concurrent spawns write identical bytes through a
/// rename, so last-wins is fine.
fn provision(dir: &Path) -> Option<PathBuf> {
    for layout in ["x", "78"] {
        let sub = dir.join(layout);
        let path = sub.join(PREFERRED);
        if std::fs::read(&path).is_ok_and(|b| b == ENTRY_COMPILED) {
            continue;
        }
        std::fs::create_dir_all(&sub).ok()?;
        let tmp = sub.join(format!(".{PREFERRED}.{}", std::process::id()));
        std::fs::write(&tmp, ENTRY_COMPILED).ok()?;
        if std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return None;
        }
    }
    Some(dir.to_path_buf())
}

/// A remote command that provides the entry on a host ghost has nothing
/// installed on, for `ssh <host> <command>`: it writes the compiled entry
/// into `${XDG_CACHE_HOME:-~/.cache}/ghost/terminfo` and execs the user's login
/// shell with `TERMINFO_DIRS` pointing there first (the trailing empty entry
/// keeps the system database for every other TERM). Where the cache can't be
/// written, the shell starts under [`FALLBACK`] instead — a TERM the host
/// resolves, rather than one it can't. A session advertising any TERM but
/// [`PREFERRED`] (a `GHOST_TERM` override) is left as it is.
///
/// sshd runs the command with the user's own login shell (`$SHELL -c`), which
/// may be fish or csh, so the command is one `sh -c '…'` whose script carries
/// the entry as a `printf` format: POSIX `sh`, no `base64`, no `!`, no newline,
/// and no backslash that is not an octal escape.
pub fn remote_bootstrap() -> String {
    let data: String = ENTRY_COMPILED
        .iter()
        .map(|&b| match b {
            b'%' => "%%".to_string(),
            b'\\' | b'\'' | b'!' => format!("\\{b:03o}"),
            0x20..=0x7e => (b as char).to_string(),
            _ => format!("\\{b:03o}"),
        })
        .collect();
    let script = format!(
        "if [ \"$TERM\" = {PREFERRED} ]; then \
         d=\"${{XDG_CACHE_HOME:-$HOME/.cache}}/ghost/terminfo\"; t=\"$d/.{PREFERRED}.$$\"; \
         if mkdir -p \"$d/x\" \"$d/78\" && printf '{data}' > \"$t\" \
         && cp \"$t\" \"$t.78\" && mv -f \"$t.78\" \"$d/78/{PREFERRED}\" \
         && mv -f \"$t\" \"$d/x/{PREFERRED}\"; \
         then TERMINFO_DIRS=\"$d:$TERMINFO_DIRS\"; export TERMINFO_DIRS; \
         else rm -f \"$t\" \"$t.78\"; TERM={FALLBACK}; export TERM; fi; fi; \
         exec \"${{SHELL:-/bin/sh}}\" -l"
    );
    format!("sh -c {}", crate::connection::sh_quote(&script))
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

    /// The legacy compiled-terminfo magic (octal 0432). ncurses 6.1+ also reads
    /// the 32-bit-number format (01036), but ncurses 5 — still on long-lived
    /// servers — reads only this one, and an unreadable entry is exactly the
    /// "terminal not fully functional" we ship it to avoid.
    #[test]
    fn the_compiled_entry_is_in_the_format_every_ncurses_reads() {
        let magic = u16::from_le_bytes([ENTRY_COMPILED[0], ENTRY_COMPILED[1]]);
        assert_eq!(magic, 0o432, "legacy format, not the 32-bit-number one");
    }

    /// The shipped binary entry is the source compiled — checked semantically
    /// (`infocmp` of each), since tic's byte layout differs across ncurses
    /// versions. Regenerate after editing the source:
    /// `tic -x -o DIR ghost-vt/assets/xterm-kitty.terminfo &&
    /// cp DIR/x/xterm-kitty ghost-vt/assets/xterm-kitty.compiled`.
    #[test]
    fn the_compiled_entry_is_its_source_compiled() {
        let (Ok(_), Ok(_)) = (
            std::process::Command::new("tic").arg("-V").output(),
            std::process::Command::new("infocmp").arg("-V").output(),
        ) else {
            eprintln!("terminfo: no tic/infocmp here; skipping");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let from_source = tmp.path().join("source");
        let src = tmp.path().join("xterm-kitty.terminfo");
        std::fs::write(&src, ENTRY_SOURCE).unwrap();
        let ok = std::process::Command::new("tic")
            .args(["-x", "-o"])
            .arg(&from_source)
            .arg(&src)
            .status()
            .unwrap()
            .success();
        assert!(ok, "the source must compile");
        let shipped = tmp.path().join("shipped");
        std::fs::create_dir_all(shipped.join("x")).unwrap();
        std::fs::write(shipped.join("x").join(PREFERRED), ENTRY_COMPILED).unwrap();
        let describe = |dir: &Path| {
            let out = std::process::Command::new("infocmp")
                .args(["-x", "-A"])
                .arg(dir)
                .arg(PREFERRED)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            // The first lines name the file infocmp read; the rest is the entry.
            String::from_utf8(out.stdout)
                .unwrap()
                .lines()
                .filter(|l| !l.starts_with('#'))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(
            describe(&shipped),
            describe(&from_source),
            "assets/xterm-kitty.compiled is stale: regenerate it from the source"
        );
    }

    /// Every login shell a remote account might have, parsing the bootstrap the
    /// way sshd hands it over (`$SHELL -c '<command>'`), and every `sh` the
    /// script itself might land on.
    fn shells() -> Vec<(&'static str, Vec<&'static str>)> {
        [
            ("sh", vec!["sh", "-c"]),
            ("dash", vec!["dash", "-c"]),
            ("fish", vec!["fish", "-c"]),
            ("busybox", vec!["busybox", "sh", "-c"]),
        ]
        .into_iter()
        .filter(|(bin, _)| {
            std::process::Command::new("sh")
                .args(["-c", &format!("command -v {bin}")])
                .output()
                .is_ok_and(|o| o.status.success())
        })
        .collect()
    }

    /// Run the bootstrap as a remote login shell would, with `home` as `$HOME`,
    /// `inner_sh` as the `sh` on `PATH`, and a stand-in login shell that reports
    /// the terminal environment it was started with.
    fn run_bootstrap(login: &[&str], inner_sh: &str, home: &Path, term: &str) -> (String, String) {
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let real = std::process::Command::new("sh")
            .args(["-c", &format!("command -v {inner_sh}")])
            .output()
            .unwrap();
        let real = String::from_utf8(real.stdout).unwrap().trim().to_string();
        let sh_path = bin.join("sh");
        let _ = std::fs::remove_file(&sh_path);
        if inner_sh == "busybox" {
            std::fs::write(&sh_path, format!("#!/bin/sh\nexec {real} sh \"$@\"\n")).unwrap();
        } else {
            std::os::unix::fs::symlink(&real, &sh_path).unwrap();
        }
        let report = home.join("report");
        let shell = home.join("login-shell");
        std::fs::write(
            &shell,
            format!(
                "#!/bin/sh\nprintf '%s\\n%s\\n' \"$TERM\" \"$TERMINFO_DIRS\" > {}\n",
                report.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        for f in [&shell, &sh_path] {
            if f.is_file() && !f.is_symlink() {
                std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        let status = std::process::Command::new(login[0])
            .args(&login[1..])
            .arg(remote_bootstrap())
            .env_clear()
            .env("HOME", home)
            .env("PATH", path)
            .env("SHELL", &shell)
            .env("TERM", term)
            .status()
            .unwrap();
        assert!(status.success(), "{login:?} with {inner_sh}: {status}");
        let out = std::fs::read_to_string(&report).unwrap();
        let mut lines = out.lines();
        (
            lines.next().unwrap_or_default().to_string(),
            lines.next().unwrap_or_default().to_string(),
        )
    }

    #[test]
    fn the_remote_bootstrap_installs_the_entry_and_points_the_shell_at_it() {
        for (name, login) in shells() {
            for inner in ["sh", "dash", "busybox"] {
                if !shells().iter().any(|(n, _)| *n == inner) {
                    continue;
                }
                let tmp = tempfile::tempdir().unwrap();
                let home = tmp.path();
                let (term, dirs) = run_bootstrap(&login, inner, home, PREFERRED);
                let db = home.join(".cache/ghost/terminfo");
                assert_eq!(term, PREFERRED, "{name}/{inner}: TERM kept");
                assert_eq!(
                    dirs,
                    format!("{}:", db.display()),
                    "{name}/{inner}: ours first, then the system default"
                );
                for layout in ["x", "78"] {
                    assert_eq!(
                        std::fs::read(db.join(layout).join(PREFERRED)).unwrap(),
                        ENTRY_COMPILED,
                        "{name}/{inner}: the entry, byte for byte, in {layout}/"
                    );
                }
            }
        }
    }

    /// No writable cache (a read-only home, a full disk): the shell still starts,
    /// under a TERM every host resolves, instead of one it cannot.
    #[test]
    fn a_remote_without_a_writable_cache_falls_back_to_xterm_256color() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        // A file where the cache dir must go makes `mkdir -p` fail.
        std::fs::write(home.join(".cache"), b"").unwrap();
        let (term, dirs) = run_bootstrap(&["sh", "-c"], "sh", home, PREFERRED);
        assert_eq!(term, FALLBACK);
        assert_eq!(dirs, "");
    }

    /// A session told to advertise some other TERM (`GHOST_TERM`) gets nothing
    /// written and nothing changed.
    #[test]
    fn the_remote_bootstrap_leaves_any_other_terminal_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let (term, dirs) = run_bootstrap(&["sh", "-c"], "sh", home, "foot");
        assert_eq!(term, "foot");
        assert_eq!(dirs, "");
        assert!(!home.join(".cache").exists());
    }

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
    fn provisions_the_embedded_entry_in_both_layouts_without_tic() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("terminfo");
        assert_eq!(provision(&dir), Some(dir.clone()));
        // Char dirs (Linux) and hex dirs (macOS ncurses), byte for byte.
        for layout in ["x", "78"] {
            assert_eq!(
                std::fs::read(dir.join(layout).join(PREFERRED)).unwrap(),
                ENTRY_COMPILED
            );
        }
        assert!(available_in(std::slice::from_ref(&dir), PREFERRED));
    }

    #[test]
    fn a_stale_provisioned_entry_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("terminfo");
        provision(&dir).unwrap();
        // An older ghost's entry.
        std::fs::write(dir.join("x").join(PREFERRED), b"stale").unwrap();
        assert_eq!(provision(&dir), Some(dir.clone()));
        assert_eq!(
            std::fs::read(dir.join("x").join(PREFERRED)).unwrap(),
            ENTRY_COMPILED
        );
    }
}
