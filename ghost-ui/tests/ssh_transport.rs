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

use ghost_vt::client::Session;
use ghost_vt::connection::ConnectionSpec;
use ghost_vt::descriptor::Descriptor;
use ghost_vt::screen::Screen;

const GHOST: &str = env!("CARGO_BIN_EXE_ghost");

/// A fake `ssh`: consume ssh options (with `-p/-i/-J/-o` taking a value) and the
/// destination, then `exec` whatever remote command follows — locally. That
/// remote command is `<ghost> __pipe <name>`, so it relays to the local session
/// socket exactly as a remote ghost would to its own. With no remote command
/// (the ssh *child* form, `ssh <target>`), it drops into a shell like real ssh.
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
         exec \"$@\"\n",
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
