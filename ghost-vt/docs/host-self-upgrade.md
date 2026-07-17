# Host self-upgrade via in-place re-exec (Phase 2 design)

**Status:** design memo, pre-build. Supersedes the cruder Phase 1.5 restart
(`ghost __restart`, `session::restart_session`) for the case where we must keep
the *running program*, not just the screen.

## Goal

Upgrade a **running** session host to a newer `ghost` binary **without killing
its child**. The child process, its PTY, the control socket, and the liveness
lock all survive; only the host's own code image is replaced. This is what lets a
remote host that predates a staged binary adopt the new protocol level while a
long-lived program (an editor, a build, a REPL) keeps running underneath.

Contrast with the shipped alternatives:

- **Phase 1.5 restart** (`__restart`): graceful SIGTERM → respawn seeded from the
  recording. The child dies; only the screen survives. Cruder, already shipped.
- **This (Phase 2)**: the child lives; the host re-execs in place around it.

**Going-forward only.** A host must already run *this* code to self-upgrade —
it cannot rescue hosts that predate the feature. That is acceptable: once a host
is on a self-upgrade-capable build, every future upgrade is live.

## What already exists — the shape we extend

The initial spawn *already* re-execs the host (`server.rs`):

- `spawn` (server.rs:190) binds the `UnixListener`, takes the session **flock**
  (the liveness source of truth — `session::list` prunes exactly when it frees),
  writes the `proto` marker, clears `FD_CLOEXEC` on the listener + lock
  (`clear_cloexec`, server.rs:328) so they survive `execv`, serializes
  `HostArgs` to a hex blob on argv (`encode_host_args`, server.rs:344), and
  `daemonize_and_exec`s (server.rs:1616).
- `run_host_if_invoked` / `run_host` (server.rs:285/297) reclaim the listener fd,
  lock fd, and blob from argv and run `host_main`.

So *"swap the host process, keep the socket + lock across an exec"* is already the
mechanism — **for spawn**. Two things the spawn re-exec does NOT carry, which a
self-upgrade must:

1. **The PTY master + child.** `host_main` opens the PTY (`open()`, server.rs:405)
   and forks the child (`spawn_child`, server.rs:1495) *after* the re-exec. For a
   self-upgrade the child is already running, so the **PTY master fd** and the
   **child pid** must cross the exec.
2. **In-place, not daemonized.** `daemonize_and_exec` double-forks (detach from
   the tty). A self-upgrade host is *already* a daemon — it must `execv` **in
   place** (same pid, same session), so the flock, the pidfile
   (`paths::pid_path`, server.rs:400), and the PTY stay valid and no new process
   appears. Add an `exec_in_place(exe, argv)` that just `execv`s (no fork).

## What must cross the exec

| Item | How it crosses | Notes |
|---|---|---|
| Listener fd | argv (CLOEXEC cleared) | as today |
| Lock fd | argv (CLOEXEC cleared) | as today — keeps the session "live" the whole time; `session::list` never sees it gone |
| **PTY master fd** | argv (CLOEXEC cleared) | new — the child stays attached to it |
| **Deferred `pts` fd** | argv (CLOEXEC cleared) | only when `start_on_attach` and the child hasn't spawned yet (server.rs:462): the host holds the slave open so the master never EOFs; drop it and the new host's first master read EOFs. Carry it, or spawn the child before upgrading. |
| **Child pid** | handoff blob | new — the new host adopts it by pid, NOT a `std::process::Child` (that handle does not survive exec) |
| **Checkpoint snapshot** | **memfd**, fd on argv | new — the emulator state (see below). NOT argv: a full dump can exceed Linux `MAX_ARG_STRLEN` (128 KiB). |
| meta / descriptor / policy / connection | **re-read from disk** | already durable; `inherited_policy` (server.rs:437) is the existing shape. Don't serialize them. |

**Blob = a second wire protocol.** Keep it minimal and **versioned**: child pid,
session name, the memfd's role, the carried fd numbers. Everything else is
re-derived from disk. A version tag on the blob lets a future new host reject an
older/newer handoff cleanly instead of misparsing.

## Emulator state: checkpoint, not replay

Do **not** "replay the recording." Recording is optional (`--no-record`) and
replay is unbounded. Instead **checkpoint at quiesce**, reusing the recreate
machinery:

- `screen.dump_without_images()` + `screen.graphics_images()` →
  `checkpoint_with_images` (server.rs:525/847) is exactly how a seeded spawn and
  the cadence checkpoint already serialize state.
- The new host rebuilds via `Screen::from_recording` + reflow — the same path
  `seed_from` takes (server.rs:483). This leans on the **dump-representable
  invariant** (a plain-ANSI dump must express the live state; a stale primary
  behind the alt screen was the classic violation — fixed in `f8e20b6a`, guarded
  by `prop_dump`).
- Write the checkpoint into a **memfd** (not the recording file, not argv). The
  new host reads it once at startup, then closes it.

**Recording file continuity.** If recording is on, don't truncate — **finalize
the in-flight brotli frame, then reopen-append** so `ghost search` history
survives the upgrade. Truncating would drop the pre-upgrade history.

## The quiesce gate — stricter than `has_pending()`

The upgrade may only fire at a clean boundary, or the new parser inherits garbage:

- `screen.has_pending()` (screen.rs:353) covers only an **incomplete trailing
  UTF-8** sequence.
- It does **not** cover a **mid-escape parser state**. If the old parser consumed
  `ESC [` but not the final byte, those bytes are gone; the new host's parser
  starts in `State::Ground` and reads the continuation (`3 1 m`) as literal text.
- So the gate is: **VT parser in `State::Ground` AND `!has_pending()`**. The
  parser state (`ghost_term` `parser.rs` `State::Ground`) is **not currently
  exposed through `Screen`** — add a `Screen::at_boundary()` (or similar) that
  reports both.

**Bounded patience, then refuse.** A chatty child may never quiesce. Poll for a
boundary for a bounded window; if it never comes, **refuse** the upgrade
("host busy, try again") rather than forcing it. Also reset the `QueryScanner`
(and any partial-sequence scratch) only at a boundary.

## In-flight input and output

- **`pty_out`** (server.rs:573) is queued child-bound input not yet drained under
  `POLLOUT`. Carry it (in the memfd or blob) and re-queue it, or drain it fully
  before the exec. Losing it drops keystrokes the child hasn't read.
- The recorder's in-flight frame must be finalized before exec (see above).

## Clients: drop them, lean on flock continuity

In-flight display/observe clients are **dropped** across the upgrade (their fds
don't cross). This is safe because the **flock is held the whole time** — so
`session::list` never shows the session gone, and a fleet never marks the tile
dead.

The client side needs one addition: **"EOF but the lock is still held ⇒
re-attach"** rather than "EOF ⇒ session dead." The transport EOFs when the old
host execs (its socket-accept state resets); the client must distinguish an
upgrade (lock held) from a real death (lock free) and re-attach to the new host.
Optionally, an appended **level-gated `ServerMsg::Restarting`** (a new protocol
message, min-level gated like `PROTO_POLICY`) sent just before the exec makes the
client's re-attach deliberate rather than inferred from EOF.

The ~millisecond window where the host's signal handlers are reset (between exec
and the new host installing them) is acceptable; note it.

## Security — right-sized

A same-UID attacker already has `ClientMsg::Input` → arbitrary command execution
in the child. So **binary signature-checking is theater — skip it.** Instead:

- Accept an "upgrade to `<path>`" request **only from the resynced DISPLAY
  client** — mirror the `ClientMsg::Policy` guard (`!c.subscribed && !c.observing`,
  server.rs:1324). An observer/subscriber can't trigger an upgrade.
- Validate the target: a **regular file, our UID, not group/world-writable**.
- **Pre-exec `__probe`** the target requiring the marker AND `proto >= our
  CURRENT level` — **no downgrades**. This doubles as "will it even run here",
  turning an exec failure into a cleanly-refused command instead of a dead host.
- Do **not** restrict the path to the staged dir — dev builds and
  `GHOST_REMOTE_GHOST` legitimately point elsewhere.

## Rough phasing

1. Expose `Screen::at_boundary()` (ground + no pending UTF-8). Cheap, testable.
2. `exec_in_place` + carrying the PTY master (and deferred `pts`) fd across a
   re-exec; the new host adopts the child by pid (pidfd on Linux, `waitpid` on
   macOS; avoid double-reap). Prove a bare re-exec keeps a child alive.
3. Checkpoint → memfd → new host rebuilds the screen; recording finalize+append.
4. The upgrade `ClientMsg` (guarded), pre-exec probe/validation, refuse-if-busy.
5. Client re-attach on lock-held EOF (+ optional `ServerMsg::Restarting`).

Each step is independently testable against the real binary (an E2E that upgrades
a host to *itself* and asserts the child's pre-upgrade state survives).

## Open questions

- macOS child adoption without pidfd — `waitpid` on a non-child? The adopted
  child *is* our child across the exec (same process, same pid keeps the
  parent-child link), so `waitpid` should still work; verify.
- Does re-queuing `pty_out` after the child may have advanced its read position
  ever duplicate input? (It shouldn't — those bytes were never delivered.)
- Interaction with the deferred-child path when an upgrade arrives *before* the
  first attach: simplest is to refuse-until-attached, or carry the `pts`.
