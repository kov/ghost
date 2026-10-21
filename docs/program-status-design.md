# Program status (OSC 7501): what a session's program is doing, at a glance

A program can tell its terminal what state it is in: working, blocked on the
user (and why), done, failed, or idle. ghost turns that into window chrome.

- The foreground session's state tints the titlebar.
- A header-bar affordance lists the background sessions that are idle, done,
  failed, or waiting on the user.
- Fleet tiles show each session's state.

Claude Code is the first program that matters. It emits these reports from
2.1.295, once the terminal answers the protocol's capability query.

The protocol is OSC 7501, the "Program Status Protocol", revision 0.4:
<https://superlogical.com/rex/docs/build/program-status>. It is a third-party
spec, not Anthropic's. This document summarizes it only as far as ghost needs.

## The protocol, as ghost uses it

**Report.** `OSC 7501 ; key=value:key=value… ST`. The terminator is ST or BEL.

- Keys match `[a-z]+`. Values use a small alphabet with no `:` or `;`, so
  nothing needs escaping.
- A malformed pair is skipped. An unknown key is ignored. When a key repeats,
  the last value wins.

| Key | Meaning |
| --- | --- |
| `state` | Required: `idle`, `working`, `done`, `blocked`, `error`, or `clear`. An unknown state drops the whole report. |
| `id` | A `/`-separated path naming a record. Absent means the root record. Children nest under parents. |
| `kind` | With `blocked`: `permission`, `question` or `auth`. |
| `progress` | 0–100, with `working` or `blocked`. Absent means indeterminate. |
| `app` | A stable program id (`claude-code`). Inherited from the nearest ancestor that has one. |
| `title`, `msg` | Base64 UTF-8. A short label, and one line of what it is doing or waiting for. |

**Records.**

- There is one record per id, and each report *replaces* its record whole.
- `clear` removes the addressed record and its descendants. A `clear` with no
  `id` removes every record.
- Records belong to the terminal, not to a screen buffer, and survive an
  alternate-screen switch. `RIS` drops them; `DECSTR` does not.
- At most 256 records. Past the cap, the least recently updated one goes.

**Lifetime.**

- A new shell prompt (OSC 133 `A`) or the exit of the attached process drops
  `working` and `blocked` records, and may drop `idle` ones.
- `done` and `error` survive both, until replaced or cleared. When to stop
  *showing* them is the terminal's call.

**Detection.**

- The program sends `OSC 7501 ; ?` and then `CSI c`. If the DA reply arrives
  first, the protocol is unsupported.
- ghost replies `OSC 7501 ; ? : states=idle,working,done,blocked,error :
  kinds=permission,question,auth ST`.
- That reply is the only thing ghost ever writes back. Records are never echoed
  and cannot be read back.

**Limits and safety.**

- A whole sequence is at most 4096 bytes.
- `msg` is at most 2048 bytes decoded (check the encoded size, 2732, before
  decoding). `title` is at most 192 bytes decoded.
- An id is at most 128 bytes, 8 levels deep, 32 bytes per segment.
- A report that breaks a limit, carries bad base64, or decodes to a control
  character is dropped whole.
- Text is never markup. Bidirectional overrides and invisible formatting are
  disarmed wherever it is drawn outside the grid.

## Where the state lives

**In the emulator, `ghost-term`, as terminal state.** That is what the spec
says a record is, and it is where ghost keeps every comparable piece of state:
the title, modes, prompt marks, kitty images.

- **Ordering comes free.** The parser sees reports and OSC 133 `A` in stream
  order, so "a prompt drops `working`" is exact, with no second scanner to keep
  in step.
- **The host and every client already run the same emulator over the same
  bytes.** The display client's replica, a fleet observer's mirror, and the
  host's own screen all hold the records, with nothing new on the wire for live
  output.
- **The dump carries it** (state a plain dump cannot express diverges on every resync). `Vt::dump`
  re-emits one `OSC 7501` report per record, after the grid, just as it re-emits
  the title. A reattaching client, and a session recreated from its recording,
  come back with the same records.
- **Process exit is the host's to apply.** Only the host sees the child exit.
  It clears `working` and `blocked` in its own `Vt` and the client applies the
  same rule on `ServerMsg::Exited`. Both run one `Vt` method, so they cannot
  disagree.

A `ProgramStatus` type in `ghost-term` holds the records keyed by id. It does
the validation and limits above, and exposes a *headline*: the record that
speaks for the session. The headline is the most urgent state, in the order
`blocked` > `error` > `done` > `working` > `idle`, ties going to the most
recently updated record. Chrome only ever shows the headline. The records
behind it exist so that clearing and replacement follow the spec exactly.

## Answering the query

`QueryScanner` (`ghost-vt/src/query.rs`) already recognizes OSC queries and
answers both the attached and detached paths in stream order. It gains
`Query::ProgramStatus` with a fixed reply.

- Because the same scanner answers the `CSI c` that follows, the 7501 reply
  always goes out first. A program then detects support whether or not anyone
  is attached.
- The reply is identity, not state. It is safe under every
  escape-sequence policy (`ghost_term::policy`) and needs no new policy switch.

## Where each view reads it

Three readers, each from the copy nearest to it:

- **The window's own sessions** — its foreground and the warm mirrors of its
  group behind it — are fed live, so their emulators already hold the records.
  The titlebar and the attention list read the headline straight from there,
  with no round trip.
- **Listings and watchers** — `ghost ls --json`, the remote `__watch` feed, a
  fleet tile that is not previewed — get it from the host:
  - At the end of each turn, if the headline changed, the host writes it into
    the session's `meta` (a `status` field, absent when there is none), so
    `SessionInfo.status` carries it with no new remote machinery.
  - It pushes `SessionEvent::StatusChanged(Option<StatusHeadline>)` to
    subscribers. The variant is appended; older subscribers skip the frame.
  - A subscriber that starts while a status is set gets the event right after
    its `Snapshot`, which therefore stays unchanged on the wire.
  - The host adds what the emulator cannot know: `since`, when the state (or
    its blocked kind) last changed — a new message under the same state keeps
    it — and `seen`, whether a display client has been attached since.

**Seen is decided where it is looked at.** The host's `seen` is right for a
session nobody in this process drives. It is wrong for a window's own group:
the window is the display client of every session in it, so the host counts
each background result as seen the moment it lands. The window therefore keeps
its own record — the report counter (`ProgramStatus::generation`) it last saw
per session — stamped by a key pressed in the foreground session and by
leaving a session (it was in front of the user). A counter rather than the
record, because a second turn's `done` says exactly what the first one did.

## What the user sees

**Titlebar, foreground session.** The headline tints the bar.

| Headline | Tint |
| --- | --- |
| `working` | violet, gentlest of all, and a line along the bar's foot |
| `blocked` | amber, stronger for `permission` and `question` |
| `done` | green, until the user has seen it |
| `error` | muted red, distinct from ghost's own failure red |
| `idle` | none |

- The progress line is as long as `progress` says, or faint across the whole
  bar when it says nothing.
- `msg` replaces the title while the session is `blocked`: what it waits for is
  what to read. Bidi overrides and invisible formatting are stripped first.
- ghost's own `Notice` still outranks all of this. A window that failed to do
  something says so first.
- A tint arriving or leaving eases over 100 ms rather than landing in one
  frame. That is true of every colour change in a window's frames (titlebar,
  card headers, focus), done as one pass over the scene (`ghost-ui-core`'s
  `fade`).

**Attention button, the window's other sessions.** In the single view, a pill
at the trailing end of the bar (inside the Details pill, when there is one)
counts the other sessions of the window's group with something to say, coloured
by the most urgent of them.

- It lists every one `blocked` or `idle`, and every one `done` or `error` the
  user has not seen; a `working` session is doing what it should.
- Clicking it unfolds a list over the top-right of the terminal, most urgent
  first: "build — needs permission", and the program's message under it.
- The list is modal like a menu: Esc or a click elsewhere folds it; a click on
  a row slides that session in front, as Ctrl-Tab does.
- A background report repaints the window only when it changes the button.
- Not yet: how long ago each status changed (the window's emulators have no
  clock; the host's `since` would serve once the window reads it).

**Fleet tiles.** Each card's header row is tinted like the titlebar, with
the same colours and rules: violet working (with the progress line along its
foot), amber waiting (and the program's message in the header), green or
muted red for a result not yet seen; idle, seen, exited and host-away cards
stay plain. A card with a live mirror reads its own emulator, so it changes
the moment the bytes arrive; one without reads the host's headline from the
listing and `StatusChanged`. "Seen" is the window's own account for the
sessions it drives (the host counts them all seen, the window being attached
to them) and the host's flag for the rest. The bell and activity marks stay in
the corner.

## Testing

Test-first, outer behaviour first:

- **`ghost-term` units:**
  - parsing, validation and every limit;
  - replacement and `clear` semantics, including descendants;
  - `app` inheritance;
  - OSC 133 `A` clearing, `RIS` and `DECSTR`, the record cap;
  - the dump replaying the records exactly.
- **`ghost-vt`:**
  - the query reply, and its ordering ahead of DA, attached and detached;
  - E2E: a detached session's `ghost ls --json` carries its headline;
  - a subscriber receives `StatusChanged`;
  - a reattach restores the records.
- **Shell (`ghost-ui/tests/shell.rs`):**
  - a session reporting `blocked` reaches the bar's status, and `done` lasts
    until a key press;
  - a background session reporting `blocked` before any window looked is
    counted on the button, named in the list, and one click away.
- **Remote (`shell_remote.rs`):** still to write — a remote background
  session's status reaching the window.

## What Claude Code sends

Measured 2026-10-20 against Claude Code 2.1.304, read back from a ghost
recording.

- It queries (`OSC 7501 ; ?`) at startup, and again when resumed.
- **Root record only.** It never sends `id`, `title` or `progress`, and every
  report carries `app=claude-code`.

| Moment | Report |
| --- | --- |
| Started, waiting for a prompt | `state=idle` |
| A turn starts | `state=working` |
| Each tool call | `state=working` + `msg`, the call's description ("Exporting this session and listing OSC 7501 reports") |
| An AskUserQuestion | `state=blocked:kind=question` + `msg`, e.g. "answer: <question> (<option> · <option>)" |
| A permission prompt | `state=blocked:kind=permission` + `msg`, e.g. "approve Bash: rm …" |
| Answered or approved | `state=working` |
| The turn ends | `state=done`, held until the next turn starts |
| Ctrl-C | `state=idle` |
| `/exit` | `state=clear`, sent three times |

So the root record alone carries everything the chrome shows. Claude Code
cleans up after itself on exit, so no stale record is left behind.

## Decisions

- **Scope of the affordance:** the window's own group. It lists the members of
  this window's group that are not its foreground session. Sessions elsewhere
  are their own windows' business, for now.
- **Leftovers when a program exits:** none from Claude Code, which clears its
  record on `/exit`. ghost follows the spec's lifetime rules as written.
- **Tints:** as in the table above, to be revisited after dogfooding.
- **Desktop notifications** (the spec allows them, rate-limited) are left for
  later.
