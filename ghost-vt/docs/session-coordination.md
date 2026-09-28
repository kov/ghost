# Session coordination: per-session push

How the frontend and CLI learn which sessions exist and what state they are in,
and how the fleet gets live previews of sessions it does not drive.

## Design choice

Each session is its own `ghost __host` process with its own control socket, and
there is no central daemon. State is **pushed per session** over that socket.
Two alternatives were rejected:

- **Filesystem watching alone** (inotify/fsevents on the runtime dir) only works
  locally, behaves differently on macOS, and still signals state through marker
  files — a bool with no owner and no ordering. It survives only as the
  *set-change trigger* (below).
- **A central coordination daemon** would decouple discovery from the filesystem,
  but reintroduces a single point of failure, version skew and liveness traps
  that process-per-session avoids. If one is ever built, it should relay the
  same per-session events rather than invent new ones.

What push does *not* decouple is discovery of the session **set**: knowing which
sessions exist still starts from the runtime-dir layout (locally) or from
`ghost __watch` (remotely, and it reads the same layout on the far side).

## Protocol surface

All in `ghost-vt/src/protocol.rs`; variants are appended only (frozen-discriminant
tests pin the ordinals).

- `ClientMsg::Subscribe` — push me state for this session; I am not a display
  client. The host replies with `ServerMsg::Snapshot(SessionState)` and then
  `ServerMsg::Event(SessionEvent)` as state changes.
- `ClientMsg::Observe` — a subscription that also receives output: `Snapshot`,
  `Event(Resized)` with the real grid, a full resync, then live `Output`.
- `ClientMsg::Hello { client }` — an opaque identity the host echoes back in
  `AttachInfo.client` while that connection holds the display. The GUI sends
  `ghost-ui:<group-id>`.
- `SessionEvent`: `Bell`, `TitleChanged`, `Attached(AttachInfo)`, `Detached`,
  `Activity`, `Renamed`, `Resized { cols, rows }`.

A client gates each verb on the host's feature level from the session's `proto`
marker (`PROTO_SUBSCRIBE = 3`, `PROTO_OBSERVE = 4`); a host below it is polled
through the marker files instead.

A subscriber or observer only watches. The host ignores `Resize`, `Input` and
`Kill` from one, so it can never become the display client, resize the PTY, type
into the child or end the session.

## Host behaviour

- **Snapshot/diff.** The host keeps `last_state` even with no subscribers, and at
  the end of each loop turn diffs it against the current state and pushes the
  difference, so a late subscriber never replays history.
- **Bell.** `SessionEvent::Bell` fires even while a client is attached (the live
  bell). The `bell` marker file keeps its old meaning: set only while nobody is
  attached, cleared on attach.
- **Markers.** `attached` and `bell` are still written for listing and for
  clients below `PROTO_SUBSCRIBE`. `SessionInfo.attached` in a listing is a bool;
  *who* holds the display is only available through a subscription.
- **Flow control.** `Activity` is sent only to a subscriber with nothing queued.
  An observer's output stops being queued past `OBSERVER_MAX_PENDING` (256 KiB);
  the observer is marked lagged, and once its queue drains it is re-seeded with
  `Resized` plus a resync in the same flush turn.
- **Liveness.** Host death is EOF on the subscription.

## Frontend behaviour

- The App keeps one `Subscriber` per local session in `subs` for state pushes and
  fans each push to every window as `UiEvent::SessionPush`.
- Fleet previews of sessions this process does not drive use `Observe`; the
  observer's output feeds the one shared emulator for that session.
- **Set changes locally:** a `notify` watch on the runtime dir (ignoring `Access`
  events, which would otherwise re-trigger themselves) sets a flag. The next wake
  sends `UiEvent::SessionsChanged`, and the window answers with
  `Cmd::ListSessions`. A subscription ending also triggers a re-list. A slow
  reconcile floor (`REFRESH_MS`, at least 5 s) is the backstop.
- **Set changes remotely:** one `ghost __watch` stream per host, which registers
  its watch before taking the first listing and emits JSON lines only on change.

## Open

- Remote sessions get no state subscription, and the observer pump forwards only
  `Resized`, so a remote session's holder identity and live bell never reach the
  fleet; it sees only the listing's `attached` bool.
- Bell count and per-client bell preferences are frontend concerns layered on
  `SessionEvent::Bell`.
