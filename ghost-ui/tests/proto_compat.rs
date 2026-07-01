//! E2E: protocol compatibility with hosts running an older binary.
//!
//! Session hosts are long-lived daemons that keep executing the binary that
//! spawned them, so a freshly-built client routinely attaches to a host that
//! predates it. An old host treats a message it cannot decode as a connection
//! error and drops the client — frozen screen, dead input. Optional messages
//! (like the theme report) must therefore only be sent to hosts that have
//! declared support, via the `proto` capability marker in the session dir.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

const GHOST: &str = env!("CARGO_BIN_EXE_ghost");

fn ghost(xdg: &Path) -> Command {
    let mut c = Command::new(GHOST);
    c.env("XDG_RUNTIME_DIR", xdg.join("run"));
    c.env("XDG_DATA_HOME", xdg.join("data"));
    c
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

#[test]
fn theme_is_not_sent_to_a_host_that_predates_it() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "old-host-theme";
    let _guard = KillOnDrop { xdg, name };

    // The child waits for a marker (raised after the client below attached and
    // detached), then queries OSC 11 while detached. The sentinel is touched
    // only if the reply carries ghost's built-in default background — proving
    // the client never delivered its theme to the host.
    let marker = xdg.join("detach-done");
    let sentinel = xdg.join("default-answered");
    let script = format!(
        "while [ ! -e '{}' ]; do sleep 0.05; done; \
         printf '\\033]11;?\\033\\\\'; \
         if IFS= read -r -s -d '\\' -t 2 reply; then case \"$reply\" in *'rgb:1010/1010/1212'*) touch '{}';; esac; fi; \
         exec sleep 60",
        marker.display(),
        sentinel.display()
    );
    let out = ghost(xdg)
        .args(["new", name, "-d", "--", "bash", "-c", &script])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new -d` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let dir = xdg.join("run/ghost").join(name);
    let sock = dir.join("sock");
    assert!(
        wait_until(Duration::from_secs(5), || sock.exists()),
        "session socket never appeared"
    );
    // Simulate a host built before the capability marker existed: an old
    // host's session dir has no `proto` file.
    let _ = std::fs::remove_file(dir.join("proto"));

    // Attach the way the GUI does — handshake, then report the theme — and
    // detach by dropping. Against an undeclared (old) host the report must be
    // skipped client-side, so the session keeps ghost's default colors.
    {
        let mut s =
            ghost_vt::client::Session::attach_path(&sock, name, 80, 24).expect("attach failed");
        s.report_theme(ghost_vt::query::ThemeColors {
            fg: [0xd0, 0xd0, 0xd0],
            bg: [0x12, 0x34, 0x56],
            cursor: [0xd0, 0xd0, 0xd0],
        })
        .expect("report_theme failed");
    }
    std::fs::write(&marker, b"").unwrap();

    assert!(
        wait_until(Duration::from_secs(5), || sentinel.exists()),
        "theme was delivered to a host that never declared support for it"
    );
}

#[test]
fn input_flows_after_a_gui_style_attach_to_an_undeclared_host() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "old-host-input";
    let _guard = KillOnDrop { xdg, name };

    // The child reads one line and touches the sentinel when it arrives; the
    // regression this pins was the host dropping the connection right after
    // the attach handshake, so input never reached the child.
    let sentinel = xdg.join("input-arrived");
    let script = format!(
        "IFS= read -r line; if [ \"$line\" = hello ]; then touch '{}'; fi; exec sleep 60",
        sentinel.display()
    );
    let out = ghost(xdg)
        .args(["new", name, "-d", "--", "bash", "-c", &script])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new -d` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let dir = xdg.join("run/ghost").join(name);
    let sock = dir.join("sock");
    assert!(
        wait_until(Duration::from_secs(5), || sock.exists()),
        "session socket never appeared"
    );
    let _ = std::fs::remove_file(dir.join("proto"));

    let mut s = ghost_vt::client::Session::attach_deferred_path(&sock, name).expect("attach");
    s.set_read_timeout(Some(Duration::from_millis(1))).unwrap();
    s.resize(80, 24).expect("resize");
    s.report_theme(ghost_vt::query::ThemeColors {
        fg: [0xd0, 0xd0, 0xd0],
        bg: [0x12, 0x34, 0x56],
        cursor: [0xd0, 0xd0, 0xd0],
    })
    .expect("report_theme");
    s.send_input(b"hello\n").expect("send_input");

    assert!(
        wait_until(Duration::from_secs(5), || sentinel.exists()),
        "input did not reach the child after a GUI-style attach"
    );
}
