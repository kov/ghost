# Content-aware selection

Design for double-click selection in ghost: word characters, wrapped lines,
URLs, and content-aware rules. Written 2026-08-08; phases land in order.

## Goal

1. Double-clicking `test_for_selection-none` selects the whole token (today it
   stops at `_` and `-`).
2. Double-clicking a URI selects the whole URI, even when it wraps across rows
   or contains `?`, `&`, `=`.
3. Content-aware trimming: double-clicking a diff path `a/src/foo.rs` selects
   `src/foo.rs`, without the `a/` or `b/` prefix.

## Prior art, and what we take from each

Two distinct mechanisms exist in the wild, and we want both.

**Character-set layer.** GNOME Terminal / VTE: word = Unicode alphanumeric plus
a configurable *exceptions* string, default `-#%&+,./=?@\_~·`
(`WORD_CHAR_EXCEPTIONS_DEFAULT` in `vte.cc`). kitty does the same with an
inclusion list (`select_by_word_characters`, default `@-./_~?&=%+#`). Alacritty
and foot invert it into a delimiter list (`semantic_escape_chars`,
`word-delimiters`). Equivalent power. We take VTE's spelling and default.

Note VTE's default omits `:` — so the char set alone can never select
`https://…` whole. That is exactly why the second layer exists, in GNOME
Terminal too.

**Content-aware layer.**

- *iTerm2 Smart Selection* — a rule set of regexes with precision levels. For
  each rule it finds the longest match *containing the clicked cell*, scores
  candidates by length × precision coefficient, highest wins. We take the
  scoring/arbitration model.
- *wezterm `hyperlink_rules`* — `{regex, format, highlight}`, where
  `highlight = N` selects only capture group N. Their shipped default already
  uses it to strip surrounding parens from a URL. We take `highlight` as the
  answer to `a/` and `b/`: match wide, select narrow.
- *kitty URL detection* — deliberately **not** regex. `url_prefixes` gives the
  schemes to anchor on; from a match it extends outward over the URL-legal
  character class minus `url_excluded_characters`. Implemented over cells, so
  wrapping is handled by construction. We take this for URLs specifically:
  URL regexes are notoriously bad at the tail (trailing `)`, `.`, `,`) and this
  is both faster and easier to tune.
- *alacritty hints* — `regex-automata` lazy DFAs, four of them (forward +
  reverse), matched directly against the grid iterator rather than a
  materialized string: scan forward to find a match end, then run the reverse
  DFA to pin the exact start. Handles scrollback and wide cells natively. It is
  the right architecture for regex *search over scrollback* — see Roadmap.
  It is overkill for click-time selection, which only needs the logical line
  around the click.

The wrapped-URL case is the one nobody solved cleanly — alacritty has it open as
a bug. Handling it is a reason to do the logical-line work properly rather than
as a nicety.

## Resolution ladder

At a double-click, first hit wins:

1. **OSC 8 hyperlink run** — if the cell's pen carries a `link_id`, select the
   maximal contiguous run with that id. Exact where a regex would guess, and
   nearly free: cells already carry the id and `hover_underlines()` already
   walks runs.
2. **Smart candidates** — the URL scanner (3a) and the regex rules (3b) each
   emit candidates. Score = span length × precision tier weight; highest wins.
   Tiers keep a short high-precision match from losing to a long sloppy one.
3. **Word characters** — alphanumeric + exceptions string.
4. **Single cell** — blanks, as today.

## Phases

### Phase 1 — word characters

`is_word_char` becomes alphanumeric + a configurable exceptions string,
defaulting to VTE's `-#%&+,./=?@\_~·`. New `ui.toml` key under `[input]`;
plumbed like `padding` (shell setter onto `TerminalView`, re-applied by
`App::reload_config`, mirrored onto warm views). `word_at` keeps scanning cells,
so wide-char handling is unchanged.

Sites: `ghost-ui-core/src/terminal.rs` `is_word_char` / `word_at`;
`ghost-ui/src/config.rs` `[input]`; `App::reload_config` in `ghost-ui/src/lib.rs`;
README config sample.

Tests (red first): double-click selects across `_` and `-`; a path selects
whole; an overridden exceptions string is honoured; the drawn selection covers
the whole token.

### Phase 2 — soft wrap visible to the UI

`Line::wrapped` is `pub(crate)` in ghost-term with no accessor, so ghost-vt and
ghost-ui-core cannot tell a soft wrap from a hard newline — which is also why
`selection_text` joins every row with `\n`.

Add `pub fn is_wrapped()` (the flag is on the line that *continues*, so row `n`
says whether row `n + 1` is its continuation), then teach the two consumers:

- `word_at` walks off either row edge into the neighbouring row when the fold
  joins them, entirely in **cell** space — no text is materialized, so the
  char-index/column divergence that wide characters cause never arises.
- `selection_text` drops the newline between a wrapped row and its continuation.

A soft-wrapped line filled the width by definition, so trailing blanks on one
are padding — the gap left when a wide glyph would have straddled the edge and
moved down whole. `content_len` treats them as layout: word walks step over them
into the continuation row, and copied text leaves them out.

**Deviation from the original plan:** no `LogicalLine` / offset table was built.
Both consumers here work in cell space, where an offset table has nothing to
map. It lands in 3b instead, with its first real consumer — a regex needs a
joined string, and *that* is what needs char-index → `(row, col)` built from
cells.

Tests: a token straddling a wrap selects both halves (from either side); a hard
newline still stops it; copying across the wrap yields no `\n` while copying
across a real newline keeps it; a wide char moved down at the wrap point keeps
cells and text aligned.

Not included: triple-click still selects the physical row, not the logical
line. Worth doing, tracked as a follow-up.

### Phase 3a — URL selection (kitty-style, no regex)

Scheme-anchored scanner: find a prefix from `URL_PREFIXES` at or before the
click, extend right over the URL-legal character class, then trim the tail —
sentence punctuation, and closing brackets with no opener inside the match, so a
*balanced* pair in a wiki-style path survives. A scheme must start a token, so
`xhttps://…` is not a link. The longest match covering the click wins, so
`http://` cannot shadow the `https://` it sits inside. Wrapped URIs work by
construction; OSC 8 runs win outright.

`LogicalCell` + `logical_line` land here rather than in 3b: the scanner is the
first thing needing a joined view, so this is where the offset table earns its
place. One entry per printed character — `(row, col, width, char, link_id)`,
built from cells. The width is what lets a match ending on a wide character
cover the whole glyph; the link id is what makes the OSC 8 rung a walk over the
same array rather than a second traversal.

Tests: a URI selected whole from a click in the middle, query included; the same
wrapped across two rows; `(…)`, a trailing `.` and a trailing `,` left out while
a balanced `(b)` inside the path is kept; a URI beating the plain word under the
pointer; an OSC 8 run selecting display text no word walk would join; and plain
words still answered by the word walk.

Not included: a double-click *drag* still extends by words rather than smart
units (iTerm2 behaves the same way), and Ctrl-click still only opens OSC 8
links — the scanner could feed it a plain URI too, a natural follow-up.

### Phase 3b — regex rules with `highlight`

`regex` lands as a direct dependency here (1.13.1). Rules are
`{ re, group, precision }` — wezterm's `highlight` under a plainer name: the
rule matches wide and the named capture group is what gets selected, so the `a/`
that makes a diff path recognizable is not the thing that gets pasted.
Containment is tested against the **whole match**, so clicking the `a/` itself
still resolves to the path it introduces.

Two rules ship, both hardcoded: `\b[ab]/(\S+)` for diff paths and
`([^\s:]+):(\d+)(?::(\d+))?` for `file:line[:col]`. The config surface follows
once the shape has settled in use.

Arbitration moved out of the `or_else` chain into `smart_span`: each rung offers
a candidate with a precision tier — hyperlink 3, URI 2, rule 1 — and the best
`(precision, whole-match length)` wins. Tier before length is what the design
needs: with `/` a word character since phase 1, the word walk's `a/src/foo.rs`
is *longer* than the rule's `src/foo.rs`, so length alone would undo the whole
phase. The same ordering keeps a URI inside a diff line a URI.

The regex needs a string while everything else is in cell indices, so
`rule_spans` builds one alongside a byte → cell-index table (one past the end
maps one past the last cell, keeping an exclusive match end exclusive).

Tests: `a/src/foo.rs` and `b/src/foo.rs` select `src/foo.rs`; clicking the `a/`
does too; `src/foo.rs:12:5` selects the path; a URL inside a diff line still
selects as a URL (the arbitration test).

## Roadmap (not in these phases)

- **Bidirectional DFA regex over the grid** for scrollback search — alacritty's
  `RegexSearch` model, four `regex-automata` lazy DFAs matching against the grid
  iterator instead of a materialized string. Deliberately out of scope for
  click-time selection, but this is the intended architecture when we do
  find-as-you-type / regex search over scrollback. `ghost-vt/src/search.rs`
  (substring, over recordings) is the thing it would grow into.
- Config surface for smart-selection rules.
