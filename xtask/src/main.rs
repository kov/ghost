//! Packaging tasks for `ghost`.
//!
//! * `cargo xtask bundle`  — build `ghost` in release and assemble
//!   `target/release/ghost.app`.
//! * `cargo xtask install` — bundle, then copy the `.app` into `/Applications`.
//! * `cargo xtask icon`    — regenerate `assets/ghost.icns` from the SVG.
//! * `cargo xtask prebuilt [<triple>…]` — cross-build the headless `ghost-host`
//!   for each target and drop it in the prebuilt dir as `ghost-<os>-<arch>`, where
//!   staging's resolver finds it. No triples ⇒ this host OS's two arches. Set
//!   `GHOST_ZIGBUILD=1` to build through `cargo zigbuild` (bundles its own
//!   sysroots, so cross-OS builds need no system cross-toolchain).
//!
//! The bundle is **relocatable and launcher-free**: the `ghost` binary has no
//! non-system dylib dependencies, falls through to the GUI when launched with no
//! argv (from Finder), and keeps its ad-hoc linker signature across a plain
//! `fs::copy`. So the real binary is `CFBundleExecutable` directly — there is no
//! launcher shim and no Homebrew/GTK environment to set up.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

type R<T> = Result<T, Box<dyn Error>>;

/// `<bundle>.app` directory name.
const BUNDLE_NAME: &str = "ghost.app";
/// Must match `APP_ID` in `ghost`'s `main.rs`.
const APP_ID: &str = "dev.ghost.Terminal";
/// The bundle executable (`CFBundleExecutable`) — the real `ghost` binary.
const EXECUTABLE: &str = "ghost";
/// Basename of the icon inside the bundle (and the `CFBundleIconFile` value).
const ICON_NAME: &str = "ghost.icns";

/// Everything `assemble_bundle` needs, with no I/O of its own to discover — so
/// it stays a pure, testable transform from inputs to an on-disk bundle.
struct BundleOpts {
    /// Already-built `ghost` executable to embed.
    binary: PathBuf,
    /// Directory to create `ghost.app` in.
    out_dir: PathBuf,
    /// `CFBundleShortVersionString` / `CFBundleVersion`.
    version: String,
    /// `.icns` app icon to embed in `Resources/`, if one exists.
    icon: Option<PathBuf>,
    /// Compiled terminfo database to embed as `Resources/terminfo` — ghost
    /// advertises `TERM=xterm-kitty` and ships the entry to back it (see
    /// `ghost-vt`'s `terminfo` module, which looks for this directory).
    terminfo: Option<PathBuf>,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("xtask: {e}");
        std::process::exit(1);
    }
}

fn run() -> R<()> {
    match std::env::args().nth(1).as_deref() {
        Some("bundle") => {
            let app = bundle()?;
            println!("built {}", app.display());
        }
        Some("install") => {
            let app = bundle()?;
            let dest = Path::new("/Applications").join(BUNDLE_NAME);
            if dest.exists() {
                fs::remove_dir_all(&dest)?;
            }
            copy_dir(&app, &dest)?;
            println!("installed {}", dest.display());
        }
        Some("icon") => {
            let icns = generate_icon()?;
            println!("generated {}", icns.display());
        }
        Some("prebuilt") => {
            build_prebuilts(&std::env::args().skip(2).collect::<Vec<_>>())?;
        }
        other => {
            return Err(format!(
                "unknown command {:?}; use `bundle`, `install`, `icon` or `prebuilt`",
                other.unwrap_or("")
            )
            .into());
        }
    }
    Ok(())
}

/// Build `ghost` in release and assemble the relocatable bundle.
fn bundle() -> R<PathBuf> {
    let ws = workspace_dir();
    let binary = build_release(&ws)?;
    let icon = manifest_dir().join("assets").join(ICON_NAME);
    let opts = BundleOpts {
        binary,
        terminfo: Some(compile_terminfo(&ws)?),
        out_dir: ws.join("target/release"),
        version: read_version(&ws.join("ghost-ui/Cargo.toml")),
        // A missing `.icns` just omits the icon; the bundle still builds.
        icon: icon.exists().then_some(icon),
    };
    assemble_bundle(&opts)
}

/// Compile ghost's vendored terminfo entry (`ghost-vt/assets`) into a fresh
/// database directory with the system `tic`, for embedding in the bundle.
fn compile_terminfo(ws: &Path) -> R<PathBuf> {
    let db = ws.join("target/release/bundle-terminfo");
    if db.exists() {
        fs::remove_dir_all(&db)?;
    }
    fs::create_dir_all(&db)?;
    let src = ws.join("ghost-vt/assets/xterm-kitty.terminfo");
    run_cmd(
        "tic",
        &["-x", "-o", &db.to_string_lossy(), &src.to_string_lossy()],
    )?;
    Ok(db)
}

/// (Re)generate `assets/ghost.icns` from `assets/ghost-icon.svg` via
/// `rsvg-convert` + `iconutil`. Run after editing the SVG.
fn generate_icon() -> R<PathBuf> {
    let assets = manifest_dir().join("assets");
    let svg = assets.join("ghost-icon.svg");
    if !svg.exists() {
        return Err(format!("missing icon source {}", svg.display()).into());
    }
    let iconset = assets.join("ghost.iconset");
    if iconset.exists() {
        fs::remove_dir_all(&iconset)?;
    }
    fs::create_dir_all(&iconset)?;
    // (px, filename) per Apple's iconset naming.
    for (px, name) in [
        (16, "icon_16x16.png"),
        (32, "icon_16x16@2x.png"),
        (32, "icon_32x32.png"),
        (64, "icon_32x32@2x.png"),
        (128, "icon_128x128.png"),
        (256, "icon_128x128@2x.png"),
        (256, "icon_256x256.png"),
        (512, "icon_256x256@2x.png"),
        (512, "icon_512x512.png"),
        (1024, "icon_512x512@2x.png"),
    ] {
        let px = px.to_string();
        run_cmd(
            "rsvg-convert",
            &[
                "-w",
                &px,
                "-h",
                &px,
                &svg.to_string_lossy(),
                "-o",
                &iconset.join(name).to_string_lossy(),
            ],
        )?;
    }
    let icns = assets.join(ICON_NAME);
    run_cmd(
        "iconutil",
        &[
            "-c",
            "icns",
            &iconset.to_string_lossy(),
            "-o",
            &icns.to_string_lossy(),
        ],
    )?;
    fs::remove_dir_all(&iconset)?;
    Ok(icns)
}

/// Run a command, erroring if it is missing or exits non-zero.
fn run_cmd(program: &str, args: &[&str]) -> R<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|e| format!("could not run `{program}`: {e}"))?;
    if !status.success() {
        return Err(format!("`{program}` failed").into());
    }
    Ok(())
}

/// Lay out `<out_dir>/ghost.app` from `opts`. Pure modulo the filesystem: no
/// discovery, no process spawning — that lives in [`bundle`].
fn assemble_bundle(opts: &BundleOpts) -> R<PathBuf> {
    let app = opts.out_dir.join(BUNDLE_NAME);
    if app.exists() {
        fs::remove_dir_all(&app)?; // start clean so stale files never linger
    }
    let macos = app.join("Contents/MacOS");
    fs::create_dir_all(&macos)?;
    let resources = app.join("Contents/Resources");
    fs::create_dir_all(&resources)?;

    // The real binary *is* `CFBundleExecutable`: a plain copy preserves its
    // ad-hoc linker signature, and with no argv it falls through to the GUI.
    let embedded = macos.join(EXECUTABLE);
    fs::copy(&opts.binary, &embedded)?;
    set_executable(&embedded)?;

    if let Some(icon) = &opts.icon {
        fs::copy(icon, resources.join(ICON_NAME))?;
    }

    if let Some(terminfo) = &opts.terminfo {
        copy_dir(terminfo, &resources.join("terminfo"))?;
    }

    fs::write(app.join("Contents/Info.plist"), info_plist(opts))?;
    fs::write(app.join("Contents/PkgInfo"), "APPL????")?;

    Ok(app)
}

// --- discovery / build helpers (not exercised by the unit test) -------------

/// This xtask crate's directory (holds `assets/`).
fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The `ghost` workspace root: parent of this xtask crate (xtask lives at root).
fn workspace_dir() -> PathBuf {
    manifest_dir()
        .parent()
        .expect("xtask lives under the ghost workspace")
        .to_path_buf()
}

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

fn build_release(ws: &Path) -> R<PathBuf> {
    let status = Command::new(cargo())
        .current_dir(ws)
        .args(["build", "--release", "-p", "ghost-ui"])
        .status()?;
    if !status.success() {
        return Err("`cargo build --release -p ghost-ui` failed".into());
    }
    let bin = ws.join("target/release/ghost");
    if !bin.exists() {
        return Err(format!("built binary not found at {}", bin.display()).into());
    }
    Ok(bin)
}

// --- prebuilt cross-builds (headless `ghost-host` for staging) --------------

/// Cross-build the headless `ghost-host` for each `triple` and copy it into the
/// prebuilt dir as `ghost-<os>-<arch>`, the exact name staging's resolver looks
/// for. No triples ⇒ this host OS's two arches. Builds continue past a failing
/// target (a missing toolchain) and the failures are reported at the end.
fn build_prebuilts(triples: &[String]) -> R<()> {
    let ws = workspace_dir();
    let defaults;
    let triples = if triples.is_empty() {
        defaults = default_triples();
        &defaults[..]
    } else {
        triples
    };
    // Validate every triple up front, so a typo fails before any long build.
    for t in triples {
        if triple_to_name(t).is_none() {
            return Err(format!("unsupported target triple: {t}").into());
        }
    }

    let out = prebuilt_dir();
    fs::create_dir_all(&out)?;
    // `cargo zigbuild` bundles sysroots for cross-OS; plain `cargo build` uses the
    // system toolchain (fine for a same-OS arch flip when it's installed).
    let subcommand = if std::env::var_os("GHOST_ZIGBUILD").is_some() {
        "zigbuild"
    } else {
        "build"
    };

    let host = host_triple();
    let mut failed = Vec::new();
    for triple in triples {
        let name = triple_to_name(triple).expect("validated above");
        println!("building ghost-host for {triple}…");
        // Building for the host's own triple: drop `--target`. A plain build uses
        // the native `cc`, whereas an explicit host target makes cc-rs reach for a
        // triple-prefixed cross compiler (`x86_64-linux-gnu-gcc`) that may be
        // absent or broken — so the "cross-build to your own arch" would fail where
        // a normal build succeeds.
        let native = host.as_deref() == Some(triple.as_str());
        let mut cmd = Command::new(cargo());
        cmd.current_dir(&ws)
            .args([subcommand, "--release", "-p", "ghost-host"]);
        if !native {
            cmd.args(["--target", triple]);
        }
        let ok = cmd.status().map(|s| s.success()).unwrap_or(false);
        if !ok {
            eprintln!("  ✗ {triple}: build failed");
            failed.push(triple.clone());
            continue;
        }
        let bin = if native {
            ws.join("target/release/ghost-host")
        } else {
            ws.join(format!("target/{triple}/release/ghost-host"))
        };
        let dest = out.join(&name);
        fs::copy(&bin, &dest)?;
        println!("  ✓ {triple} → {}", dest.display());
    }

    println!(
        "\nprebuilts in {} ({} of {} target(s) built)",
        out.display(),
        triples.len() - failed.len(),
        triples.len()
    );
    if !failed.is_empty() {
        return Err(format!(
            "could not build {} — install the target's toolchain, or set GHOST_ZIGBUILD=1",
            failed.join(", ")
        )
        .into());
    }
    Ok(())
}

/// The Rust host target triple (`rustc -vV`'s `host:` line), so a request for the
/// host's own arch can build without `--target`. `None` if `rustc` can't be run.
fn host_triple() -> Option<String> {
    let out = Command::new("rustc").arg("-vV").output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("host: ").map(|s| s.trim().to_string()))
}

/// This host OS's two arches — the same-OS arch flip that a normal toolchain
/// cross-builds (both apple arches on macOS, both linux arches on Linux). Cross-OS
/// targets must be named explicitly (and generally need `GHOST_ZIGBUILD=1`).
fn default_triples() -> Vec<String> {
    match std::env::consts::OS {
        "macos" => vec!["aarch64-apple-darwin".into(), "x86_64-apple-darwin".into()],
        _ => vec![
            "x86_64-unknown-linux-gnu".into(),
            "aarch64-unknown-linux-gnu".into(),
        ],
    }
}

/// Map a Rust target triple to the `ghost-<os>-<arch>` prebuilt filename staging's
/// resolver looks for, or `None` for a target ghost doesn't support.
fn triple_to_name(triple: &str) -> Option<String> {
    let arch = match triple.split('-').next()? {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        _ => return None,
    };
    let os = if triple.contains("linux") {
        "linux"
    } else if triple.contains("darwin") {
        "macos"
    } else {
        return None;
    };
    Some(format!("ghost-{os}-{arch}"))
}

/// Where prebuilts land: `GHOST_PREBUILT_DIR` if set (the resolver's first search
/// dir), else `<data_dir>/ghost/prebuilt` (its durable fallback). Mirrors
/// `ghost_vt::paths::data_dir` by hand — xtask stays zero-dependency on purpose.
fn prebuilt_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("GHOST_PREBUILT_DIR") {
        return PathBuf::from(d);
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            home.join(".local").join("share")
        });
    base.join("ghost").join("prebuilt")
}

/// `version` from the `[package]` table of a `Cargo.toml`.
fn read_version(manifest: &Path) -> String {
    let txt = fs::read_to_string(manifest).unwrap_or_default();
    let mut in_pkg = false;
    for line in txt.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix('[') {
            in_pkg = rest.starts_with("package]");
        } else if in_pkg
            && l.starts_with("version")
            && let Some(v) = l.split('"').nth(1)
        {
            return v.to_string();
        }
    }
    "0.0.0".into()
}

// --- bundle contents --------------------------------------------------------

fn info_plist(opts: &BundleOpts) -> String {
    let version = &opts.version;
    let icon = match opts.icon {
        Some(_) => format!("\t<key>CFBundleIconFile</key>\n\t<string>{ICON_NAME}</string>\n"),
        None => String::new(),
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>CFBundleName</key>\n\t<string>ghost</string>\n\
         \t<key>CFBundleDisplayName</key>\n\t<string>ghost</string>\n\
         \t<key>CFBundleIdentifier</key>\n\t<string>{APP_ID}</string>\n\
         \t<key>CFBundleExecutable</key>\n\t<string>{EXECUTABLE}</string>\n\
         \t<key>CFBundlePackageType</key>\n\t<string>APPL</string>\n\
         \t<key>CFBundleVersion</key>\n\t<string>{version}</string>\n\
         \t<key>CFBundleShortVersionString</key>\n\t<string>{version}</string>\n\
         {icon}\
         \t<key>LSMinimumSystemVersion</key>\n\t<string>11.0</string>\n\
         \t<key>NSHighResolutionCapable</key>\n\t<true/>\n\
         \t<key>LSApplicationCategoryType</key>\n\t<string>public.app-category.developer-tools</string>\n\
         </dict>\n\
         </plist>\n"
    )
}

// --- fs helpers -------------------------------------------------------------

#[cfg(unix)]
fn set_executable(p: &Path) -> R<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = fs::metadata(p)?.permissions();
    perm.set_mode(0o755);
    fs::set_permissions(p, perm)?;
    Ok(())
}

/// Recursively copy `src` into `dst`, preserving permission bits on files.
fn copy_dir(src: &Path, dst: &Path) -> R<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(&from)?.permissions().mode();
                fs::set_permissions(&to, fs::Permissions::from_mode(mode))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod prebuilt_tests {
    use super::*;

    #[test]
    fn triple_to_name_maps_supported_targets_and_rejects_others() {
        assert_eq!(
            triple_to_name("x86_64-unknown-linux-gnu").as_deref(),
            Some("ghost-linux-x86_64")
        );
        assert_eq!(
            triple_to_name("aarch64-unknown-linux-gnu").as_deref(),
            Some("ghost-linux-aarch64")
        );
        assert_eq!(
            triple_to_name("aarch64-apple-darwin").as_deref(),
            Some("ghost-macos-aarch64")
        );
        assert_eq!(
            triple_to_name("x86_64-apple-darwin").as_deref(),
            Some("ghost-macos-x86_64")
        );
        // musl is still linux.
        assert_eq!(
            triple_to_name("x86_64-unknown-linux-musl").as_deref(),
            Some("ghost-linux-x86_64")
        );
        // Unsupported arch or OS ⇒ no mapping.
        assert_eq!(triple_to_name("riscv64gc-unknown-linux-gnu"), None);
        assert_eq!(triple_to_name("x86_64-pc-windows-msvc"), None);
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("ghost-xtask-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// A bundle built with a stub binary, plus the scratch dir so the caller can
    /// clean up.
    fn build_test_bundle(icon: Option<PathBuf>) -> (PathBuf, PathBuf) {
        let dir = scratch();
        // A stand-in for the real binary so the test doesn't build ghost.
        let stub = dir.join("ghost");
        fs::write(&stub, b"#!/bin/sh\necho stub\n").unwrap();
        set_executable(&stub).unwrap();

        let opts = BundleOpts {
            binary: stub,
            out_dir: dir.join("out"),
            version: "1.2.3".into(),
            icon,
            terminfo: None,
        };
        let app = assemble_bundle(&opts).unwrap();
        // Idempotent: a second run over an existing bundle succeeds.
        assert!(assemble_bundle(&opts).is_ok());
        (dir, app)
    }

    #[test]
    fn embeds_a_terminfo_database_into_resources() {
        let dir = scratch();
        let stub = dir.join("ghost");
        fs::write(&stub, b"#!/bin/sh\necho stub\n").unwrap();
        set_executable(&stub).unwrap();
        // A stand-in compiled database (assemble only copies; `tic` runs in
        // `bundle()`), shaped like the layout macOS's tic produces.
        let db = dir.join("db");
        fs::create_dir_all(db.join("78")).unwrap();
        fs::write(db.join("78").join("xterm-kitty"), b"compiled-stub").unwrap();

        let opts = BundleOpts {
            binary: stub,
            out_dir: dir.join("out"),
            version: "1.2.3".into(),
            icon: None,
            terminfo: Some(db),
        };
        let app = assemble_bundle(&opts).unwrap();
        let entry = app.join("Contents/Resources/terminfo/78/xterm-kitty");
        assert!(entry.is_file(), "terminfo entry copied into Resources");
        assert_eq!(fs::read(&entry).unwrap(), b"compiled-stub");

        fs::remove_dir_all(&dir).ok();
    }

    fn plutil_lint(plist: &Path) -> bool {
        Command::new("plutil")
            .arg("-lint")
            .arg(plist)
            .status()
            .unwrap()
            .success()
    }

    #[test]
    fn assembles_a_valid_app_bundle() {
        // A stub icon so the test doesn't depend on the real asset.
        let icon_dir = scratch();
        let icon = icon_dir.join("ghost.icns");
        fs::write(&icon, b"icns-stub").unwrap();
        let (dir, app) = build_test_bundle(Some(icon));

        // Layout.
        assert!(app.join("Contents/Info.plist").is_file());
        assert!(app.join("Contents/PkgInfo").is_file());
        assert!(
            app.join("Contents/Resources/ghost.icns").is_file(),
            "icon copied into Resources"
        );

        // The real binary is `CFBundleExecutable` directly: it is executable, and
        // it is the *only* entry in MacOS (no launcher shim, no second binary).
        let exe = app.join("Contents/MacOS").join(EXECUTABLE);
        assert!(exe.is_file(), "embedded binary present");
        let mode = fs::metadata(&exe).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "binary is executable");
        let entries: Vec<_> = fs::read_dir(app.join("Contents/MacOS"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            entries,
            [std::ffi::OsString::from(EXECUTABLE)],
            "only ghost lives in MacOS"
        );

        assert_eq!(
            fs::read_to_string(app.join("Contents/PkgInfo")).unwrap(),
            "APPL????"
        );

        // Info.plist parses, and the keys round-trip per the system.
        let plist = app.join("Contents/Info.plist");
        assert!(plutil_lint(&plist), "plutil -lint failed");
        for (key, want) in [
            ("CFBundleExecutable", "ghost"),
            ("CFBundleShortVersionString", "1.2.3"),
            ("CFBundleIconFile", "ghost.icns"),
            ("CFBundleIdentifier", "dev.ghost.Terminal"),
        ] {
            let out = Command::new("plutil")
                .args(["-extract", key, "raw", "-o", "-"])
                .arg(&plist)
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), want, "{key}");
        }

        fs::remove_dir_all(&dir).ok();
        fs::remove_dir_all(&icon_dir).ok();
    }

    #[test]
    fn without_an_icon_omits_the_key() {
        let (dir, app) = build_test_bundle(None);
        let plist = app.join("Contents/Info.plist");
        assert!(plutil_lint(&plist), "plutil -lint failed");
        assert!(!app.join("Contents/Resources/ghost.icns").exists());
        // The CFBundleIconFile key must be absent (extract fails).
        assert!(
            !Command::new("plutil")
                .args(["-extract", "CFBundleIconFile", "raw", "-o", "-"])
                .arg(&plist)
                .status()
                .unwrap()
                .success(),
            "CFBundleIconFile should be absent without an icon"
        );
        fs::remove_dir_all(&dir).ok();
    }
}
