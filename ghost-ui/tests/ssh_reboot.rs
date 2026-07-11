//! SSH transport against a **real `sshd`** — Tier-1 high-fidelity remote.
//!
//! `ssh_remote.rs` gives each destination isolated dirs but fakes `ssh` with a
//! shell shim, so it can't exercise anything about the *real* ssh client/server
//! lifecycle — `ControlMaster`/`ControlPersist`, host-key handling, or what
//! happens to a multiplexed connection when the remote goes away. A whole class
//! of bugs (a session survives a remote *reboot*, a wedged control master, stale
//! control sockets) is structurally invisible to it.
//!
//! This fixture stands up a throwaway, unprivileged `sshd` on a loopback port
//! with a generated host key and a single authorized key, and drives ghost's
//! actual [`RemoteSsh`] transport at it over the real `ssh` binary. It can
//! [`reboot`](RealRemote::reboot) the host — drop every connection (so the local
//! master wedges, TCP dead but process persisted) and clear the tmpfs runtime dir
//! (so session sockets vanish), then come back on the same port — reproducing a
//! real remote reboot.
//!
//! The remote runs in its own `HOME`/XDG under a temp root (via a wrapper named as
//! `GHOST_REMOTE_GHOST`), so a remote session is genuinely off this machine's real
//! dirs, exactly like `ssh_remote.rs`.
//!
//! Skips (passes) cleanly where no `sshd` is installed, e.g. a minimal CI image.

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ghost_vt::client::Session;
use ghost_vt::connection::ConnectionSpec;
use ghost_vt::remote::RemoteSsh;
use ghost_vt::screen::Screen;

const GHOST: &str = env!("CARGO_BIN_EXE_ghost");

/// `GHOST_REMOTE_GHOST` is a process-global env var each test points at its own
/// isolated remote wrapper, so these real-`sshd` tests must not run concurrently or
/// they'd read each other's remote binary. Serialize them (recovering from a
/// poisoned lock so one test's panic doesn't cascade into "all skipped").
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Locate an `sshd` binary, or `None` so the caller can skip on a host without one.
fn find_sshd() -> Option<PathBuf> {
    ["/usr/sbin/sshd", "/usr/bin/sshd", "/usr/local/sbin/sshd"]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
}

/// Poll a loopback TCP connect until `port` accepts or `timeout` elapses.
fn wait_for_port(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let addr = format!("127.0.0.1:{port}");
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_millis(200)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if pred() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A real, unprivileged `sshd` on loopback plus everything needed to drive ghost's
/// transport at it. Killed and cleaned up on drop.
struct RealRemote {
    /// Keys, config, wrapper. Kept alive for the fixture's lifetime.
    _keys: tempfile::TempDir,
    /// Short-path dir for the ssh ControlPath (sun_path is capped at 108 bytes).
    control: tempfile::TempDir,
    /// The remote's isolated HOME/XDG root.
    remote_root: tempfile::TempDir,
    wrapper: PathBuf,
    identity: PathBuf,
    port: u16,
    sshd: Child,
}

impl RealRemote {
    /// Stand up the fixture, or `None` if no `sshd` is available (skip the test).
    fn start() -> Option<RealRemote> {
        let sshd_bin = find_sshd()?;
        let keys = tempfile::tempdir().ok()?;
        let kp = keys.path();
        let hostkey = kp.join("hostkey");
        let identity = kp.join("id");
        keygen(&hostkey);
        keygen(&identity);
        let authorized = kp.join("authorized_keys");
        std::fs::copy(identity.with_extension("pub"), &authorized).unwrap();
        std::fs::set_permissions(&authorized, std::fs::Permissions::from_mode(0o600)).unwrap();

        let remote_root = tempfile::tempdir().ok()?;
        for sub in ["run", "data", "cache", "home"] {
            std::fs::create_dir_all(remote_root.path().join(sub)).unwrap();
        }
        let wrapper = write_wrapper(kp, remote_root.path());

        let port = free_port();
        let config = kp.join("sshd_config");
        write_config(&config, port, &hostkey, &authorized);

        let sshd = spawn_sshd(&sshd_bin, &config, port)?;
        let control = tempfile::tempdir().ok()?; // short: /tmp/.tmpXXXX
        Some(RealRemote {
            _keys: keys,
            control,
            remote_root,
            wrapper,
            identity,
            port,
            sshd,
        })
    }

    /// A spec that reaches this fixture over the real ssh transport: loopback,
    /// our generated key, and host-key/prompt suppression so it never blocks on a
    /// tty or writes to the real user's `known_hosts`.
    fn spec(&self) -> ConnectionSpec {
        ConnectionSpec {
            host: "127.0.0.1".into(),
            port: Some(self.port),
            identity: Some(self.identity.clone()),
            extra: [
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "GlobalKnownHostsFile=/dev/null",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "BatchMode=yes",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            ..Default::default()
        }
    }

    /// The dir to hand [`RemoteSsh::new_in`] for the ControlPath (short enough).
    fn control_dir(&self) -> &Path {
        self.control.path()
    }

    /// The `GHOST_REMOTE_GHOST` value: a wrapper that runs the real binary in the
    /// remote's isolated HOME/XDG.
    fn remote_ghost(&self) -> &Path {
        &self.wrapper
    }

    /// Make the remote go **silent**, the way a real reboot or network partition
    /// does: the peer stops responding with **no FIN/RST**, so the local
    /// ControlMaster can't tell the connection died — it wedges (TCP dead, process
    /// kept alive by `ControlPersist`), which is the bug's trigger. `SIGSTOP` the
    /// per-connection `sshd` children to freeze the server side without closing the
    /// socket (a plain kill would RST and let the master exit cleanly — the case
    /// ghost already handles). The listener stays up, so the host is still
    /// reachable for a *fresh* connection; but ghost keeps multiplexing onto the
    /// wedged master until it reaps it. Also clears the tmpfs runtime dir, as a
    /// reboot would (session sockets vanish; persistent data survives).
    fn reboot(&mut self) {
        let _ = Command::new("pkill")
            .args(["-STOP", "-P", &self.sshd.id().to_string()])
            .status();
        let _ = std::fs::remove_dir_all(self.remote_root.path().join("run"));
        let _ = std::fs::create_dir_all(self.remote_root.path().join("run"));
    }

    /// A transient network partition — the connection is lost but the host and its
    /// sessions keep running. Same silent `SIGSTOP` of the per-connection `sshd`
    /// children as [`reboot`](Self::reboot) (so the local master wedges with no
    /// FIN/RST), but the runtime dir is **left intact**: the session hosts are
    /// separate long-lived processes, not `sshd` children, so they survive. Ghost
    /// must reap the wedged master, open a *fresh* connection, and re-attach to the
    /// still-running session (whose screen the host resyncs) — never relaunch it.
    /// The listener stays up, so a fresh connection succeeds; the frozen children
    /// are reaped on drop. New display client take-over displaces the frozen relay.
    fn drop_connection(&mut self) {
        let _ = Command::new("pkill")
            .args(["-STOP", "-P", &self.sshd.id().to_string()])
            .status();
    }
}

impl Drop for RealRemote {
    fn drop(&mut self) {
        let _ = Command::new("pkill")
            .args(["-9", "-P", &self.sshd.id().to_string()])
            .status();
        let _ = self.sshd.kill();
        let _ = self.sshd.wait();
    }
}

fn keygen(path: &Path) {
    let ok = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-q", "-f"])
        .arg(path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "ssh-keygen failed for {}", path.display());
}

fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn write_config(path: &Path, port: u16, hostkey: &Path, authorized: &Path) {
    let mut f = std::fs::File::create(path).unwrap();
    write!(
        f,
        "Port {port}\n\
         ListenAddress 127.0.0.1\n\
         HostKey {}\n\
         AuthorizedKeysFile {}\n\
         StrictModes no\n\
         UsePAM no\n\
         PasswordAuthentication no\n\
         PubkeyAuthentication yes\n\
         PrintMotd no\n\
         LogLevel ERROR\n",
        hostkey.display(),
        authorized.display(),
    )
    .unwrap();
}

/// A `ghost` launcher that runs the real binary in the remote's own HOME/XDG, so
/// a remote session lands off this machine's real dirs and a reboot's runtime-dir
/// wipe hits only the fixture.
fn write_wrapper(dir: &Path, remote_root: &Path) -> PathBuf {
    let wrapper = dir.join("ghost-remote");
    let r = remote_root.display();
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\n\
             export XDG_RUNTIME_DIR=\"{r}/run\"\n\
             export XDG_DATA_HOME=\"{r}/data\"\n\
             export XDG_CACHE_HOME=\"{r}/cache\"\n\
             export HOME=\"{r}/home\"\n\
             exec \"{GHOST}\" \"$@\"\n",
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    wrapper
}

/// Spawn `sshd` in the foreground (`-D`), so the returned [`Child`] IS the
/// listener — killing it (and its children) takes the host down.
fn spawn_sshd(sshd: &Path, config: &Path, port: u16) -> Option<Child> {
    let child = Command::new(sshd)
        .arg("-D")
        .arg("-f")
        .arg(config)
        .arg("-e") // log to stderr
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    wait_for_port(port, Duration::from_secs(5)).then_some(child)
}

/// The regression this fixture exists for. A remote reboot leaves ghost's shared
/// ControlMaster wedged — TCP dead, process persisted by `ControlPersist`. The
/// bug: `master_alive()` is `ssh -O check`, which reports the *local process* as
/// "Master running" even though the connection is dead, so `reap_wedged_master()`
/// never clears it and every reconnect multiplexes onto the corpse. Correct
/// behaviour: once the host is back, ghost reaps the wedged master and
/// re-negotiates. (Bounded well under the ~45s keepalive backstop so we're
/// testing the reap, not the master eventually self-expiring.)
#[test]
fn ghost_reconnects_a_remote_after_a_reboot() {
    let Some(mut remote) = RealRemote::start() else {
        eprintln!("ssh_reboot: no sshd available; skipping");
        return;
    };
    // Point negotiation at the isolated remote binary. This binary's only test.
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::set_var("GHOST_REMOTE_GHOST", remote.remote_ghost()) };

    let r = RemoteSsh::new_in(remote.spec(), remote.control_dir()).expect("open transport");
    assert!(
        r.negotiate().is_some(),
        "the initial connection should negotiate the remote ghost"
    );

    remote.reboot();

    assert!(
        wait_until(Duration::from_secs(20), || r.negotiate().is_some()),
        "after a remote reboot ghost must reap the wedged control master and \
         reconnect, but it never did within 20s"
    );
}

/// The recovery *mechanism* a dead REMOTE tile relies on: a session on the remote
/// is lost when the reboot wipes its tmpfs runtime dir, but ghost can recreate it
/// on the returned host over the transport (`spawn_host` = `ghost new -d`). That's
/// what `Cmd::Recreate`/`Cmd::Resurrect` route a remote id to (ghost-ui/src/main.rs)
/// instead of the local `spawn_dead` that refuses a remote id. Proven end-to-end
/// over real ssh, riding the same silent-reboot reconnect as the test above.
#[test]
fn a_remote_session_relaunches_on_its_host_after_a_reboot() {
    let Some(mut remote) = RealRemote::start() else {
        eprintln!("ssh_reboot: no sshd available; skipping");
        return;
    };
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::set_var("GHOST_REMOTE_GHOST", remote.remote_ghost()) };
    let r = RemoteSsh::new_in(remote.spec(), remote.control_dir()).expect("open transport");
    let ghost = r.negotiate().expect("initial negotiate");

    let listed = |ghost: &str| {
        r.list_sessions(ghost)
            .map(|s| s.iter().any(|i| i.name == "recovered"))
            .unwrap_or(false)
    };

    // A session exists on the remote before the reboot.
    r.spawn_host(&ghost, "recovered")
        .expect("spawn remote session");
    assert!(
        wait_until(Duration::from_secs(5), || listed(&ghost)),
        "the session was never listed on the remote before the reboot"
    );

    remote.reboot(); // wipes the runtime dir → the session's socket is gone

    // Recovery mirrors exactly what App::respawn_remote_dead / spawn_remote_session
    // now do: reap the wedged master, then relaunch on the host over a fresh
    // connection — reusing the known remote-ghost path, NOT re-negotiating (the
    // recovery path holds the already-negotiated host). Pre-fix this failed: the
    // relaunch multiplexed onto the wedged master.
    r.reap_wedged_master();
    r.spawn_host(&ghost, "recovered")
        .expect("relaunch on the returned host");
    assert!(
        wait_until(Duration::from_secs(10), || listed(&ghost)),
        "the remote session did not come back after relaunch on its host"
    );
}

/// Pump a driven session into `screen` until it renders `needle` or `timeout`.
fn pump_until(session: &mut Session, screen: &mut Screen, needle: &str, timeout: Duration) -> bool {
    wait_until(timeout, || {
        if let Ok(p) = session.pump() {
            screen.feed(&p.output);
        }
        screen.text().join("\n").contains(needle)
    })
}

/// Scenario the fixture's `drop_connection` exists for: the connection is lost
/// mid-session but the remote host and its session KEEP RUNNING (a transient
/// network partition, not a reboot). The recovery is fundamentally different from
/// a reboot — the session is NOT gone, so it must be RE-ATTACHED (and its screen
/// resynced by the host), never relaunched. This proves that recovery mechanism
/// end-to-end over real ssh: attach, put a marker on the screen, drop the
/// connection (silently, so the master wedges — the hard case), then reap the
/// wedged master and re-attach — the host resyncs the surviving screen, marker and
/// all. It is the ghost-vt half of the mid-session reconnect feature (the App-level
/// "reconnecting" state machine rides on this being recoverable).
#[test]
fn a_dropped_connection_reattaches_and_resyncs_a_surviving_session() {
    let Some(mut remote) = RealRemote::start() else {
        eprintln!("ssh_reboot: no sshd available; skipping");
        return;
    };
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::set_var("GHOST_REMOTE_GHOST", remote.remote_ghost()) };
    let r = RemoteSsh::new_in(remote.spec(), remote.control_dir()).expect("open transport");
    let ghost = r.negotiate().expect("initial negotiate");

    // A live session with recognizable content on its screen.
    r.spawn_host(&ghost, "survivor")
        .expect("spawn remote session");
    assert!(
        wait_until(Duration::from_secs(5), || r
            .list_sessions(&ghost)
            .map(|s| s.iter().any(|i| i.name == "survivor"))
            .unwrap_or(false)),
        "the session was never listed on the remote"
    );
    let mut screen = Screen::new(80, 24, 100);
    let mut session = Session::attach_ssh(r.pipe_command(&ghost, "survivor"), "survivor", 80, 24)
        .expect("attach");
    session
        .send_input(b"echo GHOSTMARK\n")
        .expect("type marker");
    assert!(
        pump_until(
            &mut session,
            &mut screen,
            "GHOSTMARK",
            Duration::from_secs(10)
        ),
        "the marker never rendered on the attached screen"
    );

    // The connection drops silently, but the session keeps running on the host.
    remote.drop_connection();
    // The driven client is now wedged onto a dead master; abandon it (the App's
    // reconnect path drops the dead Session the same way before re-attaching).
    drop(session);

    // Recovery: reap the wedged master, then RE-ATTACH (not relaunch) over a fresh
    // connection. The host take-over resyncs the surviving screen at our geometry.
    r.reap_wedged_master();
    let mut screen = Screen::new(80, 24, 100);
    let reattached = wait_until(Duration::from_secs(25), || {
        let Ok(mut s) = Session::attach_ssh(r.pipe_command(&ghost, "survivor"), "survivor", 80, 24)
        else {
            return false;
        };
        pump_until(&mut s, &mut screen, "GHOSTMARK", Duration::from_secs(5))
    });
    assert!(
        reattached,
        "after the connection dropped and was reaped, re-attaching to the surviving \
         session never resynced its screen (marker never came back)"
    );
}
