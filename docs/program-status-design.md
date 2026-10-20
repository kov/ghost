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

## Background sessions: listing, push, remote

A session that no window has in the foreground still needs to reach the
affordance. Status rides the paths the title and the bell already use.

- **Host:**
  - At the end of each turn, if the headline changed, the host writes it into
    the session's `meta`, in a new `status` field with `serde(default)`.
  - It pushes `SessionEvent::StatusChanged(Option<Headline>)` to subscribers.
    The variant is appended, and older subscribers skip the unknown frame.
  - A subscriber that starts while a status is set gets the event right after
    its `Snapshot`, which therefore stays unchanged on the wire.
- **Listing:** `SessionInfo.status` comes from `meta`. So `ghost ls --json`,
  and therefore a remote host's `__watch` feed, carry it with no new remote
  machinery.
- **Seen:**
  - A `done` or `error` headline in a background session counts as *unseen*
    until a display client attaches. Switching to the session is seeing it,
    exactly as with the bell marker.
  - The host keeps the flag with the record, and it rides along in the headline
    as `seen: bool`.
  - In the foreground, a `done` tint fades once the user presses a key in the
    window. That is UI-local and does not touch the record.

## What the user sees

**Titlebar, foreground session.** The headline tints the bar.

| Headline | Tint |
| --- | --- |
| `working` | none |
| `blocked` | warm amber, strongest for `permission` and `question` |
| `done` | soft green, until the first keypress |
| `error` | muted red, distinct from ghost's own error notice |
| `idle` | none |

- `working` shows a small progress indicator by the title instead of a tint: a
  determinate bar with `progress`, an indeterminate one otherwise.
- `msg`, when present, replaces the title while the session is `blocked`. That
  is when it is worth reading.
- ghost's own `Notice` still outranks all of this. A window that failed to do
  something says so first.

**Header-bar affordance, background sessions.** A compact button sits at the
trailing end of the bar, next to the window buttons. It covers the members of
this window's group other than its foreground session.

- It is drawn only when some background session has something to say.
- It carries a count, coloured by the most urgent headline among them.
- Clicking it unfolds a list with one row per session:
  - the session's label and host;
  - the state, with `kind` spelled out: "needs permission", "has a question";
  - `msg`, and how long ago it changed.
- Clicking a row switches to that session, exactly as clicking its fleet tile
  does: a dive into the window that holds it, or a take-over prompt.
- The list holds `blocked` headlines always, `done` and `error` while unseen,
  and `idle`.

**Fleet tiles.** Each tile gets the headline as a small badge beside the
existing bell badge.

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
  - a session reporting `blocked` tints the drawn titlebar;
  - a background session reporting `blocked` puts a row in the affordance;
  - clicking the row brings that session to the foreground.
- **Remote (`shell_remote.rs`):** a remote background session's status reaches
  the affordance through the watch feed.

## Before building: what Claude Code really sends

Claude Code's docs say only that terminals implementing the protocol "can show
whether Claude Code is working, waiting on you, or done". They do not say which
states it emits, when it emits them, or whether it clears its records when it
exits.

So the first step is the query reply alone. Then run Claude Code in a ghost
session and read the recording back, which captures the output stream, to
record what it actually sends:

- per turn;
- at a permission prompt;
- at an AskUserQuestion;
- on Ctrl-C;
- on `/exit`.

That settles the open questions below, and the record's lifetime rules are
adjusted only if reality forces it.

## Decisions

- **Scope of the affordance:** the window's own group. It lists the members of
  this window's group that are not its foreground session. Sessions elsewhere
  are their own windows' business, for now.
- **Leftovers when a program exits without clearing its records:**
  - The shell prompt clears `working` and `blocked`, but only with OSC 133
    shell integration. Without it, a stale `working` record could outlive
    Claude Code.
  - If the capture shows Claude Code leaves records behind, records are also
    dropped when the program leaves the alternate screen, which Claude Code's
    fullscreen mode uses. This is a deliberate deviation from the spec, scoped
    to that case.
- **Tints:** as in the table above, to be revisited after dogfooding.
- **Desktop notifications** (the spec allows them, rate-limited) are left for
  later.
