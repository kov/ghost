# Predictive local echo

Design for mosh-style predictive echo in ghost: typing into a remote session
shows up immediately, and the host's real output confirms it or replaces it.
Written 2026-10-20. The approach is mosh's, with only the changes our wire
needs. Mosh references are to `src/frontend/terminaloverlay.cc` (the
prediction engine), `src/statesync/completeterminal.cc` (echo ack) and
`src/network/` (RTT) in mobile-shell/mosh.

## Goal

1. On a remote-host session over a high-latency link, once the context has
   proven it echoes, a printable key, backspace, or left/right arrow is drawn
   locally on the next frame. It does not wait a round trip. As in mosh, the
   first key of a new epoch (after attach, a reset, Enter or any control key)
   still waits for its echo; see [Epochs](#epochs).
2. A wrong prediction (a password prompt, a TUI that redraws, a shell that
   completes) is removed as soon as the host's output shows it was wrong. Wrong
   predictions are never left on screen.
3. Local sessions, and remote sessions on a fast link, look exactly like today:
   prediction is never visible below mosh's RTT threshold.

Out of scope here: connection survival (keeping the ssh channel alive across
network changes) is a separate design. The one requirement it places on this
design is listed under [Reconnects](#reconnects).

## Scope: remote ghost hosts only

Prediction runs only when the session is reached through a remote ghost host
(`ConnectionSpec` remote-host transport). Echo-ack has to come from the host
that writes into the PTY whose output we see. For a `SshChild` session the host
is local and ssh runs inside its PTY, so the latency sits beyond the host and a
host-side ack would be meaningless. `SshChild` sessions and local sessions never
create a prediction engine.

## Why this is smaller than mosh

Mosh had to build a state-synchronisation protocol (SSP) so the client holds a
copy of the server's screen. Ghost already has one: every GUI session keeps a
full emulator replica, fed by `ServerMsg::Output` and re-seeded by checkpoint
resyncs. The prediction engine compares its guesses with that replica, as mosh
compares them with its framebuffer. It never writes to the replica: predictions
are an overlay applied when a frame is drawn.

## The mechanism (mosh, translated)

### Input numbering and echo ack

Mosh numbers each client→server state. The server records `(n, time written to
the PTY)` for every input it applies. On every tick it advances `echo_ack` to
the newest `n` that was written at least `ECHO_TIMEOUT` = **50 ms** ago, and
sends it along with the screen state. The 50 ms is how long the application
gets to echo the byte back. After that, the screen the client holds "should"
show the effect of input `n`.

Ghost translation:

- The display client numbers its input. `ClientMsg::InputSeq { seq: u64,
  bytes }` is a new appended variant; the old `Input` stays for older hosts and
  non-predicting clients.
- The host keeps `input_history: VecDeque<(seq, Instant)>`, stamped when the
  bytes are **handed to the PTY** (accepted by the drain, not when received), and
  applies mosh's `set_echo_ack` rule.
- When `echo_ack` advances, the host sends `ServerMsg::EchoAck(seq)` **on the
  same ordered stream as `Output`**. Ordering replaces mosh's "ack rides inside
  the state": when the client processes `EchoAck(n)`, its replica already holds
  every byte the host read from the PTY before deciding `n` was 50 ms old. That
  is the same guarantee mosh's `late_ack` gives.
- The host's poll timeout includes mosh's `wait_time`: the next moment an entry
  in `input_history` becomes 50 ms old. The ack must arrive even when the
  application prints nothing (a password prompt), because "no echo after
  50 ms" is exactly how a wrong prediction gets caught.
- Gated by a new `PROTO_PREDICT` level. Against an older host the client sends
  plain `Input` and never predicts. The remote host self-upgrade path covers
  most hosts.

Mosh also tracks `early_ack` (state received by the server), but its validity
checks ignore it (`__attribute__((unused))`), so we leave it out.

### RTT

Mosh's display triggers are expressed in `send_interval = ceil(SRTT / 2)`,
clamped to [20, 250] ms. SRTT/RTTVAR use the RFC 6298 estimator (α = 1/8,
β = 1/4) and start at SRTT = 1000 ms. So predictions are shown from the first
keystroke and turn off once the link proves fast.

Ghost has no RTT measurement today. We add `ClientMsg::Ping(u64)` /
`ServerMsg::Pong(u64)` at the same protocol level. The client pings on a
timer while attached, plus once right after attach, and feeds every Pong into
the same estimator.

The ping measures the whole path the user feels: GUI → ssh → `ghost __pipe` →
host and back, without the 50 ms echo delay.

### What gets predicted

Like mosh, the engine works on the **bytes the client sends**, run through a
small VT parser (`new_user_byte`). Predicting from the bytes rather than from key
events means kitty-keyboard encoding needs no special case: a printable key sent
as `CSI … u` parses as an unknown CSI and makes the epoch tentative, just as mosh
treats any escape sequence.

| Input | Prediction |
|---|---|
| Printable, width 1 | Cell at the predicted cursor; cells to the right shift one column (insert); cursor +1 |
| Printable in the last column | Same, plus the epoch turns tentative and the cursor wraps to the next row |
| DEL (0x7f) | Cursor −1; cells to the right shift left |
| `CSI C` / `CSI D` (also `ESC O C/D`, rewritten to CSI) | Cursor ±1 |
| CR | Epoch turns tentative; cursor goes to column 0 of the next row (on the last row, that row is predicted blank — no scroll prediction) |
| Anything else (control chars, other CSI, ESC, width ≠ 1) | Epoch turns tentative, nothing drawn |

Inserted characters take the renditions of the cell to their left (mosh's
heuristic). Cells that shift off the predicted edge are `unknown` and drawn
only as an underline when flagging is on.

### Epochs

Every unpredictable input (`become_tentative`) starts a new **prediction
epoch**. A prediction from epoch `e` is hidden until some prediction from epoch
≥ `e` has been confirmed correct. So after Enter or Ctrl-R, predictions only
show again once the new context has proven it echoes. This is what keeps
password prompts and full-screen apps from showing junk: the first keystroke is
predicted but hidden, the ack shows it was not echoed, and the epoch is
dropped. The engine starts with `prediction_epoch = 1` and `confirmed_epoch =
0`, so the first key after attach or a reset is hidden too. That is mosh's
behaviour, and it is how it feels: the first key after Enter takes one round
trip, the rest are immediate.

Known limitation: an application that sets kitty keyboard flags 8 or 16 (every
key sent as an escape code) gets no prediction at all, because every key parses
as an unknown CSI. Mosh degrades the same way for any non-plain encoding.

### Checking predictions (`cull`)

Run on every `EchoAck`, every replica update, and before each keystroke. For
each predicted cell whose `expiration_frame ≤ echo_ack`:

- **Correct** (replica cell content matches, and it is not just the original
  content): confirm its epoch; copy the real renditions onto the rest of the
  row's predictions; retire it.
- **Correct, no credit** (blank prediction, `unknown`, or it matches what was
  there before): retire it without confirming.
- **Incorrect**: if it was tentative, kill that epoch; otherwise reset the whole
  engine.
- **Pending** (not yet acked): leave it, and feed the glitch timer.

The cursor prediction is checked the same way. A wrong cursor position resets
the engine. So does a resize, and so does any resync that re-seeds the
replica (attach, `Repaint`, the reconnect across a host self-upgrade): the
overlay's snapshots of the old grid mean nothing against a rebuilt one.

### When predictions are visible, and underlined

Taken unchanged from mosh's `PredictionEngine` constants:

| Trigger | On | Off |
|---|---|---|
| Show (`srtt_trigger`) | `send_interval > 30 ms` (SRTT ≳ 60 ms) | `send_interval ≤ 20 ms` and nothing currently predicted |
| Underline (`flagging`) | `send_interval > 80 ms` | `send_interval ≤ 50 ms` |
| Glitch, show | a prediction pending ≥ 250 ms | 10 quick confirmations, ≥ 150 ms apart |
| Glitch, show + underline | a prediction pending ≥ 5 s | same repair |

Mosh's `display_preference` (`Adaptive` default, `Always`, `Never`) becomes
`[input] predictive_echo = "adaptive" | "always" | "never"` in `ui.toml`.
`Always` is what the E2E tests use, so they don't depend on the RTT
estimator's start-up. We don't adopt `Experimental` or `predict_overwrite`.

## Where it lives in ghost

**Engine:** a new pure module, `ghost-ui-core/src/predict.rs`. It holds the
mosh engine (overlay rows, cursor predictions, epochs, triggers, and the RTT
estimator inputs). It takes the clock as an argument and reads the replica
through `ghost_term::Vt` (`cursor()`, `line(n).cells()`, `size()`). It owns no
I/O.

**Ownership.** One engine per session, owned by the window view that holds the
display (`TerminalView`). It is **never** stored in the shared `SessionState`,
which only `ingest` may mutate (the call-site rule in `terminal.rs`) and which
every window's view shares. Fleet tiles and observer views have no engine.
Predictions belong to the client that is typing.

**Input.** `TerminalView` already produces the final bytes (`key` →
`encode::encode`, `UiEvent::Text`, paste) and emits `Cmd::SendInput`. The
engine is fed those same bytes at the point the view emits the command, so it
sees exactly what goes on the wire, including kitty encoding and bracketed
paste. A paste is wrapped in `ESC[200~`, so it turns tentative by itself. The
sequence number is assigned in `Session::send_input` (`ghost-vt/src/client.rs`).
The view needs the number back to stamp `expiration_frame`, so `send_input`
returns it, or `SessionState`-adjacent connection state keeps a counter both
sides read. Settle this in phase 2 against the code.

**Acks and pongs.** `Session::pump` already demultiplexes `ServerMsg`. It
surfaces `EchoAck(n)` and RTT samples alongside `SessionData`, and the root
routes them to the display view's engine, which runs `cull`. The wire ordering
only helps if the client keeps it too. Today `pump(s, 32)` merges several
`Output`s into one byte buffer before `SessionData`. The cull for `EchoAck(n)`
must run only after every `Output` that came before it on the stream has been
ingested. Otherwise a correct prediction is checked against a stale replica,
reads as incorrect, and resets the engine. Either deliver in wire order
(ingest, then cull) or carry the ack as an offset into the delivered bytes.

**Drawing.** `TerminalView::lay_out_frame` already patches the laid-out frame
after `layout_frame_at_px` (the hollow cursor). Predicted cells and the
predicted cursor are patched into `Frame.rows_layout` / `frame.cursor` there.
Two more pieces are required:

- The frame memo key gains the engine's overlay generation, or a prediction
  never reaches the screen until the next real output.
- `TerminalView::damage()` includes every row whose overlay changed, in both
  directions: when a prediction appears and when it is removed. Otherwise the
  per-session Surface skips the row and the screen goes stale.

Selection and copy read the replica, never the overlay. A selection never
copies a character the host hasn't confirmed.

**Host** (`ghost-vt/src/server.rs`). `InputSeq` bytes go into the existing
`pty_out` queue together with their seq boundary. The drain loop stamps
`(seq, now)` into `input_history` once the bytes up to that boundary have been
written. The ack timer joins the poll timeout computation. Within one poll
iteration the PTY is read and its `Output` queued *before* `echo_ack` is
recomputed (mosh's order). `EchoAck` is queued
to the display client only, after any `Output` already queued, which preserves
the ordering guarantee above. Input recording is unchanged. `Ping` gets an
immediate `Pong`, without going through the PTY.

**Latency harness.** `ghost __pipe` (`ghost-vt/src/pipe.rs`, `relay()`) is the
far-end byte relay of every remote session, and the wire is framed end to end
through it. An env-gated delay queue (`GHOST_TEST_PIPE_DELAY_MS`) holds each
chunk in a `VecDeque<(Instant, Vec<u8>)>` in both directions, preserving
order, and polls with the earliest due time as the timeout. The real-sshd
fixture (`tests/support/remote.rs`, `RealRemote`) sets it in the
`ghost-remote` wrapper it already writes. The App-level remote tests in
`tests/shell_remote.rs` (the `FocusRig` pattern: `App::headless`, type with
`UiEvent::Text`, assert on `visible_text` of the drawn Scene) are the template
for the E2E cases.

## Reconnects

A reconnect (remote host upgrade, dropped pipe, future connection survival)
resets the engine and starts input numbering over on the new connection. The
host keeps `input_history` per connection. That way a sequence number can never
be acknowledged by a connection that never saw it.

## Testing

Test-first, driving the real binary, asserting on what is drawn.

1. **Latency harness first.** A test-only delay on the remote pipe, applied in
   both directions, so a test can set e.g. 300 ms of RTT on a remote session
   (see [Where it lives](#where-it-lives-in-ghost) for the exact hook). On its
   own it proves that typed text currently takes a full RTT to appear. That
   becomes the first failing test once its assertion is "appears before the
   RTT".
2. Engine unit tests in `ghost-ui-core` (pure, clock injected): insert, backspace,
   arrows, wrap, CR, epoch confirm/kill, reset on a wrong cursor, the display
   triggers' hysteresis. These are ports of mosh's behaviour and make sense as
   unit tests, because the RTT thresholds and timers are hard to drive end to end.
3. Host unit test: echo-ack advances after 50 ms with no output, and only for
   seqs whose bytes the PTY drain accepted.
4. E2E over the latency harness (`predictive_echo = "always"`):
   - At a shell prompt, type `e` and wait for its echo. That confirms the
     epoch. Then `cho hi` is drawn character by character within one frame
     each, long before the RTT, and the final screen matches the host's.
   - At a `read -s` prompt, the first key is never drawn; later keys aren't
     either; nothing is left on screen after the ack.
   - In a full-screen app that doesn't echo (e.g. `less`), typed keys leave no
     residue.
   - Backspace and left arrow inside a typed word are drawn immediately.
5. Adaptive mode on a local session never shows a prediction (an unchanged
   frame after typing, until the echo arrives).

## Phases

1. Latency harness + the failing E2E for immediate echo.
2. Protocol: `InputSeq`, `EchoAck`, `Ping`/`Pong`, `PROTO_PREDICT`; host
   input history + ack timer; client numbering + RTT estimator.
3. Prediction engine in `ghost-ui-core` (pure), with unit tests.
4. Overlay in the drawn frame + damage for predicted rows; E2E goes green.
5. Config switch, underline flagging, glitch triggers; remaining E2E cases.
