//! Handling of the toplevel session-restore protocol. [vendored addition]
//!
//! `xdg_session_management_v1` lets a client ask the compositor to remember
//! where its windows were and, on the next run, to hand that state back. The
//! client names each toplevel; the compositor stores whatever it likes against
//! that name — geometry, workspace, maximized/fullscreen — and replays it into
//! the toplevel's *initial configure*.
//!
//! That last part is what forces this to live inside winit rather than in a
//! client crate: `restore_toplevel` is only meaningful between the
//! `xdg_toplevel` being created and the surface's first `wl_surface.commit`,
//! and both of those happen inside [`Window::new`]. A caller outside winit
//! never gets a turn in that window.
//!
//! There is no generated module for this protocol in `wayland-protocols` yet
//! (0.32.13 ships the XML and nothing else), so the bindings are scanned from
//! our own byte-for-byte copy of the staging XML. Drop [`generated`], the
//! `wayland-scanner` dependency and `protocols/` once upstream ships the module.
//!
//! [`Window::new`]: crate::platform_impl::wayland::window::Window::new

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use sctk::reexports::client::globals::{BindError, GlobalList};
use sctk::reexports::client::{delegate_dispatch, Connection, Dispatch, Proxy, QueueHandle};
use sctk::reexports::protocols::xdg::shell::client::xdg_toplevel::XdgToplevel;

use crate::platform_impl::wayland::state::WinitState;

/// Bindings scanned from `protocols/xdg-session-management-v1.xml`.
#[allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
#[allow(non_upper_case_globals, non_snake_case, unused_imports)]
#[allow(missing_docs, clippy::all)]
pub mod generated {
    use wayland_client;
    use wayland_client::protocol::*;
    use wayland_protocols::xdg::shell::client::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        use wayland_protocols::xdg::shell::client::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/xdg-session-management-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/xdg-session-management-v1.xml");
}

use generated::xdg_session_manager_v1::{Reason, XdgSessionManagerV1};
use generated::xdg_session_v1::{Event as SessionEvent, XdgSessionV1};
use generated::xdg_toplevel_session_v1::{Event as ToplevelSessionEvent, XdgToplevelSessionV1};

/// Why a session is being opened, and therefore how freely the compositor may
/// restore window-management state — see [`ActiveEventLoop::open_session`].
///
/// [`ActiveEventLoop::open_session`]: crate::platform::wayland::ActiveEventLoopExtWayland::open_session
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SessionReason {
    /// A newly launched instance, as from an app launcher.
    Launch,
    /// An instance recovering from a compositor or application crash.
    Recover,
    /// An instance being restored, as part of a restored desktop session.
    SessionRestore,
}

impl From<SessionReason> for Reason {
    fn from(reason: SessionReason) -> Self {
        match reason {
            SessionReason::Launch => Reason::Launch,
            SessionReason::Recover => Reason::Recover,
            SessionReason::SessionRestore => Reason::SessionRestore,
        }
    }
}

/// What the session's events told us, kept where the window and the public API
/// can read it without reaching back into [`WinitState`].
#[derive(Debug, Default)]
pub struct SessionShared {
    /// The id to persist. Seeded with the id we asked for and overwritten by
    /// `created`, so a caller always reads the id the *compositor* settled on:
    /// an id it has forgotten comes back as a fresh `created` rather than an
    /// error, and storing the one we sent would then point at nothing forever.
    id: Mutex<Option<String>>,

    /// Set by `replaced`: another client took the session over and everything
    /// we hold for it is now inert.
    replaced: AtomicBool,
}

impl SessionShared {
    pub fn id(&self) -> Option<String> {
        self.id.lock().unwrap().clone()
    }

    pub fn replaced(&self) -> bool {
        self.replaced.load(Ordering::Relaxed)
    }
}

/// The bound `xdg_session_manager_v1` global.
#[derive(Debug, Clone)]
pub struct SessionManager {
    manager: XdgSessionManagerV1,
}

impl SessionManager {
    pub fn new(
        globals: &GlobalList,
        queue_handle: &QueueHandle<WinitState>,
    ) -> Result<Self, BindError> {
        let manager = globals.bind(queue_handle, 1..=1, ())?;
        Ok(Self { manager })
    }

    /// Open the session named by `id`, or a brand new one when `id` is `None`.
    ///
    /// A `None`/unknown id yields a fresh session whose identifier arrives on
    /// [`SessionShared::id`]; a known one is restored under the same id.
    pub fn open(
        &self,
        reason: SessionReason,
        id: Option<&str>,
        queue_handle: &QueueHandle<WinitState>,
    ) -> Session {
        let shared = Arc::new(SessionShared {
            id: Mutex::new(id.map(ToOwned::to_owned)),
            replaced: AtomicBool::new(false),
        });
        let resource = self.manager.get_session(
            reason.into(),
            id.map(ToOwned::to_owned),
            queue_handle,
            shared.clone(),
        );
        Session { resource, shared }
    }
}

/// A session held open for this process. Dropping it without calling
/// [`Session::remove`] destroys the object and *preserves* the stored state,
/// which is what an ordinary quit wants.
#[derive(Debug)]
pub struct Session {
    resource: XdgSessionV1,
    shared: Arc<SessionShared>,
}

impl Session {
    pub fn shared(&self) -> Arc<SessionShared> {
        self.shared.clone()
    }

    /// Ask the compositor to restore `toplevel`'s state under `name`, before its
    /// surface is first committed.
    ///
    /// Always `restore_toplevel`, never `add_toplevel`: the protocol degrades an
    /// unknown name to an add — with no `restored` event — so one request covers
    /// both the first run and every run after it.
    ///
    /// The returned flag reads `true` once the compositor has said it is
    /// restoring this toplevel. Since the event is pinned to arrive before the
    /// first `xdg_toplevel.configure`, and window creation blocks until that
    /// configure lands, it is settled by the time a window exists.
    pub fn restore_toplevel(
        &self,
        toplevel: &XdgToplevel,
        name: String,
        queue_handle: &QueueHandle<WinitState>,
    ) -> ToplevelSession {
        let restored = Arc::new(AtomicBool::new(false));
        let handle = self
            .resource
            .restore_toplevel(toplevel, name, queue_handle, restored.clone());
        ToplevelSession { handle, restored }
    }

    /// Forget `name` and whatever the compositor stored against it.
    pub fn remove_toplevel(&self, name: String) {
        self.resource.remove_toplevel(name);
    }

    /// Forget the whole session, deleting its stored state. Consumes the
    /// session, since `remove` is a destructor.
    pub fn remove(self) {
        self.resource.remove();
    }

    /// Let go of the session object while *preserving* the stored state — what
    /// an ordinary quit wants, and what the spec asks for after `replaced`.
    /// Consumes the session, since `destroy` is a destructor.
    pub fn destroy(self) {
        self.resource.destroy();
    }
}

/// A client's handle on one named toplevel within a session.
#[derive(Debug)]
pub struct ToplevelSession {
    #[allow(dead_code)] // Held to keep the object alive for the toplevel's life.
    handle: XdgToplevelSessionV1,
    restored: Arc<AtomicBool>,
}

impl ToplevelSession {
    /// Whether the compositor restored previous state into this toplevel's
    /// initial configure.
    pub fn restored(&self) -> bool {
        self.restored.load(Ordering::Relaxed)
    }
}

impl Dispatch<XdgSessionManagerV1, (), WinitState> for SessionManager {
    fn event(
        _: &mut WinitState,
        _: &XdgSessionManagerV1,
        _: <XdgSessionManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        unreachable!("no events defined for xdg_session_manager_v1");
    }
}

impl Dispatch<XdgSessionV1, Arc<SessionShared>, WinitState> for SessionManager {
    fn event(
        state: &mut WinitState,
        _: &XdgSessionV1,
        event: <XdgSessionV1 as Proxy>::Event,
        shared: &Arc<SessionShared>,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        match event {
            SessionEvent::Created { session_id } => {
                *shared.id.lock().unwrap() = Some(session_id);
            }
            // Nothing to record: the id we asked for is the id we got, and which
            // toplevels were actually restored is reported per toplevel.
            SessionEvent::Restored => {}
            SessionEvent::Replaced => {
                // Flag first: the caller polls the shared state, which outlives
                // the session object below, so this must be visible whether or
                // not anything is still holding a `Session`.
                shared.replaced.store(true, Ordering::Relaxed);
                // Another client owns the id now and every request we could make
                // on this object is inert, so the spec asks us to destroy it.
                // Dropping it silently would also leave `WinitState::session`
                // occupied, and the `in_use` guard in `open_session` would then
                // refuse a fresh session for the rest of the process's life —
                // closing the one recovery a replaced client actually has.
                // `destroy`, never `remove`: the state belongs to the winner.
                if let Some(session) = state.session.take() {
                    session.destroy();
                }
            }
        }
    }
}

impl Dispatch<XdgToplevelSessionV1, Arc<AtomicBool>, WinitState> for SessionManager {
    fn event(
        _: &mut WinitState,
        _: &XdgToplevelSessionV1,
        event: <XdgToplevelSessionV1 as Proxy>::Event,
        restored: &Arc<AtomicBool>,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        match event {
            ToplevelSessionEvent::Restored => restored.store(true, Ordering::Relaxed),
        }
    }
}

delegate_dispatch!(WinitState: [XdgSessionManagerV1: ()] => SessionManager);
delegate_dispatch!(WinitState: [XdgSessionV1: Arc<SessionShared>] => SessionManager);
delegate_dispatch!(WinitState: [XdgToplevelSessionV1: Arc<AtomicBool>] => SessionManager);
