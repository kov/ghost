# Predictive local echo

Mosh-style predictive echo in ghost: typing into a remote session shows up
immediately, and the host's real output confirms it or replaces it. The
approach is mosh's, with only the changes our wire needs. Mosh references are to `src/frontend/terminaloverlay.cc` (the
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

Ghost times the link with `ClientMsg::Ping(u64)` / `ServerMsg::Pong(u64)` at
the same protocol level. `Session` pings once right after attach, then while the
user types (one ping in flight, at most every 250 ms), so an idle session never
chatters. Every Pong feeds the estimator (`ghost_vt::echo`).

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
| `CSI C` / `CSI D` with no parameters (also `ESC O C/D`, rewritten to CSI) | Cursor ±1 |
| CR | Epoch turns tentative; cursor goes to column 0 of the next row (on the last row, that row is predicted blank — no scroll prediction) |
| Anything else (control chars, other CSI, ESC, width ≠ 1) | Epoch turns tentative, nothing drawn |

Mosh moves the cursor for any `CSI … C/D`; ghost only for the bare form, because
a parameterised one (`CSI 1;5C`, Ctrl+→) is word motion in a line editor and
would only ever be guessed wrong.

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
It hot-reloads like the rest of `[input]`. The E2E tests run the default,
`adaptive`: the harness's 500 ms round trip is far above every threshold, so
they test what users get. We don't adopt `Experimental` or `predict_overwrite`.

## Where it lives in ghost

**Wire** (`ghost-vt`). `protocol.rs` has the four messages and `PROTO_PREDICT`.
`echo.rs` is the pure timing half: `EchoAck` (the host's per-connection input
history and mosh's `set_echo_ack` rule), `Rtt` (the estimator) and `InputClock`
(the client's numbering and ping schedule). `Session::send_input` numbers input
on a host at `PROTO_PREDICT` (`last_input_seq`), `Session::pump` surfaces
`Pump::echo_ack` and folds pongs into `Session::srtt`.

**Host** (`server.rs`). An `InputSeq`'s bytes go into the PTY queue (`pty_out`),
and the display `Client`'s `EchoAck` records where in the queue they end. The
drain stamps each input as written once the PTY takes its last byte. After the
drain, the newest input written ≥ 50 ms ago is acknowledged, queued behind every
`Output` read so far. The poll timeout includes the next ack's due time, so a
silent child still gets its ack. A `Ping` is answered at once.

**Engine** (`ghost-ui-core/src/predict.rs`). The mosh `PredictionEngine`,
ported: overlay rows, cursor predictions, epochs, the display triggers. Pure — it
reads the session's `ghost_term::Vt` and takes the clock as an argument. It
exposes what to draw as an `Overlay` (cells and a cursor position) plus a
generation that moves only when that changes.

**Ownership.** One engine per session, beside the replica in `SessionState` —
one per session per process, like the session's single client connection. It
is a separate field from `screen`: it never mutates the emulator or bumps
`content_gen`, so the `ingest` call-site rule is untouched. The shell drives it
through `Sessions::predict_*`:

- **Input.** The App marks the commands of a typing event (`Key`, `Text`,
  `ClipboardText`, `Pointer`) and, when one sends to a remote session, feeds the
  bytes to the engine just before `send_input`, then reports the number they
  went out under. So the engine sees exactly what goes on the wire, including
  kitty encoding and bracketed paste, and knows each input's exact number.
  Bytes the frontend sends on its own (query replies, focus reports on attach)
  aren't typing and never reach the engine.
- **Acks.** After each pump, once that pump's output has been fed to the
  replica, the App hands the engine the pump's newest ack and the current RTT.
  Since every byte that preceded the ack on the wire is in that output, the
  replica is at least as new as the host's screen when it acked.
- **Ticks.** While the engine's glitch timers can still fire, the per-wake pump
  (every 8 ms) re-judges it even with no output.
- **Reconnects.** Installing a new client for a session (`drive_with_client`)
  replaces the engine: numbering starts over and the resync re-seeds the screen.

**Drawing** (`terminal.rs`). `TerminalView::lay_out_frame` lays the overlay
over the frame: affected rows are re-laid from their cells with the predicted
cells substituted (`ghost_render::layout_cells`), and the cursor moves to the
predicted position. Only over the live screen — never scrolled into history or
frozen. The frame memo key carries the overlay generation, `drawn_cursor`
reports the predicted cursor, and `damage()` dirties the rows the overlay
covered at the last present and covers now, so a prediction appearing or being
taken back repaints with no feed behind it. Selection and copy read the
replica, never the overlay.

**Latency harness.** `ghost __pipe` (`pipe.rs`) holds every chunk for
`GHOST_TEST_PIPE_DELAY_MS` in each direction, order kept. The real-sshd fixture
arms it with `RealRemote::set_pipe_delay`; the drawn terminal text is read with
`support::drawn_terminal_text`.

## Reconnects

A reconnect (remote host upgrade, dropped pipe, future connection survival)
replaces the engine and starts input numbering over on the new connection. The
host keeps its input history per connection, so a number can never be
acknowledged by a connection that never saw it.

## Testing

- `ghost-vt` `echo` unit tests: ack timing, RTT estimator, ping schedule.
  `gui_session.rs`: a real host acknowledges numbered input after the echo
  timeout while its child prints nothing, and answers the attach ping.
- `ghost-ui-core` `predict` unit tests: epochs held back and confirmed, correct
  echoes retired, unechoed keys never drawn, a wrong guess resetting everything,
  backspace and arrows, escape-coded keys not guessed at, fast links showing
  nothing, underlining on slow ones, resize resets, generation discipline.
- `ghost-ui/tests/predictive_echo.rs`, the real App over a real sshd with a
  250 ms pipe delay each way:
  - the first key waits for its echo (and proves the harness is slow), the
    second is drawn before its own echo, and the rest within half a round trip;
    backspace too;
  - at a `stty -echo` password prompt, after Enter, no typed key is ever drawn.

Left/right arrows are covered by the unit tests only: `cat` under a canonical
tty echoes an arrow as `^[[D`, so an end-to-end arrow test needs a line editor.
A full-screen app that doesn't echo relies on the same epoch rule as the
password test.
