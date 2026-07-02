//! Recreating a dead session from its recording: a spawn seeded with a
//! predecessor's recording starts with that session's final screen already in
//! place — history survives the death — and the new child's output simply
//! continues below it, like a shell returning after a subprocess.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use ghost_vt::client::Session;
use ghost_vt::screen::Screen;

const GHOST: &str = env!("CARGO_BIN_EXE_ghost");

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

fn recording_path(xdg: &Path, name: &str) -> PathBuf {
    xdg.join("data")
        .join("ghost")
        .join("recordings")
        .join(format!("{name}.ghostrec"))
}

fn sock(xdg: &Path, name: &str) -> PathBuf {
    xdg.join("run").join("ghost").join(name).join("sock")
}

#[test]
fn a_seeded_session_starts_with_its_predecessors_screen() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();

    // First life: print a distinctive marker, then linger.
    let out = ghost(xdg)
        .args([
            "new",
            "phoenix",
            "-d",
            "--",
            "sh",
            "-c",
            "echo FIRST-LIFE-CONTENT; sleep 30",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains("phoenix")),
        "session not listed"
    );

    // Death. Killing flushes and closes the recording.
    let out = ghost(xdg).args(["kill", "phoenix"]).output().unwrap();
    assert!(out.status.success());
    assert!(
        wait_until(Duration::from_secs(5), || !ls(xdg).contains("phoenix")),
        "session still listed after kill"
    );
    let rec_path = recording_path(xdg, "phoenix");
    let rec = ghost_vt::record::read(&rec_path).expect("the recording survives the death");
    let final_screen = Screen::from_recording(&rec, 100);
    assert!(
        final_screen
            .text()
            .join("\n")
            .contains("FIRST-LIFE-CONTENT"),
        "precondition: the recording holds the first life's screen"
    );

    // Second life: same name, seeded from the first life's recording.
    let out = ghost(xdg)
        .args(["new", "phoenix", "-d", "--seed-from"])
        .arg(&rec_path)
        .args(["--", "sh", "-c", "echo SECOND-LIFE; sleep 30"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "seeded `ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _guard = KillOnDrop {
        xdg,
        name: "phoenix",
    };
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains("phoenix")),
        "recreated session not listed"
    );

    // Attach: the resync must already contain the FIRST life's content (the
    // seed), with the second life's output following it.
    let mut session =
        Session::attach_path(&sock(xdg, "phoenix"), "phoenix", 80, 24).expect("attach");
    session
        .set_read_timeout(Some(Duration::from_millis(25)))
        .unwrap();
    let mut screen = Screen::new(80, 24, 100);
    assert!(
        wait_until(Duration::from_secs(5), || {
            if let Ok(p) = session.pump() {
                screen.feed(&p.output);
            }
            let text = screen.text().join("\n");
            text.contains("FIRST-LIFE-CONTENT") && text.contains("SECOND-LIFE")
        }),
        "the seeded screen shows both lives; saw:\n{}",
        screen.text().join("\n")
    );
    // The old content sits ABOVE the new life's output, as history should.
    let text = screen.text().join("\n");
    assert!(
        text.find("FIRST-LIFE-CONTENT").unwrap() < text.find("SECOND-LIFE").unwrap(),
        "the predecessor's screen precedes the new output:\n{text}"
    );

    // The new recording is self-contained: replaying it alone (no reference to
    // the seed file) reconstructs a screen that still shows the first life.
    let out = ghost(xdg).args(["kill", "phoenix"]).output().unwrap();
    assert!(out.status.success());
    let rec2 = ghost_vt::record::read(&rec_path).expect("second-life recording");
    let replay = Screen::from_recording(&rec2, 100);
    assert!(
        replay.text().join("\n").contains("FIRST-LIFE-CONTENT"),
        "the new recording bakes the seed in:\n{}",
        replay.text().join("\n")
    );
}
