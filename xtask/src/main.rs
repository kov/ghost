//! Packaging tasks for `ghost`.
//!
//! * `cargo xtask bundle`  — build `ghost` in release and assemble
//!   `target/release/ghost.app`.
//! * `cargo xtask install` — on macOS, bundle and copy the `.app` into
//!   `/Applications`; elsewhere a freedesktop install into `--prefix <dir>`
//!   (default `$HOME/.local`): `bin/ghost`, the `.desktop` entry in
//!   `share/applications`, and the icon in the hicolor theme. Either way it then
//!   refreshes the staging prebuilts, so what ghost copies to a remote is the
//!   build just installed; a prebuilt that cannot be built warns and no more.
//! * `cargo xtask icon`    — regenerate `assets/ghost.icns` from the SVG.
//! * `cargo xtask prebuilt [<triple>…]` — cross-build the headless `ghost-host`
//!   for each target and drop it in the prebuilt dir as `ghost-<os>-<arch>`, where
//!   staging's resolver finds it. No triples ⇒ every supported platform but this
//!   one. Set `GHOST_ZIGBUILD=1` to build through `cargo zigbuild` (bundles its
//!   own sysroots, so cross-OS builds need no system cross-toolchain). A darwin
//!   target links CoreServices and CoreFoundation (the session watcher's FSEvents
//!   backend), so crossing to one also needs a macOS SDK: it is picked up from
//!   `$XDG_DATA_HOME/ghost/macos-sdk/MacOSX.sdk` (or `GHOST_MACOS_SDK`) with no
//!   flag to remember. Nobody need go to Apple for it — copy
//!   `/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk` off a Mac you already
//!   have.
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
/// Must match `ghost_ui::APP_ID` (the app id every window carries).
const APP_ID: &str = "dev.ghost.Terminal";
/// The bundle executable (`CFBundleExecutable`) — the real `ghost` binary.
const EXECUTABLE: &str = "ghost";
/// Basename of the icon inside the bundle (and the `CFBundleIconFile` value).
const ICON_NAME: &str = "ghost.icns";
/// Freedesktop icon name — the app id, so a `.desktop` file named after the id
/// and a window carrying it as its app id both resolve to the same artwork.
const ICON_ID: &str = APP_ID;

/// The `.desktop` entry shipped in `assets/`, installed verbatim.
fn desktop_asset() -> PathBuf {
    manifest_dir()
        .join("assets")
        .join(format!("{APP_ID}.desktop"))
}

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
        // macOS installs the `.app`; everywhere else it is a freedesktop install
        // (binary on PATH + `.desktop` entry + hicolor icon).
        Some("install") if cfg!(target_os = "macos") => {
            let app = bundle()?;
            let dest = Path::new("/Applications").join(BUNDLE_NAME);
            if dest.exists() {
                fs::remove_dir_all(&dest)?;
            }
            copy_dir(&app, &dest)?;
            println!("installed {}", dest.display());
            install_prebuilts();
        }
        Some("install") => {
            let prefix = install_prefix(&std::env::args().skip(2).collect::<Vec<_>>())?;
            let binary = build_release(&workspace_dir())?;
            install_freedesktop(&binary, &prefix)?;
            println!("installed into {}", prefix.display());
            install_prebuilts();
            if !on_path(&prefix.join("bin")) {
                println!(
                    "note: {} is not on your PATH — the desktop entry runs `ghost`",
                    prefix.join("bin").display()
                );
            }
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

/// Where a freedesktop install goes: `--prefix <dir>` if given, else `$GHOST_PREFIX`,
/// else `$HOME/.local` — a user install that needs no root.
fn install_prefix(args: &[String]) -> R<PathBuf> {
    match args {
        [] => {}
        [flag, dir] if flag == "--prefix" => return Ok(PathBuf::from(dir)),
        _ => return Err("usage: cargo xtask install [--prefix <dir>]".into()),
    }
    if let Some(p) = std::env::var_os("GHOST_PREFIX") {
        return Ok(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME").ok_or("neither --prefix nor $HOME to install into")?;
    Ok(PathBuf::from(home).join(".local"))
}

/// Install `binary` under `prefix` the freedesktop way: the executable in
/// `bin/`, the `.desktop` entry in `share/applications/`, and the same SVG the
/// macOS `.icns` is rendered from in the hicolor theme's scalable app icons.
///
/// The binary is written to a temporary name and renamed into place, so
/// re-installing while a ghost is running replaces the file rather than writing
/// through the inode the running process is executing (which would fail with
/// `ETXTBSY`, or worse, corrupt it).
fn install_freedesktop(binary: &Path, prefix: &Path) -> R<()> {
    let bin_dir = prefix.join("bin");
    let apps = prefix.join("share/applications");
    let icons = prefix.join("share/icons/hicolor/scalable/apps");
    for dir in [&bin_dir, &apps, &icons] {
        fs::create_dir_all(dir)?;
    }

    let staged = bin_dir.join(".ghost.new");
    fs::copy(binary, &staged)?;
    set_executable(&staged)?;
    fs::rename(&staged, bin_dir.join(EXECUTABLE))?;

    fs::copy(desktop_asset(), apps.join(format!("{APP_ID}.desktop")))?;
    fs::copy(
        manifest_dir().join("assets").join("ghost-icon.svg"),
        icons.join(format!("{ICON_ID}.svg")),
    )?;
    Ok(())
}

/// Whether `dir` is on this shell's `PATH` — used only to warn, since the
/// desktop entry's `Exec=ghost` needs it to be.
fn on_path(dir: &Path) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d == dir))
        .unwrap_or(false)
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
    // `GHOST_ZIGBUILD=1` forces zig for *everything*; otherwise it is chosen per
    // target, for the ones that actually need it.
    let force_zig = std::env::var_os("GHOST_ZIGBUILD").is_some();

    let host = host_triple();
    // Found once, not per target: both darwin arches link against the same one.
    let sdk = macos_sdk();
    let mut failed = Vec::new();
    for triple in triples {
        let name = triple_to_name(triple).expect("validated above");
        println!("building ghost-host for {triple}…");
        // Building for the host's own triple: drop `--target` so the build reuses
        // the host's configured linker and `target/release/`. Everything else is a
        // cross build; make the target available first (idempotent).
        let native = host.as_deref() == Some(triple.as_str());
        let subcommand = if force_zig || wants_zig(triple, std::env::consts::OS) {
            "zigbuild"
        } else {
            "build"
        };
        let mut cmd = Command::new(cargo());
        cmd.current_dir(&ws)
            .args([subcommand, "--release", "-p", "ghost-host"]);
        if !native {
            let _ = Command::new("rustup")
                .args(["target", "add", triple])
                .status();
            cmd.args(["--target", triple]);
        }
        // ghost-host is pure Rust, so a musl target links self-contained with the
        // bundled `rust-lld` — no C toolchain or sysroot. Force that linker via the
        // whole-invocation RUSTFLAGS (which *replaces* any config rustflags) so a
        // host-wide `[target.'cfg(target_os="linux")']` linker setting doesn't feed
        // the cross build its host-arch linker. A per-target override would instead
        // *combine* with that config and pass e.g. `-fuse-ld=mold` on to rust-lld.
        // Crossing to darwin from elsewhere: hand the linker the SDK so it can
        // resolve CoreServices and CoreFoundation, which the watcher's FSEvents
        // backend needs. On a Mac this is already the compiler's own default, so
        // don't override it there.
        if triple.contains("darwin")
            && std::env::consts::OS != "macos"
            && let Some(sdk) = &sdk
        {
            cmd.env("SDKROOT", sdk);
        }
        if triple.contains("musl") && subcommand == "build" {
            let mut flags = std::env::var("RUSTFLAGS").unwrap_or_default();
            if !flags.is_empty() {
                flags.push(' ');
            }
            flags.push_str("-C linker=rust-lld");
            cmd.env("RUSTFLAGS", flags);
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
        return Err(failure_hint(&failed, std::env::consts::OS, sdk.as_deref()).into());
    }
    Ok(())
}

/// What an install should say about a prebuilt refresh that did not fully
/// succeed — `None` when it did.
///
/// An install never *fails* on this. The prebuilts are for staging onto other
/// machines, and someone installing ghost here without a cross toolchain still
/// wants ghost installed here. But it is said out loud, with the command to retry
/// on its own: a prebuilt that silently stops being rebuilt is one that goes on
/// being staged to remotes long after it stopped matching this build, and a
/// remote host older than its client is a session that misbehaves in ways nobody
/// traces back to a stale file.
/// Whether `triple` has to be linked through `cargo zigbuild` — which bundles the
/// sysroot for the target OS — rather than a plain `cargo build`.
///
/// Only crossing the *OS* boundary needs it; an arch flip within one OS links with
/// the toolchain already here. Decided per target rather than per run so
/// `cargo xtask install` refreshes the darwin prebuilts too: making that depend on
/// an environment variable somebody has to remember is how they went stale.
fn wants_zig(triple: &str, host_os: &str) -> bool {
    triple.contains("darwin") != (host_os == "macos")
}

/// Cut the staging prebuilts as part of an install, so the binary ghost copies to
/// a remote is always the one this install just built. They used to be refreshed
/// only when someone remembered to, by hand, which is to say rarely: the
/// difference between a client and the host it staged was free to grow until
/// something broke in a way that looked like anything but version skew.
fn install_prebuilts() {
    println!("refreshing the staging prebuilts…");
    if let Some(warning) = prebuilt_refresh_warning(build_prebuilts(&[])) {
        eprintln!("{warning}");
    }
}

fn prebuilt_refresh_warning(result: R<()>) -> Option<String> {
    let e = result.err()?;
    Some(format!(
        "warning: the staging prebuilts were not fully refreshed: {e}\n\
         note: ghost is installed regardless; retry them with `cargo xtask prebuilt`"
    ))
}

/// The Rust host target triple (`rustc -vV`'s `host:` line), so a request for the
/// host's own arch can build without `--target`. `None` if `rustc` can't be run.
fn host_triple() -> Option<String> {
    let out = Command::new("rustc").arg("-vV").output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("host: ").map(|s| s.trim().to_string()))
}

/// Every platform ghost can stage to. Linux uses the **musl** targets: they link
/// self-contained with the bundled `rust-lld` (so `rustup target add` is the only
/// setup — no C toolchain, no sysroot) and produce a static binary that runs on
/// any remote regardless of its glibc. macOS uses the native darwin targets;
/// Apple's toolchain cross-builds both arches, and reaching them from Linux wants
/// `GHOST_ZIGBUILD=1`.
const SUPPORTED_TRIPLES: [&str; 4] = [
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
];

/// The triples to build when none are named: every supported platform *except*
/// the one we are building on.
///
/// Staging resolves the local platform from `current_exe` and only ever consults
/// a prebuilt for a platform that differs (see `resolve_for` in ghost-vt), so the
/// local triple is the one target that can never be needed — and the cross-OS
/// ones, which used to require naming a triple by hand, are the whole reason
/// prebuilts exist. `host` is matched by os+arch rather than by triple, so a
/// distro rustc reporting `…-linux-gnu` still counts as covering the musl target
/// for that same platform.
fn default_triples_for(host: Option<&str>) -> Vec<String> {
    let local = host.and_then(triple_to_name);
    SUPPORTED_TRIPLES
        .iter()
        .filter(|t| local.is_none() || triple_to_name(t) != local)
        .map(|t| (*t).to_string())
        .collect()
}

/// [`default_triples_for`] against this machine's own rustc host triple.
fn default_triples() -> Vec<String> {
    default_triples_for(host_triple().as_deref())
}

/// Why `failed` could not be built, and what to do about it.
///
/// This used to single out a macOS target from a non-Mac as needing Apple's SDK,
/// and steer people toward building it on a Mac instead. That is no longer true:
/// the frameworks came from `fsevent-sys` alone, and with the session watcher on
/// kqueue (see `ghost-vt`'s `notify` dependency) a darwin prebuilt links from
/// Linux with zig and nothing else. Every failure now has the same two causes —
/// a missing target, or a cross-OS link with no zig behind it.
/// The per-user data base everything durable-but-not-source lives under.
fn data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            home.join(".local").join("share")
        })
}

/// Where a macOS SDK is looked for when cross-building the darwin prebuilts,
/// given an explicit `GHOST_MACOS_SDK` and the data home to fall back under.
///
/// Kept beside the prebuilts it exists to build, for the same reason they are not
/// in the source tree: it outlives any one checkout, and a build should not
/// depend on where somebody happened to clone. Pure, so the layout is testable
/// without an SDK on disk.
fn macos_sdk_path(explicit: Option<PathBuf>, data_home: &Path) -> PathBuf {
    explicit.unwrap_or_else(|| data_home.join("ghost").join("macos-sdk").join("MacOSX.sdk"))
}

/// The macOS SDK to cross-build darwin targets against, if one is there.
fn macos_sdk() -> Option<PathBuf> {
    let p = macos_sdk_path(
        std::env::var_os("GHOST_MACOS_SDK").map(PathBuf::from),
        &data_home(),
    );
    p.is_dir().then_some(p)
}

fn failure_hint(failed: &[String], host_os: &str, sdk: Option<&Path>) -> String {
    // Cross-OS is what needs zig; a musl target from Linux links self-contained
    // with the bundled rust-lld, so don't send that case looking for it.
    let cross_os = failed
        .iter()
        .any(|t| t.contains("darwin") != (host_os == "macos"));
    let mut hint = format!(
        "could not build {} — install that target's toolchain \
         (`rustup target add <triple>`)",
        failed.join(", ")
    );
    if cross_os {
        hint.push_str(
            ", and set GHOST_ZIGBUILD=1 to link across operating systems \
             (needs `cargo install cargo-zigbuild` and zig on PATH)",
        );
    }
    // A darwin prebuilt links CoreServices and CoreFoundation, so crossing to it
    // needs an SDK. Nobody has to go to Apple for one: a Mac you can already ssh
    // to is carrying it, and copying that directory here is the whole method.
    if host_os != "macos" && failed.iter().any(|t| t.contains("darwin")) {
        match sdk {
            Some(sdk) => hint.push_str(&format!(
                ". It built against the SDK at {} — if that is the wrong one, point \
                 GHOST_MACOS_SDK at another",
                sdk.display()
            )),
            None => hint.push_str(&format!(
                ". A darwin target also needs a macOS SDK, and none is at {}: copy \
                 /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk off a Mac you \
                 already have (`ssh <mac> tar -czf - -C <that dir> MacOSX.sdk`), or \
                 set GHOST_MACOS_SDK to one",
                macos_sdk_path(None, &data_home()).display()
            )),
        }
    }
    hint
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
    data_home().join("ghost").join("prebuilt")
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

    /// Which targets need `cargo zigbuild` rather than a plain `cargo build`.
    ///
    /// This has to be decided per target, not per run. `cargo xtask install`
    /// refreshes everything without being handed an environment variable, and if
    /// reaching across operating systems needed one, the darwin prebuilts would go
    /// on being skipped by every install — which is the staleness this was all
    /// meant to end.
    #[test]
    fn only_a_cross_os_target_is_built_through_zig() {
        // From Linux: darwin crosses the OS boundary, musl does not.
        assert!(wants_zig("aarch64-apple-darwin", "linux"));
        assert!(!wants_zig("x86_64-unknown-linux-musl", "linux"));
        // And the mirror image from a Mac.
        assert!(wants_zig("x86_64-unknown-linux-musl", "macos"));
        assert!(!wants_zig("x86_64-apple-darwin", "macos"));
    }

    /// An install refreshes the prebuilts, and a prebuilt it cannot build must not
    /// take the install down with it. The two are only loosely related: prebuilts
    /// are for staging onto *other* machines, and someone installing ghost here
    /// with no cross toolchain installed still wants ghost installed here. The
    /// failure is worth saying out loud, though — a silent skip is how they went
    /// stale in the first place.
    #[test]
    fn a_prebuilt_that_cannot_be_built_warns_instead_of_failing_the_install() {
        let warning = prebuilt_refresh_warning(Err("no toolchain for it".into()))
            .expect("a failure is reported");
        assert!(
            warning.contains("no toolchain for it"),
            "carries why it failed: {warning}"
        );
        assert!(
            warning.contains("cargo xtask prebuilt"),
            "names how to retry it on its own: {warning}"
        );
        // Nothing to say when they all built.
        assert_eq!(prebuilt_refresh_warning(Ok(())), None);
    }

    #[test]
    fn a_failed_macos_target_says_where_its_sdk_should_be() {
        // A darwin prebuilt links CoreServices and CoreFoundation (the watcher's
        // FSEvents backend), so it needs an SDK — and the one thing this hint must
        // never do is stay quiet about which. It briefly claimed no SDK was
        // involved, which was true only while the watcher was on a backend that
        // could not watch a session directory at all.
        let sdk = Path::new("/somewhere/MacOSX.sdk");
        let hint = failure_hint(&["aarch64-apple-darwin".to_string()], "linux", None);
        assert!(
            hint.contains("GHOST_MACOS_SDK") && hint.contains("macos-sdk"),
            "names the variable and the place it is looked for: {hint}"
        );
        assert!(
            hint.contains("Mac you already have"),
            "names where an SDK comes from — copying one is the whole method: {hint}"
        );
        assert!(
            hint.contains("GHOST_ZIGBUILD"),
            "names what a cross-OS link does need: {hint}"
        );
        assert!(
            hint.contains("rustup target add"),
            "names what to install: {hint}"
        );

        // With one already in place the SDK is not the problem, so don't send
        // anyone to fetch a second copy of what they have.
        let hint = failure_hint(&["aarch64-apple-darwin".to_string()], "linux", Some(sdk));
        assert!(
            !hint.contains("Mac you already have"),
            "an SDK that is present is not the missing piece: {hint}"
        );
        assert!(
            hint.contains("/somewhere/MacOSX.sdk"),
            "says which one it built against, so a wrong one can be spotted: {hint}"
        );

        // A Linux target failing is the same story with one less step — no zig
        // needed for a musl target, which links with the bundled rust-lld.
        let hint = failure_hint(&["x86_64-unknown-linux-musl".to_string()], "macos", None);
        assert!(
            !hint.contains("SDK"),
            "a musl target needs no Apple SDK: {hint}"
        );
        assert!(
            hint.contains("rustup target add"),
            "names what to install: {hint}"
        );
    }

    #[test]
    fn the_sdk_is_looked_for_beside_the_prebuilts_it_builds() {
        // Found without being told, in a place that is not the source tree — the
        // same reasoning as the prebuilt dir, and next to it.
        assert_eq!(
            macos_sdk_path(None, Path::new("/data")),
            Path::new("/data/ghost/macos-sdk/MacOSX.sdk"),
        );
        // An explicit one wins, so a differently-versioned SDK can be pointed at
        // without moving anything.
        assert_eq!(
            macos_sdk_path(Some("/opt/MacOSX15.sdk".into()), Path::new("/data")),
            Path::new("/opt/MacOSX15.sdk"),
        );
    }

    #[test]
    fn the_defaults_cover_every_platform_except_the_one_we_build_on() {
        // Staging serves the LOCAL platform from `current_exe`; a prebuilt is only
        // ever consulted for a platform that differs. Defaulting to the host's own
        // OS was therefore backwards: on a Mac it produced two macOS binaries and
        // no Linux one, so a Mac could never stage to a Linux host — the case
        // prebuilts exist for. (The host is excluded by os+arch, not by triple, so
        // a gnu host still counts as covering the musl target for its platform.)
        let mac = default_triples_for(Some("aarch64-apple-darwin"));
        assert!(
            mac.iter().any(|t| t == "aarch64-unknown-linux-musl"),
            "a Mac must build the Linux prebuilts: {mac:?}"
        );
        assert!(
            mac.iter().any(|t| t == "x86_64-unknown-linux-musl"),
            "a Mac must build the Linux prebuilts: {mac:?}"
        );
        assert!(
            mac.iter().any(|t| t == "x86_64-apple-darwin"),
            "an Intel Mac remote needs a prebuilt too — same OS, different arch: {mac:?}"
        );
        assert!(
            !mac.iter().any(|t| t == "aarch64-apple-darwin"),
            "the local platform is `current_exe`, never a prebuilt: {mac:?}"
        );

        // The mirror case, and the reason the host is matched by os+arch: a distro
        // rustc reports a *gnu* triple, but the Linux prebuilt we build is *musl*.
        let linux = default_triples_for(Some("aarch64-unknown-linux-gnu"));
        assert!(
            linux.iter().any(|t| t == "aarch64-apple-darwin"),
            "a Linux box must build the macOS prebuilts: {linux:?}"
        );
        assert!(
            linux.iter().any(|t| t == "x86_64-unknown-linux-musl"),
            "the other Linux arch still needs one: {linux:?}"
        );
        assert!(
            !linux.iter().any(|t| t == "aarch64-unknown-linux-musl"),
            "gnu host covers its own os+arch, so its musl twin is not needed: {linux:?}"
        );

        // No rustc to ask: build everything rather than silently skipping one.
        assert_eq!(default_triples_for(None).len(), 4);
    }

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

    /// Whether `triple`'s std is installed, so a check for it can even be asked
    /// for. Skipping on a missing target is the same bargain the
    /// `desktop-file-validate` test makes: assert where the tool is there, stay
    /// quiet where it is not.
    fn target_installed(triple: &str) -> bool {
        Command::new("rustup")
            .args(["target", "list", "--installed"])
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .any(|l| l.trim() == triple)
            })
            .unwrap_or(false)
    }

    /// `ghost-host` — the binary staging copies to a remote — must *compile* for
    /// every platform ghost is willing to stage to. It is easy to break without
    /// noticing: the whole workspace builds for this machine, and a Linux-only
    /// syscall reaching `ghost-vt` breaks only the cross targets, which nothing
    /// here builds by default. That is how `libc::pipe2` left the macOS prebuilt
    /// unbuildable — and so frozen at whatever vintage last built — while every
    /// local build and test stayed green.
    ///
    /// A check, not a build: this catches the portability breakage, and asking
    /// for a link as well would drag in `cargo-zigbuild` and a much longer test.
    /// The link is exercised by actually cutting prebuilts (`cargo xtask
    /// prebuilt`).
    #[test]
    fn the_staged_binary_compiles_for_every_platform_we_stage_to() {
        let ws = workspace_dir();
        // Every triple is checked before anything is asserted: these breakages
        // come one per platform-specific symbol, and stopping at the first would
        // hide the next behind a second full run.
        let mut broken = Vec::new();
        for triple in SUPPORTED_TRIPLES {
            if !target_installed(triple) {
                eprintln!("skipping {triple}: `rustup target add {triple}` to cover it");
                continue;
            }
            let out = Command::new(cargo())
                .current_dir(&ws)
                .args(["check", "--target", triple, "-p", "ghost-host"])
                .output()
                .expect("run cargo check");
            if !out.status.success() {
                broken.push(format!(
                    "--- {triple} ---\n{}",
                    String::from_utf8_lossy(&out.stderr)
                ));
            }
        }
        assert!(
            broken.is_empty(),
            "ghost-host does not compile for {} of the platforms it is staged to:\n{}",
            broken.len(),
            broken.join("\n")
        );
    }
}

#[cfg(test)]
mod desktop_tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ghost-xtask-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// The value of `key` in `section` ("Desktop Entry", "Desktop Action foo", …).
    fn entry(text: &str, section: &str, key: &str) -> Option<String> {
        let mut here = false;
        for line in text.lines() {
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                here = name == section;
            } else if here && let Some(v) = line.strip_prefix(&format!("{key}=")) {
                return Some(v.to_string());
            }
        }
        None
    }

    #[test]
    fn the_desktop_entry_launches_ghost_and_offers_a_new_ssh_window() {
        let text = fs::read_to_string(desktop_asset()).expect("the desktop entry ships in assets");
        assert_eq!(
            entry(&text, "Desktop Entry", "Exec").as_deref(),
            Some("ghost")
        );
        assert_eq!(
            entry(&text, "Desktop Entry", "Icon").as_deref(),
            Some(ICON_ID),
            "the icon name is the app id, so the file dropped in hicolor is found"
        );
        assert!(
            entry(&text, "Desktop Entry", "Categories")
                .unwrap_or_default()
                .contains("TerminalEmulator"),
            "ghost lists itself as a terminal"
        );
        // The right-click / dock action asked for: same thing as Alt+S.
        assert_eq!(
            entry(&text, "Desktop Action new-ssh-window", "Name").as_deref(),
            Some("New SSH Window")
        );
        assert_eq!(
            entry(&text, "Desktop Action new-ssh-window", "Exec").as_deref(),
            Some("ghost --ssh-window"),
            "the action must use the flag the GUI parses"
        );
        assert!(
            entry(&text, "Desktop Entry", "Actions")
                .unwrap_or_default()
                .contains("new-ssh-window"),
            "an action nobody lists is an action nobody sees"
        );
        // The file's name is what the compositor matches against the window's
        // app id, so it must be the app id itself.
        assert_eq!(
            desktop_asset().file_name().unwrap().to_string_lossy(),
            format!("{APP_ID}.desktop")
        );
    }

    #[test]
    fn the_desktop_entry_passes_desktop_file_validate() {
        // The freedesktop validator is the authority on the format; skip where it
        // isn't installed rather than assert a hand-rolled subset of the spec.
        let Ok(out) = Command::new("desktop-file-validate")
            .arg(desktop_asset())
            .output()
        else {
            return;
        };
        assert!(
            out.status.success(),
            "desktop-file-validate: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn a_freedesktop_install_lands_the_binary_the_entry_and_the_icon() {
        let dir = scratch("fdo");
        let stub = dir.join("ghost");
        fs::write(&stub, b"#!/bin/sh\necho stub\n").unwrap();
        set_executable(&stub).unwrap();
        let prefix = dir.join("prefix");

        install_freedesktop(&stub, &prefix).unwrap();

        let bin = prefix.join("bin/ghost");
        assert!(bin.is_file(), "the binary is installed on PATH as `ghost`");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&bin).unwrap().permissions().mode() & 0o111,
                0o111
            );
        }
        let desktop = prefix.join(format!("share/applications/{APP_ID}.desktop"));
        assert!(desktop.is_file(), "the desktop entry is installed");
        assert_eq!(
            fs::read_to_string(&desktop).unwrap(),
            fs::read_to_string(desktop_asset()).unwrap(),
            "installed verbatim — the asset is the source of truth"
        );
        // The same artwork the macOS bundle's `.icns` is generated from, dropped
        // where the icon theme spec looks for a scalable app icon.
        let icon = prefix.join(format!("share/icons/hicolor/scalable/apps/{ICON_ID}.svg"));
        assert!(icon.is_file(), "the icon is installed as {ICON_ID}.svg");
        assert_eq!(
            fs::read(&icon).unwrap(),
            fs::read(manifest_dir().join("assets/ghost-icon.svg")).unwrap(),
            "the same source the .icns is rendered from"
        );

        // Re-installing over an existing prefix succeeds (and replaces the binary
        // by rename, so a running ghost's inode is never written through).
        install_freedesktop(&stub, &prefix).unwrap();

        fs::remove_dir_all(&dir).ok();
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
