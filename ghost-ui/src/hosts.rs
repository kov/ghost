//! What the App knows about the machines hosting sessions: a remote host's
//! connection and state ([`HostLink`]), the composite ids its sessions are known
//! by locally, the per-host `ghost __watch` stream, and the local feed that
//! discovers this machine's sessions through the same watch.

use super::*;

/// A remote host reached over the ssh transport, retained so the fleet can poll
/// it. `remote` is shared with the watcher thread; `remote_ghost` is the negotiated
/// remote binary path both the poll and any attach reuse.
#[derive(Clone)]
pub(crate) struct RemoteHost {
    pub(crate) remote: Arc<ghost_vt::remote::RemoteSsh>,
    pub(crate) remote_ghost: String,
}

/// Everything the App knows about one remote host, keyed by target in
/// [`App::hosts`]. Each part has its own lifetime: the connection, listing,
/// remembered-set, environment and watcher last while a window references the
/// host ([`App::prune_remotes`]); a retry lasts while a group remembers a member
/// on it and we are not connected; queued restores until the host reconnects or
/// their window closes. An entry holding none of them is dropped.
#[derive(Default)]
pub(crate) struct HostLink {
    /// The open transport, while connected.
    pub(crate) conn: Option<RemoteHost>,
    /// The host's latest listing, as the host reported it, from its watcher.
    /// `None` means unknown — never listed yet, or unreachable — never "empty".
    pub(crate) listing: Option<Vec<ghost_vt::session::SessionInfo>>,
    /// The session names the host still holds a descriptor for (bare,
    /// un-namespaced) — its resurrection tickets, fetched by the watcher alongside
    /// each listing. `remembered_remotes` consults it to tell a member that exited
    /// cleanly on its host from one a reboot took down. `None` means unknown (an
    /// older remote ghost, or the fetch hasn't landed), and the sweep stays
    /// conservative: not-listed members remain relaunchable.
    pub(crate) remembered: Option<HashSet<String>>,
    /// What the host said about its machine in the `__probe` handshake. Only
    /// `home` is read today — to shorten a remote session's directory for display
    /// against the home it actually belongs to.
    pub(crate) env: Option<ghost_vt::remote::HostEnv>,
    /// The live `ghost __watch` stream that keeps `listing` fresh. Dropping it
    /// stops its thread and kills its ssh.
    pub(crate) watcher: Option<RemoteWatcher>,
    /// The stop flag of the background worker retrying the host forever while a
    /// group remembers a member on it and we are not connected (see
    /// [`App::retry_remembered_hosts`]). Presence dedupes.
    ///
    /// This is what makes waiting durable: the hold outlives the drop that
    /// started it, and — because the members are remembered in `groups.toml` — a
    /// ghost that is quit and relaunched while the host is still down picks the
    /// wait back up instead of forgetting the sessions.
    pub(crate) retry: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Remote members a startup restore is waiting to re-adopt on this host (see
    /// [`PendingRemote`], [`App::reconnect_restored_remotes`] /
    /// [`App::finish_remote_reconnect`]). Each carries the window's SAVED view
    /// mode and foreground flag, which can't be read back from the live window: a
    /// restored remote-only window always opens as a fleet (no local tile to dive
    /// into, so F9 can't force it single), so the saved intent must ride along
    /// here. Queued restores for a host that never reconnects just linger, drained
    /// on a successful reconnect or when their window closes.
    pub(crate) pending_restores: Vec<PendingRemote>,
    /// Set while a transport health probe runs for the host
    /// ([`App::probe_remote_transports`]); the prober clears it when done, so one
    /// runs at a time no matter how many wake suspicions fire.
    pub(crate) probing: Arc<std::sync::atomic::AtomicBool>,
}

impl HostLink {
    /// Whether the entry holds nothing worth keeping.
    pub(crate) fn is_idle(&self) -> bool {
        self.conn.is_none()
            && self.listing.is_none()
            && self.remembered.is_none()
            && self.env.is_none()
            && self.watcher.is_none()
            && self.retry.is_none()
            && self.pending_restores.is_empty()
    }
}

/// The fleet id for remote session `real` on `target` — the composite a remote
/// session is known by *locally* (window client key, `mine`, fleet tile id), so a
/// session this window drives over the transport and the same session the watcher
/// discovers share one identity. Recovered to `(target, real)` by
/// [`remote_id_parts`]; only the transport layer uses the bare `real` id.
pub(crate) fn remote_fleet_id(target: &str, real: &str) -> String {
    format!("{target}{REMOTE_ID_SEP}{real}")
}

/// How a session id should be reached for a control action (rename/kill). A
/// remote id is *self-describing* — [`remote_fleet_id`] formats it as
/// `<target>␟<real>` — so its host and real name are recovered from the id itself,
/// never from a lookup that could lag. A remote id is thus ALWAYS routed over the transport, never spoken to a local control socket (a
/// bogus local socket yields a misleading "hosted by an older ghost" error).
pub(crate) fn remote_id_parts(id: &str) -> Option<(&str, &str)> {
    id.split_once(REMOTE_ID_SEP)
}

/// [`remote_id_parts`], owned.
pub(crate) fn remote_id_owned(id: &str) -> Option<(String, String)> {
    remote_id_parts(id).map(|(target, real)| (target.to_string(), real.to_string()))
}

/// Floor between reconnect attempts of a host's watch stream, so a host whose
/// `ghost __watch` exits at once can't spin.
const REMOTE_WATCH_RETRY: Duration = Duration::from_millis(1500);

/// Consecutive dropped watch streams (no listing pushed in between) before a
/// remote host is reported unreachable — a grace period so a momentary blip
/// doesn't flicker its members.
const REMOTE_WATCH_MAX_FAILURES: u32 = 3;

/// A remote host's listing as the local fleet knows it: each session under its
/// fleet id, its host's name for it (or its display name) shown as its display
/// name, and tagged with the host's connection so it renders as a remote tile
/// badged with the host.
pub(crate) fn remote_listing(
    target: &str,
    infos: &[ghost_vt::session::SessionInfo],
) -> Vec<ghost_ui_core::Listed> {
    let spec = ConnectionSpec::parse_target(target);
    infos
        .iter()
        .map(|i| {
            let mut info = i.clone();
            if info.display_name.is_empty() {
                info.display_name = info.name.clone();
            }
            info.connection = spec.clone();
            ghost_ui_core::Listed {
                id: remote_fleet_id(target, &i.name),
                info,
            }
        })
        .collect()
}

/// A live pushed session-set watch for one connected host: a background thread
/// runs `ghost __watch` over the (already-authenticated) transport and streams
/// each listing back as a [`UserEvent::RemoteSessions`], so the fleet updates the
/// instant a remote session changes rather than on a timer. Dropping the handle
/// stops it — the flag ends the loop and killing the in-flight ssh unwinds a read
/// blocked between listings — so a watcher lives exactly as long as its host's
/// connection in [`App::hosts`] (until the last window referencing it closes, or
/// the app exits).
pub(crate) struct RemoteWatcher {
    stop: Arc<std::sync::atomic::AtomicBool>,
    /// The currently-running `ghost __watch` child, shared so a stop can kill it
    /// mid-read (the reader is otherwise blocked until the next listing).
    child: Arc<std::sync::Mutex<Option<std::process::Child>>>,
}

impl Drop for RemoteWatcher {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut g) = self.child.lock()
            && let Some(c) = g.as_mut()
        {
            let _ = c.kill();
        }
    }
}

/// Start a [`RemoteWatcher`] for `host`: its thread reconnects the `ghost __watch`
/// stream (with a floor between attempts) until stopped, posting each fresh
/// listing and clearing the host's tiles once it has been unreachable for a grace
/// period. Off the event loop, so a slow or blocked ssh never stalls the UI.
pub(crate) fn start_remote_watcher(
    target: String,
    host: RemoteHost,
    sink: Arc<dyn EventSink>,
) -> RemoteWatcher {
    use std::sync::atomic::Ordering::Relaxed;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let child: Arc<std::sync::Mutex<Option<std::process::Child>>> = Arc::default();
    let (t_stop, t_child) = (stop.clone(), child.clone());
    std::thread::spawn(move || {
        // Asked once, here rather than at registration: the handshake is an ssh
        // round trip, and this thread is already the place a host's round trips
        // are allowed to be slow. What it answers describes the machine, so it
        // stays true for as long as that ghost is the one we speak to.
        if let Some(env) = host.remote.probe_env(&host.remote_ghost) {
            sink.post(UserEvent::RemoteEnv {
                target: target.clone(),
                env,
            });
        }
        let mut failures: u32 = 0;
        while !t_stop.load(Relaxed) {
            let pushed = watch_stream_once(&target, &host, &sink, &t_stop, &t_child);
            if t_stop.load(Relaxed) {
                break;
            }
            if pushed {
                failures = 0;
            } else {
                failures = failures.saturating_add(1);
                // Unreachable for a grace period: say so. An empty listing
                // would claim the host answered and holds nothing, turning
                // members that are likely still running into exited ones.
                if failures >= REMOTE_WATCH_MAX_FAILURES
                    && !sink.post(UserEvent::RemoteUnreachable {
                        target: target.clone(),
                    })
                {
                    break; // the event loop closed
                }
            }
            std::thread::sleep(REMOTE_WATCH_RETRY);
        }
    });
    RemoteWatcher { stop, child }
}

/// Run one `ghost __watch` stream to completion — it closes when the host exits,
/// the connection drops, or a stop kills the child — posting each JSON listing as
/// a namespaced [`UserEvent::RemoteSessions`]. Returns whether any listing was
/// pushed, so the caller tells a live host from a dead one.
fn watch_stream_once(
    target: &str,
    host: &RemoteHost,
    sink: &Arc<dyn EventSink>,
    stop: &std::sync::atomic::AtomicBool,
    child_slot: &std::sync::Mutex<Option<std::process::Child>>,
) -> bool {
    use std::io::BufRead;
    use std::sync::atomic::Ordering::Relaxed;
    let mut cmd = host.remote.watch_command(&host.remote_ghost);
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut proc = match cmd.spawn() {
        Ok(p) => p,
        Err(_) => return false,
    };
    let Some(stdout) = proc.stdout.take() else {
        return false;
    };
    if let Ok(mut g) = child_slot.lock() {
        *g = Some(proc);
    }
    let mut pushed = false;
    let mut warned_parse = false;
    let mut last_line: Option<String> = None;
    for line in std::io::BufReader::new(stdout).lines() {
        if stop.load(Relaxed) {
            break;
        }
        let Ok(line) = line else { break };
        let infos = match ghost_vt::watch::parse_listing(&line) {
            Ok(infos) => infos,
            // A parse failure means every line from this host fails the same way (a
            // field mismatch, not a torn line), so it silently costs the whole remote
            // fleet. Say so once per stream instead of dropping it without a trace.
            Err(e) => {
                if !warned_parse {
                    warned_parse = true;
                    eprintln!(
                        "ghost: cannot parse the session listing from {target} ({e}); \
                         its fleet will not update"
                    );
                }
                continue;
            }
        };
        pushed = true;
        // A *changed* listing may mean a session ended — refresh the host's
        // remembered-set (its descriptor names) so the dead-member sweep can
        // tell a clean exit (forget) from an unclean one (relaunchable). The
        // heartbeat re-emits of an unchanged listing skip the extra round trip.
        // A failed fetch posts `None`: unknown, never stale.
        let changed = last_line.as_deref() != Some(line.as_str());
        last_line = Some(line);
        if !sink.post(UserEvent::RemoteSessions {
            target: target.to_string(),
            infos,
        }) {
            stop.store(true, Relaxed); // event loop gone: end the whole watcher
            break;
        }
        if changed
            && !sink.post(UserEvent::RemoteRemembered {
                target: target.to_string(),
                names: host.remote.remembered_sessions(&host.remote_ghost).ok(),
            })
        {
            stop.store(true, Relaxed);
            break;
        }
    }
    // Reap our child (a concurrent stop may already have killed it).
    if let Ok(mut g) = child_slot.lock()
        && let Some(mut c) = g.take()
    {
        let _ = c.kill();
        let _ = c.wait();
    }
    pushed
}

/// This machine's session set, pushed: a thread streams the listing as a
/// [`UserEvent::LocalSessions`] now and on every change — the same watch
/// `ghost __watch` streams to a remote initiator, so local and remote sessions
/// are discovered one way. Dropping the handle drops the watch, which ends the
/// thread.
pub(crate) struct LocalFeed {
    /// Dropped first (see `Drop`), which ends the thread's stream.
    watcher: Option<notify::RecommendedWatcher>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for LocalFeed {
    /// Wait for the thread, so no listing (which prunes) outlives the feed.
    fn drop(&mut self) {
        self.watcher.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl LocalFeed {
    /// Start the feed, posting to `sink`. `None` (no watch backend, or no thread)
    /// leaves listings to a synchronous read each time one is wanted.
    pub(crate) fn start(sink: Arc<dyn EventSink>) -> Option<Self> {
        let (watcher, changes) = ghost_vt::watch::watch_set().ok()?;
        let thread = std::thread::Builder::new()
            .name("ghost-local-feed".into())
            .spawn(move || {
                changes.stream(|sessions| sink.post(UserEvent::LocalSessions(sessions.to_vec())))
            })
            .ok()?;
        Some(Self {
            watcher: Some(watcher),
            thread: Some(thread),
        })
    }
}
