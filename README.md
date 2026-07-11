# Shell in the Ghost

*Ghost in the Shell*, but inverted: detach or close a window and a **ghost** is left
behind with a shell inside it! It can be reattached later without losing a byte
of state. And everything the terminal shows is recorded, so any session can be
searched or replayed later.

Ghost can be used as a CLI in your favorite terminal (see [The CLI](#the-cli)
below), but the author's main use case is running it as a terminal.

ghost is a mix of [dtach], [asciinema], and [mosh], wrapped in a fleet manager. A
session keeps running in its own background process after you detach; reattaching
repaints the exact screen — scrollback, colors, alternate-screen apps,
mouse/paste/focus modes, images, and the window title all survive. It does **not**
multiplex or split panes; one session is one terminal. Instead, a single window can
hold several sessions, a **fleet** view (press `F9`) shows every session as a live
preview grid, and sessions cluster into color-coded **groups** you can attach, detach,
or kill as a unit.

![Two ghost windows: a single terminal, and a fleet view of a remote SSH group
with live session previews, per-tile and per-group actions, and an
"attached elsewhere" row](ghost.png)

[dtach]: https://github.com/crigler/dtach
[asciinema]: https://asciinema.org/
[mosh]: https://mosh.org/

## Build

```sh
cargo build --release          # binary at target/release/ghost
```

The one `ghost` binary is both halves: a bare `ghost` opens the windowed GPU
terminal; `ghost <subcommand>` runs the CLI and exits.

## The window (bare `ghost`)

Launch `ghost` with no arguments and you get a native, GPU-rendered terminal
(winit + wgpu + swash — real font shaping, ligatures, color emoji, kitty
graphics). It is also a manager for every ghost session on the machine:

- **Sessions & windows** — one window can drive several sessions at once; `Ctrl-Tab`
  cycles them. Open more windows with a new-window shortcut. Closing a window (or the
  `Close` menu item / `Cmd-W`) **detaches** its sessions rather than killing them —
  the hosts keep running, and the sessions reappear in the fleet as "detached".
- **Fleet view (`F9`)** — a grid of every session as a *live* preview: sessions this
  window drives stay fed, so their tiles keep updating; sessions elsewhere show their
  last state. Arrow keys / `Tab` move focus, `Enter` dives into a tile (adopting it
  into this window), `Esc` or `F9` returns. Per-tile buttons **kill**, **detach**, and
  **rename**; `Space` (or `Ctrl-click`) multi-selects tiles for bulk actions. Tiles are
  sectioned by locality — *this window*, *attached elsewhere*, *detached* — plus a block
  per group.
- **Groups** — a group is a color-coded set of sessions (blue, green, orange, purple,
  rose, teal). One is born automatically for each window and means "the sessions this
  window drives"; it is persisted, so it survives the window closing. From a group's
  header you can **attach all** (`Ctrl-Enter` on any member opens the whole group into
  this window), **detach**, **rename**, **dissolve**, or **kill** the group. Drag a tile
  out of a group to ungroup it.
- **Restore** — a bare `ghost` after a quit reopens the windows you had open, each
  reattaching its group's sessions (remote groups reconnect too). Pass `--fresh` to
  skip restoring and start clean.
- **SSH windows** — open a window (or a session in the current window) connected to a
  remote host; see [Remote sessions](#remote-sessions-ssh).

### Keyboard shortcuts

The primary modifier is **Cmd** on macOS and **Ctrl** elsewhere. On Linux many
window actions are *also* on `Alt` (a terminal-app convention that keeps bare `Ctrl`
free for the shell); where a bare `Ctrl` chord would collide with terminal input
(`Ctrl-S` = XOFF, `Ctrl-G` = BEL, `Ctrl-N`/`W`), the shortcut needs `Shift`.

| Action                          | macOS   | Linux                       |
| ------------------------------- | ------- | --------------------------- |
| New window                      | `Cmd-N` | `Ctrl-Shift-N` / `Alt-N`    |
| New session (this window)       | `Cmd-T` | `Alt-T`                     |
| New SSH window                  | `Cmd-S` | `Ctrl-Shift-S` / `Alt-S`    |
| New SSH session (this window)   | `Cmd-G` | `Ctrl-Shift-G` / `Alt-G`    |
| Close window (detaches)         | `Cmd-W` | `Ctrl-Shift-W`              |
| Copy                            | `Cmd-C` | `Ctrl-Shift-C` / `Alt-C`    |
| Paste                           | `Cmd-V` | `Ctrl-Shift-V` / `Alt-V`    |
| Quit                            | `Cmd-Q` | `Ctrl-Q`                    |
| Zoom in / out / reset           | `Cmd` `+` / `-` / `0` | `Ctrl` `+` / `-` / `0` |
| Toggle fleet view               | `F9`    | `F9`                        |
| Cycle sessions in this window   | `Ctrl-Tab` / `Ctrl-Shift-Tab` | `Ctrl-Tab` / `Ctrl-Shift-Tab` |

Inside the fleet: arrows / `Tab` move focus, `Enter` opens the focused tile,
`Ctrl-Enter` opens its whole group, `Space` marks a tile, `u` ungroups, `Ctrl-U`
dissolves the group, `Esc` leaves.

### Configuration

The window reads a small, hand-edited TOML at `$XDG_CONFIG_HOME/ghost/ui.toml`
(unknown keys are ignored, so a file survives version skew). It selects a color
scheme, background opacity and frosting, initial grid size and padding, base font
size + family, and the macOS Option-key behavior:

```toml
[colors]
scheme = "tango-dark"   # gnome-dark|light, tango-dark|light, solarized-dark|light, linux-console

[window]
opacity = 0.95          # 0.0..=1.0; only the default background goes translucent
blur    = true          # frost the desktop behind the window (KDE/KWin & macOS; ignored elsewhere)
frost   = 0.2           # 0.0..=1.0; self-drawn milky grain over the see-through background
columns = 100
rows    = 30
padding = 6.0

[font]
size   = 13.0
family = "Fira Code"

[input]
option_as_meta = true   # macOS: treat Option as Meta

[zoom]
factor = 1.0            # persisted across the Cmd/Ctrl +/-/0 shortcuts
```

Edits are hot-reloaded: saving `ui.toml` re-applies the color scheme, opacity,
blur, frost, and padding to every open window without a restart. Font and the
initial grid size (`columns`/`rows`) apply only to newly opened windows.

## The CLI

```sh
ghost new [NAME]               # start a session running $SHELL and attach to it
ghost new NAME -- CMD ARGS…    # …or run a specific command
ghost new -d [NAME]            # start in the background without attaching
ghost ssh [USER@]HOST          # start a session on a remote host (see below)
ghost ls                       # list live sessions (name + pid)
ghost attach NAME              # attach to a session
ghost kill NAME… | --all       # kill one or more sessions, or every local one
ghost rename OLD NEW           # rename a session (a display label; attach state untouched)
ghost search PATTERN           # grep what your sessions rendered (recordings are compressed)
ghost export NAME [FILE]       # export the recording as an asciicast (v2)
```

`ghost new` starts the session and attaches to it, like `tmux`/`screen`. Pass
`-d`/`--detached` to leave it running in the background instead (then `ghost
attach` when you want it). A session starts in the directory you launched it from
(`--cwd DIR` to override), and runs at 80×24 until the first client attaches (then
it adopts the client's size).

`ghost search` replays each recording through the emulator and greps the *rendered*
lines (a raw `grep` finds nothing — recordings are compressed), printing
`session:line: text`; `-i` for case-insensitive, `--session NAME` to scope to one.

### While attached (CLI client)

The CLI client is a transparent pipe — every byte goes straight to the session
except the detach/kill trigger, a tmux-style prefix (`Ctrl-\` by default):

| Keys              | Action                                            |
| ----------------- | ------------------------------------------------- |
| `Ctrl-\` `d`      | detach (session keeps running)                    |
| `Ctrl-\` `k`      | kill the session                                  |
| `Ctrl-\` `r`      | rename (prompts for the new name; `Esc` cancels)  |
| `Ctrl-\` `Ctrl-\` | send a literal `Ctrl-\` through                   |

`ghost new` options: `--no-record` (recording is on by default), `--scrollback N`
(replayed history bound, default 1000 lines), `--max-recording-size BYTES` (on-disk
cap, default 64 MiB).

## Remote sessions (SSH)

`ghost ssh [USER@]HOST` (or `Cmd-S` for an SSH window in the GUI) opens a session on
another machine. If that machine can run ghost, the session is a real ghost *host*
there — full recording, detach, and fleet visibility, with live previews — tunnelled
over a single SSH connection (a `ControlMaster`, so you authenticate once). If it
can't, ghost falls back to a plain `ssh` child. A session spawned in an SSH window or
group inherits the same connection.

The host path needs a `ghost` binary on the remote. ghost finds one in order:
`ghost` on the remote `PATH`, an already-staged copy, or — failing those — by
**staging** its own binary over the connection (same OS+arch only).

### Cross-architecture staging (prebuilts)

To reach a remote of a *different* OS/arch (say, an arm64 Mac from an x86-64 Linux
box), give ghost a prebuilt of the small headless binary (`ghost-host`) for that
platform. It looks for a file named `ghost-<os>-<arch>` — `os` ∈ `linux`/`macos`,
`arch` ∈ `x86_64`/`aarch64` — in, in order:

1. `$GHOST_PREBUILT_DIR`, then
2. `$XDG_DATA_HOME/ghost/prebuilt/` (`~/.local/share/ghost/prebuilt/`).

Generate them with xtask:

```sh
cargo xtask prebuilt                        # this OS's two arches → the prebuilt dir
cargo xtask prebuilt aarch64-apple-darwin   # a specific target
GHOST_ZIGBUILD=1 cargo xtask prebuilt …     # build via cargo-zigbuild (for a cross-OS
                                            # target, e.g. a Linux prebuilt from a Mac)
```

`ghost-host` is pure Rust and GUI-free, so cross-building needs no C toolchain or
sysroot. On Linux the default targets are **static musl** binaries: `rustup target
add` is the only setup (xtask does it), they link with the bundled `rust-lld`, and
being static they run on any remote regardless of its glibc. On macOS the native
Apple toolchain builds both arches. Only cross-*OS* builds (a Linux binary from a
Mac, or vice-versa) want `GHOST_ZIGBUILD=1`.

The binary is a few MB and is the only thing staged to the remote. With no matching
prebuilt ghost falls back to the ssh child, so a missing one never breaks a
connection — it only unlocks the richer host path.

## How it works

Each session is its own background process (double-forked daemon) owning one PTY
and one Unix socket — there is no central daemon. The host feeds every byte the
child writes into a headless VT emulator ([`ghost-term`](#layout)), so it always
knows what the terminal looks like even with nobody attached. On attach it sends a
**resync**: clear the screen, then repaint the current state plus bounded scrollback,
laid out at the attaching client's size. After that it streams live bytes verbatim.
The GUI keeps background sessions warm-fed the same way, so fleet previews are live
and `Ctrl-Tab` switches are instant.

Sessions survive *disconnection*, never a *reboot of the machine running the
session* (a PTY child cannot outlive its kernel).

The host and CLI client are single `poll()` loops. Signals are folded into them via
a self-pipe — an installed handler writes each delivered signal's number to a pipe
whose read end sits in the poll set — so the same code runs on Linux and macOS (no
`signalfd`/`kqueue` split).

### Storage

- Per-session runtime dir: `$XDG_RUNTIME_DIR/ghost/<name>/` (Linux), or a durable
  per-user dir where there is no `XDG_RUNTIME_DIR` — `~/.local/state/ghost`, or
  `~/Library/Application Support/ghost` on macOS — holding `sock`, `pid`, and `lock`.
  macOS's temp dir is avoided on purpose: it is reaped every few days and would strand
  a still-running session by deleting its files. Dead leftovers are pruned by a
  liveness check, not by relying on the dir being wiped. `<name>` is the session's
  *immutable* spawn-time id: `ghost rename` only sets a display label in its metadata,
  so these files never move and attached clients are never disturbed.
- Recordings: `$XDG_DATA_HOME/ghost/recordings/<name>.ghostrec` (falls back to
  `~/.local/share/ghost/…`; archival, survives reboot). A framed, per-frame-brotli
  asciicast with periodic checkpoints; `ghost export` turns it into a standard
  asciicast that `asciinema play` can replay.
- Windows/groups snapshot (`windows.toml`) in the data dir drives session restore.

## Terminal type

Sessions are spawned with `TERM=xterm-kitty` (ghost's emulator implements the kitty
feature profile, and apps enable modern features — kitty keyboard protocol,
synchronized output, graphics — based on the TERM name). ghost provides the terminfo
entry itself: precompiled in the macOS `.app` bundle, or compiled on first use into
`$XDG_DATA_HOME/ghost/terminfo` with the system `tic`, handed to children via
`TERMINFO_DIRS`. If the local curses library can't even find `xterm-kitty`, ghost
falls back to `xterm-256color`. Set `GHOST_TERM` to override the advertised `TERM`.

## Current limitations

- **Reusing a session name overwrites its prior recording** (no timestamping yet).
- **No built-in `ghost play`** — use `ghost export` + `asciinema play`.

## Layout

One workspace. The headline product is the **`ghost`** binary — a windowed GPU
terminal that also carries the CLI.

Backend:

- **`ghost-term/`** — our owned terminal-emulation core, a hard fork of asciinema's
  [avt]. Tracks authoritative screen state and produces the resync/checkpoint dumps.
  Apache-2.0; see `ghost-term/LICENSE`.
- **`ghost-vt/`** — the engine: session lifecycle, PTY, transport, recording, server
  and client.
- **`ghost-cli/`** — the `new`/`ls`/`attach`/`kill`/`rename`/`search`/`export`/`ssh`
  subcommands, as a library folded into the `ghost` binary.
- **`ghost-host/`** — the small, GUI-free headless binary (host + transport) staged
  to remotes.
- **`ghost-render/`** — pure, pixel-free terminal layout (grid → scene), shared by the
  frontend and its tests.

Frontend (winit + wgpu + swash):

- **`ghost-ui/`** — the `ghost` binary: window, event loop, and CLI dispatch.
- **`ghost-ui-core/`** — the Elm-style functional core (model → scene), headlessly
  testable; owns the single/fleet views, groups, and shortcut handling.
- **`ghost-shaper/`** (swash) and **`ghost-renderer/`** (wgpu) — text shaping and the
  GPU renderer.
- **`ghost-ui-harness/`** drives the real frontend for tests and benches;
  **`ghost-shot/`** renders scenes to PNG headlessly.
- **`vendor/winit/`** — winit with two local patches (see the `[patch.crates-io]` in
  the root manifest); excluded from the workspace.

[avt]: https://github.com/asciinema/avt

## Development

```sh
cargo test --workspace         # unit tests + binary-driven PTY E2E tests
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

Tests follow a strict test-first workflow: every fix or feature starts with a
failing test (binary-driven through the real `ghost` binary where possible), then the
implementation brings it to green. A pre-commit hook runs `fmt` and `clippy` (enable
it with `git config core.hooksPath .githooks`).

## License

MIT OR Apache-2.0, except `ghost-term/` (our avt fork) and the vendored
`vendor/winit/`, which are Apache-2.0 (see `ghost-term/LICENSE`).
