# ghost

Run a terminal in the background and reattach to it later without losing
scrollback, native mouse handling, or terminal keybindings.

ghost is a mix of [dtach], [asciinema], and [mosh]: a session keeps running in
a background process after you detach, and reattaching repaints the exact screen
state — scrollback, colors, alternate-screen apps, mouse/paste/focus modes, and
the window title all survive. It does **not** multiplex or split windows; one
session is one terminal.

[dtach]: https://github.com/crigler/dtach
[asciinema]: https://asciinema.org/
[mosh]: https://mosh.org/

## Layout

One workspace. The headline product is the **`ghost`** binary — a windowed GPU
terminal that also carries the CLI: a bare `ghost` opens the GUI, while
`ghost <subcommand>` runs the CLI and exits.

Backend:

- **`ghost-term/`** — our owned terminal-emulation core, a hard fork of
  asciinema's [avt]. Tracks authoritative screen state and produces the
  resync/checkpoint dumps. Apache-2.0; see `ghost-term/LICENSE`.
- **`ghost-vt/`** — the engine: session lifecycle, PTY, transport, recording,
  server and client.
- **`ghost-cli/`** — the `new`/`ls`/`attach`/`kill`/`rename`/`export`
  subcommands, as a library folded into the `ghost` binary.
- **`ghost-render/`** — pure, pixel-free terminal layout (grid → scene), shared
  by the frontend and its tests.

Frontend (winit + wgpu + swash):

- **`ghost-ui/`** — the `ghost` binary: window, event loop, and CLI dispatch.
- **`ghost-ui-core/`** — the Elm-style functional core (model → scene),
  headlessly testable.
- **`ghost-shaper/`** (swash) and **`ghost-renderer/`** (wgpu) — text shaping
  and the GPU renderer.
- **`ghost-ui-harness/`** drives the real frontend for tests and benches;
  **`ghost-shot/`** renders scenes to PNG headlessly.
- **`vendor/winit/`** — winit with a one-line Wayland snap-restore patch (see the
  `[patch.crates-io]` in the root manifest); excluded from the workspace.

[avt]: https://github.com/asciinema/avt

## Build

```sh
cargo build --release          # binary at target/release/ghost
```

## Usage

```sh
ghost new [NAME]               # start a session running $SHELL and attach to it
ghost new NAME -- CMD ARGS…    # …or run a specific command
ghost new -d [NAME]            # start in the background without attaching
ghost ls                       # list live sessions (name + pid)
ghost attach NAME              # attach to a session
ghost kill NAME                # kill a session and its process
ghost rename OLD NEW           # rename a running session
ghost export NAME [FILE]       # export the recording as an asciicast (v2)
```

`ghost new` starts the session and attaches to it, like `tmux`/`screen`. Pass
`-d`/`--detached` to leave it running in the background instead (then `ghost
attach` when you want it). A session starts in the directory you launched it
from, and runs at 80×24 until the first client attaches (then it adopts the
client's size).

### While attached

The client is a transparent pipe — every byte goes straight to the session
except the detach/kill trigger, a tmux-style prefix (`Ctrl-\` by default):

| Keys            | Action                              |
| --------------- | ----------------------------------- |
| `Ctrl-\` `d`    | detach (session keeps running)      |
| `Ctrl-\` `k`    | kill the session                    |
| `Ctrl-\` `r`    | rename (prompts for the new name; `Esc` cancels) |
| `Ctrl-\` `Ctrl-\` | send a literal `Ctrl-\` through   |

`ghost new` options: `--no-record` (recording is on by default),
`--scrollback N` (replayed history bound, default 1000 lines),
`--max-recording-size BYTES` (on-disk cap, default 64 MiB).

## How it works

Each session is its own background process (double-forked daemon) owning one PTY
and one Unix socket — there is no central daemon. The host feeds every byte the
child writes into a headless VT emulator, so it always knows what the terminal
looks like even with nobody attached. On attach it sends a **resync**: clear the
screen, then repaint the current state plus bounded scrollback, laid out at the
attaching client's size. After that it streams live bytes verbatim.

Sessions survive *disconnection*, never a *reboot of the machine running the
session* (a PTY child cannot outlive its kernel).

The host and client are single `poll()` loops. Signals are folded into them via
a self-pipe — an installed handler writes each delivered signal's number to a
pipe whose read end sits in the poll set — so the same code runs on Linux and
macOS (no `signalfd`/`kqueue` split).

### Storage

- Per-session runtime dir: `$XDG_RUNTIME_DIR/ghost/<name>/` (Linux), or a
  durable per-user dir where there is no `XDG_RUNTIME_DIR` — `~/.local/state/ghost`,
  or `~/Library/Application Support/ghost` on macOS — holding `sock`, `pid`, and
  `lock`. macOS's temp dir is avoided on purpose: it is reaped every few days and
  would strand a still-running session by deleting its files. Dead leftovers are
  pruned by a liveness check, not by relying on the dir being wiped. Grouping
  them in one directory makes renaming a single atomic `rename(2)`.
- Recordings: `$XDG_DATA_HOME/ghost/recordings/<name>.ghostrec` (falls back to
  `~/.local/share/ghost/…`; archival, survives reboot). A framed, per-frame-zstd
  asciicast with periodic checkpoints; `ghost export` turns it into a standard
  asciicast that `asciinema play` can replay.

## Current limitations

- **Reusing a session name overwrites its prior recording** (no timestamping
  yet).
- **No built-in `ghost play`** — use `ghost export` + `asciinema play`.

Sessions are spawned with `TERM=xterm-kitty` (ghost's emulator implements the
kitty feature profile, and apps enable modern features — kitty keyboard
protocol, synchronized output — based on the TERM name). ghost provides the
terminfo entry itself: precompiled in the macOS `.app` bundle, or compiled on
first use into `$XDG_DATA_HOME/ghost/terminfo` with the system `tic`, handed
to children via `TERMINFO_DIRS`. Set `GHOST_TERM` to override the advertised
`TERM`.

## Development

```sh
cargo test --workspace         # unit tests + binary-driven PTY E2E tests
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

Tests follow a strict test-first workflow: every fix or feature starts with a
failing test (binary-driven through the real `ghost` binary where possible),
then the implementation brings it to green. A pre-commit hook runs `fmt` and
`clippy`.

## License

MIT OR Apache-2.0, except `ghost-term/` (our avt fork) and the vendored
`vendor/winit/`, which are Apache-2.0
(see `vt/LICENSE`).
