//! Packaging tasks for `ghost-gtk`.
//!
//! * `cargo xtask bundle`  — build `ghost-gtk` in release and assemble
//!   `target/release/ghost.app`.
//! * `cargo xtask install` — bundle, then copy the `.app` into `/Applications`.
//!
//! The bundle is **not** self-contained: it links the Homebrew GTK / libadwaita
//! / VTE stack at their absolute install paths, so it runs on this machine (and
//! any with the same Homebrew deps). A small launcher script sets the data-dir
//! environment variables GTK needs when the app is started from Finder (which
//! provides almost no environment). Fully relocatable bundling — copying every
//! dylib and GTK data file in and rewriting install names — is a separate, much
//! larger job and deliberately out of scope here.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

type R<T> = Result<T, Box<dyn Error>>;

/// `<bundle>.app` directory name.
const BUNDLE_NAME: &str = "ghost.app";
/// Must match `APP_ID` in `ghost-gtk`'s `main.rs`.
const APP_ID: &str = "dev.ghost.Terminal";
/// The launcher script (`CFBundleExecutable`) that sets up the environment.
const LAUNCHER: &str = "ghost";
/// The real `ghost-gtk` binary, embedded next to the launcher.
const REAL_BIN: &str = "ghost-bin";

/// Everything `assemble_bundle` needs, with no I/O of its own to discover — so
/// it stays a pure, testable transform from inputs to an on-disk bundle.
struct BundleOpts {
    /// Already-built `ghost-gtk` executable to embed.
    binary: PathBuf,
    /// Directory to create `ghost.app` in.
    out_dir: PathBuf,
    /// Homebrew prefix baked into the launcher (e.g. `/opt/homebrew`).
    prefix: String,
    /// `CFBundleShortVersionString` / `CFBundleVersion`.
    version: String,
    /// gdk-pixbuf `loaders.cache` to point the launcher at, if one was found.
    pixbuf_cache: Option<PathBuf>,
    /// `.icns` app icon to embed in `Resources/`, if one exists.
    icon: Option<PathBuf>,
}

/// Basename of the icon inside the bundle (and the `CFBundleIconFile` value).
const ICON_NAME: &str = "ghost.icns";

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
        other => {
            return Err(format!(
                "unknown command {:?}; use `bundle`, `install` or `icon`",
                other.unwrap_or("")
            )
            .into());
        }
    }
    Ok(())
}

/// Build `ghost-gtk` in release and assemble the bundle, discovering the
/// Homebrew prefix and supporting data along the way.
fn bundle() -> R<PathBuf> {
    let ws = workspace_dir();
    let binary = build_release(&ws)?;
    let prefix = brew_prefix();
    let icon = ws.join("assets").join(ICON_NAME);
    let opts = BundleOpts {
        binary,
        out_dir: ws.join("target/release"),
        pixbuf_cache: find_pixbuf_cache(&prefix),
        version: read_version(&ws),
        prefix,
        icon: icon.exists().then_some(icon),
    };
    assemble_bundle(&opts)
}

/// (Re)generate `assets/ghost.icns` from `assets/ghost-icon.svg` via
/// `rsvg-convert` + `iconutil`. Run after editing the SVG.
fn generate_icon() -> R<PathBuf> {
    let assets = workspace_dir().join("assets");
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

    // The real binary, then the launcher that runs it with the GTK env set.
    let embedded = macos.join(REAL_BIN);
    fs::copy(&opts.binary, &embedded)?;
    set_executable(&embedded)?;
    let launcher = macos.join(LAUNCHER);
    fs::write(&launcher, launcher_script(opts))?;
    set_executable(&launcher)?;

    if let Some(icon) = &opts.icon {
        fs::copy(icon, resources.join(ICON_NAME))?;
    }

    fs::write(app.join("Contents/Info.plist"), info_plist(opts))?;
    fs::write(app.join("Contents/PkgInfo"), "APPL????")?;

    Ok(app)
}

// --- discovery / build helpers (not exercised by the unit test) -------------

/// The `ghost-gtk` workspace root: parent of this xtask crate.
fn workspace_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives under the ghost-gtk workspace")
        .to_path_buf()
}

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

fn build_release(ws: &Path) -> R<PathBuf> {
    let status = Command::new(cargo())
        .current_dir(ws)
        .args(["build", "--release", "-p", "ghost-gtk"])
        .status()?;
    if !status.success() {
        return Err("`cargo build --release -p ghost-gtk` failed".into());
    }
    let bin = ws.join("target/release/ghost-gtk");
    if !bin.exists() {
        return Err(format!("built binary not found at {}", bin.display()).into());
    }
    Ok(bin)
}

/// `brew --prefix`, falling back to the Apple-silicon default.
fn brew_prefix() -> String {
    Command::new("brew")
        .arg("--prefix")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/opt/homebrew".into())
}

/// The first `lib/gdk-pixbuf-2.0/*/loaders.cache` under `prefix`, if any. (The
/// middle component is a gdk-pixbuf ABI version like `2.10.0`.)
fn find_pixbuf_cache(prefix: &str) -> Option<PathBuf> {
    let base = Path::new(prefix).join("lib/gdk-pixbuf-2.0");
    for entry in fs::read_dir(base).ok()?.flatten() {
        let cache = entry.path().join("loaders.cache");
        if cache.exists() {
            return Some(cache);
        }
    }
    None
}

/// `version` from the `[package]` table of the ghost-gtk `Cargo.toml`.
fn read_version(ws: &Path) -> String {
    let txt = fs::read_to_string(ws.join("Cargo.toml")).unwrap_or_default();
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

fn launcher_script(opts: &BundleOpts) -> String {
    let prefix = &opts.prefix;
    let pixbuf = match &opts.pixbuf_cache {
        Some(p) => format!("export GDK_PIXBUF_MODULE_FILE=\"{}\"\n", p.display()),
        None => String::new(),
    };
    format!(
        "#!/bin/sh\n\
         # Auto-generated by `cargo xtask`. Points GTK at the Homebrew data dirs\n\
         # (icons, GSettings schemas, image loaders) so the app works from Finder.\n\
         HERE=\"$(cd \"$(dirname \"$0\")\" && pwd)\"\n\
         PREFIX=\"{prefix}\"\n\
         export XDG_DATA_DIRS=\"$PREFIX/share:${{XDG_DATA_DIRS:-/usr/local/share:/usr/share}}\"\n\
         export GSETTINGS_SCHEMA_DIR=\"$PREFIX/share/glib-2.0/schemas\"\n\
         {pixbuf}\
         export PATH=\"$PREFIX/bin:$PATH\"\n\
         exec \"$HERE/{REAL_BIN}\" \"$@\"\n"
    )
}

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
         \t<key>CFBundleExecutable</key>\n\t<string>{LAUNCHER}</string>\n\
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

    /// A bundle built with `icon: Some(..)` and a stub binary, plus the scratch
    /// dir so the caller can clean up.
    fn build_test_bundle(icon: Option<PathBuf>) -> (PathBuf, PathBuf) {
        let dir = scratch();
        // A stand-in for the real binary so the test doesn't need GTK.
        let stub = dir.join("ghost-gtk");
        fs::write(&stub, b"#!/bin/sh\necho stub\n").unwrap();
        set_executable(&stub).unwrap();

        let opts = BundleOpts {
            binary: stub,
            out_dir: dir.join("out"),
            prefix: "/opt/homebrew".into(),
            version: "1.2.3".into(),
            pixbuf_cache: Some(PathBuf::from(
                "/opt/homebrew/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache",
            )),
            icon,
        };
        let app = assemble_bundle(&opts).unwrap();
        // Idempotent: a second run over an existing bundle succeeds.
        assert!(assemble_bundle(&opts).is_ok());
        (dir, app)
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
        let launcher = app.join("Contents/MacOS").join(LAUNCHER);
        let real = app.join("Contents/MacOS").join(REAL_BIN);
        assert!(launcher.is_file(), "launcher present");
        assert!(real.is_file(), "embedded binary present");
        assert!(
            app.join("Contents/Resources/ghost.icns").is_file(),
            "icon copied into Resources"
        );

        // Launcher is executable and execs the embedded binary with the env set.
        let mode = fs::metadata(&launcher).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "launcher is executable");
        let script = fs::read_to_string(&launcher).unwrap();
        assert!(script.contains(&format!("exec \"$HERE/{REAL_BIN}\"")));
        assert!(script.contains("GDK_PIXBUF_MODULE_FILE"));
        assert!(script.contains("GSETTINGS_SCHEMA_DIR"));

        // Info.plist parses, and the version + icon round-trip per the system.
        let plist = app.join("Contents/Info.plist");
        assert!(plutil_lint(&plist), "plutil -lint failed");
        for (key, want) in [
            ("CFBundleShortVersionString", "1.2.3"),
            ("CFBundleIconFile", "ghost.icns"),
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
