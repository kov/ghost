//! `ghost-gtk` — a simple native terminal on top of ghost.
//!
//! Sidebar-primary layout: an overlay sidebar lists the whole ghost session
//! *fleet* (every session, not just ones open here), and the content area shows
//! one session's terminal at a time. Picking a row attaches/shows that session;
//! the trash button kills it (with confirmation); ＋ starts a new one. Each open
//! session is a feed-only VTE widget driven by the headless [`Session`] client;
//! closing the window detaches them all (they keep running, reattachable).
//!
//! Sessions are created in-process — `server::spawn` re-execs itself into the
//! host, so spawning is safe from this multithreaded GTK process — and the child
//! is deferred until our attach so its startup queries reach us.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use adw::prelude::*;
use gtk4::glib;
use gtk4::{EventControllerKey, PropagationPhase, gdk, gio, pango};
use vte4::prelude::*;
use vte4::{Format, Terminal};

mod settings;

use settings::Settings;

use ghost_vt::client::{self, Session};
use ghost_vt::server::{self, SpawnOpts};
use ghost_vt::session::{self, SessionInfo};
use ghost_vt::{paths, record, screen};

const APP_ID: &str = "dev.ghost.Terminal";
/// Stack page shown when no session is open.
const EMPTY_PAGE: &str = "__empty__";
/// Approximate header-bar height and window padding, added to the grid pixels so
/// the configured columns×rows roughly fit on first show (VTE then derives the
/// real grid from the actual allocation).
const HEADER_BAR_PX: i32 = 47;
const WINDOW_PAD_PX: i32 = 16;

/// A session currently open in this window: its terminal widget and the headless
/// client behind an `Option` so closing it can drop (detach) the client while the
/// drain timer, sharing the cell, notices and stops.
struct OpenSession {
    session: Rc<RefCell<Option<Session>>>,
    terminal: Terminal,
}

/// One sidebar row reduced to what's visible: `(name, title, group, current)`.
/// The list is rebuilt only when the vector of these changes — including when a
/// session moves between groups (e.g. another window takes it over).
type RowSig = (String, String, Group, bool);

/// The process-wide source of session names. Every window mints from the same
/// counter, so two windows in this single-instance app can never collide on
/// `ghost-<pid>-<n>`.
static NEXT_SESSION: AtomicU32 = AtomicU32::new(0);

/// A fresh, process-unique name for a newly created session.
fn next_session_name() -> String {
    let n = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    format!("ghost-{}-{n}", std::process::id())
}

/// Which sidebar section a session belongs to, from this window's vantage point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    /// Attached to (open in) this window.
    Here,
    /// Attached to a different window or client.
    Elsewhere,
    /// Not attached anywhere — free to open here.
    Detached,
}

impl Group {
    /// Sort key giving the section order: here, then detached (free to open
    /// here), then sessions held elsewhere.
    fn order(self) -> u8 {
        match self {
            Group::Here => 0,
            Group::Detached => 1,
            Group::Elsewhere => 2,
        }
    }

    /// The header shown above the group's first row.
    fn header(self) -> &'static str {
        match self {
            Group::Here => "This window",
            Group::Elsewhere => "Open elsewhere",
            Group::Detached => "Detached",
        }
    }
}

/// Classify a session for this window: open here takes precedence (we are its
/// attached client), otherwise the host's attach marker separates a session held
/// in another window/client from a genuinely detached one.
fn group_of(open_here: bool, info: &SessionInfo) -> Group {
    if open_here {
        Group::Here
    } else if info.attached {
        Group::Elsewhere
    } else {
        Group::Detached
    }
}

/// Shared window state. Cheap to clone (widgets are ref-counted, the rest is
/// `Rc`), so it's handed to every signal closure.
#[derive(Clone)]
struct Ui {
    window: adw::ApplicationWindow,
    split: adw::OverlaySplitView,
    stack: gtk4::Stack,
    list: gtk4::ListBox,
    content_title: adw::WindowTitle,
    open: Rc<RefCell<HashMap<String, OpenSession>>>,
    current: Rc<RefCell<Option<String>>>,
    /// Live settings (font, scheme, transparency, persisted zoom), shared so
    /// signal closures can read and update them.
    settings: Rc<RefCell<Settings>>,
    /// Last rendered sidebar signature, so periodic refreshes only rebuild the
    /// list when something actually changed.
    last_sig: Rc<RefCell<Vec<RowSig>>>,
    /// The (non-modal) preferences window while it's open, so re-triggering the
    /// shortcut focuses the existing one instead of stacking duplicates.
    prefs_window: Rc<RefCell<Option<adw::Window>>>,
}

/// Where a GUI-launched session should start. `server::spawn` captures the
/// process's working directory for the child, but a bundled launch (launchd on
/// macOS, a desktop file on Linux) starts us at `/` — so sessions would open in
/// `/`. In that case (or with no cwd at all) fall back to `home`; a real working
/// directory, e.g. when launched from a terminal, is kept. Returns the directory
/// to switch to, or `None` to leave the cwd as-is.
fn home_launch_dir(
    cwd: Option<&std::path::Path>,
    home: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    match cwd {
        Some(c) if c != std::path::Path::new("/") => None,
        _ => home.map(std::path::Path::to_path_buf),
    }
}

/// An accelerator built from the platform's primary command modifier plus `key`,
/// optionally with Shift. On macOS that modifier is Command — which GTK names
/// `<Meta>`; GTK's own `<Primary>` resolves to *Control* there, not Command, so
/// these user-facing shortcuts name the modifier explicitly. Elsewhere it stays
/// `<Primary>` (Control on Linux/X11/Wayland).
fn primary_accel(key: &str, shift: bool) -> String {
    let cmd = if cfg!(target_os = "macos") {
        "<Meta>"
    } else {
        "<Primary>"
    };
    let shift = if shift { "<Shift>" } else { "" };
    format!("{cmd}{shift}{key}")
}

fn main() -> glib::ExitCode {
    // If `server::spawn` re-exec'd us as a session host, become it and never
    // return here — before any GTK init.
    server::run_host_if_invoked();

    // A bundled launch lands us at `/`; point new sessions at the user's home
    // instead. `server::spawn` reads our cwd when it starts each session's child.
    if let Some(dir) = home_launch_dir(
        std::env::current_dir().ok().as_deref(),
        std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .as_deref(),
    ) {
        let _ = std::env::set_current_dir(dir);
    }

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(install_app_actions);
    app.connect_activate(build_window);
    app.run()
}

/// App-wide actions, registered once at startup. `new-window` opens another
/// terminal window (Cmd-N on macOS, Ctrl-N on Linux — see [`primary_accel`]); each
/// window also registers its own window-scoped actions in [`install_actions`].
fn install_app_actions(app: &adw::Application) {
    let new_window = gio::SimpleAction::new("new-window", None);
    {
        let app = app.clone();
        new_window.connect_activate(move |_, _| build_window(&app));
    }
    app.add_action(&new_window);
    app.set_accels_for_action("app.new-window", &[&primary_accel("n", false)]);
}

fn build_window(app: &adw::Application) {
    // macOS Dock-icon menu (right-click → New Window). Idempotent; needs GTK's
    // NSApp delegate to exist, which it does by the time we build a window.
    #[cfg(target_os = "macos")]
    dock::install();

    let cfg = Settings::load();
    install_css();

    // Sidebar: a fleet list under a header with a New button.
    let list = gtk4::ListBox::new();
    list.set_selection_mode(gtk4::SelectionMode::Single);
    list.add_css_class("navigation-sidebar");
    let scroller = gtk4::ScrolledWindow::builder()
        .child(&list)
        .vexpand(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .build();
    let new_button = gtk4::Button::from_icon_name("list-add-symbolic");
    new_button.set_tooltip_text(Some("New session"));
    let sidebar_header = adw::HeaderBar::new();
    sidebar_header.set_title_widget(Some(&adw::WindowTitle::new("Sessions", "")));
    sidebar_header.pack_start(&new_button);
    let sidebar = adw::ToolbarView::new();
    sidebar.add_top_bar(&sidebar_header);
    sidebar.set_content(Some(&scroller));
    // Raised top bars get an opaque, libadwaita-managed background (with a proper
    // `:backdrop` variant) so the chrome stays solid over a transparent window —
    // even when unfocused. A flat bar would inherit the window's transparency.
    sidebar.set_top_bar_style(adw::ToolbarStyle::Raised);

    // Content: a stack of terminals, plus an empty-state page.
    let stack = gtk4::Stack::new();
    let empty_new = gtk4::Button::builder()
        .label("New session")
        .halign(gtk4::Align::Center)
        .css_classes(["pill", "suggested-action"])
        .build();
    let empty = adw::StatusPage::builder()
        .icon_name("utilities-terminal-symbolic")
        .title("No session open")
        .description("Pick a session from the sidebar, or start a new one.")
        .child(&empty_new)
        .build();
    empty.add_css_class("ghost-empty");
    stack.add_named(&empty, Some(EMPTY_PAGE));

    let sidebar_toggle = gtk4::ToggleButton::new();
    sidebar_toggle.set_icon_name("sidebar-show-symbolic");
    sidebar_toggle.set_tooltip_text(Some("Show sessions"));
    let content_title = adw::WindowTitle::new("ghost", "");
    let content_header = adw::HeaderBar::new();
    content_header.set_title_widget(Some(&content_title));
    content_header.pack_start(&sidebar_toggle);
    let menu = gio::Menu::new();
    menu.append(Some("New Window"), Some("app.new-window"));
    menu.append(Some("Preferences"), Some("win.preferences"));
    let menu_button = gtk4::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .tooltip_text("Main menu")
        .build();
    content_header.pack_end(&menu_button);
    let content = adw::ToolbarView::new();
    content.add_top_bar(&content_header);
    content.set_content(Some(&stack));
    content.set_top_bar_style(adw::ToolbarStyle::Raised);

    // Always-overlay split: the sidebar floats over (and dims) the terminal —
    // "pops above" — rather than permanently splitting. The toggle shows/hides it.
    let split = adw::OverlaySplitView::builder()
        .sidebar(&sidebar)
        .content(&content)
        .collapsed(true)
        .build();
    sidebar_toggle
        .bind_property("active", &split, "show-sidebar")
        .bidirectional()
        .sync_create()
        .build();
    // Initial sidebar visibility is decided by the first `refresh()` below
    // (open iff there are existing sessions to pick) — see `show_empty`.

    // Seed the window to roughly the configured columns×rows. The cell estimate
    // is approximate; VTE derives the real grid from the actual allocation.
    let (cell_w, cell_h) = estimate_cell(cfg.font.size * cfg.zoom.scale);
    let (grid_w, grid_h) =
        settings::window_pixels(cfg.window.columns, cfg.window.rows, cell_w, cell_h);
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("ghost")
        .default_width(grid_w + WINDOW_PAD_PX)
        .default_height(grid_h + HEADER_BAR_PX + WINDOW_PAD_PX)
        .content(&split)
        .build();
    update_window_chrome(&window, cfg.window.transparency);

    let ui = Ui {
        window: window.clone(),
        split,
        stack,
        list,
        content_title,
        open: Rc::new(RefCell::new(HashMap::new())),
        current: Rc::new(RefCell::new(None)),
        last_sig: Rc::new(RefCell::new(Vec::new())),
        settings: Rc::new(RefCell::new(cfg)),
        prefs_window: Rc::new(RefCell::new(None)),
    };
    install_actions(&ui, app);

    {
        let ui = ui.clone();
        new_button.connect_clicked(move |_| ui.new_session());
    }
    {
        let ui = ui.clone();
        empty_new.connect_clicked(move |_| ui.new_session());
    }

    ui.refresh();
    // Keep the fleet list current — also reflects sessions created or killed
    // outside this window. The signature check makes an unchanged tick a no-op.
    {
        let ui = ui.clone();
        glib::timeout_add_local(Duration::from_secs(2), move || {
            ui.refresh();
            glib::ControlFlow::Continue
        });
    }
    // Closing this window detaches its sessions (they keep running and can be
    // reopened here or in another window) instead of leaving their drain timers
    // holding the clients attached for the rest of the process's life.
    {
        let ui = ui.clone();
        window.connect_close_request(move |_| {
            ui.detach_all();
            glib::Propagation::Proceed
        });
    }
    window.present();
}

impl Ui {
    /// Attach to (or, if already open, just switch to) a session and show it.
    fn open_session(&self, name: &str) {
        if !self.open.borrow().contains_key(name) {
            let session = match Session::attach_deferred(name) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("ghost-gtk: cannot attach to '{name}': {e}");
                    return;
                }
            };
            // Non-blocking reads: the drain timer polls, never blocking the UI.
            let _ = session.set_read_timeout(Some(Duration::from_millis(1)));
            let terminal = Terminal::new();
            settings::apply(&self.settings.borrow(), &terminal);
            install_clipboard_keys(&terminal);
            install_window_move_drag(&terminal);
            let session = Rc::new(RefCell::new(Some(session)));
            install_meta_keys(&terminal, &session);
            {
                // commit -> session input (keys, mouse, paste, query replies).
                let session = session.clone();
                terminal.connect_commit(move |_term, text, _size| {
                    if let Some(s) = session.borrow_mut().as_mut() {
                        let _ = s.send_input(text.as_bytes());
                    }
                });
            }
            {
                // Live content-header title when this session is the visible one.
                let ui = self.clone();
                let name = name.to_string();
                terminal.connect_window_title_changed(move |term| {
                    if ui.current.borrow().as_deref() == Some(name.as_str()) {
                        ui.content_title
                            .set_title(&title_or(term.window_title(), &name));
                    }
                });
            }
            self.stack.add_named(&terminal, Some(name));
            self.drive(name, &terminal, &session);
            self.open
                .borrow_mut()
                .insert(name.to_string(), OpenSession { session, terminal });
        }
        self.show(name);
        self.schedule_refresh();
    }

    /// Make an already-open session the visible one.
    fn show(&self, name: &str) {
        self.stack.set_visible_child_name(name);
        *self.current.borrow_mut() = Some(name.to_string());
        let title = self
            .open
            .borrow()
            .get(name)
            .map(|o| title_or(o.terminal.window_title(), name))
            .unwrap_or_else(|| name.to_string());
        self.content_title.set_title(&title);
        self.content_title.set_subtitle(name);
        // Reveal the terminal: dismiss the overlay sidebar, then focus the
        // terminal (deferred — see `focus_current_terminal`).
        self.split.set_show_sidebar(false);
        self.focus_current_terminal();
    }

    /// Focus the current session's terminal, deferred to idle so it lands AFTER
    /// the overlay sidebar's own focus handling on collapse — done inline it would
    /// be overridden, leaving focus on the sidebar toggle button.
    fn focus_current_terminal(&self) {
        let ui = self.clone();
        glib::idle_add_local_once(move || {
            if let Some(name) = ui.current.borrow().clone()
                && let Some(o) = ui.open.borrow().get(&name)
            {
                o.terminal.grab_focus();
            }
        });
    }

    /// Toggle the overlay sidebar (F9). Opening focuses the session list for
    /// keyboard navigation; closing returns focus to the current terminal.
    fn toggle_sidebar(&self) {
        let show = !self.split.shows_sidebar();
        self.split.set_show_sidebar(show);
        if show {
            self.focus_sidebar_list();
        } else {
            self.focus_current_terminal();
        }
    }

    /// Focus the sidebar's session list for keyboard navigation (F9 → arrows →
    /// Enter). Deferred to idle so it lands after the reveal maps the list, and it
    /// targets the selected (or first) row so the arrow keys have a cursor to move
    /// from and Enter activates a session.
    fn focus_sidebar_list(&self) {
        let list = self.list.clone();
        glib::idle_add_local_once(move || {
            if let Some(row) = list.selected_row().or_else(|| list.row_at_index(0)) {
                row.grab_focus();
            } else {
                list.grab_focus();
            }
        });
    }

    /// Show the empty-state page (no session open). Reveal the sidebar so an
    /// existing session can be picked — but only when the fleet is non-empty.
    /// With no sessions at all, the (empty) sidebar would just cover the
    /// empty-state "New session" button, so leave it dismissed and let that
    /// button be the call to action.
    fn show_empty(&self) {
        self.stack.set_visible_child_name(EMPTY_PAGE);
        *self.current.borrow_mut() = None;
        self.content_title.set_title("ghost");
        self.content_title.set_subtitle("");
        let has_sessions = session::list().is_ok_and(|s| !s.is_empty());
        self.split.set_show_sidebar(has_sessions);
    }

    /// Create a brand-new session (deferred start) and open it.
    fn new_session(&self) {
        let name = next_session_name();
        let opts = SpawnOpts {
            name: name.clone(),
            command: vec![], // the user's $SHELL
            size: (80, 24),  // provisional; the attach handshake sends the real size
            record: Some(paths::recording_path(&name)),
            scrollback: screen::DEFAULT_SCROLLBACK,
            max_recording_bytes: Some(record::DEFAULT_MAX_RECORDING_BYTES),
            start_on_attach: true,
        };
        if let Err(e) = server::spawn(opts) {
            eprintln!("ghost-gtk: could not start session '{name}': {e}");
            return;
        }
        self.open_session(&name);
    }

    /// Detach every session open in this window: drop each client (set its cell to
    /// `None`) so the drain timer sharing the cell stops on its next tick and the
    /// host clears its attach marker. The sessions keep running, reattachable.
    fn detach_all(&self) {
        for (_name, open) in self.open.borrow_mut().drain() {
            *open.session.borrow_mut() = None;
        }
    }

    /// Ask before killing — a kill ends the program and can't be undone.
    fn confirm_kill(&self, name: &str) {
        let dialog = adw::AlertDialog::new(
            Some("Kill session?"),
            Some(&format!(
                "\u{201c}{name}\u{201d} and its running program will be terminated. This can't be undone."
            )),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("kill", "Kill");
        dialog.set_response_appearance("kill", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let ui = self.clone();
        let name = name.to_string();
        dialog.connect_response(None, move |_, response| {
            if response == "kill" {
                ui.kill(&name);
            }
        });
        dialog.present(Some(&self.window));
    }

    /// Kill a session (and remove it from the view if it's open here).
    fn kill(&self, name: &str) {
        if let Err(e) = session::kill_session(name) {
            eprintln!("ghost-gtk: could not kill '{name}': {e}");
        }
        self.close_session(name);
    }

    /// Prompt for a new name (F2). Enter or the Rename button confirms.
    fn show_rename(&self, name: &str) {
        let entry = gtk4::Entry::new();
        entry.set_text(name);
        let dialog = adw::AlertDialog::new(Some("Rename session"), None);
        dialog.set_extra_child(Some(&entry));
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("rename", "Rename");
        dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("rename"));
        dialog.set_close_response("cancel");
        {
            let ui = self.clone();
            let old = name.to_string();
            let entry = entry.clone();
            dialog.connect_response(Some("rename"), move |_, _| {
                ui.rename(&old, entry.text().trim());
            });
        }
        {
            // Enter in the field confirms. Closing fires the "cancel" close
            // response, which the scoped handler above ignores — so no double run.
            let ui = self.clone();
            let old = name.to_string();
            let dialog = dialog.clone();
            entry.connect_activate(move |entry| {
                let new = entry.text();
                dialog.close();
                ui.rename(&old, new.trim());
            });
        }
        dialog.present(Some(&self.window));
        entry.grab_focus();
    }

    /// Rename a session via the host, then reconcile the view. A no-op for an
    /// empty or unchanged name. An open session is re-attached under the new name
    /// (the live connection survives the rename, but its name-keyed bookkeeping
    /// would otherwise go stale).
    fn rename(&self, old: &str, new: &str) {
        if new.is_empty() || new == old {
            return;
        }
        match client::rename(old, new) {
            Ok(()) => {
                if self.open.borrow().contains_key(old) {
                    self.close_session(old);
                    self.open_session(new);
                } else {
                    self.schedule_refresh();
                }
            }
            Err(e) => self.show_error("Could not rename session", &e.to_string()),
        }
    }

    /// A simple one-button error dialog.
    fn show_error(&self, heading: &str, body: &str) {
        let dialog = adw::AlertDialog::new(Some(heading), Some(body));
        dialog.add_response("ok", "OK");
        dialog.set_default_response(Some("ok"));
        dialog.set_close_response("ok");
        dialog.present(Some(&self.window));
    }

    /// Remove a session from this window — detach (drop the client) and drop its
    /// terminal — without killing it. If it was visible, fall back to another
    /// open session or the empty state.
    fn close_session(&self, name: &str) {
        if let Some(o) = self.open.borrow_mut().remove(name) {
            *o.session.borrow_mut() = None; // detach
            self.stack.remove(&o.terminal);
        }
        if self.current.borrow().as_deref() == Some(name) {
            let next = self.open.borrow().keys().next().cloned();
            match next {
                Some(n) => self.show(&n),
                None => self.show_empty(),
            }
        }
        self.schedule_refresh();
    }

    /// Rebuild the sidebar from the live fleet, but only when it actually changed
    /// (so the 2s tick is usually a no-op and never steals the selection).
    fn refresh(&self) {
        let mut sessions = session::list().unwrap_or_default();
        let current = self.current.borrow().clone();
        // Order by group (this window, then elsewhere, then detached). The sort is
        // stable and `list` already returns name-sorted, so rows stay alphabetical
        // within each group. `groups` is aligned to `sessions` for the headers.
        let groups: Vec<Group> = {
            let open = self.open.borrow();
            sessions.sort_by_key(|s| group_of(open.contains_key(&s.name), s).order());
            sessions
                .iter()
                .map(|s| group_of(open.contains_key(&s.name), s))
                .collect()
        };
        let sig: Vec<RowSig> = sessions
            .iter()
            .zip(&groups)
            .map(|(s, &g)| {
                (
                    s.name.clone(),
                    display_title(s),
                    g,
                    current.as_deref() == Some(s.name.as_str()),
                )
            })
            .collect();
        if *self.last_sig.borrow() == sig {
            return;
        }
        *self.last_sig.borrow_mut() = sig;

        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let mut current_row = None;
        for s in &sessions {
            let row = self.make_row(s);
            if current.as_deref() == Some(s.name.as_str()) {
                current_row = Some(row.clone());
            }
            self.list.append(&row);
        }
        install_group_headers(&self.list, groups);
        if let Some(row) = current_row {
            self.list.select_row(Some(&row));
        }
        if self.open.borrow().is_empty() {
            self.show_empty();
        }
    }

    /// Build one sidebar row: title (terminal title) over `name · created`, a dot
    /// marking the session shown in this window, and a kill button. The group it
    /// sits under (see [`install_group_headers`]) says whether it is open here,
    /// elsewhere, or detached.
    fn make_row(&self, s: &SessionInfo) -> adw::ActionRow {
        let row = adw::ActionRow::builder()
            .title(display_title(s))
            .subtitle(format!("{} · {}", s.name, relative_time(s.created_at)))
            .activatable(true)
            .build();
        // Terminal titles are arbitrary text, not Pango markup.
        row.set_use_markup(false);

        if self.current.borrow().as_deref() == Some(s.name.as_str()) {
            let dot = gtk4::Image::from_icon_name("media-record-symbolic");
            dot.add_css_class("accent");
            row.add_prefix(&dot);
        }

        let kill = gtk4::Button::from_icon_name("user-trash-symbolic");
        kill.add_css_class("flat");
        kill.set_valign(gtk4::Align::Center);
        kill.set_tooltip_text(Some("Kill session"));
        {
            let ui = self.clone();
            let name = s.name.clone();
            kill.connect_clicked(move |_| ui.confirm_kill(&name));
        }
        row.add_suffix(&kill);

        {
            let ui = self.clone();
            let name = s.name.clone();
            row.connect_activated(move |_| ui.open_session(&name));
        }
        // While a row is focused (e.g. after F9 + arrows): F2 renames, Delete
        // removes. Other keys proceed so list navigation and Enter still work.
        {
            let ui = self.clone();
            let name = s.name.clone();
            let keys = EventControllerKey::new();
            keys.connect_key_pressed(move |_, keyval, _, _| match keyval {
                gdk::Key::F2 => {
                    ui.show_rename(&name);
                    glib::Propagation::Stop
                }
                gdk::Key::Delete => {
                    ui.confirm_kill(&name);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            });
            row.add_controller(keys);
        }
        row
    }

    /// Pump session output into the widget and propagate resizes, on a short GLib
    /// timer (glib 0.22 has no unix-fd source). Stops itself when the session is
    /// gone (closed/detached) or ends, removing it from the view on end.
    fn drive(&self, name: &str, terminal: &Terminal, session: &Rc<RefCell<Option<Session>>>) {
        // (0,0) so the first *allocated* size is always sent — including as the
        // attach handshake (see below), since we attach deferred.
        let last_size = Rc::new(Cell::new((0i64, 0i64)));
        let ui = self.clone();
        let name = name.to_string();
        let terminal = terminal.clone();
        let session = session.clone();

        glib::timeout_add_local(Duration::from_millis(8), move || {
            let size = (terminal.column_count(), terminal.row_count());
            {
                let mut guard = session.borrow_mut();
                let Some(s) = guard.as_mut() else {
                    return glib::ControlFlow::Break; // closed: stop
                };
                // Only once the widget has a real allocation are the column/row
                // counts the true grid; before that VTE reports its construction
                // default (80x24). Since we attach deferred, the first resize sent
                // here is the handshake — gating on a real allocation makes the
                // host lay its repaint out at the real window size, not a guess.
                if terminal.width() > 0 && terminal.height() > 0 && size != last_size.get() {
                    last_size.set(size);
                    let _ = s.resize(size.0 as u16, size.1 as u16);
                }
            }
            // Drain available output, bounded so a flood can't starve the UI. The
            // borrow is released before `feed`, which can synchronously emit
            // `commit` and re-enter the borrow.
            for _ in 0..64 {
                let pumped = {
                    let mut guard = session.borrow_mut();
                    let Some(s) = guard.as_mut() else {
                        return glib::ControlFlow::Break;
                    };
                    s.pump()
                };
                match pumped {
                    Ok(p) => {
                        if !p.output.is_empty() {
                            terminal.feed(&p.output);
                        }
                        if p.ended {
                            ui.close_session(&name);
                            return glib::ControlFlow::Break;
                        }
                        if p.output.is_empty() {
                            break;
                        }
                    }
                    Err(e) => {
                        eprintln!("ghost-gtk: session '{name}' read error: {e}");
                        ui.close_session(&name);
                        return glib::ControlFlow::Break;
                    }
                }
            }
            glib::ControlFlow::Continue
        });
    }

    /// Rebuild the sidebar after the current signal handler returns (rebuilding
    /// it mid-activation would remove the very row/button being handled).
    fn schedule_refresh(&self) {
        let ui = self.clone();
        glib::idle_add_local_once(move || ui.refresh());
    }

    /// Attached (open-here) session names in fleet-list order, so cycling follows
    /// the order shown in the sidebar.
    fn attached_in_list_order(&self) -> Vec<String> {
        let open = self.open.borrow();
        session::list()
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.name)
            .filter(|n| open.contains_key(n))
            .collect()
    }

    /// Switch to the next (`forward`) or previous attached terminal, wrapping.
    fn switch_terminal(&self, forward: bool) {
        let names = self.attached_in_list_order();
        let current = self.current.borrow().clone();
        if let Some(target) = cycle_target(&names, current.as_deref(), forward) {
            self.show(&target);
            self.schedule_refresh();
        }
    }

    /// Step the persisted zoom (font-scale) and re-apply it everywhere.
    fn zoom(&self, step: fn(f64) -> f64) {
        {
            let mut s = self.settings.borrow_mut();
            s.zoom.scale = step(s.zoom.scale);
        }
        self.apply_settings_to_all();
        self.save_settings();
    }

    /// Reset zoom to 1.0.
    fn zoom_reset(&self) {
        self.settings.borrow_mut().zoom.scale = 1.0;
        self.apply_settings_to_all();
        self.save_settings();
    }

    /// Re-apply the current settings to every open terminal and the window.
    fn apply_settings_to_all(&self) {
        let s = self.settings.borrow();
        for open in self.open.borrow().values() {
            settings::apply(&s, &open.terminal);
        }
        update_window_chrome(&self.window, s.window.transparency);
    }

    /// Persist settings; a write failure is logged, never fatal.
    fn save_settings(&self) {
        if let Err(e) = self.settings.borrow().save() {
            eprintln!("ghost-gtk: could not save settings: {e}");
        }
    }

    /// The Preferences dialog. Every row live-applies to all open terminals and
    /// saves; column/row changes only affect the next launch. Built from
    /// `adw::PreferencesGroup`s in a plain box (not an `AdwPreferencesPage`) so
    /// the dialog sizes to its content instead of scrolling.
    fn show_preferences(&self) {
        // A non-modal window (not adw::Dialog): a dialog dims the parent, which
        // ruins the live preview of transparency and color scheme. Reuse the
        // window if it's already open rather than stacking duplicates.
        if let Some(existing) = self.prefs_window.borrow().clone() {
            existing.present();
            return;
        }
        let header = adw::HeaderBar::new();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        let content = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(18)
            .margin_top(12)
            .margin_bottom(18)
            .margin_start(18)
            .margin_end(18)
            .build();

        // --- Appearance: color scheme + transparency ---
        let appearance = adw::PreferencesGroup::builder().title("Appearance").build();

        let names: Vec<&str> = settings::SCHEMES.iter().map(|s| s.name).collect();
        let scheme_row = adw::ComboRow::builder().title("Color scheme").build();
        scheme_row.set_model(Some(&gtk4::StringList::new(&names)));
        let current = self.settings.borrow().colors.scheme.clone();
        let cur_idx = settings::SCHEMES
            .iter()
            .position(|s| s.id == current)
            .unwrap_or(0) as u32;
        scheme_row.set_selected(cur_idx);
        {
            let ui = self.clone();
            scheme_row.connect_selected_notify(move |row| {
                if let Some(s) = settings::SCHEMES.get(row.selected() as usize) {
                    ui.settings.borrow_mut().colors.scheme = s.id.to_string();
                    ui.apply_settings_to_all();
                    ui.save_settings();
                }
            });
        }
        appearance.add(&scheme_row);

        let trans_init =
            settings::transparency_to_slider(self.settings.borrow().window.transparency) * 100.0;
        let trans_scale = gtk4::Scale::with_range(gtk4::Orientation::Horizontal, 0.0, 100.0, 1.0);
        trans_scale.set_hexpand(true);
        trans_scale.set_draw_value(true);
        trans_scale.set_value_pos(gtk4::PositionType::Right);
        trans_scale.set_width_request(200);
        trans_scale.set_value(trans_init);
        let trans_row = adw::ActionRow::builder().title("Transparency").build();
        trans_row.add_suffix(&trans_scale);
        {
            let ui = self.clone();
            trans_scale.connect_value_changed(move |scale| {
                ui.settings.borrow_mut().window.transparency =
                    settings::slider_to_transparency(scale.value() / 100.0);
                ui.apply_settings_to_all();
                ui.save_settings();
            });
        }
        appearance.add(&trans_row);
        content.append(&appearance);

        // --- Font ---
        let font_group = adw::PreferencesGroup::builder().title("Font").build();
        let font_row = adw::ActionRow::builder().title("Family and size").build();
        let font_btn = gtk4::FontDialogButton::new(Some(gtk4::FontDialog::new()));
        font_btn.set_valign(gtk4::Align::Center);
        {
            let s = self.settings.borrow();
            let mut desc = pango::FontDescription::new();
            if !s.font.family.is_empty() {
                desc.set_family(&s.font.family);
            }
            desc.set_size((s.font.size * pango::SCALE as f64).round() as i32);
            font_btn.set_font_desc(&desc);
        }
        {
            let ui = self.clone();
            font_btn.connect_font_desc_notify(move |btn| {
                let Some(desc) = btn.font_desc() else { return };
                {
                    let mut s = ui.settings.borrow_mut();
                    if let Some(family) = desc.family() {
                        s.font.family = family.to_string();
                    }
                    if desc.size() > 0 {
                        s.font.size = desc.size() as f64 / pango::SCALE as f64;
                    }
                }
                ui.apply_settings_to_all();
                ui.save_settings();
            });
        }
        font_row.add_suffix(&font_btn);
        font_row.set_activatable_widget(Some(&font_btn));
        font_group.add(&font_row);
        content.append(&font_group);

        // --- Window: default grid size (next launch) ---
        let win_group = adw::PreferencesGroup::builder()
            .title("Window")
            .description("Default size for new windows (applied on next launch)")
            .build();
        let cols_init = f64::from(self.settings.borrow().window.columns);
        let cols_adj = gtk4::Adjustment::new(cols_init, 20.0, 500.0, 1.0, 10.0, 0.0);
        let cols_row = adw::SpinRow::builder()
            .title("Columns")
            .adjustment(&cols_adj)
            .build();
        {
            let ui = self.clone();
            cols_row.connect_value_notify(move |row| {
                ui.settings.borrow_mut().window.columns = row.value() as u16;
                ui.save_settings();
            });
        }
        let rows_init = f64::from(self.settings.borrow().window.rows);
        let rows_adj = gtk4::Adjustment::new(rows_init, 5.0, 300.0, 1.0, 10.0, 0.0);
        let rows_row = adw::SpinRow::builder()
            .title("Rows")
            .adjustment(&rows_adj)
            .build();
        {
            let ui = self.clone();
            rows_row.connect_value_notify(move |row| {
                ui.settings.borrow_mut().window.rows = row.value() as u16;
                ui.save_settings();
            });
        }
        win_group.add(&cols_row);
        win_group.add(&rows_row);
        content.append(&win_group);

        toolbar.set_content(Some(&content));

        let window = adw::Window::new();
        window.set_title(Some("Preferences"));
        window.set_transient_for(Some(&self.window));
        window.set_modal(false);
        window.set_default_size(440, -1);
        window.set_content(Some(&toolbar));
        self.prefs_window.replace(Some(window.clone()));
        {
            let ui = self.clone();
            window.connect_close_request(move |_| {
                ui.prefs_window.replace(None);
                glib::Propagation::Proceed
            });
        }
        window.present();
    }
}

/// What to show as a session's primary label: its terminal title, else the
/// command it runs, else "shell".
fn display_title(s: &SessionInfo) -> String {
    if !s.title.is_empty() {
        s.title.clone()
    } else if !s.command.is_empty() {
        s.command.join(" ")
    } else {
        "shell".to_string()
    }
}

/// A VTE window title if non-empty, else a fallback.
fn title_or(title: Option<glib::GString>, fallback: &str) -> String {
    title
        .map(|t| t.to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

/// A compact "time since creation" label.
fn relative_time(created_at: Option<i64>) -> String {
    let Some(ts) = created_at else {
        return "unknown".to_string();
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(ts);
    let secs = (now - ts).max(0);
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// Give the list libadwaita-style section headers, derived from each row's group.
/// Called after every rebuild with `groups` aligned to the rows' order, so each
/// header sits above the first row of its contiguous group.
fn install_group_headers(list: &gtk4::ListBox, groups: Vec<Group>) {
    list.set_header_func(move |row, _before| {
        let idx = row.index();
        if idx < 0 {
            return;
        }
        let idx = idx as usize;
        let Some(&group) = groups.get(idx) else {
            return;
        };
        let starts_group = idx == 0 || groups.get(idx - 1) != Some(&group);
        if !starts_group {
            row.set_header(None::<&gtk4::Widget>);
            return;
        }
        let label = gtk4::Label::builder()
            .label(group.header())
            .xalign(0.0)
            .margin_top(12)
            .margin_bottom(4)
            .margin_start(12)
            .margin_end(12)
            .build();
        label.add_css_class("dim-label");
        label.add_css_class("caption-heading");
        row.set_header(Some(&label));
    });
}

/// Install window actions (preferences, zoom, new/close, tab switching) with
/// their accelerators. GTK's `<Primary>` is Control on both Linux *and* macOS, so
/// the user-facing command shortcuts go through [`primary_accel`] to get Command
/// on macOS; the rest keep `<Primary>` (still Control everywhere).
fn install_actions(ui: &Ui, app: &adw::Application) {
    type Handler = Box<dyn Fn(&Ui)>;
    let actions: [(&str, Handler); 9] = [
        ("preferences", Box::new(Ui::show_preferences)),
        ("new-session", Box::new(Ui::new_session)),
        ("close-window", Box::new(|ui| ui.window.close())),
        ("zoom-in", Box::new(|ui| ui.zoom(settings::zoom_in))),
        ("zoom-out", Box::new(|ui| ui.zoom(settings::zoom_out))),
        ("zoom-reset", Box::new(Ui::zoom_reset)),
        ("toggle-sidebar", Box::new(Ui::toggle_sidebar)),
        ("next-tab", Box::new(|ui| ui.switch_terminal(true))),
        ("previous-tab", Box::new(|ui| ui.switch_terminal(false))),
    ];
    for (name, handler) in actions {
        let action = gio::SimpleAction::new(name, None);
        let handler_ui = ui.clone();
        action.connect_activate(move |_, _| handler(&handler_ui));
        ui.window.add_action(&action);
    }

    app.set_accels_for_action("win.preferences", &["<Primary>comma"]);
    app.set_accels_for_action(
        "win.zoom-in",
        &["<Primary>plus", "<Primary>equal", "<Primary>KP_Add"],
    );
    app.set_accels_for_action("win.zoom-out", &["<Primary>minus", "<Primary>KP_Subtract"]);
    app.set_accels_for_action("win.zoom-reset", &["<Primary>0"]);
    app.set_accels_for_action("win.toggle-sidebar", &["F9"]);
    // New session — Cmd+T on macOS, Ctrl+T on Linux (the conventional new-tab key).
    app.set_accels_for_action("win.new-session", &[&primary_accel("t", false)]);
    // Close the window — Cmd+Shift+W on macOS, Ctrl+Shift+W on Linux.
    app.set_accels_for_action("win.close-window", &[&primary_accel("w", true)]);
    // Cycle attached terminals. Accelerators (not a key controller) get first
    // shot in the global capture phase, ahead of GTK's Tab focus-traversal.
    // Literal Control, not Primary — Cmd+Tab is the macOS app switcher. Shift+Tab
    // arrives as the ISO_Left_Tab keysym, so bind both forms; Ctrl+Page_Up/Down is
    // the conventional terminal binding (and a fallback where Ctrl+Tab is grabbed
    // by the compositor).
    app.set_accels_for_action("win.next-tab", &["<Control>Tab", "<Control>Page_Down"]);
    app.set_accels_for_action(
        "win.previous-tab",
        &[
            "<Control><Shift>Tab",
            "<Control>ISO_Left_Tab",
            "<Control>Page_Up",
        ],
    );
}

/// Rough pixels per character cell for a point size (≈96 dpi, typical monospace
/// aspect and line height). Only seeds the initial window size; not exact.
fn estimate_cell(size_pt: f64) -> (i32, i32) {
    let h = (size_pt * (96.0 / 72.0) * 1.2).round().max(1.0) as i32;
    let w = (h as f64 * 0.55).round().max(1.0) as i32;
    (w, h)
}

thread_local! {
    /// One shared provider whose CSS we rewrite to drop the window's background
    /// (and keep the empty page opaque), gated by the `ghost-transparent` class.
    /// Kept here (not a local) so [`update_window_chrome`] can rewrite it.
    static TRANSPARENCY_CSS: gtk4::CssProvider = gtk4::CssProvider::new();
}

/// Register the transparency CSS provider on the display once.
fn install_css() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Some(display) = gdk::Display::default() else {
            return;
        };
        TRANSPARENCY_CSS.with(|provider| {
            gtk4::style_context_add_provider_for_display(
                &display,
                provider,
                gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        });
    });
}

/// Rewrite the transparency CSS for the current `transparency`. When it's > 0 the
/// window's own background is dropped so a terminal's background alpha (see
/// [`settings::apply`]) reveals the desktop behind it. Only the terminal area goes
/// see-through: the header bars stay solid via their raised toolbar style (set in
/// [`build_window`], libadwaita-managed in both focus states) and the empty-state
/// page keeps its own opaque background.
fn update_window_chrome(window: &adw::ApplicationWindow, transparency: f64) {
    const TRANSPARENT_CSS: &str = "\
        window.ghost-transparent { background-color: transparent; }\n\
        window.ghost-transparent .ghost-empty,\n\
        window.ghost-transparent .ghost-empty:backdrop { background-color: @window_bg_color; }\n";
    TRANSPARENCY_CSS.with(|provider| {
        if transparency > 0.0 {
            provider.load_from_string(TRANSPARENT_CSS);
            window.add_css_class("ghost-transparent");
        } else {
            provider.load_from_string("");
            window.remove_css_class("ghost-transparent");
        }
    });
}

/// The session to switch to when cycling the attached terminals: the next entry
/// in `names` when `forward`, else the previous, wrapping around either end.
/// `current` is the visible session. Returns `None` when there's nothing to
/// switch to (fewer than two attached). An unknown/missing `current` starts from
/// the first entry.
fn cycle_target(names: &[String], current: Option<&str>, forward: bool) -> Option<String> {
    if names.len() < 2 {
        return None;
    }
    let idx = current
        .and_then(|c| names.iter().position(|n| n == c))
        .unwrap_or(0);
    let next = if forward {
        (idx + 1) % names.len()
    } else {
        (idx + names.len() - 1) % names.len()
    };
    Some(names[next].clone())
}

/// Whether a button press on the terminal should start a window move rather than
/// reach VTE. macOS moves a window from anywhere on it with Control-Command-drag;
/// VTE would instead read Control-drag as block (rectangular) selection and
/// swallow the gesture. We reserve the press for a move only when both Control
/// and Command are held on the primary button — plain Control-drag stays VTE's
/// block selection. The Command (⌘) key maps to `META_MASK` on the GDK macOS
/// backend; on other backends that bit is effectively never set, so this is inert
/// off macOS.
fn is_window_move_drag(button: u32, mods: gdk::ModifierType) -> bool {
    button == gdk::BUTTON_PRIMARY
        && mods.contains(gdk::ModifierType::CONTROL_MASK)
        && mods.contains(gdk::ModifierType::META_MASK)
}

/// Make Control-Command-drag move the window from anywhere on the terminal,
/// matching how macOS drags windows. VTE claims Control-drag for block selection,
/// so intercept the press in the capture phase (ahead of VTE): claim the sequence
/// to deny VTE, then start a real toplevel move. Mirrors GtkWindowHandle's
/// drag-to-move, including the widget→surface coordinate translation `begin_move`
/// expects.
fn install_window_move_drag(term: &Terminal) {
    let click = gtk4::GestureClick::new();
    click.set_button(gdk::BUTTON_PRIMARY);
    click.set_propagation_phase(PropagationPhase::Capture);
    let term_move = term.clone();
    click.connect_pressed(move |gesture, _n_press, x, y| {
        if !is_window_move_drag(gesture.current_button(), gesture.current_event_state()) {
            return;
        }
        // Take the sequence so VTE never starts a selection from this press.
        gesture.set_state(gtk4::EventSequenceState::Claimed);
        let Some(native) = term_move.native() else {
            return;
        };
        let Some(surface) = native.surface() else {
            return;
        };
        let Ok(toplevel) = surface.downcast::<gdk::Toplevel>() else {
            return;
        };
        let Some(device) = gesture.device() else {
            return;
        };
        // begin_move wants surface-relative coordinates: translate the widget-local
        // press point into the native, then offset by the native's surface transform.
        let Some(p) =
            term_move.compute_point(&native, &gtk4::graphene::Point::new(x as f32, y as f32))
        else {
            return;
        };
        let (tx, ty) = native.surface_transform();
        toplevel.begin_move(
            &device,
            gesture.current_button() as i32,
            f64::from(p.x()) + tx,
            f64::from(p.y()) + ty,
            gesture.current_event_time(),
        );
    });
    term.add_controller(click);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardAction {
    Copy,
    Paste,
}

/// The clipboard action a key chord triggers, or `None` if it isn't one. The
/// shortcuts VTE doesn't bind itself: Ctrl-Shift-C/V everywhere, plus the
/// platform's primary clipboard modifier — Command on macOS (⌘ → `META_MASK` on
/// the GDK macOS backend, like the window-move gesture) and Alt elsewhere. On
/// macOS Option is *Meta* (see [`meta_input_bytes`]), not a clipboard key.
fn clipboard_action(keyval: gdk::Key, state: gdk::ModifierType) -> Option<ClipboardAction> {
    let ctrl_shift = state.contains(gdk::ModifierType::CONTROL_MASK)
        && state.contains(gdk::ModifierType::SHIFT_MASK);
    let primary = if cfg!(target_os = "macos") {
        state.contains(gdk::ModifierType::META_MASK)
    } else {
        state.contains(gdk::ModifierType::ALT_MASK)
    };
    if !(ctrl_shift || primary) {
        return None;
    }
    if keyval == gdk::Key::c || keyval == gdk::Key::C {
        return Some(ClipboardAction::Copy);
    }
    if keyval == gdk::Key::v || keyval == gdk::Key::V {
        return Some(ClipboardAction::Paste);
    }
    None
}

/// The bytes an Option/Alt-modified key sends to the session when we treat Option
/// as Meta, or `None` to let the key reach VTE untouched.
///
/// macOS' GDK backend delivers Option as a bare Alt modifier without the
/// ESC-prefix a terminal needs, so we synthesize it. The nav/edit keys get their
/// conventional sequences on every platform — `Option+←/→` word-motion (`M-b` /
/// `M-f`) and `Option+Backspace/Delete` word-kill (`M-BS` ESC DEL / `M-d`; on the
/// Mac "delete" is Backspace and forward-delete arrives as `Delete`). On macOS
/// every other `Option+<printable>` also becomes `ESC <char>`, so the whole
/// readline Meta family (`M-.`, `M-u`, `M-y`, …) works from one rule instead of a
/// per-binding list; this is what "Use Option as Meta key" does in Terminal.app,
/// at the cost of typing Option special characters. Elsewhere VTE already
/// ESC-prefixes Alt itself, so only the nav keys are remapped.
fn meta_input_bytes(keyval: gdk::Key, state: gdk::ModifierType) -> Option<Vec<u8>> {
    if !state.contains(gdk::ModifierType::ALT_MASK) {
        return None;
    }
    let nav: Option<&[u8]> = if keyval == gdk::Key::Left {
        Some(b"\x1bb")
    } else if keyval == gdk::Key::Right {
        Some(b"\x1bf")
    } else if keyval == gdk::Key::BackSpace {
        Some(b"\x1b\x7f")
    } else if keyval == gdk::Key::Delete {
        Some(b"\x1bd")
    } else {
        None
    };
    if let Some(seq) = nav {
        return Some(seq.to_vec());
    }
    // Generic Option-as-Meta for printables — macOS only (elsewhere VTE does it),
    // and never when Control is also held (that's a distinct chord for VTE).
    if cfg!(target_os = "macos")
        && !state.contains(gdk::ModifierType::CONTROL_MASK)
        && let Some(ch) = keyval.to_unicode()
        && !ch.is_control()
    {
        let mut buf = vec![0x1b];
        let mut tmp = [0u8; 4];
        buf.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
        return Some(buf);
    }
    None
}

/// Copy/paste shortcuts VTE doesn't bind itself (see [`clipboard_action`]).
/// Capture phase + Stop so Alt+v is a paste, not a Meta-v keystroke sent to the
/// session.
fn install_clipboard_keys(term: &Terminal) {
    let keys = EventControllerKey::new();
    keys.set_propagation_phase(PropagationPhase::Capture);
    let term_keys = term.clone();
    keys.connect_key_pressed(move |_ctrl, keyval, _code, state| {
        match clipboard_action(keyval, state) {
            Some(ClipboardAction::Copy) => term_keys.copy_clipboard_format(Format::Text),
            Some(ClipboardAction::Paste) => term_keys.paste_clipboard(),
            None => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    term.add_controller(keys);
}

/// Option-as-Meta key handling (see [`meta_input_bytes`]). The terminal is
/// feed-only, so we send the escape sequence to the session ourselves; capture
/// phase + Stop keeps VTE from turning Option into a composed character or a bare
/// Meta keystroke.
fn install_meta_keys(term: &Terminal, session: &Rc<RefCell<Option<Session>>>) {
    let keys = EventControllerKey::new();
    keys.set_propagation_phase(PropagationPhase::Capture);
    let session = session.clone();
    keys.connect_key_pressed(move |_ctrl, keyval, _code, state| {
        let Some(bytes) = meta_input_bytes(keyval, state) else {
            return glib::Propagation::Proceed;
        };
        if let Some(s) = session.borrow_mut().as_mut() {
            let _ = s.send_input(&bytes);
        }
        glib::Propagation::Stop
    });
    term.add_controller(keys);
}

/// macOS Dock-icon menu integration.
///
/// GTK4 dropped the old `gtk-mac-integration` Dock-menu API and exposes no
/// replacement — macOS only offers a custom Dock menu through the
/// `applicationDockMenu:` method on the `NSApplication` delegate, which GTK owns.
/// So we inject that method (and the action it invokes) into GTK's delegate class
/// at runtime, leaving the rest of the delegate untouched. The single "New
/// Window" item activates the existing `app.new-window` GAction, so it shares the
/// exact code path as the menu's New Window and ⌘N.
#[cfg(target_os = "macos")]
mod dock {
    use std::ffi::CStr;
    use std::sync::atomic::{AtomicBool, Ordering};

    use gtk4::gio;
    use gtk4::gio::prelude::*;
    use objc2::ffi::class_addMethod;
    use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
    use objc2::{class, msg_send, sel};

    /// Returns the Dock menu AppKit asks for: one "New Window" item targeting the
    /// delegate's injected `ghostNewWindow:`. Returned autoreleased, per the
    /// `applicationDockMenu:` contract.
    unsafe extern "C" fn application_dock_menu(
        this: *mut AnyObject,
        _cmd: Sel,
        _app: *mut AnyObject,
    ) -> *mut AnyObject {
        unsafe {
            let menu: *mut AnyObject = msg_send![class!(NSMenu), new];
            let item: *mut AnyObject = msg_send![class!(NSMenuItem), alloc];
            let item: *mut AnyObject = msg_send![
                item,
                initWithTitle: ns_string("New Window"),
                action: sel!(ghostNewWindow:),
                keyEquivalent: ns_string(""),
            ];
            let _: () = msg_send![item, setTarget: this];
            let _: () = msg_send![menu, addItem: item];
            let _: () = msg_send![item, release];
            // `autorelease` returns the receiver; objc2 verifies the encoding, so
            // it must be typed as an object, not `()`.
            let menu: *mut AnyObject = msg_send![menu, autorelease];
            menu
        }
    }

    /// The Dock item's action: activate `app.new-window` on the running
    /// GApplication, exactly as the in-app menu and ⌘N do.
    unsafe extern "C" fn ghost_new_window(
        _this: *mut AnyObject,
        _cmd: Sel,
        _sender: *mut AnyObject,
    ) {
        if let Some(app) = gio::Application::default() {
            app.activate_action("new-window", None);
        }
    }

    /// An autoreleased `NSString` from `s`.
    unsafe fn ns_string(s: &str) -> *mut AnyObject {
        let c = std::ffi::CString::new(s).unwrap_or_default();
        unsafe { msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()] }
    }

    unsafe fn add_method(cls: *mut AnyClass, sel: Sel, imp: Imp, types: &CStr) {
        // Fails only when the method already exists (e.g. we ran twice) — fine.
        unsafe {
            let _ = class_addMethod(cls, sel, imp, types.as_ptr());
        }
    }

    /// Inject the Dock-menu method into GTK's `NSApp` delegate. Retries on each
    /// call until the delegate exists, then no-ops.
    pub fn install() {
        static DONE: AtomicBool = AtomicBool::new(false);
        if DONE.load(Ordering::Relaxed) {
            return;
        }
        unsafe {
            let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            if app.is_null() {
                return;
            }
            let delegate: *mut AnyObject = msg_send![app, delegate];
            if delegate.is_null() {
                return;
            }
            let cls: *mut AnyClass = msg_send![delegate, class];
            add_method(
                cls,
                sel!(applicationDockMenu:),
                std::mem::transmute::<
                    unsafe extern "C" fn(*mut AnyObject, Sel, *mut AnyObject) -> *mut AnyObject,
                    Imp,
                >(application_dock_menu),
                c"@@:@",
            );
            add_method(
                cls,
                sel!(ghostNewWindow:),
                std::mem::transmute::<unsafe extern "C" fn(*mut AnyObject, Sel, *mut AnyObject), Imp>(
                    ghost_new_window,
                ),
                c"v@:@",
            );
            DONE.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ClipboardAction, Group, clipboard_action, cycle_target, group_of, is_window_move_drag,
        meta_input_bytes, next_session_name,
    };
    use ghost_vt::session::SessionInfo;
    use gtk4::gdk;
    use gtk4::gdk::ModifierType as M;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn info(name: &str, attached: bool) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            pid: 1,
            created_at: None,
            title: String::new(),
            command: vec![],
            attached,
        }
    }

    #[test]
    fn primary_accel_is_command_on_macos_not_control() {
        use super::primary_accel;
        if cfg!(target_os = "macos") {
            // macOS: Command (⌘), which GTK names <Meta> — not <Primary>/<Control>.
            assert_eq!(primary_accel("n", false), "<Meta>n");
            assert_eq!(primary_accel("t", false), "<Meta>t");
            assert_eq!(primary_accel("w", true), "<Meta><Shift>w");
        } else {
            assert_eq!(primary_accel("n", false), "<Primary>n");
            assert_eq!(primary_accel("w", true), "<Primary><Shift>w");
        }
        // The whole point: never Control for these on macOS.
        assert!(!primary_accel("n", false).contains("<Control>"));
    }

    #[test]
    fn clipboard_shortcuts_match_platform_modifier() {
        // Ctrl-Shift-C/V copy & paste on every platform.
        assert_eq!(
            clipboard_action(gdk::Key::C, M::CONTROL_MASK | M::SHIFT_MASK),
            Some(ClipboardAction::Copy)
        );
        assert_eq!(
            clipboard_action(gdk::Key::v, M::CONTROL_MASK | M::SHIFT_MASK),
            Some(ClipboardAction::Paste)
        );
        // Bare Ctrl+C is not a copy — it must reach the session as ^C.
        assert_eq!(clipboard_action(gdk::Key::c, M::CONTROL_MASK), None);
        // Plain c with no modifier is just typing.
        assert_eq!(clipboard_action(gdk::Key::c, M::empty()), None);

        // The primary clipboard modifier is Command on macOS (Option is Meta
        // there, not a clipboard key) and Alt elsewhere.
        let cmd_c = clipboard_action(gdk::Key::c, M::META_MASK);
        let alt_c = clipboard_action(gdk::Key::c, M::ALT_MASK);
        if cfg!(target_os = "macos") {
            assert_eq!(cmd_c, Some(ClipboardAction::Copy));
            assert_eq!(alt_c, None);
        } else {
            assert_eq!(alt_c, Some(ClipboardAction::Copy));
            assert_eq!(cmd_c, None);
        }
    }

    #[test]
    fn option_nav_keys_send_readline_word_sequences() {
        // Word motion (M-b/M-f) and word kill (M-BS/M-d) — on every platform.
        assert_eq!(
            meta_input_bytes(gdk::Key::Left, M::ALT_MASK).as_deref(),
            Some(b"\x1bb".as_slice())
        );
        assert_eq!(
            meta_input_bytes(gdk::Key::Right, M::ALT_MASK).as_deref(),
            Some(b"\x1bf".as_slice())
        );
        assert_eq!(
            meta_input_bytes(gdk::Key::BackSpace, M::ALT_MASK).as_deref(),
            Some(b"\x1b\x7f".as_slice())
        );
        assert_eq!(
            meta_input_bytes(gdk::Key::Delete, M::ALT_MASK).as_deref(),
            Some(b"\x1bd".as_slice())
        );
        // Without Option/Alt they're ordinary keys — let VTE handle them.
        assert_eq!(meta_input_bytes(gdk::Key::Left, M::empty()), None);
        assert_eq!(meta_input_bytes(gdk::Key::BackSpace, M::empty()), None);
    }

    #[test]
    fn option_letter_is_meta_on_macos_only() {
        // The whole readline Meta family flows from one rule: Option+b -> ESC b
        // (M-b). On macOS only — elsewhere VTE ESC-prefixes Alt itself.
        let got = meta_input_bytes(gdk::Key::b, M::ALT_MASK);
        if cfg!(target_os = "macos") {
            assert_eq!(got.as_deref(), Some(b"\x1bb".as_slice()));
        } else {
            assert_eq!(got, None);
        }
        // Plain b is always just typing.
        assert_eq!(meta_input_bytes(gdk::Key::b, M::empty()), None);
    }

    #[test]
    fn gui_launch_falls_back_to_home_only_without_a_real_cwd() {
        use super::home_launch_dir;
        use std::path::{Path, PathBuf};

        let home = Path::new("/Users/kov");
        // Bundled launch (launchd/Finder) starts us at `/`: fall back to home.
        assert_eq!(
            home_launch_dir(Some(Path::new("/")), Some(home)),
            Some(PathBuf::from("/Users/kov"))
        );
        // No cwd at all: also fall back to home.
        assert_eq!(home_launch_dir(None, Some(home)), Some(PathBuf::from(home)));
        // A real working directory (e.g. launched from a terminal) is kept as-is.
        assert_eq!(
            home_launch_dir(Some(Path::new("/Users/kov/Projects/ghost")), Some(home)),
            None
        );
        // Nothing to fall back to: leave cwd untouched rather than guess.
        assert_eq!(home_launch_dir(Some(Path::new("/")), None), None);
    }

    #[test]
    fn control_command_primary_drag_is_a_window_move() {
        // The macOS gesture: primary button with both Control and Command held
        // (⌘ → META on the GDK macOS backend) moves the window.
        assert!(is_window_move_drag(
            gdk::BUTTON_PRIMARY,
            M::CONTROL_MASK | M::META_MASK
        ));
        // Extra modifiers (e.g. Shift) along for the ride still count.
        assert!(is_window_move_drag(
            gdk::BUTTON_PRIMARY,
            M::CONTROL_MASK | M::META_MASK | M::SHIFT_MASK
        ));
    }

    #[test]
    fn other_drags_are_left_to_vte() {
        // Plain Control-drag stays VTE's block (rectangular) selection.
        assert!(!is_window_move_drag(gdk::BUTTON_PRIMARY, M::CONTROL_MASK));
        // Command alone is not the move gesture.
        assert!(!is_window_move_drag(gdk::BUTTON_PRIMARY, M::META_MASK));
        // Neither is a non-primary button, even with the right modifiers.
        assert!(!is_window_move_drag(
            gdk::BUTTON_SECONDARY,
            M::CONTROL_MASK | M::META_MASK
        ));
    }

    #[test]
    fn group_of_separates_here_elsewhere_and_detached() {
        // Open in this window wins regardless of the host marker (we are the
        // attached client).
        assert_eq!(group_of(true, &info("s", false)), Group::Here);
        assert_eq!(group_of(true, &info("s", true)), Group::Here);
        // Not open here: the marker tells "elsewhere" from "detached".
        assert_eq!(group_of(false, &info("s", true)), Group::Elsewhere);
        assert_eq!(group_of(false, &info("s", false)), Group::Detached);
    }

    #[test]
    fn group_order_is_here_then_detached_then_elsewhere() {
        // Detached (free to open here) ranks above sessions held elsewhere.
        assert!(Group::Here.order() < Group::Detached.order());
        assert!(Group::Detached.order() < Group::Elsewhere.order());
    }

    #[test]
    fn session_names_are_unique_per_call() {
        let a = next_session_name();
        let b = next_session_name();
        assert_ne!(a, b, "two windows must not mint the same session name");
        assert!(a.starts_with("ghost-"));
    }

    #[test]
    fn cycles_forward_wrapping_past_the_last() {
        let n = names(&["a", "b", "c"]);
        assert_eq!(cycle_target(&n, Some("a"), true).as_deref(), Some("b"));
        assert_eq!(cycle_target(&n, Some("b"), true).as_deref(), Some("c"));
        assert_eq!(cycle_target(&n, Some("c"), true).as_deref(), Some("a"));
    }

    #[test]
    fn cycles_backward_wrapping_before_the_first() {
        let n = names(&["a", "b", "c"]);
        assert_eq!(cycle_target(&n, Some("a"), false).as_deref(), Some("c"));
        assert_eq!(cycle_target(&n, Some("c"), false).as_deref(), Some("b"));
    }

    #[test]
    fn nothing_to_switch_to_below_two() {
        assert_eq!(cycle_target(&names(&[]), None, true), None);
        assert_eq!(cycle_target(&names(&["only"]), Some("only"), true), None);
    }

    #[test]
    fn unknown_or_missing_current_starts_from_the_first() {
        let n = names(&["a", "b"]);
        assert_eq!(cycle_target(&n, Some("gone"), true).as_deref(), Some("b"));
        assert_eq!(cycle_target(&n, None, true).as_deref(), Some("b"));
    }
}
