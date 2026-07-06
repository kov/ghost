//! SSH transport against a *genuinely separate* remote host.
//!
//! `ssh_transport.rs` proves the transport plumbing, but it lets the "remote"
//! session live in the initiator's own XDG dirs — so a remote session is
//! reachable locally, and any bug that depends on "the remote's state is not on
//! this machine" (rename/title propagation, `ls` isolation, routing) is masked.
//!
//! Here the fake `ssh` gives each destination its **own** `XDG_RUNTIME_DIR`,
//! `XDG_DATA_HOME` and `HOME`, rooted under `$GHOST_REMOTE_ROOT` and keyed by the
//! ssh destination. A "remote" ghost then only shares the wire with the
//! initiator — never a directory — so a remote session is invisible to a local
//! `ghost ls` and only the transport can reach it, exactly like a real host.

use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use ghost_vt::client::Session;
use ghost_vt::connection::ConnectionSpec;
use ghost_vt::screen::Screen;
use ghost_vt::session::SessionInfo;

const GHOST: &str = env!("CARGO_BIN_EXE_ghost");

/// A fake `ssh` that models a distinct host per destination: it consumes ssh
/// options (`-p/-i/-J/-o` take a value) and the destination, derives that
/// destination's own dirs under `$GHOST_REMOTE_ROOT`, exports them, and runs the
/// remaining remote command there (or a shell, like ssh dropping into a login).
/// Because the exported dirs are the *remote's*, nothing the remote writes lands
/// in the initiator's XDG — the isolation the real network gives us for free.
fn isolated_shim() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let ssh = dir.path().join("ssh");
    std::fs::write(
        &ssh,
        "#!/bin/sh\n\
         dest=\n\
         while [ $# -gt 0 ]; do\n\
         \x20 case \"$1\" in\n\
         \x20   -p|-i|-J|-o) shift 2 ;;\n\
         \x20   -*) shift ;;\n\
         \x20   *) dest=$1; shift; break ;;\n\
         \x20 esac\n\
         done\n\
         tag=$(printf '%s' \"$dest\" | tr -c 'A-Za-z0-9._-' '_')\n\
         root=\"$GHOST_REMOTE_ROOT/$tag\"\n\
         mkdir -p \"$root/run\" \"$root/data\" \"$root/home/.cache\"\n\
         export XDG_RUNTIME_DIR=\"$root/run\"\n\
         export XDG_DATA_HOME=\"$root/data\"\n\
         export XDG_CACHE_HOME=\"$root/home/.cache\"\n\
         export HOME=\"$root/home\"\n\
         [ $# -eq 0 ] && exec sh\n\
         exec sh -c \"$*\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

/// A `ghost` command run on the initiator with its own (local) XDG — never the
/// remote root, so it can only ever see *local* sessions.
fn local_ghost(xdg: &Path) -> Command {
    let mut c = Command::new(GHOST);
    c.env("XDG_RUNTIME_DIR", xdg.join("run"));
    c.env("XDG_DATA_HOME", xdg.join("data"));
    c
}

/// A command that runs `remote` on `target` *over the transport*: the shim as
/// `ssh`, with `$GHOST_REMOTE_ROOT` set so the remote lands in its own dirs. Built
/// from the real [`ConnectionSpec::ssh_command`], so it exercises the same argv
/// assembly (and single-quoting) the initiator uses.
fn transport(remote_root: &Path, ssh: &Path, target: &str, remote: &[&str]) -> Command {
    let spec = ConnectionSpec::parse_target(target).unwrap();
    let argv = spec.ssh_command(&[], remote);
    let mut c = Command::new(ssh);
    c.args(&argv[1..]); // drop the leading "ssh"; we invoke the shim directly
    c.env("GHOST_REMOTE_ROOT", remote_root);
    c
}

/// `ghost ls` output as seen from the remote host (over the transport).
fn remote_ls(remote_root: &Path, ssh: &Path, target: &str) -> String {
    let out = transport(remote_root, ssh, target, &[GHOST, "ls"])
        .output()
        .expect("run remote `ghost ls`");
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

/// Read JSON listings off `rx` until one satisfies `pred` or the deadline passes.
fn wait_for(
    rx: &Receiver<String>,
    timeout: Duration,
    mut pred: impl FnMut(&[SessionInfo]) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        match rx.recv_timeout(left) {
            Ok(line) => {
                if let Ok(sessions) = serde_json::from_str::<Vec<SessionInfo>>(&line)
                    && pred(&sessions)
                {
                    return true;
                }
            }
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return false,
        }
    }
}

/// Kill a remote session (over the transport) on drop.
struct KillRemote<'a> {
    remote_root: &'a Path,
    ssh: &'a Path,
    target: &'a str,
    name: &'a str,
}
impl Drop for KillRemote<'_> {
    fn drop(&mut self) {
        let _ = transport(
            self.remote_root,
            self.ssh,
            self.target,
            &[GHOST, "kill", self.name],
        )
        .output();
    }
}

/// The fidelity guarantee this harness exists for: a session spawned on the
/// remote is genuinely remote — invisible to a local `ghost ls`, reachable only
/// over the transport.
#[test]
fn a_remote_session_is_invisible_locally_and_visible_over_the_transport() {
    let local = tempfile::tempdir().unwrap();
    let xdg = local.path();
    let remote = tempfile::tempdir().unwrap();
    let root = remote.path();
    let shim = isolated_shim();
    let ssh = shim.path().join("ssh");
    let target = "dev@example";

    // Spawn a session on the remote, over the transport.
    let out = transport(
        root,
        &ssh,
        target,
        &[GHOST, "new", "-d", "boxsess", "--", "sleep", "600"],
    )
    .output()
    .unwrap();
    assert!(
        out.status.success(),
        "remote `ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _guard = KillRemote {
        remote_root: root,
        ssh: &ssh,
        target,
        name: "boxsess",
    };

    // It is listed over the transport…
    assert!(
        wait_until(Duration::from_secs(5), || {
            remote_ls(root, &ssh, target).contains("boxsess")
        }),
        "the remote session was never listed over the transport"
    );
    // …its files live under the remote root, not the initiator's dirs…
    assert!(
        root.join("dev_example/run/ghost/boxsess").is_dir(),
        "the remote session should live under the remote root"
    );
    // …and a local `ghost ls` cannot see it at all.
    let local_ls = local_ghost(xdg).arg("ls").output().unwrap();
    let local_ls = String::from_utf8_lossy(&local_ls.stdout);
    assert!(
        !local_ls.contains("boxsess"),
        "a remote session must not be visible in a local `ghost ls`; saw:\n{local_ls}"
    );
}

/// End-to-end over the transport: a title change on the remote (which only
/// rewrites the remote `<session>/meta`) reaches a `ghost __watch` running on the
/// remote and streamed back over the transport — the remote-context regression
/// for the recursive-watch fix, on a genuinely isolated host.
#[test]
fn a_remote_title_change_propagates_over_the_watch_transport() {
    let remote = tempfile::tempdir().unwrap();
    let root = remote.path();
    let shim = isolated_shim();
    let ssh = shim.path().join("ssh");
    let target = "dev@example";

    // A remote session running a shell, so we can drive a title change into it.
    transport(
        root,
        &ssh,
        target,
        &[GHOST, "new", "-d", "titled", "--", "sh"],
    )
    .output()
    .unwrap();
    let _guard = KillRemote {
        remote_root: root,
        ssh: &ssh,
        target,
        name: "titled",
    };

    // Stream the remote listing over the transport (the fleet's `__watch` path).
    let mut watch = transport(root, &ssh, target, &[GHOST, "__watch"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = watch.stdout.take().unwrap();
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(l) = line else { break };
            if tx.send(l).is_err() {
                break;
            }
        }
    });

    assert!(
        wait_for(&rx, Duration::from_secs(5), |s| {
            s.iter().any(|i| i.name == "titled" && i.title.is_empty())
        }),
        "the remote session was not streamed over the watch transport"
    );

    // Attach over the transport and drive an OSC 2 title change into the remote
    // shell; the remote host rewrites its `titled/meta`, and its recursive watcher
    // must push the fresh title back over the transport well under the heartbeat.
    let pipe = transport(root, &ssh, target, &[GHOST, "__pipe", "titled"]);
    let mut session = Session::attach_ssh(pipe, "titled", 80, 24).expect("attach over transport");
    session
        .set_read_timeout(Some(Duration::from_millis(25)))
        .unwrap();
    // Round-trip a marker first, so we know the shell is live before the OSC.
    let mut screen = Screen::new(80, 24, 100);
    session.send_input(b"printf READY\n").unwrap();
    assert!(
        wait_for_screen(&mut session, &mut screen, "READY"),
        "the remote shell never echoed; saw:\n{}",
        screen.text().join("\n")
    );
    session
        .send_input(b"printf '\\033]2;REMOTE-TITLE\\007'\n")
        .unwrap();

    assert!(
        wait_for(&rx, Duration::from_secs(5), |s| {
            s.iter()
                .any(|i| i.name == "titled" && i.title == "REMOTE-TITLE")
        }),
        "the remote title change never propagated over the watch transport"
    );

    let _ = watch.kill();
    let _ = watch.wait();
}

/// Pump a session and feed its output into `screen` until `needle` renders or a
/// short deadline passes.
fn wait_for_screen(session: &mut Session, screen: &mut Screen, needle: &str) -> bool {
    wait_until(Duration::from_secs(5), || {
        if let Ok(p) = session.pump() {
            screen.feed(&p.output);
        }
        screen.text().join("\n").contains(needle)
    })
}
