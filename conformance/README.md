# Terminal conformance harness (esctest2)

This runs [esctest2](https://github.com/ThomasDickey/esctest2) — Thomas Dickey's
maintained fork of George Nachman's `esctest` — against ghost's terminal
emulator, to catch escape-sequence divergences from xterm before they become
user-visible bugs.

## Licensing — read before touching

`esctest2/` is **GPLv2** (see `esctest2/LICENSE`). It is vendored here as a
pinned copy (`esctest2/.pinned-commit`) and **runs as a separate `python3` child
process**. Nothing in it is compiled or linked into any ghost crate, so the
shipped `ghost` binary stays `MIT OR Apache-2.0`. Do **not** port esctest code
into a Rust crate or otherwise link it — keep it a standalone test tool invoked
over a pipe.

## How it works

esctest is designed to run *as the terminal's child*: it writes control
sequences to its stdout (which the terminal renders) and reads the terminal's
replies (cursor-position reports, text-area size, rectangle checksums, …) from
its stdin. It then checks each reply against what xterm would do and writes a
pass/fail report to a logfile.

`run.sh` drives that against ghost:

1. Redirects the XDG dirs to a throwaway tempdir (so the harness never touches
   real sessions or recordings), then runs the `ghost` binary with `GHOST_ESCTEST`
   set to the esctest invocation.
2. `GHOST_ESCTEST` puts `ghost` into a headless host mode
   (`ghost-ui/src/main.rs::esctest_host`): it spawns the esctest command as a
   real ghost session's child and drives the **same `TerminalModel` the GUI
   uses** over the PTY — feeding esctest's control sequences into the model and
   writing every reply (`Cmd::SendInput`) back. No window, no renderer; esctest
   only observes what a program can.
3. esctest exits, `run.sh` greps the summary line out of the logfile (esctest2
   exits 0 even on failures) and sets its own exit status from it.

The session is spawned at **80×25** because esctest normalises the terminal to
25 rows × 80 cols and reads the size back with `CSI 18 t`; matching that keeps
bottom-row and size-dependent tests honest.

## Running

```sh
cargo build -p ghost-ui          # produces target/debug/ghost
conformance/run.sh               # default cursor/CPR include-set
```

Knobs (env):

| var                        | default                          | meaning                                  |
|----------------------------|----------------------------------|------------------------------------------|
| `INCLUDE`                  | `CUP\|CUU\|CUD\|CHA\|VPA\|CNL\|CPL` | regex of test names to run (`--include`) |
| `MAX_VT_LEVEL`             | `4`                              | esctest `--max-vt-level`                 |
| `GHOST_BIN`                | `target/debug/ghost`             | which binary to drive                    |
| `GHOST_ESCTEST_TIMEOUT_SECS` | `120`                          | wall-clock backstop for a wedged child   |

```sh
INCLUDE='CUP' conformance/run.sh          # one family
MAX_VT_LEVEL=3 INCLUDE='CHA' conformance/run.sh
```

`run.sh` prints `SKIP` and exits 0 if `python3` is missing, so a CI job can opt
in without hard-failing on hosts without it. It runs in minutes at most for
small include-sets; the whole suite is slow (per-cell round trips) — keep CI on
a curated include-set.

## Status / roadmap

**P0a (done):** the harness runs end-to-end on cursor-motion tests, which assert
via CPR (`CSI 6 n`) and text-area size (`CSI 18 t`) — both already answered by
ghost, so this needed no emulator change. Current default set: **34 pass, 2
fail**, both `*_RespectsOriginMode`. That first finding is real (it reproduces in
isolation, so it is not a cross-test state leak): those tests set a
**left/right margin region** (`DECLRMM` `CSI ?69h` + `DECSLRM` `CSI Pl;Pr s`)
and expect origin mode (`DECOM`) to make `CUP` relative to it. ghost has no
left/right margins, so the margin-relative origin is wrong — a genuine VT420
feature gap to schedule, not a quick fix.

**P0b (next):** DECSTR (`CSI ! p`) soft-reset. esctest's per-test `reset()` sends
it before every test; ghost ignores it today, so mode state leaks across tests.
It doesn't cause the origin-mode failures above, but it will cause *spurious*
failures as the include-set widens (a mode left set by test N breaks test N+1).
Fix test-first in `ghost-term`, then re-run.

**P0c:** DECRQCRA (`CSI … * y`) rectangle-checksum replies, esctest's primary
screen-read primitive — unlocks the bulk of content assertions (ED/EL/ICH/DCH/
IRM/tabs/…). Three touch points: compute in `ghost-term`, recognise in
`ghost-vt/src/query.rs`, format in `ghost_vt::query::Query::reply`.

**Blind spot:** esctest cannot test focus reporting, mouse encoding, or paste —
it can't drive those windowed-only inputs. Those stay covered by the
`ghost-ui-core` model tests.
