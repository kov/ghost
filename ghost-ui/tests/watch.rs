//! End-to-end test for `ghost __watch`, the pushed session-set stream that
//! replaces the fleet's poll: it emits the listing once, then again whenever the
//! session set changes.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use ghost_vt::client::Session;
use ghost_vt::session::SessionInfo;
use rustix::fs::{FlockOperation, OFlags, flock};

fn ghost(xdg: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_ghost"));
    c.env("XDG_RUNTIME_DIR", xdg.join("run"));
    c.env("XDG_DATA_HOME", xdg.join("data"));
    c
}

/// Kill a spawned child on drop.
struct KillChild(Child);
impl Drop for KillChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
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

#[test]
fn watch_streams_the_listing_and_pushes_on_change() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();

    // Start the watcher; a reader thread hands each JSON line to the test.
    let mut child = ghost(xdg)
        .arg("__watch")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let _guard = KillChild(child);
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(l) = line else { break };
            if tx.send(l).is_err() {
                break;
            }
        }
    });

    // The initial listing arrives immediately, and is empty (no sessions yet).
    assert!(
        wait_for(&rx, Duration::from_secs(5), |s| s.is_empty()),
        "no initial (empty) listing was pushed"
    );

    // Creating a session pushes a fresh listing that includes it — no polling.
    ghost(xdg)
        .args(["new", "-d", "watch-test", "--", "sleep", "600"])
        .output()
        .unwrap();
    let saw_it = wait_for(&rx, Duration::from_secs(5), |s| {
        s.iter().any(|i| i.name == "watch-test")
    });
    let _ = ghost(xdg).args(["kill", "watch-test"]).output();
    assert!(saw_it, "the new session was not pushed to the watcher");
}

fn sock(xdg: &Path, name: &str) -> PathBuf {
    xdg.join("run").join("ghost").join(name).join("sock")
}

/// A title change writes `<session>/meta` from the host's run loop — a write
/// *inside* the per-session subdir, with no process opening the runtime dir. It
/// is the cleanest artifact-proof probe of the push-on-change path: unlike a
/// rename (whose CLI incidentally `opendir`s the runtime dir and so wakes the
/// watch by side effect), nothing here pokes the watched directory, so a push can
/// only come from the watcher actually noticing the meta write.
#[test]
fn watch_pushes_on_title_change() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();

    // A session running a shell, so we can drive an OSC title change into it.
    ghost(xdg)
        .args(["new", "-d", "titled", "--", "sh"])
        .output()
        .unwrap();
    let _guard2 = KillOnDrop {
        xdg,
        name: "titled",
    };

    let mut child = ghost(xdg)
        .arg("__watch")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let _guard = KillChild(child);
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(l) = line else { break };
            if tx.send(l).is_err() {
                break;
            }
        }
    });

    // Wait until the watcher has seen the session with its default (empty) title.
    assert!(
        wait_for(&rx, Duration::from_secs(5), |s| {
            s.iter().any(|i| i.name == "titled" && i.title.is_empty())
        }),
        "the session was not pushed to the watcher"
    );

    // Attach (connects to the socket directly — does NOT open the runtime dir) and
    // drive an OSC 2 title change through the shell. The host processes it and
    // rewrites `titled/meta`; the watcher must push the fresh title well under the
    // 30s heartbeat, proving it noticed the in-subdir meta write.
    let mut session = Session::attach_path(&sock(xdg, "titled"), "titled", 80, 24).expect("attach");
    session
        .set_read_timeout(Some(Duration::from_millis(25)))
        .unwrap();
    session
        .send_input(b"printf '\\033]2;HELLO-TITLE\\007'\n")
        .unwrap();

    assert!(
        wait_for(&rx, Duration::from_secs(5), |s| {
            s.iter()
                .any(|i| i.name == "titled" && i.title == "HELLO-TITLE")
        }),
        "the title change was not pushed to the watcher within the heartbeat window"
    );
}

/// A `cd` must reach the watcher promptly, not on the heartbeat.
///
/// The directory a session is working in is what a session branched off it opens
/// in, and for a remote host that inheritance reads the *listing* — so a cwd the
/// listing carries half a minute late is inheritance that silently gives the
/// wrong answer for half a minute. Reported from a real mac: the new session kept
/// landing in `~`, and "eventually" landed in the right place.
///
/// The cause is that a cwd change is the one listing field that leaves no trace
/// in the watched tree. Everything else the listing reports — a session
/// appearing, its title, its attach state — is a write under the *runtime* dir,
/// while the cwd lives in the durable descriptor under the *data* dir. So the
/// write happened where nobody was looking and only the 30s heartbeat carried it.
#[test]
fn watch_pushes_on_working_directory_change() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let moved_to = xdg.join("elsewhere");
    std::fs::create_dir_all(&moved_to).unwrap();

    ghost(xdg)
        .args(["new", "-d", "wanderer", "--", "sh"])
        .output()
        .unwrap();
    let _guard2 = KillOnDrop {
        xdg,
        name: "wanderer",
    };

    let mut child = ghost(xdg)
        .arg("__watch")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let _guard = KillChild(child);
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
            s.iter().any(|i| i.name == "wanderer")
        }),
        "the session was not pushed to the watcher"
    );

    // Move the shell, exactly as a user typing `cd` would.
    let target = moved_to.canonicalize().unwrap();
    let mut session =
        Session::attach_path(&sock(xdg, "wanderer"), "wanderer", 80, 24).expect("attach");
    session
        .set_read_timeout(Some(Duration::from_millis(25)))
        .unwrap();
    session
        .send_input(format!("cd {}\n", target.display()).as_bytes())
        .unwrap();

    // Well under the 30s heartbeat: this has to be the watch noticing, not the
    // keepalive coming round.
    assert!(
        wait_for(&rx, Duration::from_secs(5), |s| {
            s.iter().any(|i| {
                i.name == "wanderer" && i.cwd.as_deref() == Some(&*target.to_string_lossy())
            })
        }),
        "the new working directory was not pushed within the heartbeat window"
    );
}

/// A child that never stops writing is still looked at.
///
/// The look waits for the child to settle, which on its own would mean a session
/// producing continuous output is never looked at at all — its directory frozen
/// at whatever it was when the noise started. So the wait is capped, and this is
/// what holds the cap in place: the shell moves and then writes without pause, so
/// the only way the new directory can be reported is the cap expiring.
#[test]
fn a_ceaselessly_writing_child_still_reports_where_it_moved() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let moved_to = xdg.join("elsewhere");
    std::fs::create_dir_all(&moved_to).unwrap();

    ghost(xdg)
        .args(["new", "-d", "noisy", "--", "sh"])
        .output()
        .unwrap();
    let _guard2 = KillOnDrop { xdg, name: "noisy" };

    let mut child = ghost(xdg)
        .arg("__watch")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let _guard = KillChild(child);
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
            s.iter().any(|i| i.name == "noisy")
        }),
        "the session was not pushed to the watcher"
    );

    // Move, then write forever with no gap for the settle to land in.
    let target = moved_to.canonicalize().unwrap();
    let mut session = Session::attach_path(&sock(xdg, "noisy"), "noisy", 80, 24).expect("attach");
    session
        .set_read_timeout(Some(Duration::from_millis(25)))
        .unwrap();
    session
        .send_input(format!("cd {}; while :; do echo noise; done\n", target.display()).as_bytes())
        .unwrap();

    assert!(
        wait_for(&rx, Duration::from_secs(10), |s| {
            s.iter()
                .any(|i| i.name == "noisy" && i.cwd.as_deref() == Some(&*target.to_string_lossy()))
        }),
        "a child that never pauses must still have its directory reported"
    );
}

/// A session that becomes listable *while* `__watch` is still building its first
/// listing must still be streamed.
///
/// `ghost new -d` returns as soon as the host is forked: the session's lock is
/// already held, but its `pid` is not written until the host has exec'd and come
/// up, and `session::list` deliberately keeps a directory out of the listing
/// until that pid appears. So the instant a session becomes *visible* is a `pid`
/// write landing some milliseconds after the spawn command exited — routinely
/// after a `__watch` started right behind it has taken its first snapshot. Emit
/// that snapshot before registering the watcher and the write falls in the gap:
/// the session is in no listing and no change is pending, so it stays invisible
/// until the 30s heartbeat.
///
/// In the wild the gap is a fraction of a millisecond, which is what made this a
/// roughly 1-in-10 flake in the remote watch test rather than a plain bug. Here
/// it is held open on purpose: `list` reads each session's `lock` with an
/// ordinary read-only open, and opening a FIFO read-only blocks until a writer
/// arrives — so a session directory whose `lock` is a FIFO parks the scan
/// mid-listing for as long as we like.
#[test]
fn a_session_that_becomes_listable_mid_scan_is_still_streamed() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let runtime = xdg.join("run").join("ghost");
    std::fs::create_dir_all(&runtime).unwrap();

    // Two session directories. Which one the scan reaches first is the
    // directory's own order, not alphabetical — so lay both down and ask.
    for name in ["sess-a", "sess-b"] {
        std::fs::create_dir_all(runtime.join(name)).unwrap();
    }
    let order: Vec<String> = std::fs::read_dir(&runtime)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    // The first one scanned is the session under test; the second parks the scan
    // *after* it, so its pid write cannot sneak into the initial listing.
    let (late, blocker) = (&order[0], &order[1]);

    // The one under test looks exactly like a session mid-spawn: a live host
    // holds its lock, and no pid yet. Holding the flock here stands in for that
    // host — `list` reads liveness from the lock, and nothing else about a host
    // is needed to be listed.
    let lock = std::fs::File::create(runtime.join(late).join("lock")).unwrap();
    flock(&lock, FlockOperation::NonBlockingLockExclusive).unwrap();

    let fifo = runtime.join(blocker).join("lock");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success(),
        "could not create the blocking lock"
    );

    let mut child = ghost(xdg)
        .arg("__watch")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let _guard = KillChild(child);
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(l) = line else { break };
            if tx.send(l).is_err() {
                break;
            }
        }
    });

    // A non-blocking open for writing succeeds only once a reader is waiting on
    // the FIFO — so this returning is proof the scan is parked, and parked
    // *past* the session under test.
    let deadline = Instant::now() + Duration::from_secs(5);
    let release = loop {
        match rustix::fs::open(
            &fifo,
            OFlags::WRONLY | OFlags::NONBLOCK,
            rustix::fs::Mode::empty(),
        ) {
            Ok(fd) => break fd,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => panic!("the watcher never reached the blocking lock: {e}"),
        }
    };

    // The moment the flake turns on: the session becomes listable now, with the
    // first listing already past it and the watcher not yet registered.
    std::fs::write(
        runtime.join(late).join("pid"),
        std::process::id().to_string(),
    )
    .unwrap();

    // The scan finishes on its own now, with the initial (still empty)
    // listing. The writer stays open: the scan judges the blocker dead and
    // re-checks its lock before pruning, and that second open of the FIFO would
    // park again with no writer.
    assert!(
        wait_for(&rx, Duration::from_secs(5), |s| {
            s.iter().any(|i| i.name == *late)
        }),
        "a session that came up while the first listing was being built was \
         never streamed — the change landed before the watcher was registered, \
         so nothing will report it until the 30s heartbeat"
    );
    drop(release);
}

/// Kill a session by name on drop, so a failing assertion still cleans up.
struct KillOnDrop<'a> {
    xdg: &'a Path,
    name: &'a str,
}
impl Drop for KillOnDrop<'_> {
    fn drop(&mut self) {
        let _ = ghost(self.xdg).args(["kill", self.name]).output();
    }
}
