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
- `ClientMsg::Attach { cols, rows, client }` — attach as the display client,
  naming it, in one message (`PROTO_ATTACH = 8`). The identity names the client
  kind and its machine: `ghost-ui@<machine>:<group-id>` for a window,
  `cli@<machine>` for a terminal attach. The host echoes it in
  `AttachInfo.client` and writes it into the `attached` marker.
- `ClientMsg::Hello { client }` — the same identity, sent after a `Resize` by
  clients (or to hosts) predating `Attach`. Between the two messages the host
  holds a display client with no identity, so subscribers can see a transient
  `Attached(None)`.
- `ClientMsg::SetGroup(Option<String>)` — put the session in a group, or take
  it out (`PROTO_ATTACH`). The host keeps the group id in `meta` (so listings
  report `SessionInfo.group`) and in the durable descriptor (so the membership
  outlives the host, and a relaunched session comes back in its group). A
  watcher's is ignored.
- `SessionEvent`: `Bell`, `TitleChanged`, `Attached(AttachInfo)`, `Detached`,
  `Activity`, `Renamed`, `Resized { cols, rows }`, `GroupChanged`.

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
  clients below `PROTO_SUBSCRIBE`. The `attached` marker's contents are the
  holder's identity, so a listing reports `SessionInfo.holder` as well as
  `attached`.
- **Flow control.** `Activity` is sent only to a subscriber with nothing queued.
  An observer's output stops being queued past `OBSERVER_MAX_PENDING` (256 KiB);
  the observer is marked lagged, and once its queue drains it is re-seeded with
  `Resized` plus a resync in the same flush turn.
- **Liveness.** Host death is EOF on the subscription.

## Frontend behaviour

- The App keeps one `Subscriber` per local session in `subs` for state pushes and
  fans each push to every window as `UiEvent::SessionPush`.
- Fleet previews of sessions this process does not drive use `Observe`; the
  observer's output feeds the one shared emulator for that session. Windows do
  not request observers: each reports the sessions it previews, and after every
  batch of commands the App reconciles one source per session (a client if a
  window drives it, else an observer if a window previews it, else nothing). A
  failed observer is retried at the next listing.
- **Set changes, local and remote alike,** come from one watch
  (`watch::watch_set` + `SetChanges::stream`): the runtime tree recursively plus
  the descriptors dir, `Access` events ignored (a listing's own reads would
  otherwise re-trigger it), registered before the first listing is taken, and
  emitting only on change plus a 30 s heartbeat. Locally the App runs it on a
  thread (`LocalFeed`) posting `UserEvent::LocalSessions`; for each connected
  host it reads the same stream as JSON lines from `ghost __watch` over ssh. A
  host that stops answering is reported as unreachable, never as an empty
  listing, so its members wait for it instead of reading as exited.
- Each listing arriving sends `UiEvent::SessionsChanged`, and the window answers
  with `Cmd::ListSessions`, served from the latest listings. A subscription
  ending also triggers a re-list. A slow reconcile floor (`REFRESH_MS`, at least
  5 s) is the backstop.
- **Claiming and pruning names** are serialized by `<runtime>/.lock`: a spawn
  makes the session directory and takes its lock under it, and a prune re-judges
  a directory under it before removing it.

## Open

- Remote sessions get no state subscription, and the observer pump forwards only
  `Resized`, so a remote session's live bell never reaches the fleet. Its holder
  is in the listing (`holder`), which the fleet does not read yet.
- Bell count and per-client bell preferences are frontend concerns layered on
  `SessionEvent::Bell`.
