# CLI attach and terminal policy

`ghost attach` (and the SSH pipe that reuses the same client) is a **transparent
pipe**: it forwards the child's output bytes verbatim to whatever real terminal
the user launched it in, and forwards that terminal's input back to the child.
This note records how terminal policy — the rules for what a program on the
session's tty may change or ask about the terminal (see `ghost_term::policy` and
the `escape-sequence-policy` design) — applies across that pipe, and why it is
deliberately *not* enforced end-to-end there.

## Two emulators, two jurisdictions

The host runs an emulator that models the session's screen; it adopts and
enforces a `TerminalPolicy` for everything it answers itself — DA/DSR reports,
title reports, and, load-bearing, it **never answers an OSC 52 clipboard *read***.
The GUI frontend adds a second layer (`ActionPolicy`) governing what the *display*
does, and reports its policy to the host at attach (`Session::report_policy`,
called only from `ghost-ui`).

A CLI attach has **no ghost-controlled frontend**. The outer terminal — xterm,
kitty, Terminal.app, whatever the user ran `ghost attach` inside — is the display,
and ghost does not control it. `run_attach` never calls `report_policy`, so:

- The **host emulator** keeps enforcing the policy it was spawned with (or last
  had reported by a GUI client). That governs the state ghost *models* and the
  replies ghost *itself* sends while detached or on behalf of the session.
- The **outer terminal** governs what actually reaches the user's display and,
  crucially, **answers its own queries**. A child sequence the host forwards
  verbatim — including an OSC 52 clipboard *read* — is answered (or refused) by
  that outer terminal, not by ghost. If the outer terminal is permissive, it can
  hand a program the clipboard on ghost's behalf, and ghost cannot stop it from
  out here.

## Why not clamp the pipe

We deliberately do **not** try to scrub or shape the byte stream in the CLI pipe:

- The pipe's whole contract is transparency — a real terminal on the other end
  expects the session's bytes unaltered, and its own scrollback, clipboard, and
  query handling are the user's, not ghost's to override.
- The resync the host sends on attach clears only the *visible* screen, never the
  outer terminal's scrollback (see `Screen::resync`); by the same principle we do
  not reach past the pipe to police the terminal's other behaviours.
- Policy is a property of a *display* ghost owns. Over a transparent pipe there
  isn't one — the jurisdiction is the outer terminal's.

## What this means in practice

A user who needs ghost's guarantees (notably the OSC 52 read denial) enforced at
the display must attach through the **GUI**, or run `ghost attach` inside a
terminal that itself denies the sequence. The alternative once considered —
having `run_attach` report `allow_all()` so at least the host's stance is explicit
— was rejected: it would only *loosen* the host emulator to match a permissive
outer terminal, buying nothing, while a stricter host policy that the outer
terminal ignores is the honest state of the world, not a bug to paper over.
