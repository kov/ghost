# Host self-upgrade via in-place re-exec (Phase 2 design)

**Status:** design memo, reviewed (Opus stood in for Fable: *SOUND-WITH-FIXES* —
the core POSIX bet holds; the P0/P1/P2 corrections below are folded in). Step 1
(the quiesce-boundary primitive) is built; the rest is pre-build. Supersedes the
cruder Phase 1.5 restart (`ghost __restart`, `session::restart_session`) for the
case where we must keep the *running program*, not just the screen.

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

   **`execv` failure must RESUME the loop, not exit (P0).** `execv` only returns
   on failure, and `daemonize_and_exec` handles that with `_exit(127)`
   (server.rs:1646) — correct *there* (a throwaway forked child), catastrophic
   *here*: this process **is** the live host holding the PTY master, the child,
   and the flock. An `_exit` orphans the child, closes the master (child gets
   SIGHUP and dies), and frees the lock — killing exactly what the feature
   protects. The pre-probe (§Security) shrinks the odds but can't rule out
   `E2BIG` (fds + blob on argv), `ETXTBSY` (target being rewritten), `ENOMEM`, or
   a probe→exec TOCTOU. So `exec_in_place` returns `Err`, and the caller
   **refuses the upgrade and resumes `host_main`**. Consequence: every pre-exec
   step must be **non-destructive and reversible** — flushing the recorder and
   draining `pty_out` are; the checkpoint read is; keep it that way so a failed
   exec leaves a fully-working host.

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

**Deferred `pts` needs no carrying (resolved).** The security gate accepts an
upgrade only from a *resynced display client* — and a resynced display client
means the deferred child has already spawned (server.rs:1014) and `pts` is
already consumed. So the carry-`pts` case cannot co-occur with a legitimate
upgrade: a defensive **"refuse if the child hasn't spawned yet"** is enough;
carrying `pts` is dead weight.

**memfd vs tempfile.** Both work; an **unlinked tempfile** the new host opens by
path is arguably simpler (no CLOEXEC-clear + fd-on-argv bookkeeping) and
equivalent. No fd-number collision either way: the carried fds stay open across
exec, so the kernel won't reissue their numbers to the new host's own `open`s —
the guarantee `spawn` already relies on.

## Failure and signal safety (P0)

The window between `execv` and the new host installing its handlers is **not
cosmetic**. `signals::make` (signals.rs) is the self-pipe + `sigaction` trick and
does **not** block signals (`SigSet::empty()`, no `sigprocmask`). Across `execv`,
dispositions reset to `SIG_DFL` and the signals stay unmasked — so a `SIGTERM`
(a racing `ghost kill`) or `SIGINT` arriving in that window is delivered at
**default disposition = terminate**, orphaning the child and bricking the
session.

**Required:** `sigprocmask`-**block SIGTERM/SIGINT before `execv`**. The signal
mask *is* preserved across exec, so they stay **pending** (not fired) until the
new host is ready. The new host must then **explicitly `sigprocmask`-unblock**
after reinstalling handlers — `signals::make` only sets a per-handler mask, never
the process mask, so it won't clear this on its own. A blocked signal that was
pending fires the instant it's unblocked, now at the new host's handler.

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

**Recording file continuity (P1).** If recording is on, don't truncate —
finalize the in-flight brotli frame, then reopen-append so `ghost search` history
survives. But `Recorder::new`/`FileRecorder::create` unconditionally write
`magic + version + Header` and `File::create`-truncate (record.rs:235/357) — reusing
either injects a **second header mid-file**, which `read_bytes` parses as a bogus
frame and the torn-frame tolerance then silently discards **everything after the
upgrade**. So a **new header-less `open_append`** is required, mirroring the
compaction reopen (record.rs:312): (a) `OpenOptions::append`, write no header;
(b) rehydrate `written_hashes` via `stored_image_hashes` (record.rs:310) or a
returning image becomes a dangling reference; (c) carry the `t_ms` time base
forward, or `Instant::now()` resets timestamps toward zero and breaks the
monotonicity the format assumes (`timestamps_are_monotonic`).

## The quiesce gate — stricter than `has_pending()`

The upgrade may only fire at a clean boundary, or the new parser inherits garbage:

- `screen.has_pending()` (screen.rs:353) covers only an **incomplete trailing
  UTF-8** sequence.
- It does **not** cover a **mid-escape parser state**. If the old parser consumed
  `ESC [` but not the final byte, those bytes are gone; the new host's parser
  starts in `State::Ground` and reads the continuation (`3 1 m`) as literal text.
- A **chunked kitty-graphics transfer** also straddles the boundary and is NOT
  caught by the above: between chunks the main parser is back at `State::Ground`
  with no pending UTF-8 (each chunk is a complete APC), while a half-assembled
  image is buffered. Only the *completed* image is checkpointed, so a handoff
  mid-transfer silently drops it. The gate must also require **no in-flight
  graphics chunk**.
- So the gate is: **parser in `State::Ground` AND `!has_pending()` AND not
  graphics-chunking**. **SHIPPED** as `Screen::at_boundary()`
  (`Vt::parser_at_ground` + `Vt::graphics_chunking`), commits `5474209` +
  `ea674a2`.
- **Confirmed NOT gaps** (checked against the code): synchronized-output (mode
  2026) is a mode bit applied immediately and re-emitted on dump (a
  frontend-only present-hold, not a parser straddle); OSC/DCS/APC-mid are all
  non-`Ground` states; the `QueryScanner` is fed byte-identical output and only
  diverges mid-escape, which `Ground` already excludes.

**Bounded patience, then refuse.** A chatty child may never quiesce. Poll for a
boundary for a bounded window; if it never comes, **refuse** the upgrade
("host busy, try again") rather than forcing it. Also reset the `QueryScanner`
(and any partial-sequence scratch) only at a boundary.

## In-flight input and output

- **`pty_out`** (server.rs:573) is queued child-bound input not yet drained under
  `POLLOUT`. Carry it (in the memfd or blob) and re-queue it, or drain it fully
  before the exec. Losing it drops keystrokes the child hasn't read.
- The recorder's in-flight frame must be finalized before exec (see above).

## Clients: drop them, lean on continuity (P1 — corrected)

In-flight display/observe clients are **dropped** across the upgrade (their fds
don't cross). This is safe because the **flock is held the whole time** (so
`session::list` never shows the session gone) *and* the **listener fd crosses the
exec** (CLOEXEC cleared) — so the socket path stays bound throughout. A
reconnecting client's `connect()` is queued by the kernel and accepted by the new
host, never `ECONNREFUSED`, across the whole window.

Re-attach splits by transport — **the first draft had this backwards**:

- **Remote (the primary case): lean on the EXISTING reconnect, no lock check.** A
  remote client is on another machine and *cannot* flock the remote lock file, so
  a lock check is impossible over SSH anyway. It isn't needed: `Session::pump`
  already sets `disconnected` on a bare EOF (client.rs:277) precisely so the GUI
  reconnects to a session that may still live on the far side. The remote upgrade
  rides that existing reconnect-probe path unchanged.
- **Local CLI attach: teach it to reconnect.** `run_attach` (client.rs:571) has
  **no** reconnect — it prints "session ended" and exits on any EOF, so a plain
  `ghost attach` would drop on every upgrade. Here (and only here, locally) the
  "EOF but the lock is still held ⇒ re-attach, else it really ended" distinction
  belongs, plus an actual reconnect loop.

Optionally, an appended **level-gated `ServerMsg::Restarting`** (min-level gated
like `PROTO_POLICY`) sent just before the exec makes re-attach deliberate rather
than inferred from EOF — a refinement, not required, since the queued-connect +
`disconnected` path already covers it.

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

## Adopt-by-pid is a refactor, not a one-liner (P2 scope)

The child is currently `Option<std::process::Child>` throughout — `child_exited`,
`kill_child`, `child_cwd` (server.rs:1460/1562/1529) call `.wait()`/`.kill()`.
`std::process::Child` has no from-pid constructor and does not survive exec, so an
adopted child must be reaped via raw `waitpid`/pidfd behind a **new child
abstraction touching every child site**. This is *sound* — the child genuinely
stays our child across an in-place `execv` (same pid, unchanged PPID), which is
why `waitpid` still works on macOS too — but budget it as real scope.

## Rough phasing

1. **DONE** — `Screen::at_boundary()` (parser ground + no pending UTF-8 + no
   graphics chunk). Commits `5474209`, `ea674a2`.
2. A from-pid child abstraction (raw `waitpid`/pidfd) replacing
   `Option<std::process::Child>` at every site. Pure refactor, no behavior change.
3. `exec_in_place` (returns `Err` on failure → **resume the loop**) + carrying the
   PTY master fd; **`sigprocmask`-block SIGTERM/SIGINT around the exec**; the new
   host adopts the child by pid and unblocks. Prove a bare re-exec keeps a child
   alive and a racing SIGTERM doesn't kill it.
4. Checkpoint → memfd (or unlinked tempfile) → new host rebuilds the screen;
   recording header-less `open_append` (rehydrate hashes, carry `t_ms`).
5. The upgrade `ClientMsg` (display-client-guarded), pre-exec probe/validation,
   refuse-if-busy / refuse-if-no-child-yet, drain `pty_out`.
6. Local `run_attach` reconnect (lock-held ⇒ re-attach); the remote path already
   rides the existing `disconnected` reconnect. Optional `ServerMsg::Restarting`.

Each step is independently testable against the real binary — the capstone E2E
upgrades a host to *itself* and asserts the child's pre-upgrade state (a marker on
screen, the same child pid) survives, and that a SIGTERM racing the upgrade does
not kill it.

## Open questions (mostly resolved by review)

- macOS child adoption: **resolved** — the child stays our child across in-place
  `execv` (same pid, unchanged PPID), so `waitpid` works; no pidfd needed.
- Deferred-child-before-first-attach: **resolved** — the display-client-only gate
  means the child has already spawned, so refuse-if-no-child-yet suffices; don't
  carry `pts`.
- Does re-queuing `pty_out` ever duplicate input after the child advanced its read
  position? (Shouldn't — those bytes were never delivered; confirm in step 5.)
