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
use gtk4::{EventControllerKey, PropagationPhase, gdk};
use vte4::prelude::*;
use vte4::{Format, Terminal};

use ghost_vt::client::Session;
use ghost_vt::server::{self, SpawnOpts};
use ghost_vt::session::{self, SessionInfo};
use ghost_vt::{paths, record, screen};

const APP_ID: &str = "dev.ghost.Terminal";
/// Stack page shown when no session is open.
const EMPTY_PAGE: &str = "__empty__";

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
    /// Last rendered sidebar signature, so periodic refreshes only rebuild the
    /// list when something actually changed.
    last_sig: Rc<RefCell<Vec<RowSig>>>,
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
    stack.add_named(&empty, Some(EMPTY_PAGE));

    let sidebar_toggle = gtk4::ToggleButton::new();
    sidebar_toggle.set_icon_name("sidebar-show-symbolic");
    sidebar_toggle.set_tooltip_text(Some("Show sessions"));
    let content_title = adw::WindowTitle::new("ghost", "");
    let content_header = adw::HeaderBar::new();
    content_header.set_title_widget(Some(&content_title));
    content_header.pack_start(&sidebar_toggle);
    let content = adw::ToolbarView::new();
    content.add_top_bar(&content_header);
    content.set_content(Some(&stack));

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

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("ghost")
        .default_width(1000)
        .default_height(640)
        .content(&split)
        .build();

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
    };

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
            let session = match Session::attach(name, 80, 24) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("ghost-gtk: cannot attach to '{name}': {e}");
                    return;
                }
            };
            // Non-blocking reads: the drain timer polls, never blocking the UI.
            let _ = session.set_read_timeout(Some(Duration::from_millis(1)));
            let terminal = Terminal::new();
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
        if let Some(o) = self.open.borrow().get(name) {
            o.terminal.grab_focus();
        }
        // Reveal the terminal: dismiss the overlay sidebar.
        self.split.set_show_sidebar(false);
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
        row
    }

    /// Pump session output into the widget and propagate resizes, on a short GLib
    /// timer (glib 0.22 has no unix-fd source). Stops itself when the session is
    /// gone (closed/detached) or ends, removing it from the view on end.
    fn drive(&self, name: &str, terminal: &Terminal, session: &Rc<RefCell<Option<Session>>>) {
        let last_size = Rc::new(Cell::new((80i64, 24i64)));
        let ui = self.clone();
        let name = name.to_string();
        let terminal = terminal.clone();
        let session = session.clone();

        glib::timeout_add_local(Duration::from_millis(8), move || {
            // Grid resize -> Resize, correcting the provisional handshake size.
            let size = (terminal.column_count(), terminal.row_count());
            {
                let mut guard = session.borrow_mut();
                let Some(s) = guard.as_mut() else {
                    return glib::ControlFlow::Break; // closed: stop
                };
                if size != last_size.get() && size.0 > 0 && size.1 > 0 {
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
