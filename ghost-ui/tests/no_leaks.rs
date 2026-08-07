//! The suite must not leak session hosts.
//!
//! A host outlives its client *on purpose* — that is the whole feature — so
//! nothing reaps one for us. A test that spawns a session and then drops its
//! temp `$XDG_RUNTIME_DIR` orphans that host for good: the socket it would be
//! reached through is gone with the directory.
//!
//! That is not just untidy. Every host holds an inotify instance (and so does the
//! shell it runs), the per-user cap is 128, and once it is gone `inotify_init`
//! returns EMFILE — so the watch and title tests start failing in hundredths of a
//! second and read exactly like a regression in code that never changed. Enough
//! suite runs in a day and the machine is unusable for the suite. This test is the
//! gate on that.

mod support;

use std::path::Path;
use std::time::Duration;

use ghost_vt::remote::RemoteSsh;
use support::remote::{RealRemote, retry_some};
use support::{spawn_session, wait_until, with_isolated_xdg};

/// Whether a process is still around. Orphans are reparented to init and reaped
/// promptly, so a live `/proc/<pid>` means a live host, not a zombie.
fn alive(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[test]
fn an_isolated_run_leaves_no_session_hosts_behind() {
    let pids = with_isolated_xdg(|_tmp| {
        spawn_session("leak-check-1");
        spawn_session("leak-check-2");
        assert!(
            wait_until(Duration::from_secs(5), || {
                ghost_vt::session::list().map(|l| l.len()).unwrap_or(0) == 2
            }),
            "the sessions never came up, so this proves nothing about cleanup"
        );
        ghost_vt::session::list()
            .expect("list")
            .iter()
            .map(|i| i.pid)
            .collect::<Vec<_>>()
    });
    assert_eq!(pids.len(), 2, "captured both hosts' pids inside the run");
    for pid in pids {
        assert!(
            wait_until(Duration::from_secs(5), || !alive(pid)),
            "session host {pid} outlived its isolated run — with its runtime dir \
             gone it can never be reached again, and it holds an inotify instance \
             until someone kills it by hand"
        );
    }
}

/// The real-`sshd` fixture leaks the same way, in a place the local sweep cannot
/// see: a *remote* session's host runs under the fixture's own runtime root
/// (`<root>/run`), so `kill --all` in the test process's env never lists it. These
/// are the ones that survived after the local leak was plugged — ten per suite run.
#[test]
fn a_dropped_sshd_fixture_leaves_no_remote_hosts_behind() {
    let Some(remote) = RealRemote::start() else {
        eprintln!("no_leaks: no sshd available; skipping");
        return;
    };
    // SAFETY: process-global, but the fixture holds the real-sshd exclusion for
    // its whole life, so no other test reads this concurrently.
    unsafe { std::env::set_var("GHOST_REMOTE_GHOST", remote.remote_ghost()) };
    let r = RemoteSsh::new_in(remote.spec(), remote.control_dir()).expect("open transport");
    let ghost = retry_some(Duration::from_secs(10), || r.negotiate().ok()).expect("negotiate");
    r.spawn_host(&ghost, "leak-remote").expect("spawn remote");
    assert!(
        wait_until(Duration::from_secs(10), || r
            .list_sessions(&ghost)
            .map(|s| s.iter().any(|i| i.name == "leak-remote"))
            .unwrap_or(false)),
        "the remote session never came up, so this proves nothing about cleanup"
    );
    let pids: Vec<i32> = r
        .list_sessions(&ghost)
        .expect("list")
        .iter()
        .map(|i| i.pid)
        .collect();
    assert!(!pids.is_empty(), "captured the remote host's pid");
    drop(r);
    drop(remote);
    for pid in pids {
        assert!(
            wait_until(Duration::from_secs(10), || !alive(pid)),
            "remote session host {pid} outlived the fixture that spawned it"
        );
    }
}
