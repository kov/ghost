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

/// One sidebar row reduced to what's visible: `(name, title, open-here, current)`.
/// The list is rebuilt only when the vector of these changes.
type RowSig = (String, String, bool, bool);

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
    counter: Rc<Cell<u32>>,
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

fn main() -> glib::ExitCode {
    // If `server::spawn` re-exec'd us as a session host, become it and never
    // return here — before any GTK init.
    server::run_host_if_invoked();

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_window);
    app.run()
}

fn build_window(app: &adw::Application) {
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
        counter: Rc::new(Cell::new(0)),
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
            let session = Rc::new(RefCell::new(Some(session)));
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
        let n = self.counter.get();
        self.counter.set(n + 1);
        let name = format!("ghost-{}-{n}", std::process::id());
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
        let sessions = session::list().unwrap_or_default();
        let current = self.current.borrow().clone();
        let sig: Vec<RowSig> = {
            let open = self.open.borrow();
            sessions
                .iter()
                .map(|s| {
                    (
                        s.name.clone(),
                        display_title(s),
                        open.contains_key(&s.name),
                        current.as_deref() == Some(s.name.as_str()),
                    )
                })
                .collect()
        };
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
        if let Some(row) = current_row {
            self.list.select_row(Some(&row));
        }
        if self.open.borrow().is_empty() {
            self.show_empty();
        }
    }

    /// Build one sidebar row: title (terminal title) over `name · created`, an
    /// "open here" dot, and a kill button.
    fn make_row(&self, s: &SessionInfo) -> adw::ActionRow {
        let row = adw::ActionRow::builder()
            .title(display_title(s))
            .subtitle(format!("{} · {}", s.name, relative_time(s.created_at)))
            .activatable(true)
            .build();
        // Terminal titles are arbitrary text, not Pango markup.
        row.set_use_markup(false);

        if self.open.borrow().contains_key(&s.name) {
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

/// Install window actions (preferences, zoom in/out/reset) with their
/// accelerators. `<Primary>` resolves to Ctrl on Linux and Cmd on macOS, so one
/// binding covers both platforms.
fn install_actions(ui: &Ui, app: &adw::Application) {
    type Handler = Box<dyn Fn(&Ui)>;
    let actions: [(&str, Handler); 8] = [
        ("preferences", Box::new(Ui::show_preferences)),
        ("new-session", Box::new(Ui::new_session)),
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
    // New session — Ctrl+T on Linux, Cmd+T on macOS (the conventional new-tab key).
    app.set_accels_for_action("win.new-session", &["<Primary>t"]);
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

/// Copy/paste shortcuts VTE doesn't bind itself: Alt+C/V (consistent across mac
/// and linux) and Ctrl-Shift-C/V. Capture phase + Stop so Alt+v is a paste, not
/// a Meta-v keystroke sent to the session.
fn install_clipboard_keys(term: &Terminal) {
    let keys = EventControllerKey::new();
    keys.set_propagation_phase(PropagationPhase::Capture);
    let term_keys = term.clone();
    keys.connect_key_pressed(move |_ctrl, keyval, _code, state| {
        let alt = state.contains(gdk::ModifierType::ALT_MASK);
        let ctrl_shift = state.contains(gdk::ModifierType::CONTROL_MASK)
            && state.contains(gdk::ModifierType::SHIFT_MASK);
        if !(alt || ctrl_shift) {
            return glib::Propagation::Proceed;
        }
        if keyval == gdk::Key::c || keyval == gdk::Key::C {
            term_keys.copy_clipboard_format(Format::Text);
            return glib::Propagation::Stop;
        }
        if keyval == gdk::Key::v || keyval == gdk::Key::V {
            term_keys.paste_clipboard();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    term.add_controller(keys);
}

#[cfg(test)]
mod tests {
    use super::cycle_target;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
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
