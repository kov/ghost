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
ghost, so this needed no emulator change.

**P0b (already present):** DECSTR (`CSI ! p`) soft-reset. esctest's per-test
`reset()` sends it before every test; ghost already handles it
(`Decstr → soft_reset`), so mode state does not leak across tests. Confirmed by
the DECSTR family passing 13/13 once DECRQCRA (below) let its assertions read the
screen.

**P0c (done):** DECRQCRA (`CSI Pid;Pp;Pt;Pl;Pb;Pr * y`) rectangle-checksum
replies — esctest's primary screen-read primitive (a program can't see a cell
directly). Three touch points: compute in `ghost_term::Vt::rect_checksum`,
recognise in `ghost-vt/src/query.rs::classify_csi`, format in
`ghost_vt::query::Query::reply` via a `ReplyCtx::checksum` closure wired in both
the attached model (`ghost-ui-core`) and the detached host (`ghost-vt/server`).
The algorithm is validated directly by known-vector unit tests in
`ghost-term/src/vt.rs`; end-to-end, the content families that pass (ED, EL, ICH,
…) confirm the wiring reads real cells.

**⚠️ Never pass `--force`.** In esctest2, `--force` makes `Raise()` a no-op —
*every assertion is skipped*, so tests "pass" without checking anything (only a
timed-out query still fails). `run.sh` deliberately omits it; the suite still
runs past failures because `RunTest` catches each test's exception and tallies
pass/fail. (An early version of this harness used `--force` and reported wildly
inflated pass counts — don't repeat that.)

**Honest baseline (no `--force`):** a broad cursor+content sweep runs roughly
**64 pass / 42 fail**. The failures cluster into real, tracked feature gaps:

- **Selective erase / ISO protection** (`DECSCA` `CSI " q`, `DECSED`, `DECSEL`,
  `*_respectsISOProtection`) — the biggest cluster (~26 tests). ghost's `Pen`
  has no protected-cell attribute, so selective erase can't spare protected
  cells. A distinct feature.
- **Left/right margins** (`DECLRMM` `CSI ?69h` + `DECSLRM` `CSI Pl;Pr s`) — a
  VT420 feature, **in progress**, being landed in slices:
  1. ✅ state + parsing + margin-relative cursor addressing + origin-relative CPR
     (`*_RespectsOriginMode` 3/3, CUP/CHA 6/0).
  2. ✅ autowrap at the right margin — an explicit `pending_wrap` flag replaces
     the old implicit `col == cols` sentinel (which couldn't represent
     `right_margin + 1`); the cursor now parks *on* the last usable column with
     the wrap deferred. Also lands: HT stops at the right margin, and DECRQM
     reports DECLRMM's real state. `DECAWM`/`DECLRMM` families 15/0.
  3. scroll box — LF/IND/RI/SU/SD scroll only `[left..=right]×[top..=bottom]`,
     landed in two steps:
     - ✅ 3a: the boxed-scroll primitive + SU/SD. `Buffer::scroll_{up,down}_within`
       dispatch on a full-width predicate — the fast whole-`Line` path (scrollback
       + `wrapped` flags intact) when columns span the width, else a per-row
       cell-copy core (`Line::split_wide_at`/`write_cols`) that touches only
       `[cols]`, never scrollback, at O(box area). `SU`/`SD` families 18/0.
     - ⬜ 3b: cursor-motion scrolls — LF/IND/NEL/RI freeze when the cursor is
       outside the box (`*_MovesDoesNotScrollOutsideLeftRight`), and autowrap must
       not set `wrapped` inside a box.
  4. ⬜ insert/delete within margins — ICH/DCH bounded by the right margin,
     IL/DL/DECIC/DECDC within the box.
  - Also still open (independent of the slices): CUF/CUB stopping at the
     left/right margin, and reverse-wraparound (`DECSET ?45`, `ReverseWrapInline`)
     for BS/CUB across wrapped lines.
- **CIE Lab/Luv OSC color specs** (`ChangeColor`/`ChangeSpecialColor_CIE*`) —
  ghost doesn't parse those color-space forms. Niche.

**Note on `--include`:** the regex is `re.search` over `Class.test_name`, so
`EL` also matches `CIELab`/`CIELuv`. Anchor when you mean a family — `^EL` /
`ELTests`.

**Blind spot:** esctest cannot test focus reporting, mouse encoding, or paste —
it can't drive those windowed-only inputs. Those stay covered by the
`ghost-ui-core` model tests.
