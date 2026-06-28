//! Resize coalescing — defer the expensive relayout during an interactive resize.
//!
//! A window resize is the costliest event the shell handles. In a single terminal
//! it reflows the screen and resizes the child PTY (a SIGWINCH the program usually
//! answers with a full repaint); in the fleet view every tile's preview texture
//! re-renders at the new size. Doing all that at every pixel of a drag pegs a
//! software rasterizer (lavapipe) and floods the children with resizes.
//!
//! [`ResizeCoalescer`] records the latest requested size and reports *when* to
//! actually commit it: once the drag settles (no new size for [`SETTLE_MS`]), or —
//! during a long continuous drag — at most once per [`MAX_MS`], so the content
//! still refreshes occasionally instead of freezing. Between commits the shell
//! stretch-blits the last crisp frame (see the renderer's snapshot path), which is
//! a single textured quad and stays cheap no matter how many tiles are on screen.
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

/// A resize target: the new surface size in physical pixels and the device scale.
pub type Target = (u32, u32, f64);

/// Coalesces a burst of resize events into occasional commits — see the module
/// docs. Cheap to copy and `Default` (a zero-interval coalescer commits eagerly,
/// which no caller uses; construct with [`ResizeCoalescer::new`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct ResizeCoalescer {
    settle_ms: u64,
    max_ms: u64,
    /// The latest requested size awaiting commit, if any.
    pending: Option<Target>,
    /// When the most recent size was noted (for the settle check).
    last_note_ms: u64,
    /// When the current uncommitted gesture began (for the max-interval check).
    anchor_ms: u64,
}

impl ResizeCoalescer {
    pub fn new(settle_ms: u64, max_ms: u64) -> Self {
        Self {
            settle_ms,
            max_ms,
            pending: None,
            last_note_ms: 0,
            anchor_ms: 0,
        }
    }

    /// Record that the window was resized to `(w, h)` at device `scale` at
    /// `now_ms`. Idempotently keeps only the latest size; the first note of a new
    /// gesture also anchors the max-interval clock.
    pub fn note(&mut self, now_ms: u64, w: u32, h: u32, scale: f64) {
        if self.pending.is_none() {
            self.anchor_ms = now_ms;
        }
        self.pending = Some((w, h, scale));
        self.last_note_ms = now_ms;
    }

    /// The size to commit now, if any: the drag has settled (no new size for
    /// `settle_ms`) or the max interval since the gesture began has elapsed.
    /// Returns the target and clears the pending size; otherwise returns `None`.
    pub fn poll(&mut self, now_ms: u64) -> Option<Target> {
        let target = self.pending?;
        let settled = now_ms.saturating_sub(self.last_note_ms) >= self.settle_ms;
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

    #[test]
    fn nothing_to_commit_without_a_resize() {
        let mut c = ResizeCoalescer::new(SETTLE_MS, MAX_MS);
        assert_eq!(c.poll(0), None);
        assert_eq!(c.poll(10_000), None);
    }

    #[test]
    fn a_single_resize_commits_after_settling() {
        let mut c = ResizeCoalescer::new(80, 250);
        c.note(0, 1000, 600, 1.0);
        // Before the settle window elapses, nothing commits.
        assert_eq!(c.poll(0), None);
        assert_eq!(c.poll(79), None);
        // Once it has been quiet for the settle window, the size commits.
        assert_eq!(c.poll(80), Some((1000, 600, 1.0)));
        assert_eq!(c.poll(1000), None, "the committed size is not re-emitted");
    }

    #[test]
    fn rapid_resizes_coalesce_to_the_latest_size() {
        let mut c = ResizeCoalescer::new(80, 250);
        // A drag streams sizes faster than the settle window.
        c.note(0, 1000, 600, 1.0);
        c.note(20, 1010, 600, 1.0);
        c.note(40, 1020, 600, 1.0);
        // Still moving — no commit yet (settle resets on every note).
        assert_eq!(c.poll(60), None);
        // 80 ms after the *last* note, the most recent size (only) commits.
        assert_eq!(c.poll(120), Some((1020, 600, 1.0)));
        assert_eq!(c.poll(121), None, "one commit drains the pending size");
    }

    #[test]
    fn a_pause_mid_drag_commits_then_a_resumed_drag_commits_again() {
        let mut c = ResizeCoalescer::new(80, 250);
        c.note(0, 1000, 600, 1.0);
        assert_eq!(c.poll(80), Some((1000, 600, 1.0)));
        // The user resumes dragging after the pause: a fresh gesture commits anew.
        c.note(200, 1200, 700, 1.0);
        assert_eq!(c.poll(279), None);
        assert_eq!(c.poll(280), Some((1200, 700, 1.0)));
    }

    #[test]
    fn a_long_continuous_drag_refreshes_at_the_max_interval() {
        let mut c = ResizeCoalescer::new(80, 250);
        // Sizes arrive every 16 ms and never pause, so the settle check never fires.
        let mut committed = None;
        for step in 0..40u64 {
            let t = step * 16;
            c.note(t, 1000 + step as u32, 600, 1.0);
            // Poll the way the event loop does, a touch after the note.
            if let Some(target) = c.poll(t + 1) {
                committed = Some((t, target));
                break;
            }
        }
        let (t, target) =
            committed.expect("a continuous drag must still commit via the max interval");
        assert!(
            (MAX_MS..MAX_MS + 16).contains(&t),
            "first refresh should land around the max interval, not before; got {t}ms"
        );
        // It committed the size current at that moment, not the stale anchor size.
        assert_eq!(target.0, 1000 + (t / 16) as u32);
    }

    #[test]
    fn the_max_interval_re_anchors_so_refreshes_are_periodic() {
        let mut c = ResizeCoalescer::new(80, 250);
        // A continuous drag: first refresh at ~250ms, the next ~250ms after that.
        let mut commits = Vec::new();
        for step in 0..70u64 {
            let t = step * 16;
            c.note(t, step as u32, 0, 1.0);
            if c.poll(t + 1).is_some() {
                commits.push(t);
            }
        }
        assert!(
            commits.len() >= 2,
            "a >1s continuous drag should refresh more than once, got {commits:?}"
        );
        let gap = commits[1] - commits[0];
        assert!(
            (MAX_MS..MAX_MS + 32).contains(&gap),
            "refreshes should be ~MAX_MS apart, got a {gap}ms gap ({commits:?})"
        );
    }

    #[test]
    fn settle_takes_priority_when_a_drag_stops_before_the_max_interval() {
        // A short drag (under MAX_MS) that then stops must commit via settle, once.
        let mut c = ResizeCoalescer::new(80, 250);
        for step in 0..5u64 {
            let t = step * 16; // last note at 64ms (< 250ms max)
            c.note(t, step as u32, 0, 1.0);
            assert_eq!(c.poll(t + 1), None, "still dragging at {t}ms");
        }
        // Stops at 64ms; settle fires at 64+80 = 144ms (< 250ms anchor+max).
        assert_eq!(c.poll(143), None);
        assert_eq!(c.poll(144), Some((4, 0, 1.0)));
        assert_eq!(c.poll(300), None, "no spurious second commit");
    }
}
