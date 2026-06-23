# Session coordination: replacing the 500 ms poll with per-session push

**Status:** Decided (design) · **Date:** 2026-06-23 · **Scope:** how the
frontend/CLI learn about session existence and state, plus the live-bell and
observer-attach features that ride the same seam · **Work status:** NOT STARTED
— research/design only.

## Problem

The fleet overview (and anything that lists sessions) learns about sessions by
**polling the filesystem**:

- `Cmd::ListSessions` → `ghost_vt::session::list()` → `list_in()` reads
  `paths::runtime_dir()` and, per entry, stats marker files
  (`attached: path.join("attached").exists()`, `bell: …` in `session.rs`).
- The fleet drives this on a timer (`frontends/ghost-ui/core/src/fleet.rs`,
  `REFRESH_MS = 500`).

Two problems:

1. **Latency and waste.** Up to 500 ms to notice a new session, a title change,
   an attach/detach, or a bell — and a full directory re-stat every tick even
   when nothing changed.
2. **Layout coupling.** Hosts, clients, and the CLI all hard-code the on-disk
   layout `runtime_dir()/<name>/{sock,pid,lock,meta,attached,bell}`
   (`ghost-vt/src/paths.rs`). State is signalled by the *presence of marker
   files*, which is lossy (a bool — no count, no ordering, no identity) and
   **cannot cross the deferred remote transport**: a remote host shares no
   filesystem with the client.

## Options considered

A research workflow (2026-06-23) scored three approaches (judge panel:
C = 22, D = 22, B = 18, A = 16).

### A — inotify / fsevents on the runtime dir
Watch `runtime_dir()` and reconcile on notify instead of on a timer.
- **Pro:** small; removes the steady-state poll.
- **Con:** still layout-coupled (it watches files), **local-only**, and
  macOS-divergent (fsevents semantics differ). It's a *trigger* optimization,
  not a decoupling.
- **Verdict:** demoted to *just* the set-change trigger — one `ListSessions`
  fired from a `notify` watch, with a slow reconcile floor as a backstop.

### B — central coordination daemon
A single long-lived process that owns the registry and relays events.
- **Pro:** fully decouples discovery from the filesystem; natural home for
  remote-fleet relaying.
- **Con:** reintroduces a single point of failure, version skew, and liveness
  traps that the process-per-session design deliberately avoids.
- **Verdict:** deferred to the remote-fleet end-state. When built, it relays the
  *same* per-session events option C defines.

### C — daemonless per-session push  ← chosen
Each host serves a new **`Subscribe`** verb on its **existing per-session
control socket**; subscribers are pushed typed `ServerMsg` events, and host
death is observed as socket EOF.
- **Pro:** no new process, no new codec — pure new `ClientMsg`/`ServerMsg`
  variants over the existing `Conn`/`Transport` framing. The frontend already
  opens that socket for a live tile, so the seam exists. The same mechanism
  serves live bell and observer-attach (below).
- **Con (honest residual):** it decouples session **state**, not session-**set**
  discovery — knowing *which* sessions exist still begins with the directory
  listing. Full discovery decoupling is B's job, later.

## Chosen design (C)

### Protocol surface
Today (`ghost-vt/src/protocol.rs`): postcard-serialized, length-prefixed frames
(`FrameReader`).
- `ClientMsg`: `Input`, `Resize`, `Detach`, `Kill`, `Rename(String)`, `Repaint`.
- `ServerMsg`: `Output(Vec<u8>)`, `Exited(i32)`, `RenameResult { ok, message }`.

Add:
- `ClientMsg::Subscribe` — "push me state events for this session; I am **not** a
  display client." A subscriber never sends `Resize`, so it never steals the
  display or resizes the PTY (see observer-attach).
- `ServerMsg::Snapshot(SessionState)` — sent once on subscribe so the client
  starts consistent before any delta.
- `ServerMsg::Event(SessionEvent)` — pushed thereafter:

```rust
enum SessionEvent {
    Bell,
    TitleChanged(String),
    Attached(AttachInfo),   // richer than today's bare `attached` bool
    Detached,
    Activity,               // output produced — drives the fleet activity badge
    Renamed(String),
}
```

`AttachInfo` carries **window identity**, so the fleet can distinguish
*ThisWindow* from *Elsewhere* with fidelity (exactly what multi-window needs),
replacing the lossy `attached` marker.

### Liveness
Host death = socket EOF on the subscription — no heartbeat, no marker staleness.
This is how a display client already learns the host is gone.

### Markers stay during migration
The `attached`/`bell` marker files are **dual-written** so the polling path keeps
working until every consumer is switched over. The `notify` watch on
`runtime_dir()` becomes the set-change trigger (replacing the steady 500 ms
timer), with a slow reconcile floor as a backstop.

## What rides this seam

- **Live bell** *(folded into this scope, 2026-06-23)*. The fleet badge for a
  *detached* session that rang **already works**: `ghost-term` counts BEL → the
  host writes the `bell` marker while no client is attached → `SessionInfo.bell`
  → `BadgeKind::Bell`. What's missing is the **focused/attached** real-time
  reaction (flash / OS urgency), and that is precisely `SessionEvent::Bell`. So
  live bell is not a standalone feature — it's the first consumer of this
  redesign.
- **Observer-attach (live foreign previews).** The fleet wants live previews of
  sessions owned by another window without stealing them. A subscriber that also
  receives output but never sends `Resize` is a read-only OUTPUT observer — a
  small extension on the same seam (the host already treats "sends `Resize`" as
  the thing that makes a client the display client).
- **Multi-window fidelity.** `AttachInfo` with window identity gives the fleet
  accurate ThisWindow / Elsewhere / Detached grouping across windows.

## Migration (7 test-first steps)

1. **Protocol surface** — add `Subscribe`, `Snapshot`, `Event`, `SessionEvent`,
   `AttachInfo` (+ frame round-trip tests).
2. **Host: subscribe** — handle `Subscribe` → reply `Snapshot`, register the
   subscriber.
3. **Host: deltas** — emit `Event`s, **dual-written** with the existing marker
   files.
4. **Frontend: consume** — the fleet reacts to `Event`s instead of polling for
   state.
5. **Trigger** — replace the 500 ms timer with a `notify` set-change watch + slow
   reconcile floor.
6. **Death = EOF** — subscriptions clean up on host exit.
7. **Coalescing + flow control** — don't flood a slow subscriber with
   `Activity`/`Output`.

## Open / deferred

- Session-**set** discovery stays layout-coupled (C decouples state only); full
  decoupling is the central daemon's job (B), later, for the remote fleet.
- Audio bell, bell count/coalescing semantics, and per-client bell preferences
  are frontend concerns layered on top of `SessionEvent::Bell`.

---

*Provenance: this consolidates the 2026-06-23 research-workflow verdict (formerly
only in agent memory / the workflow transcript). The companion frontend backlog
lives in the foundation-parity notes; window chrome is in
`frontends/ghost-ui/docs/window-decorations.md`.*
