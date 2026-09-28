//! End-to-end tests for the subscription surface: a client that sends
//! `Subscribe` is a state observer, not a display client. The host answers
//! with one `Snapshot` of the session's mutable state and — crucially — does
//! not treat the subscriber as an attach: no `attached` marker appears, and
//! an unseen-bell marker is not cleared by someone merely watching.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use ghost_vt::client::{Client, Subscriber};
use ghost_vt::protocol::{
    AttachInfo, ClientMsg, PROTO_SUBSCRIBE, ServerMsg, SessionEvent, SessionState,
};

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

/// Kills a session on drop so a failed test never leaks a daemon.
struct KillOnDrop<'a> {
    xdg: &'a Path,
    name: &'a str,
}

impl Drop for KillOnDrop<'_> {
    fn drop(&mut self) {
        let _ = ghost(self.xdg).args(["kill", self.name]).output();
    }
}

/// Spawn a real, eager, detached session running `script` and wait for it to
/// be listed. Returns its session dir.
fn spawn_session(xdg: &Path, name: &str, script: &str) -> std::path::PathBuf {
    let out = ghost(xdg)
        .args(["new", name, "-d", "--", "sh", "-c", script])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains(name)),
        "session not listed"
    );
    xdg.join("run").join("ghost").join(name)
}

/// Pump the connection until a `Snapshot` arrives (or time runs out).
fn recv_snapshot(client: &mut Client, timeout: Duration) -> Option<SessionState> {
    client
        .set_read_timeout(Some(Duration::from_millis(25)))
        .unwrap();
    let start = Instant::now();
    while start.elapsed() < timeout {
        let msgs = match client.recv_ready() {
            Ok(Some(msgs)) => msgs,
            Ok(None) => return None, // EOF: the host dropped us
            Err(_) => continue,      // read timeout — keep waiting
        };
        for msg in msgs {
            if let ServerMsg::Snapshot(state) = msg {
                return Some(state);
            }
        }
    }
    None
}

/// Pump the connection, appending every pushed event to `seen`, until `pred`
/// is satisfied (or time runs out — the assertion then shows what arrived).
fn recv_events_until(
    client: &mut Client,
    seen: &mut Vec<SessionEvent>,
    timeout: Duration,
    mut pred: impl FnMut(&[SessionEvent]) -> bool,
) {
    client
        .set_read_timeout(Some(Duration::from_millis(25)))
        .unwrap();
    let start = Instant::now();
    while start.elapsed() < timeout {
        if pred(seen) {
            return;
        }
        let msgs = match client.recv_ready() {
            Ok(Some(msgs)) => msgs,
            Ok(None) => break,  // EOF
            Err(_) => continue, // read timeout — keep waiting
        };
        for msg in msgs {
            if let ServerMsg::Event(e) = msg {
                seen.push(e);
            }
        }
    }
    assert!(pred(seen), "expected event did not arrive; got {seen:?}");
}

#[test]
fn the_subscriber_api_delivers_the_snapshot_then_events_then_eof() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "subscriber-api-test";
    let _guard = KillOnDrop { xdg, name };

    let session_dir = spawn_session(xdg, name, "sleep 60");
    let sock = session_dir.join("sock");

    // The typed observer wrapper: connects, verifies the host serves
    // subscriptions, and subscribes in one step. Pumps never block.
    let mut sub = Subscriber::connect_path(&sock).expect("subscriber connect");

    // First pump(s) deliver the snapshot.
    let mut snapshot = None;
    assert!(
        wait_until(Duration::from_secs(5), || {
            let p = sub.pump().unwrap();
            snapshot = snapshot.take().or(p.snapshot);
            snapshot.is_some()
        }),
        "no snapshot delivered"
    );
    assert_eq!(snapshot.unwrap().attached, None);

    // A display client attaching arrives as a pushed event.
    let mut display = Client::connect_path(&sock).expect("display connect");
    display
        .send(&ClientMsg::Resize { cols: 80, rows: 24 })
        .unwrap();
    let mut events = Vec::new();
    assert!(
        wait_until(Duration::from_secs(5), || {
            events.extend(sub.pump().unwrap().events);
            events.contains(&SessionEvent::Attached(AttachInfo { client: None }))
        }),
        "no Attached event; got {events:?}"
    );

    // Killing the session ends the subscription: pump reports it ended.
    drop(display);
    let out = ghost(xdg).args(["kill", name]).output().unwrap();
    assert!(out.status.success());
    assert!(
        wait_until(Duration::from_secs(5), || sub.pump().unwrap().ended),
        "subscription did not observe the host's death as EOF"
    );
}

#[test]
fn the_observer_api_delivers_grid_then_screen() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "observer-api-test";
    let _guard = KillOnDrop { xdg, name };

    let session_dir = spawn_session(xdg, name, "printf 'OBSERVED'; sleep 60");
    let sock = session_dir.join("sock");

    let mut obs = Subscriber::observe_path(&sock).expect("observer connect");
    let mut snapshot = None;
    let mut vt: Option<ghost_vt::screen::Screen> = None;
    assert!(
        wait_until(Duration::from_secs(5), || {
            let p = obs.pump().unwrap();
            snapshot = snapshot.take().or(p.snapshot);
            for e in p.events {
                if let SessionEvent::Resized { cols, rows } = e {
                    vt = Some(ghost_vt::screen::Screen::new(cols, rows, 0));
                }
            }
            if let Some(vt) = vt.as_mut() {
                vt.feed(&p.output);
            }
            snapshot.is_some()
                && vt
                    .as_ref()
                    .is_some_and(|v| v.text().join("\n").contains("OBSERVED"))
        }),
        "the observer API did not deliver snapshot + grid + screen"
    );
}

#[test]
fn a_subscriber_is_pushed_state_events_as_the_session_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "events-test";
    let _guard = KillOnDrop { xdg, name };

    // The child waits for a line of input, then rings the bell and sets the
    // terminal title — observable state changes we trigger on demand.
    let session_dir = spawn_session(
        xdg,
        name,
        "read line; printf '\\a'; printf '\\033]2;hello\\007'; sleep 60",
    );
    let sock = session_dir.join("sock");

    // Subscribe first, so every later change is a delta on the snapshot.
    let mut sub = Client::connect_path(&sock).expect("subscriber connect");
    sub.send(&ClientMsg::Subscribe).unwrap();
    let state = recv_snapshot(&mut sub, Duration::from_secs(5)).expect("snapshot");
    assert_eq!(state.attached, None);
    let mut seen = Vec::new();

    // An identified display client attaches -> Attached(window-1).
    let mut display = Client::connect_path(&sock).expect("display connect");
    display
        .send(&ClientMsg::Hello {
            client: "window-1".to_string(),
        })
        .unwrap();
    display
        .send(&ClientMsg::Resize { cols: 80, rows: 24 })
        .unwrap();
    let attached = SessionEvent::Attached(AttachInfo {
        client: Some("window-1".to_string()),
    });
    recv_events_until(&mut sub, &mut seen, Duration::from_secs(5), |seen| {
        seen.contains(&attached)
    });

    // Waking the child rings the bell and sets the title. The bell rings while
    // a display client is attached: the live event fires anyway (that is the
    // point of the push), while the unseen-bell marker stays clear.
    display.send(&ClientMsg::Input(b"\n".to_vec())).unwrap();
    recv_events_until(&mut sub, &mut seen, Duration::from_secs(5), |seen| {
        seen.contains(&SessionEvent::Bell)
            && seen.contains(&SessionEvent::TitleChanged("hello".to_string()))
            && seen.contains(&SessionEvent::Activity)
    });
    assert!(
        !session_dir.join("bell").exists(),
        "a bell witnessed by an attached client must not be marked unseen"
    );

    // Renaming the session -> Renamed.
    display
        .send(&ClientMsg::Rename("otter".to_string()))
        .unwrap();
    recv_events_until(&mut sub, &mut seen, Duration::from_secs(5), |seen| {
        seen.contains(&SessionEvent::Renamed("otter".to_string()))
    });

    // Dropping the display client -> Detached.
    drop(display);
    recv_events_until(&mut sub, &mut seen, Duration::from_secs(5), |seen| {
        seen.contains(&SessionEvent::Detached)
    });
}

#[test]
fn an_observer_mirrors_the_screen_without_becoming_a_display_client() {
    use ghost_vt::protocol::PROTO_OBSERVE;

    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "observe-test";
    let _guard = KillOnDrop { xdg, name };

    // The child waits for input, then prints a marker we expect to see
    // through the observer's read-only mirror.
    let session_dir = spawn_session(xdg, name, "read line; printf 'MORE-CONTENT'; sleep 60");
    let sock = session_dir.join("sock");

    let mut obs = Client::connect_path(&sock).expect("observer connect");
    assert!(
        obs.proto() >= PROTO_OBSERVE,
        "host must advertise the observe level it serves (got {})",
        obs.proto()
    );
    obs.set_read_timeout(Some(Duration::from_millis(25)))
        .unwrap();
    obs.send(&ClientMsg::Observe).unwrap();

    // The observation starts with the session's real grid, then a resync of
    // the current screen; we mirror it into our own emulator, sized by the
    // pushed grid — never by anything we sent.
    let mut vt: Option<ghost_vt::screen::Screen> = None;
    let mut grid = (0u16, 0u16);
    let drain =
        |obs: &mut Client, vt: &mut Option<ghost_vt::screen::Screen>, grid: &mut (u16, u16)| {
            for msg in obs.recv_ready().ok().flatten().unwrap_or_default() {
                match msg {
                    ServerMsg::Event(SessionEvent::Resized { cols, rows }) => {
                        *grid = (cols, rows);
                        *vt = Some(ghost_vt::screen::Screen::new(cols, rows, 0));
                    }
                    ServerMsg::Output(bytes) => {
                        if let Some(vt) = vt.as_mut() {
                            vt.feed(&bytes);
                        }
                    }
                    _ => {}
                }
            }
        };
    assert!(
        wait_until(Duration::from_secs(5), || {
            drain(&mut obs, &mut vt, &mut grid);
            vt.is_some()
        }),
        "no Resized grid arrived"
    );
    assert!(grid.0 > 0 && grid.1 > 0);

    // Watching must not attach, and an attach must not be needed for the
    // mirror to work: wake the child via a real display client and see its
    // output arrive live on the observer.
    assert!(
        !session_dir.join("attached").exists(),
        "observing must not set the attached marker"
    );
    let mut display = Client::connect_path(&sock).expect("display connect");
    display
        .send(&ClientMsg::Resize { cols: 80, rows: 24 })
        .unwrap();
    display.send(&ClientMsg::Input(b"\n".to_vec())).unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || {
            drain(&mut obs, &mut vt, &mut grid);
            vt.as_ref()
                .is_some_and(|v| v.text().join("\n").contains("MORE-CONTENT"))
        }),
        "live output did not reach the observer's mirror; screen: {:?}",
        vt.as_ref().map(|v| v.text())
    );

    // The display client resizing the PTY re-grids the mirror and re-seeds it
    // (a reflowed screen can't be patched incrementally from outside).
    display
        .send(&ClientMsg::Resize {
            cols: 100,
            rows: 30,
        })
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || {
            drain(&mut obs, &mut vt, &mut grid);
            grid == (100, 30)
                && vt
                    .as_ref()
                    .is_some_and(|v| v.text().join("\n").contains("MORE-CONTENT"))
        }),
        "the observer did not follow the display resize; grid {grid:?}, screen: {:?}",
        vt.as_ref().map(|v| v.text())
    );
}

#[test]
fn a_lagging_observer_is_capped_and_heals_by_resync() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "observe-lag-test";
    let _guard = KillOnDrop { xdg, name };

    // On wake the child floods ~4MB through the PTY, then prints a marker.
    const FLOOD: usize = 4 * 1024 * 1024;
    let session_dir = spawn_session(
        xdg,
        name,
        &format!(
            "read line; head -c {FLOOD} /dev/zero | tr '\\0' x; printf 'END-MARKER'; sleep 60"
        ),
    );
    let sock = session_dir.join("sock");

    let mut obs = Client::connect_path(&sock).expect("observer connect");
    obs.set_read_timeout(Some(Duration::from_millis(25)))
        .unwrap();
    obs.send(&ClientMsg::Observe).unwrap();

    let mut vt: Option<ghost_vt::screen::Screen> = None;
    let mut grid = (0u16, 0u16);
    let mut received = 0usize;
    let mut resized = 0usize;
    #[allow(clippy::too_many_arguments)]
    let drain = |obs: &mut Client,
                 vt: &mut Option<ghost_vt::screen::Screen>,
                 received: &mut usize,
                 grid: &mut (u16, u16),
                 resized: &mut usize| {
        for msg in obs.recv_ready().ok().flatten().unwrap_or_default() {
            match msg {
                ServerMsg::Event(SessionEvent::Resized { cols, rows }) => {
                    *grid = (cols, rows);
                    *resized += 1;
                    // The client rebuilds its mirror on every Resized — the
                    // host prefaces each resync with one for exactly this.
                    *vt = Some(ghost_vt::screen::Screen::new(cols, rows, 0));
                }
                ServerMsg::Output(bytes) => {
                    *received += bytes.len();
                    if let Some(vt) = vt.as_mut() {
                        vt.feed(&bytes);
                    }
                }
                _ => {}
            }
        }
    };
    assert!(
        wait_until(Duration::from_secs(5), || {
            drain(&mut obs, &mut vt, &mut received, &mut grid, &mut resized);
            vt.is_some()
        }),
        "no Resized grid arrived"
    );
    let observed_grid = grid;
    let resized_before_flood = resized;

    // Trigger the flood while the observer is NOT reading, so the host's
    // outbound queue for it fills past any cap. Resize the display client to the
    // grid the observer already saw, so this resize does not itself re-grid the
    // session (which would emit a Resized of its own and muddy the count below).
    let mut display = Client::connect_path(&sock).expect("display connect");
    display
        .send(&ClientMsg::Resize {
            cols: observed_grid.0,
            rows: observed_grid.1,
        })
        .unwrap();
    display.send(&ClientMsg::Input(b"\n".to_vec())).unwrap();
    std::thread::sleep(Duration::from_secs(2));

    // Now drain: the mirror must converge on the post-flood screen (the host
    // re-seeds a lagged observer with a resync once it catches up)…
    assert!(
        wait_until(Duration::from_secs(10), || {
            drain(&mut obs, &mut vt, &mut received, &mut grid, &mut resized);
            vt.as_ref()
                .is_some_and(|v| v.text().join("\n").contains("END-MARKER"))
        }),
        "the lagged observer never converged; received {received} bytes"
    );
    // …and the healing re-seed is prefaced by a Resized, exactly as the regrid
    // path is: it is the observer's cue to rebuild its mirror before the resync
    // lands, so the dump never reflows onto a stale grid. Without that preamble
    // the count stays at the single observation-start Resized.
    assert!(
        resized > resized_before_flood,
        "the lagged-drain re-seed sent no Resized preamble (resized={resized}, was {resized_before_flood})"
    );
    // …while receiving far less than the flood: the host dropped, not
    // buffered, what the observer was too slow to take.
    assert!(
        received < FLOOD / 2,
        "observer received {received} bytes — the host buffered the flood instead of capping it"
    );
}

#[test]
fn a_subscriber_gets_a_snapshot_without_becoming_the_display_client() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "subscribe-test";
    let _guard = KillOnDrop { xdg, name };

    // The child rings the bell at startup with nobody attached, so the session
    // carries an unseen-bell notification the snapshot must report.
    let session_dir = spawn_session(xdg, name, "printf '\\a'; sleep 60");
    let bell_marker = session_dir.join("bell");
    assert!(
        wait_until(Duration::from_secs(5), || bell_marker.exists()),
        "unattached bell was not marked"
    );

    let sock = session_dir.join("sock");
    let mut sub = Client::connect_path(&sock).expect("subscriber connect");
    assert!(
        sub.proto() >= PROTO_SUBSCRIBE,
        "host must advertise the subscribe level it serves (got {})",
        sub.proto()
    );
    sub.send(&ClientMsg::Subscribe).unwrap();

    let state = recv_snapshot(&mut sub, Duration::from_secs(5))
        .expect("host answered the subscription with a snapshot");
    assert_eq!(state.attached, None, "nobody is attached");
    assert!(state.bell, "the unseen bell is part of the snapshot");

    // A subscriber is NOT a display client: watching must not mark the session
    // attached, and must not count as "seeing" the bell.
    assert!(
        !session_dir.join("attached").exists(),
        "subscribing must not set the attached marker"
    );
    assert!(
        bell_marker.exists(),
        "subscribing must not clear the bell marker"
    );
}

#[test]
fn the_snapshot_reports_the_identified_display_client() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "subscribe-attached-test";
    let _guard = KillOnDrop { xdg, name };

    let session_dir = spawn_session(xdg, name, "sleep 60");
    let sock = session_dir.join("sock");

    // A display client that identifies itself the way a real window does —
    // its identity embeds its window-group id — then completes the attach
    // handshake (the first Resize).
    let identity = ghost_ui_core::group::window_identity("win-1234-0");
    let mut display = Client::connect_path(&sock).expect("display connect");
    display
        .send(&ClientMsg::Hello {
            client: identity.clone(),
        })
        .unwrap();
    display
        .send(&ClientMsg::Resize { cols: 80, rows: 24 })
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || session_dir
            .join("attached")
            .exists()),
        "display client did not attach"
    );

    let mut sub = Client::connect_path(&sock).expect("subscriber connect");
    sub.send(&ClientMsg::Subscribe).unwrap();
    let state = recv_snapshot(&mut sub, Duration::from_secs(5))
        .expect("host answered the subscription with a snapshot");
    assert_eq!(
        state.attached,
        Some(AttachInfo {
            client: Some(identity.clone()),
        }),
        "the snapshot names the identified display client"
    );
    // The relayed identity parses back to the holder's group id — what the
    // fleet buckets elsewhere-tiles by.
    assert_eq!(
        state
            .attached
            .and_then(|a| a.client)
            .as_deref()
            .and_then(ghost_ui_core::group::holder_group),
        Some("win-1234-0".to_string()),
        "the round-tripped identity names the holding window's group"
    );
    assert!(!state.bell);

    drop(display);
}

#[test]
fn past_the_subscriber_cap_the_oldest_subscriber_is_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "subscriber-cap-test";
    let _guard = KillOnDrop { xdg, name };

    let session_dir = spawn_session(xdg, name, "sleep 60");
    let sock = session_dir.join("sock");

    // Subscribe one at a time, each confirmed by its snapshot, so the host has
    // them in a known order: `subs[0]` is the oldest.
    let cap = ghost_vt::server::MAX_SUBSCRIBERS;
    let mut subs = Vec::new();
    for i in 0..=cap {
        let mut sub = Subscriber::connect_path(&sock).expect("subscriber connect");
        let mut got = false;
        assert!(
            wait_until(Duration::from_secs(5), || {
                got |= sub.pump().unwrap().snapshot.is_some();
                got
            }),
            "subscriber {i} got no snapshot"
        );
        subs.push(sub);
    }

    let oldest_dropped = wait_until(Duration::from_secs(5), || {
        subs[0].pump().map(|p| p.ended).unwrap_or(true)
    });
    let second_kept = !subs[1].pump().unwrap().ended;
    let newest_kept = !subs[cap].pump().unwrap().ended;

    assert!(
        oldest_dropped,
        "one subscriber past the cap must drop the oldest"
    );
    assert!(second_kept, "only the oldest is dropped");
    assert!(newest_kept, "the newest subscriber is kept");
}

/// Open a raw observer connection (the verb, not the typed wrapper) so the test
/// can also send what a watcher must not: `Resize`, `Input`, `Kill`.
fn raw_observer(sock: &Path) -> Client {
    let mut c = Client::connect_path(sock).expect("observer connect");
    c.send(&ClientMsg::Observe).unwrap();
    c.set_read_timeout(Some(Duration::from_millis(25))).unwrap();
    c
}

/// Pump `c` until a `Resized` event arrives and return its grid, or `None` on
/// EOF / timeout. Re-sending `Observe` makes the host answer with the session's
/// current grid, after everything sent before it on this connection.
fn next_grid(c: &mut Client, timeout: Duration) -> Option<(u16, u16)> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        let msgs = match c.recv_ready() {
            Ok(Some(msgs)) => msgs,
            Ok(None) => return None,
            Err(_) => continue,
        };
        for msg in msgs {
            if let ServerMsg::Event(SessionEvent::Resized { cols, rows }) = msg {
                return Some((cols, rows));
            }
        }
    }
    None
}

#[test]
fn an_observers_resize_does_not_regrid_the_session() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "observer-resize-test";
    let _guard = KillOnDrop { xdg, name };
    let sock = spawn_session(xdg, name, "sleep 60").join("sock");

    let mut obs = raw_observer(&sock);
    let before = next_grid(&mut obs, Duration::from_secs(5)).expect("initial grid");
    obs.send(&ClientMsg::Resize { cols: 41, rows: 11 }).unwrap();
    obs.send(&ClientMsg::Observe).unwrap();
    let after = next_grid(&mut obs, Duration::from_secs(5)).expect("grid after resize");

    assert_ne!(
        before,
        (41, 11),
        "precondition: the session starts at another size"
    );
    assert_eq!(after, before, "a watcher must not re-grid the session");
}

#[test]
fn an_observers_input_never_reaches_the_child() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "observer-input-test";
    let _guard = KillOnDrop { xdg, name };
    let sock = spawn_session(
        xdg,
        name,
        "stty -echo; read line; echo \"GOT:$line.\"; sleep 60",
    )
    .join("sock");

    let mut obs = raw_observer(&sock);
    obs.send(&ClientMsg::Input(b"watcher\r".to_vec())).unwrap();
    // A real display client types next; whichever line the child reads first
    // is what it prints.
    let mut display = Client::connect_path(&sock).expect("display connect");
    display
        .send(&ClientMsg::Resize { cols: 80, rows: 24 })
        .unwrap();
    display
        .send(&ClientMsg::Input(b"driver\r".to_vec()))
        .unwrap();

    let mut out = Vec::new();
    let got = wait_until(Duration::from_secs(5), || {
        if let Ok(Some(msgs)) = obs.recv_ready() {
            for m in msgs {
                if let ServerMsg::Output(b) = m {
                    out.extend_from_slice(&b);
                }
            }
        }
        String::from_utf8_lossy(&out).contains("GOT:")
    });
    let text = String::from_utf8_lossy(&out).into_owned();

    assert!(got, "the child printed nothing; output: {text:?}");
    assert!(
        text.contains("GOT:driver."),
        "only the display client's input may reach the child; output: {text:?}"
    );
}

#[test]
fn an_observers_kill_leaves_the_session_running() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "observer-kill-test";
    let _guard = KillOnDrop { xdg, name };
    let sock = spawn_session(xdg, name, "sleep 60").join("sock");

    let mut obs = raw_observer(&sock);
    next_grid(&mut obs, Duration::from_secs(5)).expect("initial grid");
    obs.send(&ClientMsg::Kill).unwrap();
    obs.send(&ClientMsg::Observe).unwrap();
    let answered = next_grid(&mut obs, Duration::from_secs(5)).is_some();

    assert!(answered, "the host ended the session on a watcher's Kill");
    assert!(ls(xdg).contains(name), "the session is still listed");
}

#[test]
fn a_kill_from_a_control_connection_discards_the_session_like_a_display_kill() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "control-kill-test";
    let _guard = KillOnDrop { xdg, name };
    spawn_session(xdg, name, "sleep 60");
    let data = xdg.join("data").join("ghost");
    let descriptor = data.join("sessions").join(format!("{name}.json"));
    let recording = data.join("recordings").join(format!("{name}.ghostrec"));
    assert!(
        wait_until(Duration::from_secs(5), || descriptor.exists()),
        "precondition: the session wrote its descriptor"
    );
    let recorded = recording.exists();

    // A control connection: no Resize, so never the display client.
    let sock = xdg.join("run").join("ghost").join(name).join("sock");
    let mut control = Client::connect_path(&sock).expect("control connect");
    control.send(&ClientMsg::Kill).unwrap();
    let gone = wait_until(Duration::from_secs(5), || !ls(xdg).contains(name));

    assert!(recorded, "precondition: the session records");
    assert!(gone, "a control connection's Kill ends the session");
    assert!(
        !descriptor.exists() && !recording.exists(),
        "an explicit kill throws the session away: descriptor and recording go"
    );
}

/// The session's `holder` as `ghost ls --json` reports it.
fn listed_holder(xdg: &Path, name: &str) -> Option<String> {
    let out = ghost(xdg).args(["ls", "--json"]).output().ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    v.as_array()?
        .iter()
        .find(|s| s["name"] == name)?
        .get("holder")?
        .as_str()
        .map(str::to_string)
}

#[test]
fn an_attach_names_its_holder_in_the_listing() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "attach-holder-test";
    let _guard = KillOnDrop { xdg, name };
    let sock = spawn_session(xdg, name, "sleep 60").join("sock");

    let mut display = Client::connect_path(&sock).expect("display connect");
    display
        .send(&ClientMsg::Attach {
            cols: 80,
            rows: 24,
            client: "test@box:win-1-1".into(),
        })
        .unwrap();

    assert!(
        wait_until(Duration::from_secs(5), || {
            listed_holder(xdg, name).as_deref() == Some("test@box:win-1-1")
        }),
        "the listing never named the holder; got {:?}",
        listed_holder(xdg, name)
    );
    drop(display);
    assert!(
        wait_until(Duration::from_secs(5), || listed_holder(xdg, name)
            .is_none()),
        "a detached session still names a holder"
    );
}

#[test]
fn an_attach_is_announced_with_its_identity_from_the_first_event() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "attach-announce-test";
    let _guard = KillOnDrop { xdg, name };
    let sock = spawn_session(xdg, name, "sleep 60").join("sock");

    let mut sub = Subscriber::connect_path(&sock).expect("subscriber connect");
    let mut got_snapshot = false;
    assert!(
        wait_until(Duration::from_secs(5), || {
            got_snapshot |= sub.pump().unwrap().snapshot.is_some();
            got_snapshot
        }),
        "no snapshot"
    );

    let mut display = Client::connect_path(&sock).expect("display connect");
    display
        .send(&ClientMsg::Attach {
            cols: 80,
            rows: 24,
            client: "test@box:win-1-1".into(),
        })
        .unwrap();

    let mut attached = Vec::new();
    wait_until(Duration::from_secs(5), || {
        for e in sub.pump().unwrap().events {
            if let SessionEvent::Attached(info) = e {
                attached.push(info.client);
            }
        }
        !attached.is_empty()
    });
    assert_eq!(
        attached.first(),
        Some(&Some("test@box:win-1-1".to_string())),
        "the first Attached event must already carry the identity; got {attached:?}"
    );
}

#[test]
fn a_resize_then_hello_still_names_the_holder_in_the_listing() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "legacy-holder-test";
    let _guard = KillOnDrop { xdg, name };
    let sock = spawn_session(xdg, name, "sleep 60").join("sock");

    // What a client predating `Attach` sends.
    let mut display = Client::connect_path(&sock).expect("display connect");
    display
        .send(&ClientMsg::Resize { cols: 80, rows: 24 })
        .unwrap();
    display
        .send(&ClientMsg::Hello {
            client: "ghost-ui:win-9-9".into(),
        })
        .unwrap();

    assert!(
        wait_until(Duration::from_secs(5), || {
            listed_holder(xdg, name).as_deref() == Some("ghost-ui:win-9-9")
        }),
        "the listing never named the holder; got {:?}",
        listed_holder(xdg, name)
    );
}

/// End the session's host the way a reboot does (SIGTERM), which keeps its
/// descriptor and recording so it can be relaunched; `ghost kill` would discard
/// them. Waits until the session is no longer listed.
fn terminate_host(xdg: &Path, name: &str) {
    let pid = std::fs::read_to_string(xdg.join("run").join("ghost").join(name).join("pid"))
        .expect("host pid");
    let ok = Command::new("kill")
        .args(["-TERM", pid.trim()])
        .status()
        .unwrap()
        .success();
    assert!(ok, "SIGTERM the host");
    assert!(
        wait_until(Duration::from_secs(5), || !ls(xdg).contains(name)),
        "the host did not exit"
    );
}

/// The session's `group` as `ghost ls --json` reports it.
fn listed_group(xdg: &Path, name: &str) -> Option<String> {
    let out = ghost(xdg).args(["ls", "--json"]).output().ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    v.as_array()?
        .iter()
        .find(|s| s["name"] == name)?
        .get("group")?
        .as_str()
        .map(str::to_string)
}

#[test]
fn a_sessions_group_is_listed_and_can_be_cleared() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "group-listed-test";
    let _guard = KillOnDrop { xdg, name };
    let sock = spawn_session(xdg, name, "sleep 60").join("sock");

    let mut control = Client::connect_path(&sock).expect("control connect");
    control
        .send(&ClientMsg::SetGroup(Some("win-1-1".into())))
        .unwrap();
    let joined = wait_until(Duration::from_secs(5), || {
        listed_group(xdg, name).as_deref() == Some("win-1-1")
    });
    control.send(&ClientMsg::SetGroup(None)).unwrap();
    let left = wait_until(Duration::from_secs(5), || listed_group(xdg, name).is_none());

    assert!(joined, "the listing never named the group");
    assert!(left, "the listing still names a group after it was cleared");
}

#[test]
fn a_sessions_group_outlives_its_host_in_the_descriptor() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "group-durable-test";
    let _guard = KillOnDrop { xdg, name };
    let sock = spawn_session(xdg, name, "sleep 60").join("sock");
    let descriptor = xdg
        .join("data")
        .join("ghost")
        .join("sessions")
        .join(format!("{name}.json"));
    assert!(
        wait_until(Duration::from_secs(5), || descriptor.exists()),
        "precondition: the session wrote its descriptor"
    );

    let mut control = Client::connect_path(&sock).expect("control connect");
    control
        .send(&ClientMsg::SetGroup(Some("win-2-2".into())))
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || {
            listed_group(xdg, name).as_deref() == Some("win-2-2")
        }),
        "precondition: the group was set"
    );
    drop(control);

    // A host that dies uncleanly keeps its descriptor (the session can be
    // relaunched); the group must be in it.
    terminate_host(xdg, name);
    let d: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&descriptor).expect("descriptor kept")).unwrap();
    assert_eq!(d["group"], "win-2-2", "the descriptor keeps the group");
}

#[test]
fn a_subscriber_is_told_when_the_session_changes_group() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "group-event-test";
    let _guard = KillOnDrop { xdg, name };
    let sock = spawn_session(xdg, name, "sleep 60").join("sock");

    let mut sub = Subscriber::connect_path(&sock).expect("subscriber connect");
    let mut got_snapshot = false;
    assert!(
        wait_until(Duration::from_secs(5), || {
            got_snapshot |= sub.pump().unwrap().snapshot.is_some();
            got_snapshot
        }),
        "no snapshot"
    );
    let mut control = Client::connect_path(&sock).expect("control connect");
    control
        .send(&ClientMsg::SetGroup(Some("win-3-3".into())))
        .unwrap();

    let mut events = Vec::new();
    let told = wait_until(Duration::from_secs(5), || {
        events.extend(sub.pump().unwrap().events);
        events.contains(&SessionEvent::GroupChanged(Some("win-3-3".into())))
    });
    assert!(told, "no GroupChanged event; got {events:?}");
}

#[test]
fn a_watcher_cannot_change_a_sessions_group() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "group-watcher-test";
    let _guard = KillOnDrop { xdg, name };
    let sock = spawn_session(xdg, name, "sleep 60").join("sock");

    let mut obs = raw_observer(&sock);
    next_grid(&mut obs, Duration::from_secs(5)).expect("initial grid");
    obs.send(&ClientMsg::SetGroup(Some("win-4-4".into())))
        .unwrap();
    // Re-observing answers after the SetGroup has been handled.
    obs.send(&ClientMsg::Observe).unwrap();
    next_grid(&mut obs, Duration::from_secs(5)).expect("grid after SetGroup");

    assert_eq!(listed_group(xdg, name), None, "a watcher set the group");
}

#[test]
fn a_relaunched_session_comes_back_in_its_group() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "group-relaunch-test";
    let _guard = KillOnDrop { xdg, name };
    let sock = spawn_session(xdg, name, "sleep 60").join("sock");

    let mut control = Client::connect_path(&sock).expect("control connect");
    control
        .send(&ClientMsg::SetGroup(Some("win-5-5".into())))
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || {
            listed_group(xdg, name).as_deref() == Some("win-5-5")
        }),
        "precondition: the group was set"
    );
    drop(control);
    // An unclean end keeps the descriptor; a new host under the same name is
    // that session relaunched.
    terminate_host(xdg, name);
    spawn_session(xdg, name, "sleep 60");

    assert_eq!(
        listed_group(xdg, name).as_deref(),
        Some("win-5-5"),
        "the relaunched session is back in its group"
    );
}
