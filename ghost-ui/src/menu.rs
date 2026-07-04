//! The native macOS menu bar.
//!
//! winit already installs the standard **App** submenu (About / Hide / Hide
//! Others / Show All / Quit) for us; this module appends the File / Edit / View /
//! Window submenus a native terminal is expected to have. The ghost core already
//! owns every one of these commands as a keyboard shortcut, so a custom menu item
//! never re-implements the behaviour — it just re-injects the exact chord (or
//! command) the core already understands, keeping a single source of truth.
//!
//! Menu clicks arrive on AppKit's main thread; a small objc2 target object
//! ([`imp::MenuTarget`]) forwards each one over an [`EventLoopProxy`] as a
//! [`UserEvent::Menu`], which the shell's `user_event` handler turns into the
//! matching effect on the focused window. The standard window commands
//! (Close / Minimize / Zoom, and Cmd-` cycling through the auto-managed Window
//! menu) use AppKit's own selectors, so they need no routing at all.
//!
//! The pure part below (the [`MenuAction`] → [`MenuIntent`] mapping and the item
//! tags) is platform-independent and unit-tested; the objc2 construction lives in
//! the macOS-only [`imp`] submodule and is verified with the `GHOST_MENU_DUMP`
//! probe (a native menu can't be click-driven under the test sandbox).

use ghost_ui_core::Cmd;
use ghost_ui_core::input::{Key, Mods, NamedKey};

/// A user event delivered to the winit event loop from outside the normal input
/// stream. Today the only source is a native menu selection.
///
/// A cross-thread message posted to the event loop via its `EventLoopProxy`.
///
/// The `Menu` variant is only ever constructed by the macOS-only [`imp`] target;
/// off macOS nothing posts it, so it's allowed to go unconstructed there without
/// tripping `-D warnings`. `RemoteSessions` is posted by the remote-fleet poller
/// thread; `ConnectFinished` by the ssh-connect worker thread.
#[derive(Clone, Debug)]
pub enum UserEvent {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Menu(MenuAction),
    /// A remote host's latest session listing (namespaced ids), for the fleet.
    RemoteSessions {
        target: String,
        infos: Vec<ghost_vt::session::SessionInfo>,
    },
    /// The background half of an ssh connect finished (negotiate/stage/spawn ran
    /// off the event loop): the main loop attaches the window over the result.
    ConnectFinished {
        wid: winit::window::WindowId,
        spec: ghost_vt::connection::ConnectionSpec,
        /// The session name spawned on the remote (bare, transport-addressed).
        name: String,
        outcome: ConnectOutcome,
    },
    /// Staging progress for an in-flight connect: `sent` of `total` bytes copied
    /// to the remote. Drives the connect prompt's progress bar.
    ConnectProgress {
        wid: winit::window::WindowId,
        sent: u64,
        total: u64,
    },
}

/// The result of the off-loop half of an ssh connect (after auth): what the main
/// loop should do to finish it.
#[derive(Clone, Debug)]
pub enum ConnectOutcome {
    /// A remote ghost was negotiated (staged if needed) and the host spawned;
    /// attach the window over the transport using this remote binary.
    Transport { remote_ghost: String },
    /// The remote can't host ghost — fall back to a local ssh child.
    Fallback,
    /// The setup failed; show the message on the connect prompt.
    Error(String),
}

/// A custom menu item that maps back onto a ghost command. The native window
/// commands (Close / Minimize / Zoom / cycle) are handled by AppKit directly and
/// are deliberately absent here.
///
/// Its variants and the `tag`/`from_tag` helpers below are exercised only by the
/// macOS-only [`imp`] target (the cross-platform code merely maps an already-built
/// action via [`menu_intent`]), so off macOS they are intentionally unused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub enum MenuAction {
    NewWindow,
    NewSession,
    Copy,
    Paste,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    ToggleFleet,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl MenuAction {
    /// The stable tag stored on the `NSMenuItem`, so the target can recover the
    /// action from `[sender tag]`. Starts at 1 because 0 is `NSMenuItem`'s default
    /// tag (an untagged item must never resolve to an action).
    pub fn tag(self) -> isize {
        match self {
            MenuAction::NewWindow => 1,
            MenuAction::NewSession => 2,
            MenuAction::Copy => 3,
            MenuAction::Paste => 4,
            MenuAction::ZoomIn => 5,
            MenuAction::ZoomOut => 6,
            MenuAction::ZoomReset => 7,
            MenuAction::ToggleFleet => 8,
        }
    }

    /// Recover an action from an `NSMenuItem` tag; `None` for an untagged (0) or
    /// unknown item, so a stray click can never be misread as an action.
    pub fn from_tag(tag: isize) -> Option<Self> {
        Some(match tag {
            1 => MenuAction::NewWindow,
            2 => MenuAction::NewSession,
            3 => MenuAction::Copy,
            4 => MenuAction::Paste,
            5 => MenuAction::ZoomIn,
            6 => MenuAction::ZoomOut,
            7 => MenuAction::ZoomReset,
            8 => MenuAction::ToggleFleet,
            _ => return None,
        })
    }
}

/// What the shell should do for a menu action, expressed with only pure core
/// types so the routing is unit-testable (the shell matches on this in
/// `user_event`). Not `Eq` — [`Cmd`] carries non-`Eq` payloads.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuIntent {
    /// Open a fresh fleet window — works even with no window focused.
    NewWindow,
    /// Issue a command to the focused window.
    FocusedCmd(Cmd),
    /// Re-inject a key chord into the focused window, so it flows through the
    /// exact `classify_shortcut` path a real keypress would.
    FocusedKey(Key, Mods),
}

/// Map a menu action to its effect. Copy / Paste / Zoom re-use the Command chords
/// the core already classifies (Cmd is `Mods::SUPER`), so the menu replays a
/// keystroke rather than duplicating the shortcut logic. Toggle Fleet replays the
/// bare F9 the core interprets directly.
pub fn menu_intent(action: MenuAction) -> MenuIntent {
    match action {
        MenuAction::NewWindow => MenuIntent::NewWindow,
        MenuAction::NewSession => MenuIntent::FocusedCmd(Cmd::SpawnSession),
        MenuAction::Copy => MenuIntent::FocusedKey(Key::Char("c".into()), Mods::SUPER),
        MenuAction::Paste => MenuIntent::FocusedKey(Key::Char("v".into()), Mods::SUPER),
        MenuAction::ZoomIn => MenuIntent::FocusedKey(Key::Char("=".into()), Mods::SUPER),
        MenuAction::ZoomOut => MenuIntent::FocusedKey(Key::Char("-".into()), Mods::SUPER),
        MenuAction::ZoomReset => MenuIntent::FocusedKey(Key::Char("0".into()), Mods::SUPER),
        MenuAction::ToggleFleet => MenuIntent::FocusedKey(Key::Named(NamedKey::F9), Mods::NONE),
    }
}

#[cfg(target_os = "macos")]
pub use imp::{dump, install};

#[cfg(target_os = "macos")]
mod imp {
    //! The AppKit side: an objc2 target object that forwards `ghostMenuAction:`
    //! clicks over the event-loop proxy, and the menu construction that appends
    //! our submenus to winit's menu bar.

    use std::cell::RefCell;

    use objc2::ffi::class_addMethod;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
    use objc2::{ClassType, DeclaredClass, declare_class, msg_send, msg_send_id, mutability, sel};
    use objc2_app_kit::{NSApplication, NSEventModifierFlags, NSMenu, NSMenuItem};
    use objc2_foundation::{MainThreadMarker, NSObject, NSObjectProtocol, NSString, ns_string};
    use winit::event_loop::EventLoopProxy;

    use super::{MenuAction, UserEvent};

    pub struct MenuTargetIvars {
        proxy: EventLoopProxy<UserEvent>,
    }

    declare_class!(
        /// The AppKit target that receives clicks on our custom menu items and
        /// forwards them to the event loop. Kept alive for the process lifetime in
        /// [`MENU`] (an `NSMenu` does not retain its items' targets).
        pub struct MenuTarget;

        unsafe impl ClassType for MenuTarget {
            type Super = NSObject;
            type Mutability = mutability::MainThreadOnly;
            const NAME: &'static str = "GhostMenuTarget";
        }

        impl DeclaredClass for MenuTarget {
            type Ivars = MenuTargetIvars;
        }

        unsafe impl NSObjectProtocol for MenuTarget {}

        unsafe impl MenuTarget {
            #[method(ghostMenuAction:)]
            fn ghost_menu_action(&self, sender: &NSMenuItem) {
                let tag = unsafe { sender.tag() };
                if let Some(action) = MenuAction::from_tag(tag) {
                    // The proxy just wakes the run loop we're already in; the
                    // shell handles the event on the next turn.
                    let _ = self.ivars().proxy.send_event(UserEvent::Menu(action));
                }
            }
        }
    );

    impl MenuTarget {
        fn new(mtm: MainThreadMarker, proxy: EventLoopProxy<UserEvent>) -> Retained<Self> {
            let this = mtm.alloc().set_ivars(MenuTargetIvars { proxy });
            unsafe { msg_send_id![super(this), init] }
        }
    }

    thread_local! {
        /// The retained target, kept alive for the process. `NSMenu` holds its
        /// items but not their target, so if this dropped, every custom item's
        /// click would message a freed object.
        static MENU: RefCell<Option<Retained<MenuTarget>>> = const { RefCell::new(None) };

        /// The Dock icon's right-click menu, built once and kept alive for the
        /// process. AppKit asks for it via `applicationDockMenu:` (see
        /// [`application_dock_menu`]); we return this same menu each time.
        static DOCK_MENU: RefCell<Option<Retained<NSMenu>>> = const { RefCell::new(None) };
    }

    /// The app delegate's `applicationDockMenu:`, injected onto winit's delegate
    /// class in [`install_dock_menu`]. Returns the Dock menu we built, autoreleased
    /// as the method's contract requires. `this`/`_app` are unused: the menu's item
    /// already targets our [`MenuTarget`], so the Dock action needs nothing from the
    /// delegate.
    unsafe extern "C" fn application_dock_menu(
        _this: *mut AnyObject,
        _cmd: Sel,
        _app: *mut AnyObject,
    ) -> *mut AnyObject {
        DOCK_MENU.with(|m| match m.borrow().as_ref() {
            Some(menu) => Retained::autorelease_return(menu.clone()).cast(),
            None => std::ptr::null_mut(),
        })
    }

    /// Append ghost's File / Edit / View / Window submenus to the menu bar winit
    /// already installed, wiring custom items to `target`. Idempotent — a second
    /// call is a no-op — and must run on the main thread.
    pub fn install(proxy: EventLoopProxy<UserEvent>) {
        let mtm = MainThreadMarker::new().expect("the menu is installed on the main thread");
        if MENU.with(|m| m.borrow().is_some()) {
            return;
        }
        let app = NSApplication::sharedApplication(mtm);
        let target = MenuTarget::new(mtm, proxy);
        // winit sets the main menu (with the App submenu) in
        // applicationDidFinishLaunching, which precedes `resumed`; fall back to a
        // fresh bar only if that ever changes, so the App submenu still leads.
        let bar = unsafe { app.mainMenu() }.unwrap_or_else(|| {
            let bar = NSMenu::new(mtm);
            app.setMainMenu(Some(&bar));
            bar
        });

        // File: New Window (Cmd-N), New Session (Cmd-T), Close (Cmd-W, native).
        let file = submenu(mtm, &bar, ns_string!("File"));
        file.addItem(&action_item(
            mtm,
            &target,
            ns_string!("New Window"),
            MenuAction::NewWindow,
            ns_string!("n"),
        ));
        file.addItem(&action_item(
            mtm,
            &target,
            ns_string!("New Session"),
            MenuAction::NewSession,
            ns_string!("t"),
        ));
        file.addItem(&NSMenuItem::separatorItem(mtm));
        // performClose: sends the key window a close, which winit surfaces as
        // WindowEvent::CloseRequested — the same "close = detach" path Cmd-W takes.
        file.addItem(&native_item(
            mtm,
            ns_string!("Close Window"),
            sel!(performClose:),
            ns_string!("w"),
        ));

        // Edit: Copy (Cmd-C), Paste (Cmd-V) — routed so they mirror the terminal's
        // own selection/clipboard handling rather than AppKit's inert copy:/paste:.
        let edit = submenu(mtm, &bar, ns_string!("Edit"));
        edit.addItem(&action_item(
            mtm,
            &target,
            ns_string!("Copy"),
            MenuAction::Copy,
            ns_string!("c"),
        ));
        edit.addItem(&action_item(
            mtm,
            &target,
            ns_string!("Paste"),
            MenuAction::Paste,
            ns_string!("v"),
        ));
        edit.addItem(&NSMenuItem::separatorItem(mtm));
        // AppKit's own Character Viewer (the system emoji picker). Nil-targeted:
        // the responder chain ends at NSApplication, which implements it. Bound
        // to Ctrl-Cmd-Space: the chord is not a global hotkey — in apps where it
        // works it is the key equivalent of their (usually AppKit auto-added)
        // Edit-menu item, so ours must carry it too. The picked characters
        // arrive through the IME commit path like any composed text.
        let emoji = native_item(
            mtm,
            ns_string!("Emoji & Symbols"),
            sel!(orderFrontCharacterPalette:),
            ns_string!(" "),
        );
        emoji.setKeyEquivalentModifierMask(
            NSEventModifierFlags::NSEventModifierFlagControl
                | NSEventModifierFlags::NSEventModifierFlagCommand,
        );
        edit.addItem(&emoji);

        // View: font zoom (Cmd +/-/0) and the fleet overview toggle (F9).
        let view = submenu(mtm, &bar, ns_string!("View"));
        view.addItem(&action_item(
            mtm,
            &target,
            ns_string!("Zoom In"),
            MenuAction::ZoomIn,
            ns_string!("="),
        ));
        view.addItem(&action_item(
            mtm,
            &target,
            ns_string!("Zoom Out"),
            MenuAction::ZoomOut,
            ns_string!("-"),
        ));
        view.addItem(&action_item(
            mtm,
            &target,
            ns_string!("Actual Size"),
            MenuAction::ZoomReset,
            ns_string!("0"),
        ));
        view.addItem(&NSMenuItem::separatorItem(mtm));
        // No key equivalent: F9 is not a Command chord, so the core already
        // handles the keystroke; this is just the discoverable menu entry.
        view.addItem(&action_item(
            mtm,
            &target,
            ns_string!("Toggle Fleet Overview"),
            MenuAction::ToggleFleet,
            ns_string!(""),
        ));

        // Window: the standard native items plus AppKit's auto-managed window list
        // (and Cmd-` cycling) once we hand it to `setWindowsMenu`.
        let window = submenu(mtm, &bar, ns_string!("Window"));
        window.addItem(&native_item(
            mtm,
            ns_string!("Minimize"),
            sel!(performMiniaturize:),
            ns_string!("m"),
        ));
        window.addItem(&native_item(
            mtm,
            ns_string!("Zoom"),
            sel!(performZoom:),
            ns_string!(""),
        ));
        window.addItem(&NSMenuItem::separatorItem(mtm));
        window.addItem(&native_item(
            mtm,
            ns_string!("Bring All to Front"),
            sel!(arrangeInFront:),
            ns_string!(""),
        ));
        unsafe { app.setWindowsMenu(Some(&window)) };

        install_dock_menu(mtm, &app, &target);

        MENU.with(|m| *m.borrow_mut() = Some(target));
    }

    /// Give the Dock icon's right-click menu a "New Window" item. AppKit sources the
    /// Dock menu from the app delegate's `applicationDockMenu:`, and winit owns the
    /// delegate, so we add that one method to its class at runtime (winit does not
    /// implement it) and hand back a menu we build here. The item routes through the
    /// same [`MenuTarget`] as File > New Window — the target holds the event-loop
    /// proxy — so it works even with no window focused or the app in the background.
    fn install_dock_menu(mtm: MainThreadMarker, app: &NSApplication, target: &MenuTarget) {
        let menu = NSMenu::new(mtm);
        menu.addItem(&action_item(
            mtm,
            target,
            ns_string!("New Window"),
            MenuAction::NewWindow,
            ns_string!(""),
        ));
        DOCK_MENU.with(|m| *m.borrow_mut() = Some(menu));

        // winit installs its delegate in applicationDidFinishLaunching (before
        // `resumed`, where this runs), so it is present; skip the Dock menu if not.
        let delegate: *mut AnyObject = unsafe { msg_send![app, delegate] };
        if delegate.is_null() {
            eprintln!("ghost-ui: no app delegate; Dock menu unavailable");
            return;
        }
        let cls: &AnyClass = unsafe { msg_send![delegate, class] };
        let imp: Imp = unsafe {
            std::mem::transmute::<
                unsafe extern "C" fn(*mut AnyObject, Sel, *mut AnyObject) -> *mut AnyObject,
                Imp,
            >(application_dock_menu)
        };
        // `AnyClass` wraps `objc_class`, so the pointer casts straight through. Adding
        // the method returns false only if it already exists (a second install) — the
        // `MENU`-set guard above makes that unreachable, and it would be harmless.
        // Encoding: returns an object (`@`), args self (`@`) + _cmd (`:`) + sender (`@`).
        let cls_ptr = core::ptr::from_ref(cls)
            .cast::<objc2::ffi::objc_class>()
            .cast_mut();
        unsafe {
            class_addMethod(
                cls_ptr,
                sel!(applicationDockMenu:).as_ptr(),
                Some(imp),
                c"@@:@".as_ptr(),
            );
        }
    }

    /// Add a titled submenu to the bar and return it for populating.
    fn submenu(mtm: MainThreadMarker, bar: &NSMenu, title: &NSString) -> Retained<NSMenu> {
        let holder = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(mtm.alloc(), title, None, ns_string!(""))
        };
        let menu = NSMenu::new(mtm);
        // The submenu's own title is used by AppKit for auto-managed menus (e.g.
        // Window); harmless for the rest.
        unsafe { menu.setTitle(title) };
        holder.setSubmenu(Some(&menu));
        bar.addItem(&holder);
        menu
    }

    /// A menu item that routes to our target via `ghostMenuAction:`, tagged with
    /// the action so the target can recover it. A lowercase-letter key equivalent
    /// carries AppKit's default Command modifier, so these are Cmd-<key>.
    fn action_item(
        mtm: MainThreadMarker,
        target: &MenuTarget,
        title: &NSString,
        action: MenuAction,
        key: &NSString,
    ) -> Retained<NSMenuItem> {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc(),
                title,
                Some(sel!(ghostMenuAction:)),
                key,
            )
        };
        let obj: &AnyObject = target;
        unsafe {
            item.setTarget(Some(obj));
            item.setTag(action.tag());
        }
        item
    }

    /// A menu item wired to a standard AppKit selector (target `nil`, so AppKit
    /// dispatches it down the responder chain to the key window).
    fn native_item(
        mtm: MainThreadMarker,
        title: &NSString,
        selector: Sel,
        key: &NSString,
    ) -> Retained<NSMenuItem> {
        unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(mtm.alloc(), title, Some(selector), key)
        }
    }

    /// Print the installed menu bar's structure to stdout (one `MENU` line per
    /// item: indent depth, title, key equivalent, selector). Drives the
    /// `GHOST_MENU_DUMP` verification path — a native menu can't be clicked under
    /// the test sandbox, so we assert on this instead.
    pub fn dump() {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        match unsafe { app.mainMenu() } {
            Some(bar) => dump_menu(&bar, 0),
            None => println!("MENU\t<no menu bar>"),
        }
        dump_dock_menu(&app);
    }

    /// Ask the app delegate for the Dock menu exactly as AppKit would and print it
    /// under a `DOCK` header — proving the injected `applicationDockMenu:` is live
    /// and returns our routed item (a native Dock menu can't be right-clicked in the
    /// sandbox, so we assert on this instead).
    fn dump_dock_menu(app: &NSApplication) {
        let delegate: *mut AnyObject = unsafe { msg_send![app, delegate] };
        if delegate.is_null() {
            return;
        }
        let responds: bool =
            unsafe { msg_send![delegate, respondsToSelector: sel!(applicationDockMenu:)] };
        if !responds {
            return;
        }
        let dock: *mut NSMenu =
            unsafe { msg_send![delegate, applicationDockMenu: std::ptr::null_mut::<AnyObject>()] };
        if let Some(dock) = unsafe { dock.as_ref() } {
            println!("DOCK");
            dump_menu(dock, 1);
        }
    }

    fn dump_menu(menu: &NSMenu, depth: usize) {
        let n = unsafe { menu.numberOfItems() };
        for i in 0..n {
            let Some(item) = (unsafe { menu.itemAtIndex(i) }) else {
                continue;
            };
            let title = unsafe { item.title() };
            let key = unsafe { item.keyEquivalent() };
            let action = unsafe { item.action() }
                .map(|s| s.name().to_string())
                .unwrap_or_default();
            let indent = "  ".repeat(depth);
            // Non-default modifier masks (AppKit defaults to plain Command) get
            // a `mods=` suffix, so chords like Ctrl-Cmd-Space are assertable.
            let mask = unsafe { item.keyEquivalentModifierMask() };
            let mods = if mask == NSEventModifierFlags::NSEventModifierFlagCommand {
                String::new()
            } else {
                let names = [
                    (NSEventModifierFlags::NSEventModifierFlagControl, "ctrl"),
                    (NSEventModifierFlags::NSEventModifierFlagOption, "opt"),
                    (NSEventModifierFlags::NSEventModifierFlagShift, "shift"),
                    (NSEventModifierFlags::NSEventModifierFlagCommand, "cmd"),
                ];
                let joined = names
                    .iter()
                    .filter(|(flag, _)| mask.contains(*flag))
                    .map(|(_, name)| *name)
                    .collect::<Vec<_>>()
                    .join("-");
                format!("\tmods={joined}")
            };
            println!("MENU\t{indent}{title}\tkey={key}\taction={action}{mods}");
            if let Some(sub) = unsafe { item.submenu() } {
                dump_menu(&sub, depth + 1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_action_tags_round_trip_and_reject_strays() {
        for action in [
            MenuAction::NewWindow,
            MenuAction::NewSession,
            MenuAction::Copy,
            MenuAction::Paste,
            MenuAction::ZoomIn,
            MenuAction::ZoomOut,
            MenuAction::ZoomReset,
            MenuAction::ToggleFleet,
        ] {
            assert_eq!(MenuAction::from_tag(action.tag()), Some(action));
            assert!(
                action.tag() > 0,
                "0 is NSMenuItem's default; tags start at 1"
            );
        }
        // An untagged item (0) or an unknown tag is never an action.
        assert_eq!(MenuAction::from_tag(0), None);
        assert_eq!(MenuAction::from_tag(-1), None);
        assert_eq!(MenuAction::from_tag(999), None);
    }

    #[test]
    fn menu_intents_reinject_the_core_chords() {
        // The window-level actions bypass the terminal.
        assert_eq!(menu_intent(MenuAction::NewWindow), MenuIntent::NewWindow);
        assert_eq!(
            menu_intent(MenuAction::NewSession),
            MenuIntent::FocusedCmd(Cmd::SpawnSession)
        );
        // Copy/Paste/Zoom replay the exact Command chords `classify_shortcut`
        // understands, so the menu never forks the shortcut logic.
        assert_eq!(
            menu_intent(MenuAction::Copy),
            MenuIntent::FocusedKey(Key::Char("c".into()), Mods::SUPER)
        );
        assert_eq!(
            menu_intent(MenuAction::Paste),
            MenuIntent::FocusedKey(Key::Char("v".into()), Mods::SUPER)
        );
        assert_eq!(
            menu_intent(MenuAction::ZoomIn),
            MenuIntent::FocusedKey(Key::Char("=".into()), Mods::SUPER)
        );
        assert_eq!(
            menu_intent(MenuAction::ZoomOut),
            MenuIntent::FocusedKey(Key::Char("-".into()), Mods::SUPER)
        );
        assert_eq!(
            menu_intent(MenuAction::ZoomReset),
            MenuIntent::FocusedKey(Key::Char("0".into()), Mods::SUPER)
        );
        // Toggle Fleet replays the bare F9 the core interprets directly.
        assert_eq!(
            menu_intent(MenuAction::ToggleFleet),
            MenuIntent::FocusedKey(Key::Named(NamedKey::F9), Mods::NONE)
        );
    }
}
