//! Single-instance guard for the windowed UI.
//!
//! Only one ghost UI process may own the runtime dir at a time. Ownership is an
//! exclusive `flock` on `<runtime>/ui.lock`, held for the process's whole life
//! and released automatically on exit or crash — so the next launch takes over
//! with no stale-file cleanup. The owner also listens on `<runtime>/ui.sock`; a
//! later launch connects there, asks for a new window, and exits, instead of
//! starting fresh and adopting (stealing) the running instance's sessions.
//!
//! Only the bare windowed launch ([`crate::interactive`]) goes through this — a
//! `ghost <subcommand>` (the CLI) and a re-exec'd session host both return long
//! before it, so neither contends for the lock.

use rustix::fs::{FlockOperation, flock};
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

/// What a later launch asks the owner to open. Sent as the single byte the
/// protocol has always carried, so an older owner reading `w` still opens a
/// window; an unknown byte degrades to a plain window rather than nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Request {
    /// A bare `ghost`: open a new window like File > New Window.
    NewWindow,
    /// `ghost --ssh-window` (the desktop entry's action, the Alt+S shortcut):
    /// open a window showing the "connect to a host" prompt.
    NewSshWindow,
}

impl Request {
    fn byte(self) -> u8 {
        match self {
            Request::NewWindow => b'w',
            Request::NewSshWindow => b's',
        }
    }

    fn from_byte(b: u8) -> Self {
        match b {
            b's' => Request::NewSshWindow,
            _ => Request::NewWindow,
        }
    }
}

/// This process's role in the single-instance protocol, decided at launch by
/// [`acquire`].
pub enum Role {
    /// We own the UI: run normally. `_lock` is held for the process's life (its
    /// release hands ownership to the next launch), and `listener`, when present,
    /// accepts new-window requests from later launches. Both are `None` only when
    /// the filesystem wouldn't cooperate — we still run, just without forwarding.
    Primary {
        _lock: Option<File>,
        listener: Option<UnixListener>,
    },
    /// A UI already owns the runtime dir; a new-window request was forwarded to
    /// it (best effort). This process should exit without touching sessions.
    Secondary,
}

/// Decide this process's [`Role`] against the real runtime dir. `want` is what
/// this launch asked for, forwarded to the owner when there already is one.
pub fn acquire(want: Request) -> Role {
    acquire_in(&ghost_vt::paths::runtime_dir(), want)
}

/// [`acquire`] over an explicit runtime `dir`, so it can be tested against a
/// tempdir without touching the process's real XDG location or env.
fn acquire_in(dir: &Path, want: Request) -> Role {
    let _ = std::fs::create_dir_all(dir);
    let lock_path = dir.join("ui.lock");
    let sock_path = dir.join("ui.sock");
    let lock = match File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false) // a pure lock file — never truncate or write its contents
        .open(&lock_path)
    {
        Ok(f) => f,
        // Can't even make a lock file: run standalone rather than block the user.
        Err(_) => {
            return Role::Primary {
                _lock: None,
                listener: None,
            };
        }
    };
    match flock(&lock, FlockOperation::NonBlockingLockExclusive) {
        // Nobody else holds it: we are the owner.
        Ok(()) => {
            // Clear any socket a crashed owner left behind — safe, because holding
            // the lock means no live owner is bound to it — then bind our own.
            let _ = std::fs::remove_file(&sock_path);
            let listener = UnixListener::bind(&sock_path).ok();
            Role::Primary {
                _lock: Some(lock),
                listener,
            }
        }
        // Held by a live owner: forward a new-window request and step aside.
        Err(rustix::io::Errno::WOULDBLOCK) => {
            forward(&sock_path, want);
            Role::Secondary
        }
        // An odd lock error: run standalone rather than risk exiting silently.
        Err(_) => Role::Primary {
            _lock: Some(lock),
            listener: None,
        },
    }
}

/// Ask the running owner to open a window of the kind this launch wanted. Best
/// effort, with a short retry to cover the sliver between the owner taking the
/// lock and binding the socket.
fn forward(sock_path: &Path, want: Request) {
    for attempt in 0..10 {
        if let Ok(mut s) = UnixStream::connect(sock_path) {
            let _ = s.write_all(&[want.byte()]);
            return;
        }
        if attempt < 9 {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
}

/// Spawn the owner's accept loop: run `on_request` once per forwarded request,
/// with the kind of window it asked for. Consumes `listener` and runs until the
/// process exits.
pub fn serve<F>(listener: UnixListener, on_request: F)
where
    F: Fn(Request) + Send + 'static,
{
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { continue };
            // Read the tiny request; a short/failed read still means "a launch
            // happened", so fall back to a plain window rather than dropping it.
            let mut buf = [0u8; 8];
            let n = conn.read(&mut buf).unwrap_or(0);
            on_request(if n > 0 {
                Request::from_byte(buf[0])
            } else {
                Request::NewWindow
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ghost-instance-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_forwarded_launch_carries_which_kind_of_window_was_asked_for() {
        // The desktop entry's "New SSH Window" action runs `ghost --ssh-window`
        // against an already-running ghost, so what reaches the owner must say
        // *connect prompt*, not just "a window".
        let dir = scratch("kind");
        let Role::Primary {
            listener: Some(listener),
            _lock,
        } = acquire_in(&dir, Request::NewWindow)
        else {
            panic!("the first launch should be the owner");
        };
        let (tx, rx) = mpsc::channel();
        serve(listener, move |req| {
            let _ = tx.send(req);
        });
        assert!(matches!(
            acquire_in(&dir, Request::NewSshWindow),
            Role::Secondary
        ));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Request::NewSshWindow,
            "the owner is told to open a connect window"
        );
        assert!(matches!(
            acquire_in(&dir, Request::NewWindow),
            Role::Secondary
        ));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Request::NewWindow,
            "a plain launch still asks for a plain window"
        );
        drop(_lock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_launch_steps_aside_and_forwards_a_new_window_request() {
        let dir = scratch("forward");
        // The first launch owns the runtime dir.
        let Role::Primary {
            listener: Some(listener),
            _lock,
        } = acquire_in(&dir, Request::NewWindow)
        else {
            panic!("the first launch should be the owner");
        };
        let (tx, rx) = mpsc::channel();
        serve(listener, move |_| {
            let _ = tx.send(());
        });
        // A second launch against the same dir can't take the lock: it becomes a
        // secondary and forwards a new-window request to the owner.
        assert!(
            matches!(acquire_in(&dir, Request::NewWindow), Role::Secondary),
            "the second launch steps aside"
        );
        assert!(
            rx.recv_timeout(Duration::from_secs(2)).is_ok(),
            "the owner receives the forwarded new-window request"
        );
        drop(_lock);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ownership_is_reclaimed_after_the_previous_instance_exits() {
        let dir = scratch("reclaim");
        // Own it, then drop the guard at the end of this statement — modelling the
        // previous instance exiting, which releases the lock.
        assert!(matches!(
            acquire_in(&dir, Request::NewWindow),
            Role::Primary { .. }
        ));
        // A later launch reclaims ownership rather than becoming a secondary.
        assert!(
            matches!(acquire_in(&dir, Request::NewWindow), Role::Primary { .. }),
            "the freed lock is reclaimed by the next launch"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
