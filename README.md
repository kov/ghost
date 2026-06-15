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

- **`vt/`** — a vendored fork of [avt] (asciinema's VT engine, package name kept
  as `avt`), the low-level terminal emulator. Tracks authoritative screen state
  and produces the resync/checkpoint dumps. Apache-2.0; see `vt/LICENSE`.
- **`ghost-vt/`** — the main library: session lifecycle, PTY, transport,
  recording, server and client. This is what a GUI terminal would depend on.
- **`ghost-cli/`** — the reference CLI binary, `ghost`.

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

- Sockets + pidfiles: `$XDG_RUNTIME_DIR/ghost/<name>.sock` / `.pid` (ephemeral —
  wiped on reboot, which doubles as stale-socket cleanup).
- Recordings: `$XDG_DATA_HOME/ghost/recordings/<name>.ghostrec` (falls back to
  `~/.local/share/ghost/…`; archival, survives reboot). A framed, per-frame-zstd
  asciicast with periodic checkpoints; `ghost export` turns it into a standard
  asciicast that `asciinema play` can replay.

## Current limitations

- **Reusing a session name overwrites its prior recording** (no timestamping
  yet).
- **No built-in `ghost play`** — use `ghost export` + `asciinema play`.
- **`TERM` is inherited** from the `ghost new` environment; it is not normalized.

## Development

```sh
cargo test --workspace         # unit tests + binary-driven PTY E2E tests
cargo fmt --all
cargo clippy --all-targets
```

Tests follow a strict test-first workflow: every fix or feature starts with a
failing test (binary-driven through the real `ghost` binary where possible),
then the implementation brings it to green. A pre-commit hook runs `fmt` and
`clippy`.

## License

MIT OR Apache-2.0, except the vendored `vt/` engine, which is Apache-2.0
(see `vt/LICENSE`).
