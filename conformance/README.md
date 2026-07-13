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

**Honest baseline (no `--force`):** the whole suite (`INCLUDE='.'`) runs
**442 pass / 45 known bugs / 81 fail**. The failures cluster into real, tracked
feature gaps — what's left is listed under "Still open" at the end:

- **Selective erase / ISO protection** — ✅ **DONE** (slice 7). Cells carry a
  three-state `Protection` (`None`/`Dec`/`Iso`) on the pen: DECSCA (`CSI Ps " q`)
  sets DEC protection, SPA/EPA (`ESC V`/`ESC W`) the ISO guarded area. An
  `EraseGuard` threaded through `Buffer::erase`/`Line::clear` makes each erase
  family spare the right cells, matching xterm: plain ED/EL/ECH spare ISO;
  DECSED/DECSEL (`CSI ? Ps J`/`K`) spare both (xterm respects ISO here for
  back-compat — their `doesNotRespectISOProtect` tests are @knownBug on xterm);
  DECSERA (`CSI Pt;Pl;Pb;Pr $ {`) spares only DEC. Protection survives a dump
  (protected runs replay wrapped in DECSCA/SPA controls), DECSTR clears it, and
  DECRQSS `" q` reports the DECSCA bit. `DECSED` 17/0, `DECSEL` 10/0, `DECSERA`
  8/0, `ECH`/`ED`/`EL` protection tests pass, `DECSTR_DECSCA` + `DECRQSS_DECSCA`
  pass.
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
     - ✅ 3b: cursor-motion scrolls — LF/IND/NEL/RI at the bottom/top margin
       scroll the box only from within the left/right margins, and are inert
       (no scroll, no move) outside them; an in-box autowrap no longer sets the
       `wrapped` flag. `IND`/`LF`/`NEL`/`RI` families 0 fail
       (`*_MovesDoesNotScrollOutsideLeftRight`).
  4. insert/delete within margins:
     - ✅ 4a: ICH/DCH bounded by the right margin — no-op outside the L/R box,
       else insert/delete within `[cursor..=right]` (`Line::{insert,delete}_within`
       via `copy_within` + edge mending; `shift_right`/`delete`/insert-mode fold
       onto them at full width). `ICH`/`DCH` families 12/0.
     - ✅ 4b: IL/DL scroll the box (via `scroll_{down,up}_within`) and are a
       no-op unless the cursor is in the scroll region on both axes
       (`cursor_in_scroll_region()`) — which also fixed a pre-existing bug where
       they scrolled `cursor.row..rows` above/below the DECSTBM region.
       `IL`/`DL` families 16/0.
     - ✅ 4c: DECIC/DECDC (insert/delete columns) — new parser arms `CSI Pn ' }`
       / `CSI Pn ' ~`; they rewrite the same `[cursor..=right]` column band across
       every row of the scroll region (`Buffer::{insert,delete}_columns`) and are a
       no-op unless the cursor is in the scroll region on both axes. A `push_csi`
       fix rides along: a true intermediate (`'`, 0x20–0x2F) now serialises *after*
       the params (ECMA-48 order), where a private-marker prefix (`<=>?`) still
       leads — the old order emitted `CSI ' Pn }`, which re-parses to nothing.
       `DECIC`/`DECDC` families 14/0.
  5. margin-aware cursor moves (independent follow-ups to the box slices):
     - ✅ 5a: CUF/CUB (and BS) stop at the left/right margin when the cursor starts
       on the inside of it, but run to the screen edge when it starts outside
       (folded into the shared `move_cursor_to_rel_col`). `CUF`/`CUB` families 10/0.
     - ✅ 5b: reverse-wraparound (`DECSET ?45`, needs DECAWM) — a leftward BS/CUB at
       the left edge wraps up to the right margin of the row above, wrapping around
       from the top of the scroll region to its bottom (`move_cursor_back_wrapping`);
       a pending wrap is consumed in place. DECSTR clears `?45` (esctest relies on
       that). A BS that lands on a *fresh line-feed position* skips the first wrap
       (a `prev_op_was_line_feed` flag set at the end of `execute` for LF/IND/NEL,
       cleared by any other op) — so `NEL` then `BS` stays put while `CUP` then `BS`
       wraps; the distinction is the landing op, not the wrapped flag (a
       wrapped-flag model breaks `AfterNoWrappedInlines`). `BS`/`CUB`/`CUF` **27/0**.
  6. reporting/query follow-ups:
     - ✅ 6a: DECRQSS DECSLRM (`DCS $ q s ST`) reports the current margins as
       `DCS 1 $ r Pl;Pr s ST` — a `left_right_margins` field threaded through
       `ReplyCtx` (attached model + detached host), fed by
       `ghost_term::Vt::left_right_margins`. `DECRQSS_DECSLRM` passes.
     - ✅ 6b: DECSCL conformance levels (`CSI Pl " p`) — a hard reset that then
       applies the VT level (1–5), which gates version-specific features: ANSI-mode
       DECRQM (`CSI Ps $ p`, new) answers only at level ≥ 3 (silent below, how a
       host probes the level), DECLRMM (`?69`) needs ≥ 4, DECNCSM (`?95`) needs ≥ 5.
       Level defaults to 5 so nothing regresses; RIS resets it; a non-default level
       leads a state dump (DECSCL hard-resets). `DECSCL` Level2/Level3 pass.
     - ✅ 6c: DECCOLM (`?3`) 80↔132 column mode, gated by Allow80To132 (`?40`,
       xterm's `c132`) — resizes the grid, resets the full scroll region, homes the
       cursor and clears (unless DECNCSM). The self-resize is surfaced bottom-up:
       `Screen::feed` reconciles its size from `Vt::size` after each feed, so
       `CSI 18 t` and any recording follow it. `DECSCL_Level4`, `DECSET_DECCOLM`,
       `DECSET_Allow80To132` pass (the GUI follows the resize — slice 8).
       (`DECSET_DECNCSM` needs `--xterm-winops` and is skipped.)
     - ✅ 6d: the reporting cluster — DECRQSS DECSTBM (`r` → `1$r Pt;Pb r`) and SGR
       (`m` → `1$r <pen> m`, the pen's op list led by a `0` reset, sharing
       `parser::sgr_op_param` with the `Sgr` dump); plus DECRQM for the legacy
       modes ghost doesn't act on. A new `ModeReport` enum
       (Set/Reset/PermanentlySet/PermanentlyReset/Unrecognized) replaces the
       `Option<bool>` mode reports so DECRPM can answer 3/4: inert DEC modes
       (DECSCLM/DECSCNM/DECPFF/DECPEX/DECNRCM/DECNKM/DECBKM) and ANSI KAM/SRM
       round-trip their 1/2 bit (the DEC ones via the non-display `tracked_modes`
       set, KAM/SRM via dedicated fields); the legacy graphic/format modes (ANSI
       GATM/…/EBM, DEC DECHCCM) report 4 permanently-reset. DECCOLM's `?3` bit is
       tracked apart from the physical column count (`column_mode_132`) so a grid
       the window later reconciles to some other width can't defeat it — and it is
       what tells RIS whether the width is the program's (slice 8). `DECRQM` **33/0**;
       `DECRQSS_DECSTBM`/`DECRQSS_SGR` pass. Still unreported: DECRQSS DECSCA (part
       of selective erase) and the niche selectors (DECSCL/DECSLPP/DECSNLS/…).
- **Rectangular area operations** — ✅ **DONE** (slice 9). DECSERA already had the
  shape of it, so `rect_bounds` (origin-relative coordinates, clamped to the
  addressable region, margins deliberately *not* confining it — that's what the
  `*_ignoresMargins` tests pin) was pulled out of it and the rest followed:
  **DECERA** (`$ z`) is DECSERA with a different erase guard (it clears
  DEC-protected cells; both spare the ISO guarded area, as plain ED/EL do);
  **DECFRA** (`$ x`) fills with `Pch` — `Line::clear` generalised to `Line::fill`,
  so the guard and wide-glyph edge-mending carry over, and an out-of-range fill
  character is ignored rather than printed; **DECCRA** (`$ v`) copies, source and
  destination free to overlap (`Buffer::copy_rect` snapshots every source row
  first), the destination a *corner* whose copy is clipped to what fits. Page
  params are parsed and ignored — one page. None of them moves the cursor.
  `DECCRA` 10/0, `DECERA` 7/0, `DECFRA` 7/0. (DECSACE/DECCARA/DECRARA have no
  esctest coverage and are not implemented.)
- **Margin-aware odds and ends** — ✅ **DONE** (slice 10). **DECFI**/**DECBI**
  (`ESC 9`/`ESC 6`): at the right/left margin of the box its contents shift by a
  column (`delete_columns`/`insert_columns`, from DECIC/DECDC) and the cursor
  holds; elsewhere it just steps, outside the margins included, and is ignored at
  the screen edge. **CNL/CPL**: a vertical move plus a *carriage return* — which
  goes to the left margin, not column 1 — so they call `cr()` now (which also
  clears the pending wrap they used to leave armed). **DECALN**: resets the margins
  on both axes and homes the cursor before filling; it's a whole-screen pattern,
  nothing may confine it. `DECFI` 5/0, `DECBI` 5/0, `CNL` 5/0, `CPL` 5/0,
  `DECALN` 3/0.
- **GUI DECCOLM window resize** — ✅ **DONE** (slice 8). The grid used to be the
  window's alone: `TerminalModel` snapped the screen back after every feed, so a
  DECCOLM self-resize survived only until the next window event. Now the model
  *follows* the program — it adopts the new grid, emits `Cmd::ResizeWindow` (the
  shell calls winit's `request_inner_size` at the pixel size that grid needs) and
  `Cmd::Resize` (the child gets its SIGWINCH, as after xterm's DECCOLM). The
  window may clamp or refuse the request; whatever size it reports next arrives as
  a `UiEvent::Resize` and wins, which is the fallback the old snap-back used to be.
  Removing the snap-back exposed a real bug it had been hiding: RIS must leave
  132-column mode. `Terminal::hard_reset` now takes the grid back to 80 columns
  when — and only when — `column_mode_132` is set, so a `reset` in a 200-column
  window doesn't shrink it (xterm makes the same pair of checks).
  `DECSET_DECCOLM`, `DECSET_Allow80To132`, `RIS_ResetDECCOLM` pass.
- **OSC color** — ✅ **the protocol half is DONE** (slice 11). The indexed palette
  (OSC 4/104) and the dynamic colors (OSC 10–12/110–112) were already in; this
  adds the **special colors** — the color an app asks bold, underline, blink,
  reverse or italic text to be painted in. OSC 5 names them from 0 and OSC 105
  resets them; xterm addresses the same five through OSC 4 at `256 + c`, past the
  indexed palette, so the palette index widened to `u16` and the terminal routes
  it (`SPECIAL_COLOR_BASE`), with OSC 5 folding onto that one path. Each query is
  answered in the form it was asked in. Ghost *tracks* them but paints attribute
  text in the pen's own color — as xterm does with `colorBDMode` and friends off
  — so an unset one reads back the theme foreground.

  A prerequisite fell out of it: **XTGETTCAP** answered `DCS + q 436F ST` with a
  lowercased `1+r436f=…`, and esctest string-matches the name it sent. It was
  falling back to "16 colors" and addressing the special colors at `16 + c` —
  inside the palette, where the round-trips passed for the wrong reason. The
  reply now echoes the name hex for hex. `ChangeSpecialColor` 12/0,
  `ResetSpecialColor` 4/1, `ChangeColor`/`ChangeDynamicColor` clean but for the
  color-space specs below.

**Still open** (the 81 failures, biggest first — measured with a full
`INCLUDE='.'` sweep, so these counts are honest):

- **X11 color-space specs** (21): the `rgbi:` / `CIEXYZ:` / `CIEuvY:` / `CIExyY:`
  / `CIELab:` / `CIELuv:` / `TekHVC:` forms of a color spec, across `ChangeColor`,
  `ChangeSpecialColor` and `ChangeDynamicColor` (the `rgb:` and `#rgb` forms every
  real program uses all pass). **Deliberately not implemented.** xterm hands these
  to `XParseColor`, which converts them through Xlib's Xcms using the *screen's*
  color characterization — and with no `XDCCC_LINEAR_RGB_*` properties on the root
  window (nobody runs `xcmsdb`), Xlib falls back to a built-in default in
  `src/xcms/LRGB.c`: the measured gamma tables of a **1990 Tektronix CRT**. That is
  where esctest's expected values come from — `rgbi:0.5/0.5/0.5` → `c1c1/bbbb/bbbb`
  is a reverse lookup through three per-channel CRT tables, not a gamma formula.
  Matching them bit-for-bit means porting those tables plus the TekHVC gamut-clip
  solver. No modern terminal does (kitty and Ghostty parse `rgbi:` with a naive
  `round(f * 255)` and would fail the same tests; alacritty, foot and wezterm don't
  parse it at all). Revisit only if a real program is found to emit one.
- **`ResetSpecialColor_Dynamic`** (1): asserts that OSC 110 restores the
  foreground esctest itself set with OSC 10 in its per-test `reset()` (it sets
  `#000`), i.e. that a reset returns the *last set* color. Ghost takes OSC 110
  back to the theme's foreground, which is what the user configured and what the
  other terminals do. The test bakes in an assumption about xterm's startup
  resources; not a gap we intend to close.
- **XTWINOPS** (~28): window ops — iconify, position, maximize, the title stack.
  `Cmd::ResizeWindow` (slice 8) is the plumbing a real `CSI 8;h;w t` would need,
  but several of these need a window manager the headless harness can't drive.
- **DECDSR** (~11): the niche device-status reports (printer, keyboard, locator).
- **DECRQSS** (6): the selectors we don't answer —
  DECSACE/DECSASD/DECSCL/DECSLPP/DECSNLS/DECSSDT.
- **DA / DA2 / DECID** (5): device-attribute strings.
- Odds and ends: `XtermSave`, `SCORC`, `DECRC`, `DECSET_ALTBUF`/`MoreFix`,
  `RIS_ResetTitleMode`, `DECSET_TiteInhibit`.

**Note on `--include`:** the regex is `re.search` over `Class.test_name`, so
`EL` also matches `CIELab`/`CIELuv`. Anchor when you mean a family — `^EL` /
`ELTests`.

**Blind spot:** esctest cannot test focus reporting, mouse encoding, or paste —
it can't drive those windowed-only inputs. Those stay covered by the
`ghost-ui-core` model tests.
