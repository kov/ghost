//! Damage-claim audit: every feed that changes a viewport row's *rendered*
//! content must report that row in `Changes::lines` (the per-feed dirty-row
//! hint `ghost_vt::Screen::feed` forwards and the renderer's `TermDamage`
//! ultimately trusts). An under-report here leaves a stale row in the
//! foreground texture until something forces a full redraw — the recurring
//! "fleet preview live, foreground frozen" stall.
//!
//! Ground truth mirrors `ghost_render::rows_differ_outside`: a row "changed"
//! when its cells (char + width + pen) differ across the feed. The claim must
//! be a superset ("these rows definitely changed" is the documented contract —
//! over-reporting is fine, missing a changed row never is). Cursor-only
//! changes are deliberately out of scope: the cursor travels on a separate,
//! diffed channel (`ghost_vt::Screen::cursor_damage`), not in the row hint.

use ghost_term::{Cell, Vt};
use proptest::prelude::*;

/// The rendered viewport as raw cells. `Line::wrapped` is excluded on
/// purpose: it drives reflow, not pixels.
fn viewport(vt: &Vt) -> Vec<Vec<Cell>> {
    vt.view().map(|line| line.cells().to_vec()).collect()
}

/// Feed `seq` as one feed→render cycle; return the claimed dirty rows and the
/// rows whose cells actually changed.
fn feed_cycle(vt: &mut Vt, seq: &str) -> (Vec<usize>, Vec<usize>) {
    let before = viewport(vt);
    let claimed = vt.feed_str(seq).lines;
    let after = viewport(vt);
    assert_eq!(
        before.len(),
        after.len(),
        "viewport height changed mid-audit (no resize op was fed)"
    );
    let changed = before
        .iter()
        .zip(&after)
        .enumerate()
        .filter(|(_, (b, a))| b != a)
        .map(|(row, _)| row)
        .collect();
    (claimed, changed)
}

/// Assert the claim covers every changed row, returning both sets for
/// follow-up assertions.
#[track_caller]
fn assert_claim_covers(vt: &mut Vt, seq: &str) -> (Vec<usize>, Vec<usize>) {
    let (claimed, changed) = feed_cycle(vt, seq);
    let missed: Vec<usize> = changed
        .iter()
        .copied()
        .filter(|row| !claimed.contains(row))
        .collect();
    assert!(
        missed.is_empty(),
        "TermDamage under-report feeding {seq:?}: rows {missed:?} changed \
         outside the claim {claimed:?} (changed: {changed:?})"
    );
    (claimed, changed)
}

/// A terminal with `setup` applied and the dirty state drained, so the next
/// feed's claim is exactly one cycle's damage.
fn prepared(cols: usize, rows: usize, setup: &str) -> Vt {
    let mut vt = Vt::new(cols, rows);
    vt.feed_str(setup);
    vt
}

// --- printing ---

#[test]
fn print_covers_its_row() {
    let mut vt = prepared(8, 4, "\x1b[2;1H");
    let (claimed, changed) = assert_claim_covers(&mut vt, "hi");
    assert_eq!(changed, vec![1]);
    assert_eq!(claimed, vec![1]);
}

#[test]
fn autowrap_print_covers_both_rows() {
    // 'd' fills row 0's last column, 'e' wraps onto row 1: both rows change.
    let mut vt = prepared(4, 3, "abc");
    let (claimed, changed) = assert_claim_covers(&mut vt, "de");
    assert_eq!(changed, vec![0, 1]);
    assert_eq!(claimed, vec![0, 1]);
}

#[test]
fn wide_char_wrapping_at_last_column_covers_the_row_it_lands_on() {
    // A double-width char can't fit the single trailing cell of row 0, so it
    // relocates to row 1 (row 0's cells stay as they were).
    let mut vt = prepared(4, 3, "abc");
    assert_claim_covers(&mut vt, "日");
}

#[test]
fn wrap_scroll_at_bottom_covers_the_whole_scrolled_region() {
    // Printing past the last column of the bottom row wraps AND scrolls:
    // every viewport row's content shifts.
    let mut vt = prepared(4, 3, "r0\r\nr1\r\nabc");
    assert_claim_covers(&mut vt, "de");
}

#[test]
fn print_without_autowrap_covers_the_overwritten_row() {
    // DECAWM off: printing at the right edge keeps overwriting the last cell.
    let mut vt = prepared(4, 3, "\x1b[?7labc");
    assert_claim_covers(&mut vt, "xyz");
}

#[test]
fn print_over_wide_head_covers_row() {
    // Overwriting a wide char's head also blanks its tail — same row.
    let mut vt = prepared(8, 3, "日本\x1b[1;1H");
    assert_claim_covers(&mut vt, "x");
}

#[test]
fn print_over_wide_tail_covers_row() {
    // Overwriting a wide char's tail blanks the head to its left — same row.
    let mut vt = prepared(8, 3, "日本\x1b[1;2H");
    assert_claim_covers(&mut vt, "x");
}

#[test]
fn insert_mode_print_covers_row() {
    let mut vt = prepared(8, 3, "abcdef\x1b[4h\x1b[1;2H");
    assert_claim_covers(&mut vt, "XY");
}

#[test]
fn rep_covers_row() {
    let mut vt = prepared(8, 3, "ab");
    assert_claim_covers(&mut vt, "\x1b[3b");
}

#[test]
fn combining_mark_outside_placeholder_run_covers_row() {
    // Without combining-mark support a zero-width mark occupies its own cell.
    let mut vt = prepared(8, 3, "a");
    assert_claim_covers(&mut vt, "\u{0305}");
}

// --- scrolling: full viewport and DECSTBM regions ---

#[test]
fn lf_scroll_at_bottom_covers_whole_viewport() {
    let mut vt = prepared(4, 3, "r0\r\nr1\r\nr2");
    let (claimed, _) = assert_claim_covers(&mut vt, "\r\n");
    assert_eq!(claimed, vec![0, 1, 2]);
}

#[test]
fn region_scroll_su_covers_region_and_spares_the_rest() {
    // DECSTBM rows 2..4 (0-based 1..=3) of a 5-row screen; SU scrolls only the
    // region. Rows 0 and 4 must not change; the claim must cover the region.
    let mut vt = prepared(4, 5, "r0\r\nr1\r\nr2\r\nr3\r\nr4\x1b[2;4r");
    let (_, changed) = assert_claim_covers(&mut vt, "\x1b[S");
    assert!(
        !changed.contains(&0) && !changed.contains(&4),
        "rows outside the scroll region must not change, got {changed:?}"
    );
}

#[test]
fn region_reverse_scroll_sd_covers_region() {
    let mut vt = prepared(4, 5, "r0\r\nr1\r\nr2\r\nr3\r\nr4\x1b[2;4r");
    assert_claim_covers(&mut vt, "\x1b[T");
}

#[test]
fn ri_at_top_margin_covers_region() {
    let mut vt = prepared(4, 5, "r0\r\nr1\r\nr2\r\nr3\r\nr4\x1b[2;4r\x1b[2;1H");
    assert_claim_covers(&mut vt, "\x1bM");
}

#[test]
fn lf_at_bottom_margin_covers_region() {
    let mut vt = prepared(4, 5, "r0\r\nr1\r\nr2\r\nr3\r\nr4\x1b[2;4r\x1b[4;1H");
    assert_claim_covers(&mut vt, "\n");
}

#[test]
fn region_pinned_to_top_scroll_covers_region() {
    // top margin 0 with a bottom margin above the last row exercises the
    // scrollback-insert branch of Buffer::scroll_up.
    let mut vt = prepared(4, 5, "r0\r\nr1\r\nr2\r\nr3\r\nr4\x1b[1;3r\x1b[3;1H");
    let (_, changed) = assert_claim_covers(&mut vt, "\n");
    assert!(
        !changed.contains(&3) && !changed.contains(&4),
        "rows below the region must not change, got {changed:?}"
    );
}

#[test]
fn scroll_with_capped_scrollback_covers_viewport() {
    let mut vt = Vt::builder().size(4, 3).scrollback_limit(1).build();
    vt.feed_str("a\r\nb\r\nc\r\nd\r\ne");
    assert_claim_covers(&mut vt, "\r\nf");
}

// --- insert/delete line and char ---

#[test]
fn il_covers_cursor_row_to_bottom_margin() {
    let mut vt = prepared(4, 5, "r0\r\nr1\r\nr2\r\nr3\r\nr4\x1b[2;1H");
    assert_claim_covers(&mut vt, "\x1b[2L");
}

#[test]
fn dl_covers_cursor_row_to_bottom_margin() {
    let mut vt = prepared(4, 5, "r0\r\nr1\r\nr2\r\nr3\r\nr4\x1b[2;1H");
    assert_claim_covers(&mut vt, "\x1b[M");
}

#[test]
fn il_below_bottom_margin_covers_changed_rows() {
    // Cursor below the scroll region: IL shifts cursor.row..rows.
    let mut vt = prepared(4, 5, "r0\r\nr1\r\nr2\r\nr3\r\nr4\x1b[1;2r\x1b[4;1H");
    assert_claim_covers(&mut vt, "\x1b[L");
}

#[test]
fn ich_covers_row_including_wide_char_splits() {
    // ICH shifts a wide pair right; a tail stranded at the shift point or the
    // line end is blanked — all on the cursor row.
    let mut vt = prepared(6, 3, "a日b\x1b[1;1H");
    let (claimed, changed) = assert_claim_covers(&mut vt, "\x1b[2@");
    assert_eq!(changed, vec![0]);
    assert_eq!(claimed, vec![0]);
}

#[test]
fn dch_covers_row_including_wide_char_splits() {
    let mut vt = prepared(6, 3, "a日b\x1b[1;2H");
    assert_claim_covers(&mut vt, "\x1b[P");
}

#[test]
fn ech_covers_row() {
    let mut vt = prepared(6, 3, "abcdef\x1b[1;2H");
    assert_claim_covers(&mut vt, "\x1b[3X");
}

// --- erasing ---

#[test]
fn el_variants_cover_row_even_when_only_the_pen_changes() {
    // With a colored background pen, EL changes blank cells' rendering (bg
    // fill) without changing any glyph — still a change.
    for el in ["\x1b[K", "\x1b[1K", "\x1b[2K"] {
        let mut vt = prepared(6, 3, "abcdef\x1b[1;3H\x1b[41m");
        let (claimed, changed) = assert_claim_covers(&mut vt, el);
        assert_eq!(changed, vec![0], "{el:?} ground truth");
        assert_eq!(claimed, vec![0], "{el:?} claim");
    }
}

#[test]
fn ed_below_covers_cursor_row_to_bottom() {
    let mut vt = prepared(4, 4, "r0\r\nr1\r\nr2\r\nr3\x1b[2;2H");
    assert_claim_covers(&mut vt, "\x1b[J");
}

#[test]
fn ed_above_covers_top_to_cursor_row() {
    let mut vt = prepared(4, 4, "r0\r\nr1\r\nr2\r\nr3\x1b[3;2H");
    assert_claim_covers(&mut vt, "\x1b[1J");
}

#[test]
fn ed_all_covers_viewport() {
    let mut vt = prepared(4, 4, "r0\r\nr1\r\nr2\r\nr3");
    assert_claim_covers(&mut vt, "\x1b[2J");
}

// --- whole-screen transitions ---

#[test]
fn alt_screen_enter_and_leave_cover_viewport() {
    let mut vt = prepared(4, 3, "r0\r\nr1\r\nr2");
    let (claimed, _) = assert_claim_covers(&mut vt, "\x1b[?1049h");
    assert_eq!(claimed, vec![0, 1, 2], "enter claims the whole viewport");

    vt.feed_str("alt");
    assert_claim_covers(&mut vt, "\x1b[?1049l");
}

#[test]
fn ris_covers_viewport() {
    let mut vt = prepared(4, 3, "r0\r\nr1\r\nr2");
    let (claimed, _) = assert_claim_covers(&mut vt, "\x1bc");
    assert_eq!(claimed, vec![0, 1, 2]);
}

#[test]
fn decaln_covers_viewport() {
    let mut vt = prepared(4, 3, "");
    let (claimed, _) = assert_claim_covers(&mut vt, "\x1b#8");
    assert_eq!(claimed, vec![0, 1, 2]);
}

#[test]
fn resize_reports_every_row() {
    // Vt::resize's Changes must cover the reflowed viewport (the model layers
    // force whole-view damage on a resize anyway; this pins the core's part).
    let mut vt = prepared(8, 4, "aaaaaaaaaa\r\nbb");
    let claimed = vt.resize(6, 4).lines;
    assert_eq!(claimed, vec![0, 1, 2, 3], "narrower reflow claims all rows");

    let claimed = vt.resize(6, 6).lines;
    assert_eq!(
        claimed,
        vec![0, 1, 2, 3, 4, 5],
        "growing rows claims the new viewport"
    );
}

// --- ground-truth sanity: ops that change no cells ---

#[test]
fn cursor_moves_and_style_changes_touch_no_cells() {
    // These change the cursor or pen only; the *content* hint owes nothing
    // (the drawn cursor travels on ghost_vt's separate cursor-damage channel).
    for seq in [
        "\x1b[2;3H",
        "\x1b[A",
        "\x1b[B",
        "\x1b[C",
        "\x1b[D",
        "\x1b7",
        "\x1b8",
        "\x1b[31;1m",
        "\x1b[?25l",
        "\x1b[?25h",
        "\x1b[2 q",
        "\t",
        "\u{8}",
        "\r",
    ] {
        let mut vt = prepared(8, 4, "r0\r\nr1\r\nr2\r\nr3");
        let (_, changed) = feed_cycle(&mut vt, seq);
        assert_eq!(changed, Vec::<usize>::new(), "{seq:?} changed cells");
    }
}

// --- property sweep: any op stream's claim covers the cells it changed ---

/// One parser-level operation: text (narrow and wide), controls, and the CSI /
/// ESC sequences that move, scroll, insert, delete, erase, switch screens, and
/// set the modes that shape all of the above (DECSTBM, DECOM, DECAWM, IRM,
/// LNM, alt screen, DECTCEM).
fn op() -> impl Strategy<Value = String> {
    prop_oneof![
        "[ -~]{1,6}",
        Just("日本".to_string()),
        Just("é\u{0305}".to_string()),
        Just("\r\n".to_string()),
        Just("\n".to_string()),
        Just("\t".to_string()),
        Just("\u{8}".to_string()),
        Just("\x1bM".to_string()),
        Just("\x1bD".to_string()),
        Just("\x1bE".to_string()),
        Just("\x1b7".to_string()),
        Just("\x1b8".to_string()),
        Just("\x1b#8".to_string()),
        Just("\x1bc".to_string()),
        (1u16..=7, 1u16..=12).prop_map(|(r, c)| format!("\x1b[{r};{c}H")),
        (0u16..=7).prop_map(|n| format!("\x1b[{n}J")),
        (0u16..=3).prop_map(|n| format!("\x1b[{n}K")),
        (0u16..=6).prop_map(|n| format!("\x1b[{n}L")),
        (0u16..=6).prop_map(|n| format!("\x1b[{n}M")),
        (0u16..=6).prop_map(|n| format!("\x1b[{n}@")),
        (0u16..=6).prop_map(|n| format!("\x1b[{n}P")),
        (0u16..=6).prop_map(|n| format!("\x1b[{n}X")),
        (0u16..=6).prop_map(|n| format!("\x1b[{n}S")),
        (0u16..=6).prop_map(|n| format!("\x1b[{n}T")),
        (0u16..=6).prop_map(|n| format!("\x1b[{n}b")),
        (0u16..=6).prop_map(|n| format!("\x1b[{n}A")),
        (0u16..=6).prop_map(|n| format!("\x1b[{n}B")),
        (0u16..=12).prop_map(|n| format!("\x1b[{n}C")),
        (0u16..=12).prop_map(|n| format!("\x1b[{n}D")),
        (0u16..=12).prop_map(|n| format!("\x1b[{n}G")),
        (0u16..=7).prop_map(|n| format!("\x1b[{n}d")),
        (1u16..=6, 1u16..=6).prop_map(|(t, b)| format!("\x1b[{t};{b}r")),
        prop::sample::select(vec![6u16, 7, 25, 1047, 1048, 1049])
            .prop_flat_map(|m| prop::bool::ANY
                .prop_map(move |on| format!("\x1b[?{m}{}", if on { 'h' } else { 'l' }))),
        prop::sample::select(vec![4u16, 20]).prop_flat_map(|m| prop::bool::ANY
            .prop_map(move |on| format!("\x1b[{m}{}", if on { 'h' } else { 'l' }))),
        prop::sample::select(vec![0u16, 1, 4, 7, 31, 42]).prop_map(|n| format!("\x1b[{n}m")),
    ]
}

proptest! {
    /// The workhorse: drive the terminal into an arbitrary state, then audit
    /// one feed cycle — every viewport row whose cells changed must be in the
    /// claim. A shrunk failure here is a concrete core under-report.
    #[test]
    fn prop_claim_covers_every_changed_row(
        setup in prop::collection::vec(op(), 0..12),
        audited in prop::collection::vec(op(), 1..4),
    ) {
        let mut vt = Vt::builder().size(10, 5).scrollback_limit(3).build();
        vt.feed_str(&setup.concat());

        let seq = audited.concat();
        let before = viewport(&vt);
        let claimed = vt.feed_str(&seq).lines;
        let after = viewport(&vt);

        prop_assert_eq!(before.len(), after.len());
        for (row, (b, a)) in before.iter().zip(&after).enumerate() {
            if b != a {
                prop_assert!(
                    claimed.contains(&row),
                    "TermDamage under-report: setup {:?}, feeding {:?} changed row {} outside the claim {:?}",
                    setup.concat(), seq, row, claimed
                );
            }
        }
    }
}
