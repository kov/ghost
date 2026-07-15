//! What a program on the other end of the tty is allowed to do to the terminal.
//!
//! Everything the emulator honours is something a program *asked for* by writing
//! bytes — and the program need not be one the user trusts. It may be `ssh`'d in
//! from a machine they don't own, or the output of a `curl | sh` gone wrong, or
//! just a `cat` of a file with an escape sequence buried in it. Most of what it
//! can ask for is harmless and load-bearing (that's why terminals honour it), but
//! some of it reaches past the screen: it changes the window, the desktop, or what
//! the terminal *says back*.
//!
//! This is the seam where that gets decided. The [`Default`] leaves on everything
//! a program needs to drive its own screen and turns off the three unprompted
//! reaches a program rarely needs — reading the title back, resizing the window
//! from output, and taking the window over — matching what xterm ships.
//! [`TerminalPolicy::allow_all`] keeps the lot for the callers that need it.
//!
//! # The two policies, and why there are two
//!
//! [`TerminalPolicy`] governs what changes the **screen's state**: the title, the
//! palette, the cursor's shape, the grid's size. All of it rides
//! [`Terminal::dump`](crate::Terminal::dump) — and ghost runs *two* emulators over
//! the same byte stream, the GUI's and the session host's, with the host's dump
//! re-seeding the GUI every time it attaches. So this policy has to be enforced in
//! the emulator, and it has to be **the same in both processes**: a GUI that
//! allowed something the host denied would watch it vanish on the next reconnect,
//! and the two would answer the same program's queries differently depending on
//! whether anyone was looking. It follows that this policy belongs to the
//! *session*, fixed when the session is spawned — not to the window that happens
//! to be showing it.
//!
//! [`ActionPolicy`] governs the **side effects**: writing the system clipboard,
//! minimizing the window. These never touch the screen and are never dumped — the
//! emulator only queues them, and the headless host drains them into the void. So
//! they are *not* enforced here: they are enforced where the queue is consumed, by
//! the frontend that would actually perform them. That is the only place that
//! knows enough to decide — whether the session is the one on screen or one being
//! previewed in the fleet, and (later) whether to stop and ask the user. An
//! emulator that dropped the payload would make asking impossible.
//!
//! In short: [`TerminalPolicy`] is frozen at spawn and shared by both emulators;
//! [`ActionPolicy`] is the frontend's, and can change while the session runs.
//!
//! # What is deliberately not here
//!
//! - **Reading the clipboard.** A terminal that answers an OSC 52 query hands any
//!   program that can write to the tty whatever the user last copied — and the
//!   answer arrives on the program's stdin, indistinguishable from typing. Ghost
//!   does not implement it, and there is no field for it *on purpose*: a knob is
//!   an invitation to turn it.
//! - **The bell**, and **hyperlinks**. Neither is a terminal-state hazard: the
//!   bell is presentation (and the session host counts bells for the fleet's
//!   unseen-activity marker, so denying it in the emulator would break a feature),
//!   and a hyperlink is only dangerous when *followed*, which the frontend already
//!   guards at the click.
//! - **The alternate screen, autowrap, origin mode** and the rest of the modes a
//!   TUI cannot live without. A knob nobody can turn off isn't a policy.
//!
//! # Scope
//!
//! These policies bind ghost's own emulators. A plain `ghost attach` from some
//! other terminal is a byte pipe: the escape sequences land in *that* terminal and
//! it honours them by its own rules, which are none of ghost's business.

/// What a program may change about the terminal's own state.
///
/// Enforced inside the emulator (see [`Terminal::execute`](crate::Terminal)), so
/// the GUI and the session host must be given the *same* one — see the module
/// docs. [`Default`] is the safe set; [`Self::allow_all`] is everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TerminalPolicy {
    /// Set the window/icon title (OSC 0/1/2) and push/pop the title stack
    /// (XTWINOPS `CSI 22/23 t`).
    ///
    /// Denying this also empties what a title *query* answers with, which is the
    /// point: a title the program chose, read back into the shell, is xterm's
    /// classic reflection trick. It also costs the fleet its card names for that
    /// session — the cards fall back to the session's name.
    pub title: bool,
    /// Resize the grid from within the output: XTWINOPS `CSI 4/8 t` and DECSLPP,
    /// and the DECCOLM 80↔132 switch.
    ///
    /// Denying it also strips `?40` (Allow80To132) — the mode that arms DECCOLM —
    /// or a program would simply re-arm the switch itself. The size a program asks
    /// for is clamped either way (see `MAX_PROGRAM_COLS`/`MAX_PROGRAM_ROWS`); this
    /// is about whether it may ask at all.
    pub program_resize: bool,
    /// Repaint the terminal in its own colors: the indexed palette (OSC 4/104),
    /// the special colors (OSC 5/105), and the dynamic fore/back/cursor colors
    /// (OSC 10/11/12 and 110/111/112).
    ///
    /// A denial still honours the *resets* — a program that gives the colors back
    /// is always taken up on it, so a denied session can't be left discolored.
    pub colors: bool,
    /// Change the cursor's shape and blink (DECSCUSR).
    pub cursor_style: bool,
    /// Draw images (the kitty graphics protocol).
    ///
    /// A denial is silent, and that is correct here: a program detects graphics
    /// support by racing a query against DA1, so a session that denies them simply
    /// reads as a terminal that has none.
    pub graphics: bool,
    /// Report progress to the desktop's taskbar (OSC 9;4).
    pub progress: bool,
    /// Read the title *back* (XTWINOPS `CSI 20/21 t`).
    ///
    /// Separate from [`Self::title`] because the two hazards are opposite ends of
    /// the same sequence: setting a title is cosmetic, but *reading* one puts text
    /// the program chose onto the shell's stdin — it sets a title and asks for it
    /// back. Ghost's OSC parser drops C0 controls from a title, so the reflected
    /// text cannot carry a newline and so cannot execute on its own; it lands in the
    /// shell's line-edit buffer and runs only if the user then presses Enter. Real,
    /// but narrower than xterm's classic version — xterm disables the report by
    /// default all the same, and keeps the set. A denial is still answered, just
    /// with an empty title: a query that goes unanswered hangs the program that
    /// asked.
    pub report_title: bool,
    /// Ask to be sent mouse events (`?1000`/`?1002`/`?1003`/`?1006` and friends).
    ///
    /// Not a security matter so much as an expectation one: a program that turns
    /// mouse reporting on takes the user's mouse away from the terminal — no
    /// select-to-copy, no right-click menu — and a program that turns it on and
    /// dies leaves it that way.
    pub mouse_report: bool,
}

impl Default for TerminalPolicy {
    /// The safe defaults for a real session: everything a program may do to its
    /// own terminal, minus the two unprompted hazards it rarely needs — reading
    /// the title *back* (the reflection trick, see [`Self::report_title`]) and
    /// resizing the window/grid from output ([`Self::program_resize`]). xterm
    /// ships both off. [`Self::allow_all`] keeps the lot for the callers that need
    /// it (the conformance harness).
    fn default() -> Self {
        Self {
            report_title: false,
            program_resize: false,
            ..Self::allow_all()
        }
    }
}

impl TerminalPolicy {
    /// Everything on — a *separate* constructor from [`Default`] (which now argues
    /// three of these down, as xterm does), so the callers that genuinely need the
    /// lot don't quietly lose it. The conformance harness is one: esctest drives the
    /// window ops, the title stack and the palette, and it should keep passing on its
    /// own terms rather than be a hostage to what we decide is safe for a stranger's
    /// tty.
    pub fn allow_all() -> Self {
        Self {
            title: true,
            program_resize: true,
            colors: true,
            cursor_style: true,
            graphics: true,
            progress: true,
            report_title: true,
            mouse_report: true,
        }
    }

    /// Nothing a program asks for beyond drawing on its own screen.
    pub fn deny_all() -> Self {
        Self {
            title: false,
            program_resize: false,
            colors: false,
            cursor_style: false,
            graphics: false,
            progress: false,
            report_title: false,
            mouse_report: false,
        }
    }
}

/// What a program may do *outside* the terminal: to the window, to the desktop.
///
/// NOT enforced in the emulator — see the module docs. The emulator queues these
/// (`take_window_ops`, `take_clipboard_writes`) and the frontend decides at the
/// moment it would carry them out, because only the frontend knows whether this
/// session is the one the user is looking at.
///
/// Orthogonal to that: a *background* or fleet-previewed session doesn't get to do
/// these at all, whatever the policy says — that isn't a preference, it's that the
/// window and the clipboard aren't its to touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionPolicy {
    /// Put text on the system clipboard (OSC 52).
    ///
    /// Reading it back is not offered and never will be (module docs).
    pub clipboard_write: bool,
    /// Minimize, maximize or full-screen the window (XTWINOPS `CSI 1/2/9/10 t`).
    pub window_control: bool,
}

impl Default for ActionPolicy {
    /// Safe default: a program may put text on the clipboard (remote copy is too
    /// useful to lose, and read-back is impossible anyway), but may not take the
    /// window over — iconify/maximize/full-screen — unprompted.
    fn default() -> Self {
        Self {
            window_control: false,
            ..Self::allow_all()
        }
    }
}

impl ActionPolicy {
    /// Everything on, whatever the defaults become.
    pub fn allow_all() -> Self {
        Self {
            clipboard_write: true,
            window_control: true,
        }
    }

    /// Nothing that reaches outside the terminal.
    pub fn deny_all() -> Self {
        Self {
            clipboard_write: false,
            window_control: false,
        }
    }
}

/// Both policies together — what a program on one session's tty may do, to the
/// terminal and to the desktop.
///
/// They are enforced in different places for good reasons (see the module docs),
/// but they are chosen together, by the user, in one place. Passing them as a pair
/// keeps a caller from setting one and forgetting the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SessionPolicy {
    pub terminal: TerminalPolicy,
    pub action: ActionPolicy,
}

impl SessionPolicy {
    /// Everything on, whatever the defaults become.
    pub fn allow_all() -> Self {
        Self {
            terminal: TerminalPolicy::allow_all(),
            action: ActionPolicy::allow_all(),
        }
    }

    /// Nothing a program asks for beyond drawing on its own screen.
    pub fn deny_all() -> Self {
        Self {
            terminal: TerminalPolicy::deny_all(),
            action: ActionPolicy::deny_all(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_denies_the_unprompted_hazards() {
        // A real session's default: everything a program may do to its own
        // terminal, minus the three unprompted hazards it rarely needs — reading
        // the title back (a reflection trick), resizing the window/grid from
        // output, and taking the window over (iconify/maximize/full-screen). This
        // is what the GUI and the detached host both start from.
        let t = TerminalPolicy::default();
        assert!(!t.report_title, "title read-back off by default");
        assert!(!t.program_resize, "program-driven resize off by default");
        // The rest a program may still do to its own terminal.
        assert!(t.title);
        assert!(t.colors);
        assert!(t.cursor_style);
        assert!(t.graphics);
        assert!(t.progress);
        assert!(t.mouse_report);

        let a = ActionPolicy::default();
        assert!(!a.window_control, "window take-over off by default");
        assert!(a.clipboard_write, "clipboard write (remote copy) stays on");

        // `allow_all` is unchanged — the conformance harness pins it.
        assert!(TerminalPolicy::allow_all().report_title);
        assert!(TerminalPolicy::allow_all().program_resize);
        assert!(ActionPolicy::allow_all().window_control);
        assert_ne!(TerminalPolicy::default(), TerminalPolicy::allow_all());
    }
}
