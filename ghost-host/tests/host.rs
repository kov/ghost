//! `ghost-host` is the headless binary staged to remotes, so it must do
//! everything a remote host is asked to do — answer the transport probe, and
//! create/list/host/kill sessions — without a GUI. These drive the real binary.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use ghost_vt::client::Session;
use ghost_vt::screen::Screen;

const HOST: &str = env!("CARGO_BIN_EXE_ghost-host");

fn ghost_host(xdg: &Path) -> Command {
    let mut c = Command::new(HOST);
    c.env("XDG_RUNTIME_DIR", xdg.join("run"));
    c.env("XDG_DATA_HOME", xdg.join("data"));
    c
}

fn ls(xdg: &Path) -> String {
    let out = ghost_host(xdg).arg("ls").output().expect("run `ls`");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn sock(xdg: &Path, name: &str) -> PathBuf {
    xdg.join("run").join("ghost").join(name).join("sock")
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

#[test]
fn probe_reports_the_transport_marker() {
    // The initiator runs `<remote> __probe` to accept ghost-host as a transport
    // host, so it must print the marker (with a proto level) and exit 0.
    let out = ghost_host(Path::new("/nonexistent"))
        .arg("__probe")
        .output()
        .expect("run `__probe`");
    assert!(out.status.success(), "__probe should exit 0");
    let line = String::from_utf8_lossy(&out.stdout);
    assert!(
        line.contains("ghost-transport") && line.contains("proto="),
        "unexpected probe line: {line:?}"
    );
}

#[test]
fn no_subcommand_exits_nonzero_without_hanging() {
    // With no GUI to launch, the bare binary must fail fast (not block waiting on a
    // window it can never open).
    let out = ghost_host(Path::new("/nonexistent"))
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run with no subcommand");
    assert!(
        !out.status.success(),
        "a headless build with no subcommand must exit non-zero"
    );
}

#[test]
fn hosts_a_real_session_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();

    // Create a detached session whose child prints a marker then idles.
    let out = ghost_host(xdg)
        .args([
            "new",
            "-d",
            "work",
            "--",
            "sh",
            "-c",
            "echo HOSTED; sleep 60",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains("work")),
        "the session never listed"
    );

    // Attach and watch the screen: ghost-host really runs the PTY and streams it.
    let mut session = Session::attach_path(&sock(xdg, "work"), "work", 80, 24).expect("attach");
    session
        .set_read_timeout(Some(Duration::from_millis(25)))
        .unwrap();
    let mut screen = Screen::new(80, 24, 100);
    let saw = wait_until(Duration::from_secs(5), || {
        if let Ok(p) = session.pump() {
            screen.feed(&p.output);
        }
        screen.text().join("\n").contains("HOSTED")
    });
    drop(session);

    // Kill it before asserting, so a failure never leaks the host.
    let _ = ghost_host(xdg).args(["kill", "work"]).output();
    assert!(saw, "the hosted child's output never arrived");
}
