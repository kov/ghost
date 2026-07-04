//! SSH-as-transport: attach to a *real ghost host* over an SSH tunnel. The
//! remote host is a real local session; the "ssh" is a shim that strips ssh's
//! options + destination and execs the remainder (`ghost __pipe <name>`) locally
//! — so the whole path is exercised (transport argv → SshChild → `ghost __pipe`
//! relay → the host's control socket → framed protocol back) with no network and
//! no real ssh.
//!
//! Distinct from `ssh.rs` (the local ssh *child*): there the session's child is
//! `ssh …`; here the session is an ordinary host and ssh is only the transport.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use ghost_vt::client::{Session, Subscriber};
use ghost_vt::connection::ConnectionSpec;
use ghost_vt::descriptor::Descriptor;
use ghost_vt::protocol::SessionEvent;
use ghost_vt::screen::Screen;

const GHOST: &str = env!("CARGO_BIN_EXE_ghost");

/// A fake `ssh`: consume ssh options (with `-p/-i/-J/-o` taking a value) and the
/// destination, then run whatever remote command follows — locally. Like real
/// ssh, it does NOT preserve argv boundaries: it space-joins the remote words and
/// hands them to a shell (`sh -c "$*"`), so the command must be quoted to survive
/// (this is what catches remote-quoting bugs). With no remote command (the ssh
/// *child* form, `ssh <target>`), it drops into a shell like real ssh.
fn shim_ssh() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let ssh = dir.path().join("ssh");
    std::fs::write(
        &ssh,
        "#!/bin/sh\n\
         while [ $# -gt 0 ]; do\n\
         \x20 case \"$1\" in\n\
         \x20   -p|-i|-J|-o) shift 2 ;;\n\
         \x20   -*) shift ;;\n\
         \x20   *) shift; break ;;\n\
         \x20 esac\n\
         done\n\
         [ $# -eq 0 ] && exec sh\n\
         exec sh -c \"$*\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

fn ghost(xdg: &Path) -> Command {
    let mut c = Command::new(GHOST);
    c.env("XDG_RUNTIME_DIR", xdg.join("run"));
    c.env("XDG_DATA_HOME", xdg.join("data"));
    c
}

fn ls(xdg: &Path) -> String {
    let out = ghost(xdg).arg("ls").output().expect("run `ghost ls`");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if pred() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

struct KillOnDrop<'a> {
    xdg: &'a Path,
    name: &'a str,
}

impl Drop for KillOnDrop<'_> {
    fn drop(&mut self) {
        let _ = ghost(self.xdg).args(["kill", self.name]).output();
    }
}

/// The ssh transport command a real attach builds, but with the shim as `ssh`
/// and this test binary as the remote `ghost`.
fn transport_cmd(xdg: &Path, ssh: &Path, spec: &ConnectionSpec, name: &str) -> Command {
    let argv = spec.transport_argv(GHOST, name);
    // Drop the leading "ssh" — we invoke the shim directly as the program.
    let mut c = Command::new(ssh);
    c.args(&argv[1..]);
    // The exec'd `ghost __pipe` must find the same session dir as the host.
    c.env("XDG_RUNTIME_DIR", xdg.join("run"));
    c.env("XDG_DATA_HOME", xdg.join("data"));
    c
}

#[test]
fn attaching_over_the_ssh_transport_shows_the_remote_hosts_screen() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let shim = shim_ssh();
    let ssh = shim.path().join("ssh");

    // The "remote" host: an ordinary local session whose child prints a marker
    // then idles, so its screen has content to replay on attach.
    let out = ghost(xdg)
        .args([
            "new",
            "-d",
            "remote",
            "--",
            "sh",
            "-c",
            "echo REMOTE-HELLO; exec sh",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _guard = KillOnDrop {
        xdg,
        name: "remote",
    };
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains("remote")),
        "the host session never listed"
    );

    // Attach over the SSH transport (via the shim) and watch the screen: the
    // host's output arrives across the tunnel, framed protocol intact.
    let spec = ConnectionSpec::parse_target("dev@example").unwrap();
    let cmd = transport_cmd(xdg, &ssh, &spec, "remote");
    let mut session = Session::attach_ssh(cmd, "remote", 80, 24).expect("attach over ssh");
    session
        .set_read_timeout(Some(Duration::from_millis(25)))
        .unwrap();

    let mut screen = Screen::new(80, 24, 100);
    assert!(
        wait_until(Duration::from_secs(10), || {
            if let Ok(p) = session.pump() {
                screen.feed(&p.output);
            }
            screen.text().join("\n").contains("REMOTE-HELLO")
        }),
        "the remote host's screen never arrived over the ssh transport; saw:\n{}",
        screen.text().join("\n")
    );

    // The tunnel is bidirectional: typed input reaches the remote child and its
    // echo comes back.
    session.send_input(b"echo TUNNELED-BACK\n").unwrap();
    assert!(
        wait_until(Duration::from_secs(10), || {
            if let Ok(p) = session.pump() {
                screen.feed(&p.output);
            }
            screen.text().join("\n").contains("TUNNELED-BACK")
        }),
        "input never round-tripped over the ssh transport; saw:\n{}",
        screen.text().join("\n")
    );
}

#[test]
fn observing_over_the_ssh_transport_mirrors_the_remote_screen() {
    // R2 live remote previews: a read-only observe over the transport delivers the
    // remote session's snapshot + grid + current screen, same as a local observe.
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let shim = shim_ssh();
    let ssh = shim.path().join("ssh");

    ghost(xdg)
        .args([
            "new",
            "-d",
            "watched",
            "--",
            "sh",
            "-c",
            "printf OBSERVED-REMOTE; sleep 60",
        ])
        .output()
        .unwrap();
    let _guard = KillOnDrop {
        xdg,
        name: "watched",
    };
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains("watched")),
        "the host session never listed"
    );

    let spec = ConnectionSpec::parse_target("dev@example").unwrap();
    let cmd = transport_cmd(xdg, &ssh, &spec, "watched");
    let mut obs = Subscriber::observe_ssh(cmd).expect("observe over ssh");

    let mut snapshot = None;
    let mut screen: Option<Screen> = None;
    assert!(
        wait_until(Duration::from_secs(10), || {
            let p = obs.pump().unwrap();
            snapshot = snapshot.take().or(p.snapshot);
            for e in p.events {
                if let SessionEvent::Resized { cols, rows } = e {
                    screen = Some(Screen::new(cols, rows, 0));
                }
            }
            if let Some(s) = screen.as_mut() {
                s.feed(&p.output);
            }
            snapshot.is_some()
                && screen
                    .as_ref()
                    .is_some_and(|s| s.text().join("\n").contains("OBSERVED-REMOTE"))
        }),
        "the remote screen never arrived over the observe tunnel"
    );
}

/// `ghost ssh <target>` invoked with the shim as `ssh` and this binary as the
/// remote ghost. `remote_ghost` sets `GHOST_REMOTE_GHOST` (a real path uses the
/// transport; a bogus one forces negotiation to fail → the ssh-child fallback).
fn ghost_ssh(xdg: &Path, shim: &Path, remote_ghost: &str, args: &[&str]) -> std::process::Output {
    let path = format!(
        "{}:{}",
        shim.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut c = Command::new(GHOST);
    c.arg("ssh")
        .args(args)
        .env("XDG_RUNTIME_DIR", xdg.join("run"))
        .env("XDG_DATA_HOME", xdg.join("data"))
        .env("PATH", path)
        .env("GHOST_REMOTE_GHOST", remote_ghost);
    c.output().expect("run `ghost ssh`")
}

fn descriptor(xdg: &Path, name: &str) -> Descriptor {
    // The host writes its descriptor asynchronously after spawn; wait for it.
    let path = xdg.join("data/ghost/sessions").join(format!("{name}.json"));
    assert!(
        wait_until(Duration::from_secs(5), || path.exists()),
        "descriptor for '{name}' was never written"
    );
    serde_json::from_slice(&std::fs::read(&path).expect("descriptor written")).unwrap()
}

#[test]
fn ghost_ssh_uses_the_transport_when_the_remote_has_ghost() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let shim = shim_ssh();

    // The probe (`ghost __probe`) succeeds via the shim, so `ghost ssh` takes
    // the transport: it spawns a real remote host rather than an ssh child.
    let out = ghost_ssh(xdg, shim.path(), GHOST, &["dev@example", "-d"]);
    assert!(
        out.status.success(),
        "`ghost ssh` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _guard = KillOnDrop {
        xdg,
        name: "ssh-example",
    };
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains("ssh-example")),
        "the remote host was never created"
    );

    // A transport session is a plain ghost host (its child is the remote $SHELL),
    // so its descriptor carries NO connection — that's the tell that distinguishes
    // it from the ssh-child fallback, which stores the spec.
    assert!(
        descriptor(xdg, "ssh-example").connection.is_none(),
        "a transport session is a plain host, not an ssh child"
    );
}

#[test]
fn ghost_ssh_stages_the_binary_when_the_remote_lacks_ghost() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let home = tempfile::tempdir().unwrap();
    let shim = shim_ssh();

    // No ghost on PATH (restricted to system dirs), none staged, no override —
    // so `ghost ssh` copies our own binary to the (sandboxed) remote home and
    // then uses the transport. arch matches: it's the same machine.
    let path = format!("{}:/usr/bin:/bin", shim.path().display());
    let out = Command::new(GHOST)
        .args(["ssh", "dev@example", "-d"])
        .env("XDG_RUNTIME_DIR", xdg.join("run"))
        .env("XDG_DATA_HOME", xdg.join("data"))
        .env("HOME", home.path())
        .env("PATH", &path)
        .env_remove("GHOST_REMOTE_GHOST")
        .output()
        .expect("run `ghost ssh`");
    assert!(
        out.status.success(),
        "`ghost ssh` (staging) failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _guard = KillOnDrop {
        xdg,
        name: "ssh-example",
    };

    // The binary landed, version-stamped, under the remote home's cache…
    let bin_dir = home.path().join(".cache/ghost/bin");
    let staged = std::fs::read_dir(&bin_dir)
        .ok()
        .and_then(|d| {
            d.filter_map(Result::ok)
                .find(|e| e.file_name().to_string_lossy().starts_with("ghost-"))
        })
        .expect("a ghost binary was staged under the remote cache");
    assert!(staged.path().is_file());

    // …and a real remote host is running — a plain host (no recorded
    // connection), the tell that the transport was used, not the ssh child.
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains("ssh-example")),
        "the remote host was never created"
    );
    assert!(
        descriptor(xdg, "ssh-example").connection.is_none(),
        "a transport session is a plain host, not an ssh child"
    );
}

/// [`shim_ssh`] plus a fake `uname` on the same PATH that reports
/// `$GHOST_FAKE_UNAME` instead of this host's real platform — so a test can make
/// the "remote" look like a different OS/arch and exercise cross-arch staging.
fn shim_ssh_faking_uname() -> tempfile::TempDir {
    let dir = shim_ssh();
    let uname = dir.path().join("uname");
    std::fs::write(&uname, "#!/bin/sh\nprintf '%s\\n' \"$GHOST_FAKE_UNAME\"\n").unwrap();
    std::fs::set_permissions(&uname, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

/// A `(uname string, prebuilt filename)` for a platform guaranteed different from
/// this host's — the OS is always flipped, so it's cross-arch no matter where the
/// suite runs (including the user's mac/arm64).
fn foreign_platform() -> (String, String) {
    let (sys, os) = if cfg!(target_os = "linux") {
        ("Darwin", "macos")
    } else {
        ("Linux", "linux")
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "aarch64"
    } else {
        "x86_64"
    };
    (format!("{sys} {arch}"), format!("ghost-{os}-{arch}"))
}

#[test]
fn ghost_ssh_stages_a_cross_arch_prebuilt_when_the_remote_differs() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let home = tempfile::tempdir().unwrap();
    let prebuilt = tempfile::tempdir().unwrap();
    let shim = shim_ssh_faking_uname();

    // The "remote" reports a foreign platform; a prebuilt for it sits in the
    // search dir — actually a copy of our own binary, so once staged it runs and
    // answers `__probe` when the (locally-exec'd) remote checks the staged copy.
    let (fake_uname, prebuilt_name) = foreign_platform();
    std::fs::copy(GHOST, prebuilt.path().join(&prebuilt_name)).unwrap();

    let path = format!("{}:/usr/bin:/bin", shim.path().display());
    let out = Command::new(GHOST)
        .args(["ssh", "dev@example", "-d"])
        .env("XDG_RUNTIME_DIR", xdg.join("run"))
        .env("XDG_DATA_HOME", xdg.join("data"))
        .env("HOME", home.path())
        .env("PATH", &path)
        .env("GHOST_FAKE_UNAME", &fake_uname)
        .env("GHOST_PREBUILT_DIR", prebuilt.path())
        .env_remove("GHOST_REMOTE_GHOST")
        .output()
        .expect("run `ghost ssh`");
    assert!(
        out.status.success(),
        "`ghost ssh` (cross-arch staging) failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _guard = KillOnDrop {
        xdg,
        name: "ssh-example",
    };

    // The cross-arch prebuilt was staged under the remote home cache…
    let bin_dir = home.path().join(".cache/ghost/bin");
    assert!(
        std::fs::read_dir(&bin_dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().starts_with("ghost-")),
        "the cross-arch prebuilt was not staged under the remote cache"
    );
    // …and a real remote host is running from it (a plain host, no recorded
    // connection — the tell that the transport was used, not the ssh child).
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains("ssh-example")),
        "the remote host was never created from the prebuilt"
    );
    assert!(
        descriptor(xdg, "ssh-example").connection.is_none(),
        "a transport session is a plain host, not an ssh child"
    );
}

#[test]
fn ghost_ssh_falls_back_to_the_ssh_child_when_no_prebuilt_matches_the_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let home = tempfile::tempdir().unwrap();
    let prebuilt = tempfile::tempdir().unwrap(); // empty: no prebuilt for any platform
    let shim = shim_ssh_faking_uname();

    // A foreign remote but no matching prebuilt, no override, no ghost on PATH →
    // negotiation finds nothing stageable and falls back to the local ssh child,
    // which records the connection spec (the ssh-child tell).
    let (fake_uname, _) = foreign_platform();
    let path = format!("{}:/usr/bin:/bin", shim.path().display());
    let out = Command::new(GHOST)
        .args(["ssh", "dev@example", "-d"])
        .env("XDG_RUNTIME_DIR", xdg.join("run"))
        .env("XDG_DATA_HOME", xdg.join("data"))
        .env("HOME", home.path())
        .env("PATH", &path)
        .env("GHOST_FAKE_UNAME", &fake_uname)
        .env("GHOST_PREBUILT_DIR", prebuilt.path())
        .env_remove("GHOST_REMOTE_GHOST")
        .output()
        .expect("run `ghost ssh`");
    assert!(
        out.status.success(),
        "`ghost ssh` fallback failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _guard = KillOnDrop {
        xdg,
        name: "ssh-example",
    };
    let spec = descriptor(xdg, "ssh-example")
        .connection
        .expect("the ssh-child fallback records the connection");
    assert_eq!(spec.target(), "dev@example");
}

#[test]
fn staging_prunes_older_cached_binaries_keeping_the_newest_few() {
    use std::time::{Duration, SystemTime};

    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let home = tempfile::tempdir().unwrap();
    let shim = shim_ssh();

    // Pre-seed the remote cache with several older staged builds (empty stand-ins,
    // distinct increasing mtimes so `old-1` is the oldest). They accumulate the way
    // a dev's rebuild-reconnect loop leaves a ~126 MiB copy per build.
    let bin_dir = home.path().join(".cache/ghost/bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let fakes = 6;
    for i in 1..=fakes {
        let f = std::fs::File::create(bin_dir.join(format!("ghost-old-{i}"))).unwrap();
        // old-1 = now-9h … old-6 = now-4h — all older than the fresh stage below.
        let mtime = SystemTime::now() - Duration::from_secs((10 - i as u64) * 3600);
        f.set_modified(mtime).unwrap();
    }

    // A stage: no ghost on PATH, none staged under this hash, no override — so the
    // fresh build copies over (arch matches; same machine) and then sweeps the dir.
    let path = format!("{}:/usr/bin:/bin", shim.path().display());
    let out = Command::new(GHOST)
        .args(["ssh", "dev@example", "-d"])
        .env("XDG_RUNTIME_DIR", xdg.join("run"))
        .env("XDG_DATA_HOME", xdg.join("data"))
        .env("HOME", home.path())
        .env("PATH", &path)
        .env_remove("GHOST_REMOTE_GHOST")
        .output()
        .expect("run `ghost ssh`");
    assert!(
        out.status.success(),
        "`ghost ssh` (staging) failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _guard = KillOnDrop {
        xdg,
        name: "ssh-example",
    };

    let names: Vec<String> = std::fs::read_dir(&bin_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("ghost-"))
        .collect();

    // The freshly-staged real binary survived (it's the newest)…
    assert!(
        names
            .iter()
            .any(|n| n.starts_with(&format!("ghost-{}-", env!("CARGO_PKG_VERSION")))),
        "the freshly-staged binary is gone; dir holds {names:?}"
    );
    // …the oldest pre-seeded copy was pruned…
    assert!(
        !names.iter().any(|n| n == "ghost-old-1"),
        "the oldest cached binary was not pruned; dir holds {names:?}"
    );
    // …and a sweep happened rather than unbounded accumulation (well under the
    // seeded pile: at most a couple of recent fakes survive alongside the real one).
    assert!(
        names.len() < fakes,
        "no pruning happened — dir still holds {} entries: {names:?}",
        names.len()
    );
}

#[test]
fn ghost_ssh_falls_back_to_the_ssh_child_when_no_transport_is_possible() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let shim = shim_ssh();

    // An explicit remote-ghost override that doesn't answer the probe: negotiation
    // gives up (the override skips staging), so `ghost ssh` falls back to the
    // local ssh child — which records the connection spec (the ssh child's tell).
    let out = ghost_ssh(
        xdg,
        shim.path(),
        "/nonexistent/ghost",
        &["dev@example", "-d"],
    );
    assert!(
        out.status.success(),
        "`ghost ssh` fallback failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _guard = KillOnDrop {
        xdg,
        name: "ssh-example",
    };
    let spec = descriptor(xdg, "ssh-example")
        .connection
        .expect("the ssh-child fallback records the connection");
    assert_eq!(spec.target(), "dev@example");
}

/// A `__pipe` to a session that isn't there fails cleanly rather than hanging.
#[test]
fn piping_to_a_missing_session_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let out = ghost(xdg)
        .args(["__pipe", "nope"])
        .output()
        .expect("run `ghost __pipe`");
    assert!(
        !out.status.success(),
        "piping to a missing session must fail"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("nope"),
        "the error names the session; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
