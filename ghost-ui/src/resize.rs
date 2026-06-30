//! Resize coalescing — defer the expensive relayout during an interactive resize.
//!
//! A window resize is the costliest event the shell handles. In a single terminal
//! it reflows the screen and resizes the child PTY (a SIGWINCH the program usually
//! answers with a full repaint); in the fleet view every tile's preview texture
//! re-renders at the new size. Doing all that at every pixel of a drag pegs a
//! software rasterizer (lavapipe) and floods the children with resizes.
//!
//! Only a *drag* — a rapid stream of resizes — is worth coalescing. An isolated
//! resize (a maximize, a tiling snap, an un-maximize, or the very first grab of a
//! drag) is applied immediately and crisply: deferring it buys nothing (there is
//! no stream to collapse) and the stretch-blit + delayed surface/model resize only
//! shows a stale frame and can race the compositor's resize handshake. So
//! [`ResizeCoalescer::note`] returns [`Step::CommitNow`] for an isolated resize and
//! [`Step::Defer`] once resizes are streaming.
//!
//! For the deferred stream it records the latest requested size and reports *when*
//! to commit it via [`ResizeCoalescer::poll`]: once the drag settles (no new size
//! for [`SETTLE_MS`]), or — during a long continuous drag — at most once per
//! [`MAX_MS`], so the content still refreshes instead of freezing. Between commits
//! the shell stretch-blits the last crisp frame (see the renderer's snapshot path),
//! a single textured quad that stays cheap no matter how many tiles are on screen.
//!
//! It is pure (driven by an external millisecond clock) so its behaviour is
//! unit-testable without a window or GPU, exactly like [`crate::pacer`].

/// Commit once the window has held a size this long without changing (~5 frames):
/// short enough that releasing the drag snaps to crisp almost immediately, long
/// enough that an ordinary drag's stream of sizes coalesces into one relayout.
pub const SETTLE_MS: u64 = 80;

/// During a *continuous* drag (sizes never pause long enough to settle), commit a
/// real relayout at least this often so the content keeps refreshing rather than
/// staying frozen-stretched. Generous, so a normal quick resize never hits it.
pub const MAX_MS: u64 = 250;

/// A resize counts as part of a drag only if it arrives within this long of the
/// previous one. Comfortably above a drag's per-frame cadence (~8–16 ms) yet well
/// below the gap between deliberate, separate actions (maximize, then un-maximize),
/// so one-shot resizes are recognised as isolated and applied at once.
pub const DRAG_GAP_MS: u64 = 100;

/// A resize target: the new surface size in physical pixels and the device scale.
pub type Target = (u32, u32, f64);

/// What to do with a resize the instant it arrives.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    /// Apply the relayout now — this resize stands alone (maximize, snap,
    /// un-maximize, or a drag's first grab), so there is nothing to coalesce.
    CommitNow(Target),
    /// A drag is streaming: stretch-blit and defer the relayout until [`poll`]
    /// reports the gesture has settled.
    ///
    /// [`poll`]: ResizeCoalescer::poll
    Defer,
}

/// Coalesces a burst of resize events into occasional commits — see the module
/// docs. Cheap to copy and `Default` (a zero-interval coalescer commits eagerly,
/// which no caller uses; construct with [`ResizeCoalescer::new`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct ResizeCoalescer {
    settle_ms: u64,
    max_ms: u64,
    drag_gap_ms: u64,
    /// The latest requested size awaiting commit, if any.
    pending: Option<Target>,
    /// When the most recent size was noted (`None` until the first ever), used for
    /// both the settle check and the is-this-a-drag gap.
    last_note_ms: Option<u64>,
    /// When the current uncommitted gesture began (for the max-interval check).
    anchor_ms: u64,
}

impl ResizeCoalescer {
    pub fn new(settle_ms: u64, max_ms: u64, drag_gap_ms: u64) -> Self {
        Self {
            settle_ms,
            max_ms,
            drag_gap_ms,
            pending: None,
            last_note_ms: None,
            anchor_ms: 0,
        }
    }

    /// Record that the window was resized to `(w, h)` at device `scale` at
    /// `now_ms`, and decide what to do with it. An isolated resize (none pending
    /// and the previous one not recent) returns [`Step::CommitNow`] for the caller
    /// to apply at once; a resize that continues a rapid stream returns
    /// [`Step::Defer`], is stored as the latest pending size, and anchors the
    /// max-interval clock on the first deferred size of the gesture.
    pub fn note(&mut self, now_ms: u64, w: u32, h: u32, scale: f64) -> Step {
        let target = (w, h, scale);
        let continuing = self.pending.is_some()
            || self
                .last_note_ms
                .is_some_and(|t| now_ms.saturating_sub(t) <= self.drag_gap_ms);
        self.last_note_ms = Some(now_ms);
        if !continuing {
            self.pending = None;
            return Step::CommitNow(target);
        }
        if self.pending.is_none() {
            self.anchor_ms = now_ms;
        }
        self.pending = Some(target);
        Step::Defer
    }

    /// The size to commit now, if any: the drag has settled (no new size for
    /// `settle_ms`) or the max interval since the gesture began has elapsed.
    /// Returns the target and clears the pending size; otherwise returns `None`.
    pub fn poll(&mut self, now_ms: u64) -> Option<Target> {
        let target = self.pending?;
        // `pending` is `Some`, so `last_note_ms` was set when it was stored.
        let last = self.last_note_ms.unwrap_or(now_ms);
        let settled = now_ms.saturating_sub(last) >= self.settle_ms;
        let maxed = now_ms.saturating_sub(self.anchor_ms) >= self.max_ms;
        if settled || maxed {
            self.pending = None;
            // Re-anchor so a *mid-drag* (maxed) commit schedules the next refresh a
            // full interval out; after a settle commit `pending` is `None`, so the
            // next gesture's first `note` re-anchors anyway.
            self.anchor_ms = now_ms;
            Some(target)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coalescer() -> ResizeCoalescer {
        ResizeCoalescer::new(80, 250, 100)
    }

    #[test]
    fn nothing_to_commit_without_a_resize() {
        let mut c = coalescer();
        assert_eq!(c.poll(0), None);
        assert_eq!(c.poll(10_000), None);
    }

    #[test]
    fn an_isolated_resize_commits_immediately() {
        let mut c = coalescer();
        // A one-shot resize (maximize / snap / un-maximize): apply it at once, with
        // nothing left for `poll` to settle-commit later.
        assert_eq!(
            c.note(500, 1920, 1080, 1.0),
            Step::CommitNow((1920, 1080, 1.0))
        );
        assert_eq!(c.poll(600), None);
        assert_eq!(c.poll(10_000), None);
    }

    #[test]
    fn a_later_isolated_resize_commits_immediately_again() {
        let mut c = coalescer();
        assert_eq!(
            c.note(500, 1000, 600, 1.0),
            Step::CommitNow((1000, 600, 1.0))
        );
        // Much later (e.g. maximize, then un-maximize): still recognised as isolated.
        assert_eq!(
            c.note(2000, 800, 480, 1.0),
            Step::CommitNow((800, 480, 1.0))
        );
        assert_eq!(c.poll(2100), None);
    }

    #[test]
    fn a_drag_commits_the_first_step_then_defers_and_coalesces() {
        let mut c = coalescer();
        // The grab's first size lands immediately (one crisp relayout)...
        assert_eq!(
            c.note(500, 1000, 600, 1.0),
            Step::CommitNow((1000, 600, 1.0))
        );
        // ...then the rapid stream that follows defers and coalesces.
        assert_eq!(c.note(516, 1010, 600, 1.0), Step::Defer);
        assert_eq!(c.note(532, 1020, 600, 1.0), Step::Defer);
        // Still moving — no commit yet (settle resets on every note).
        assert_eq!(c.poll(560), None);
        // 80 ms after the *last* note, the most recent size (only) commits.
        assert_eq!(c.poll(612), Some((1020, 600, 1.0)));
        assert_eq!(c.poll(613), None, "one commit drains the pending size");
    }

    #[test]
    fn a_pause_resets_the_gesture_so_the_next_resize_is_immediate() {
        let mut c = coalescer();
        // A drag: first step immediate, the next deferred and settle-committed.
        assert_eq!(
            c.note(500, 1000, 600, 1.0),
            Step::CommitNow((1000, 600, 1.0))
        );
        assert_eq!(c.note(516, 1100, 600, 1.0), Step::Defer);
        assert_eq!(c.poll(596), Some((1100, 600, 1.0)));
        // After a pause well beyond the drag gap, a fresh resize is isolated again.
        assert_eq!(
            c.note(900, 1200, 700, 1.0),
            Step::CommitNow((1200, 700, 1.0))
        );
        assert_eq!(c.poll(1000), None, "nothing deferred to settle-commit");
    }

    #[test]
    fn a_long_continuous_drag_refreshes_at_the_max_interval() {
        let mut c = coalescer();
        // Sizes arrive every 16 ms and never pause. The first is immediate; the
        // deferred stream then refreshes via the max interval since it never settles.
        assert_eq!(c.note(0, 1000, 600, 1.0), Step::CommitNow((1000, 600, 1.0)));
        let mut committed = None;
        for step in 1..40u64 {
            let t = step * 16;
            assert_eq!(c.note(t, 1000 + step as u32, 600, 1.0), Step::Defer);
            // Poll the way the event loop does, a touch after the note.
            if let Some(target) = c.poll(t + 1) {
                committed = Some((t, target));
                break;
            }
        }
        let (t, target) =
            committed.expect("a continuous drag must still commit via the max interval");
        // The deferred stream anchors at the first deferred size (t = 16), so the
        // first max-interval refresh lands ~MAX_MS after that.
        assert!(
            (16 + MAX_MS..16 + MAX_MS + 16).contains(&t),
            "first refresh should land around the max interval, not before; got {t}ms"
        );
        // It committed the size current at that moment, not the stale anchor size.
        assert_eq!(target.0, 1000 + (t / 16) as u32);
    }

    #[test]
    fn settle_takes_priority_when_a_drag_stops_before_the_max_interval() {
        // A short drag (under MAX_MS) that then stops must commit via settle, once.
        let mut c = coalescer();
        assert_eq!(c.note(0, 0, 0, 1.0), Step::CommitNow((0, 0, 1.0)));
        for step in 1..5u64 {
            let t = step * 16; // last note at 64ms (< 250ms max)
            assert_eq!(c.note(t, step as u32, 0, 1.0), Step::Defer);
            assert_eq!(c.poll(t + 1), None, "still dragging at {t}ms");
        }
        // Stops at 64ms; settle fires at 64+80 = 144ms (< the 16+250 max).
        assert_eq!(c.poll(143), None);
        assert_eq!(c.poll(144), Some((4, 0, 1.0)));
        assert_eq!(c.poll(300), None, "no spurious second commit");
    }
}
